//! Agent 单轮执行器：模型流式调用的计时 / 用量来源，与工具调用的缓存、并发和计时。
//!
//! - **只读结果缓存**（同一次运行内）：键 = 工具名 + 规范化参数；命中前核对作品状态戳
//!   （文件版本、待审队列、章节状态与记忆、细纲确认、技能版本 / 启停、本书设置与文件可见性、作品元数据），
//!   任一变化即失效重读；任何有副作用的工具执行后整体清空。只缓存成功结果。
//!   绕过本服务直接改磁盘文件不会更新状态戳（不在缓存保证范围内）。
//! - **有限并发**：同一轮里连续的只读调用最多 `PARALLEL` 个同时执行（作用域线程）；数据库查询仍经单连接锁
//!   串行，文件读取可以重叠。有副作用的调用（提案、草稿、起草正文）逐个顺序执行。
//!   结果始终按模型给出的原顺序回填，tool_call_id 一一对应。
//! - **指标**：每次模型调用的首字时间（首个流式片段）、首个正文片段时间、总耗时、提示 / 产出字数、
//!   用量来源（上游报告或估算）；每次工具调用的耗时、是否命中缓存、是否并发执行；重试次数与运行总耗时。
use super::agent_runtime::{self, RunSpec};
use super::agent_tools;
use super::chat::{self, Ctx};
use anyhow::{anyhow, Result};
use molan_core::db::Db;
use molan_llm::{ChatParams, CompletionState, LlmEvent, StreamOpts};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

/// 同一轮只读调用的并发上限。
pub(crate) const PARALLEL: usize = 4;

pub(crate) fn is_read_only(name: &str) -> bool {
    agent_runtime::READ_TOOLS.contains(&name)
}

/// 作品状态戳：任一相关事实变化都会改变它；查询失败返回唯一值（等于不缓存）。
pub(crate) fn state_stamp(db: &Db, book: &str) -> String {
    // 逐项查询：按需建表的表（如 outline_confirm）尚不存在时该项记为空，不影响其余各项
    const PARTS: [&str; 8] = [
        "SELECT COUNT(*)||':'||COALESCE(SUM(revision),0)||':'||COALESCE(MAX(updated_at),0) AS v FROM file_revision WHERE book_id=?1",
        "SELECT COUNT(*)||':'||COALESCE(MAX(updated_at),0) AS v FROM pending_chapter WHERE book_id=?1",
        "SELECT COUNT(*)||':'||COALESCE(MAX(updated_at),0) AS v FROM chapter_state WHERE book_id=?1",
        "SELECT COUNT(*)||':'||COALESCE(MAX(updated_at),0) AS v FROM chapter_memory WHERE book_id=?1",
        "SELECT COUNT(*)||':'||COALESCE(MAX(confirmed_at),0) AS v FROM outline_confirm WHERE book_id=?1",
        "SELECT COUNT(*)||':'||COALESCE(SUM(rev),0)||':'||COALESCE(SUM(enabled),0) AS v FROM skills WHERE ?1 IS NOT NULL",
        "SELECT COALESCE(group_concat(key||'='||value, '|'),'') AS v FROM (SELECT key, value FROM settings WHERE instr(key, ?1) > 0 ORDER BY key)",
        "SELECT COALESCE(MAX(updated_at),0) AS v FROM books WHERE id=?1",
    ];
    let parts: Vec<String> = PARTS
        .iter()
        .map(|sql| {
            db.q_json(sql, &[&book as &dyn rusqlite::ToSql])
                .ok()
                .and_then(|r| r.first().map(|x| x["v"].to_string()))
                .unwrap_or_default()
        })
        .collect();
    molan_core::continuity::content_hash(&parts.join("\u{1f}"))
}

/// 同一次运行内的只读结果缓存。
#[derive(Default)]
pub(crate) struct ToolCache {
    map: HashMap<String, (String, Value)>,
}

impl ToolCache {
    fn key(name: &str, args: &Value) -> String {
        // serde_json 对象键有序（未开 preserve_order）：同参数不同键序得到同一个键
        format!("{}\u{1f}{}", name, args)
    }
    fn get(&self, stamp: &str, name: &str, args: &Value) -> Option<Value> {
        self.map
            .get(&Self::key(name, args))
            .filter(|(s, _)| s == stamp)
            .map(|(_, v)| v.clone())
    }
    fn put(&mut self, stamp: &str, name: &str, args: &Value, v: &Value) {
        self.map
            .insert(Self::key(name, args), (stamp.to_string(), v.clone()));
    }
    pub(crate) fn clear(&mut self) {
        self.map.clear();
    }
}

/// 运行指标（随回执与 run_status 返回）。
pub(crate) struct Metrics {
    started: Instant,
    first_token_ms: Option<u128>,
    model_calls: Vec<Value>,
    tools: Vec<Value>,
    pub(crate) retries: u32,
}

impl Metrics {
    pub(crate) fn start() -> Self {
        Metrics {
            started: Instant::now(),
            first_token_ms: None,
            model_calls: Vec::new(),
            tools: Vec::new(),
            retries: 0,
        }
    }
    pub(crate) fn to_json(&self) -> Value {
        let tool_ms: u128 = self
            .tools
            .iter()
            .filter_map(|t| t["ms"].as_u64())
            .map(u128::from)
            .sum();
        json!({
            "totalMs": self.started.elapsed().as_millis() as u64,
            "firstTokenMs": self.first_token_ms.map(|v| v as u64),
            "modelCalls": self.model_calls,
            "tools": self.tools,
            "toolMs": tool_ms as u64,
            "cacheHits": self.tools.iter().filter(|t| t["cached"] == true).count(),
            "retries": self.retries,
            "usageEstimated": self.model_calls.iter().any(|c| c["usageSource"] == "estimated"),
        })
    }
}

/// 一次模型调用的结果。
pub(crate) struct RoundOut {
    pub(crate) res: Result<CompletionState>,
    pub(crate) full: String,
    pub(crate) usage: Option<Value>,
    pub(crate) calls: Option<Value>,
}

fn chars_of(msgs: &[Value]) -> usize {
    msgs.iter()
        .map(|m| {
            m["content"]
                .as_str()
                .map(|c| c.chars().count())
                .unwrap_or(0)
        })
        .sum()
}

/// 流式调用一次模型：增量 / 思考转发给前端；记录首字时间、总耗时、用量来源。
pub(crate) async fn stream_round(
    cx: &mut Ctx<'_>,
    params: ChatParams,
    opts: StreamOpts,
    metrics: &mut Metrics,
) -> RoundOut {
    let t0 = Instant::now();
    let prompt_chars = chars_of(&params.messages);
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let sse = async move {
        let r = molan_llm::chat_completion_stream_opts(params, &ev_tx, opts).await;
        drop(ev_tx);
        r
    };
    tokio::pin!(sse);
    let (mut full, mut usage, mut calls) = (String::new(), None, None);
    let (mut first_any, mut first_text): (Option<u128>, Option<u128>) = (None, None);
    let mut handle = |ev: LlmEvent, full: &mut String, out: &mut Vec<(bool, String)>| {
        let now = t0.elapsed().as_millis();
        match ev {
            LlmEvent::Delta(t) => {
                first_any.get_or_insert(now);
                first_text.get_or_insert(now);
                full.push_str(&t);
                out.push((true, t));
            }
            LlmEvent::Reasoning(t) => {
                first_any.get_or_insert(now);
                out.push((false, t));
            }
            LlmEvent::Meta(m) => {
                if m.is_some() {
                    usage = m;
                }
            }
            LlmEvent::ToolCalls(v) => calls = Some(v),
        }
    };
    let res = loop {
        let mut out = Vec::new();
        tokio::select! {
            r = &mut sse => break r,
            ev = ev_rx.recv() => if let Some(ev) = ev { handle(ev, &mut full, &mut out) },
        }
        forward(cx, out).await;
    };
    while let Some(ev) = ev_rx.recv().await {
        let mut out = Vec::new();
        handle(ev, &mut full, &mut out);
        forward(cx, out).await;
    }
    if metrics.first_token_ms.is_none() {
        if let Some(ms) = first_any {
            // 运行级首字时间：从收到请求起算（含计划冻结、上下文组装等准备工作）
            metrics.first_token_ms =
                Some(metrics.started.elapsed().as_millis() - (t0.elapsed().as_millis() - ms));
        }
    }
    let outcome = match &res {
        Ok(CompletionState::ToolCalls) => "toolCalls".to_string(),
        Ok(_) if full.trim().is_empty() => "empty".to_string(),
        Ok(_) => "ok".to_string(),
        Err(e) => format!(
            "error: {}",
            e.to_string().chars().take(80).collect::<String>()
        ),
    };
    metrics.model_calls.push(json!({
        "firstTokenMs": first_any.map(|v| v as u64), "firstTextMs": first_text.map(|v| v as u64),
        "totalMs": t0.elapsed().as_millis() as u64, "promptChars": prompt_chars,
        "outputChars": full.chars().count(), "outcome": outcome,
        "usage": usage, "usageSource": if usage.is_some() { "upstream" } else { "estimated" },
    }));
    RoundOut {
        res,
        full,
        usage,
        calls,
    }
}

async fn forward(cx: &mut Ctx<'_>, out: Vec<(bool, String)>) {
    for (is_delta, t) in out {
        if is_delta {
            cx.delta(&t).await
        } else {
            cx.reasoning(&t).await
        }
    }
}

/// 一次工具调用的结果（按模型给出的原顺序）。
pub(crate) struct ToolOut {
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) result: Result<Value>,
}

/// 一组只读调用：先查缓存，未命中的按 `PARALLEL` 分批在作用域线程里并发执行。
/// 返回 (结果, 耗时 ms, 是否命中缓存)，顺序与输入一致。
pub(crate) fn run_read_group(
    db: &Db,
    book: &str,
    items: &[(String, Value)],
    cache: &mut ToolCache,
) -> Vec<(Result<Value>, u128, bool)> {
    let stamp = state_stamp(db, book);
    let mut out: Vec<Option<(Result<Value>, u128, bool)>> = Vec::new();
    let mut todo = Vec::new();
    // 同一轮里重复的同参调用只执行一次，其余复用结果（记为命中缓存）
    let mut first_of: HashMap<String, usize> = HashMap::new();
    let mut dups: Vec<(usize, usize)> = Vec::new();
    for (i, (name, args)) in items.iter().enumerate() {
        if let Some(v) = cache.get(&stamp, name, args) {
            out.push(Some((Ok(v), 0, true)));
            continue;
        }
        out.push(None);
        match first_of.entry(ToolCache::key(name, args)) {
            std::collections::hash_map::Entry::Occupied(e) => dups.push((i, *e.get())),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(i);
                todo.push(i);
            }
        }
    }
    let run = |i: usize| {
        let t = Instant::now();
        let r = agent_tools::dispatch_tool(db, book, &items[i].0, &items[i].1);
        (r, t.elapsed().as_millis())
    };
    for chunk in todo.chunks(PARALLEL) {
        let done: Vec<(usize, Result<Value>, u128)> = if chunk.len() == 1 {
            let (r, ms) = run(chunk[0]);
            vec![(chunk[0], r, ms)]
        } else {
            std::thread::scope(|s| {
                let hs: Vec<_> = chunk
                    .iter()
                    .map(|&i| (i, s.spawn(move || run(i))))
                    .collect();
                hs.into_iter()
                    .map(|(i, h)| match h.join() {
                        Ok((r, ms)) => (i, r, ms),
                        Err(_) => (i, Err(anyhow!("工具执行线程异常退出")), 0),
                    })
                    .collect()
            })
        };
        for (i, r, ms) in done {
            if let Ok(v) = &r {
                cache.put(&stamp, &items[i].0, &items[i].1, v);
            }
            out[i] = Some((r, ms, false));
        }
    }
    for (i, f) in dups {
        out[i] = Some(match &out[f] {
            Some((Ok(v), _, _)) => (Ok(v.clone()), 0, true),
            Some((Err(e), _, _)) => (Err(anyhow!(e.to_string())), 0, true),
            None => (Err(anyhow!("工具未执行")), 0, false),
        });
    }
    out.into_iter()
        .map(|o| o.unwrap_or_else(|| (Err(anyhow!("工具未执行")), 0, false)))
        .collect()
}

/// 执行一轮的工具调用：作用域校验 → 连续只读调用成组（缓存 + 有限并发）→ 有副作用的逐个顺序执行。
/// 取消在每组 / 每个调用前检查；返回 (按原顺序的结果, 是否被取消)。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_tools(
    cx: &Ctx<'_>,
    db: &Db,
    book: &str,
    spec: &RunSpec,
    calls: &[Value],
    cache: &mut ToolCache,
    metrics: &mut Metrics,
    io: (&CancellationToken, &Sender<String>, &str, &str),
) -> (Vec<ToolOut>, bool) {
    let (cancel, tx, channel, request_id) = io;
    let planned: Vec<(String, String, Value)> = calls
        .iter()
        .map(|c| {
            let args = serde_json::from_str(c["function"]["arguments"].as_str().unwrap_or("{}"))
                .unwrap_or_else(|_| json!({}));
            let id = c["id"].as_str().unwrap_or("").to_string();
            (
                id,
                c["function"]["name"].as_str().unwrap_or("").to_string(),
                args,
            )
        })
        .collect();
    let mut outs: Vec<ToolOut> = Vec::new();
    let mut i = 0;
    while i < planned.len() {
        if cancel.is_cancelled() || chat::take_abort(request_id) {
            return (outs, true);
        }
        let ok = |j: usize| spec.check_call(&planned[j].1, &planned[j].2).is_ok();
        let mut j = i;
        while j < planned.len() && is_read_only(&planned[j].1) && ok(j) {
            j += 1;
        }
        let group: Vec<usize> = (i..j).collect();
        let group = if group.is_empty() { vec![i] } else { group };
        for &k in &group {
            cx.ev(
                json!({"type":"tool","callId":planned[k].0,"name":planned[k].1,"status":"running"}),
            )
            .await;
        }
        if j > i {
            let items: Vec<(String, Value)> = group
                .iter()
                .map(|&k| (planned[k].1.clone(), planned[k].2.clone()))
                .collect();
            let parallel = items.len() > 1;
            for (k, (r, ms, cached)) in group.iter().zip(run_read_group(db, book, &items, cache)) {
                metrics.tools.push(json!({"name": planned[*k].1, "ms": ms as u64, "cached": cached, "parallel": parallel && !cached, "ok": r.is_ok()}));
                outs.push(ToolOut {
                    call_id: planned[*k].0.clone(),
                    name: planned[*k].1.clone(),
                    result: r,
                });
            }
            i = j;
        } else {
            let (id, name, args) = &planned[i];
            let t = Instant::now();
            let r = match spec.check_call(name, args) {
                Ok(()) => {
                    agent_tools::dispatch_tool_io(db, book, name, args, cancel, tx, channel).await
                }
                Err(e) => Err(e),
            };
            if !is_read_only(name) {
                cache.clear(); // 有副作用（或被拒绝的写）之后不再信任任何缓存
            }
            metrics.tools.push(json!({"name": name, "ms": t.elapsed().as_millis() as u64, "cached": false, "parallel": false, "ok": r.is_ok()}));
            outs.push(ToolOut {
                call_id: id.clone(),
                name: name.clone(),
                result: r,
            });
            i += 1;
        }
    }
    (outs, false)
}

#[cfg(test)]
#[path = "tool_exec_tests.rs"]
mod tests;
