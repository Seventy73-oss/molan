//! Agent 对话主循环：独立命令 agent_turn，不改 chat_stream。
//!
//! 循环有限且可恢复：工具轮数上限 + token 预算 + 取消令牌贯穿每轮与每个工具执行前；
//! 运行态落 molan_core::agent_run（幂等键 session+request，重放返回回执不重跑）；
//! 任务作用域、工具权限、计划/上下文冻结与产物落库见 agent_runtime。
//!
//! 错误语义（上游问题如实呈现，系统只做同模型有限重试，绝不静默换模型）：
//! - 取消 → interrupted（保留残片为可恢复产物，不冒充完成）；
//! - 瞬态且零输出 / 空回复 → RetryPolicy 同模型退避重试（计入本 run）；
//! - 不完整（截断/过滤/裸EOF/工具参数坏）→ interrupted + 残片保留；
//! - 渠道不支持 tools → 结构化 TOOLS_UNSUPPORTED，可由作者选择「直接生成」模式重试；
//! - token 预算耗尽 → budget_exhausted；工具轮数用尽 → 给模型一次无工具收尾轮。
//!
//! 事件协议（NDJSON {"ch":..,"e":{..}}）：meta / plan / context / delta / reasoning / step /
//! tool{callId,name,status,summary,artifact} / artifact{artifact} / notice / error{code} / interrupted / done。
use super::agent_runtime::{self, RunSpec};
use super::agent_tools;
use super::chat::{self, Ctx};
use super::tool_exec::{self, Metrics, ToolCache};
use crate::handlers::channel_id;
use anyhow::{anyhow, Result};
use molan_core::agent_run::{self, BeginRun, BUDGET_TOKENS_DEFAULT, MAX_TOOL_ROUNDS_DEFAULT};
use molan_core::db::Db;
use molan_core::stats;
use molan_llm::{ChatParams, StreamOpts};
use serde_json::{json, Value};
use std::sync::Arc;

pub(crate) use super::agent_runtime::run_status;

/// abort_chat 兜底：requestId 为空且带 sessionId 时，取消该会话**在飞**的 agent run。
/// 只取消在飞令牌、不登记任何会话标记：旧实现在飞取消后标记无人消费（循环先命中令牌退出），
/// 会让同会话下一条请求零输出即中断；无在飞 run 时同样不留痕迹（F1）。
pub(crate) fn abort_by_args(args: &Value) {
    let has_req = args
        .get("requestId")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    let sid = args
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if has_req || sid.is_empty() {
        return;
    }
    agent_runtime::abort_session(&sid);
}

/// RAII：run 收尾（含 panic/提前返回）注销 requestId 令牌并清掉未消费的取消标记。
struct AbortGuard(String);
impl Drop for AbortGuard {
    fn drop(&mut self) {
        chat::unregister_abort(&self.0);
        let _ = chat::take_abort(&self.0);
    }
}

/// dispatch_stream 入口（st 提供 root/记忆补发；核心逻辑在 run_agent_turn，便于直测）。
pub(crate) async fn agent_turn(
    st: &Arc<super::super::AppState>,
    _cmd: &str,
    args: &Value,
    tx: &tokio::sync::mpsc::Sender<String>,
) -> Result<Option<Value>> {
    let book = args
        .get("bookId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let r = run_agent_turn(&st.db, &st.root, args, tx).await;
    // 手动线收尾：对话内定稿的章由审批 saga 预置 memory_job=pending，这里补发真实抽取
    if r.is_ok() && !book.is_empty() {
        super::chapter_service::sweep_pending_memory(st, &book).await;
    }
    r
}

/// 上游是否明示不支持 tools（400/参数错误 + tool 关键词）。判定保守。
fn looks_tools_unsupported(e: &anyhow::Error) -> bool {
    let m = e.to_string().to_lowercase();
    (m.contains("400") || m.contains("bad request") || m.contains("invalid_request_error"))
        && (m.contains("tool") || m.contains("function calling"))
}

fn setting_i64(db: &Db, key: &str, ok: impl Fn(i64) -> bool, default: i64) -> i64 {
    molan_llm::get_setting(db, key)
        .parse()
        .ok()
        .filter(|n| ok(*n))
        .unwrap_or(default)
}

fn msgs_chars(msgs: &[Value]) -> usize {
    msgs.iter()
        .map(|m| {
            m["content"]
                .as_str()
                .map(|c| c.chars().count())
                .unwrap_or(0)
        })
        .sum()
}

pub(crate) async fn run_agent_turn(
    db: &Db,
    root: &std::path::Path,
    args: &Value,
    tx: &tokio::sync::mpsc::Sender<String>,
) -> Result<Option<Value>> {
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").trim().to_string();
    if a("onEvent").is_null() {
        return Err(anyhow!("缺少事件通道"));
    }
    if chat::shutting_down() {
        return Err(anyhow!("服务正在优雅停机，请稍后重试"));
    }
    let channel = channel_id(&a("onEvent"));
    let mut cx = Ctx::new(tx, &channel);
    let mut metrics = Metrics::start(); // 运行指标从收到请求起计时
    let session_id = s("sessionId");
    let book_id = s("bookId");
    let message = Some(s("message"))
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| s("displayText"));
    if session_id.is_empty() {
        return Err(anyhow!("未选择会话，请重新选择本书会话"));
    }
    if message.is_empty() {
        return Err(anyhow!("消息为空"));
    }
    let rows = db.q_json(
        "SELECT book_id FROM sessions WHERE id=?1",
        &[&session_id as &dyn rusqlite::ToSql],
    )?;
    if rows.first().and_then(|r| r["bookId"].as_str()) != Some(book_id.as_str()) {
        return Err(anyhow!("会话不存在或不属于当前书，请刷新会话列表"));
    }
    if book_id.is_empty() || !molan_core::files::valid_book_id(db, &book_id) {
        return Err(anyhow!("书籍不存在或已删除"));
    }
    let spec = RunSpec::from_args(args)?;
    let request_id = Some(s("requestId"))
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| format!("auto-{}", uuid::Uuid::new_v4()));
    let max_rounds = setting_i64(
        db,
        "agent_max_tool_rounds",
        |n| (1..=32).contains(&n),
        MAX_TOOL_ROUNDS_DEFAULT,
    );
    let budget = setting_i64(db, "agent_budget_tokens", |n| n >= 0, BUDGET_TOKENS_DEFAULT);
    let chn = molan_llm::resolve_agent_channel(db, "chat")
        .or_else(|| molan_llm::active_channel(db))
        .filter(|c| !c["baseUrl"].as_str().unwrap_or("").is_empty())
        .ok_or_else(|| {
            anyhow!("未配置可用的模型渠道。请在 设置→渠道管理 里添加你自己的 API（baseUrl + Key）")
        })?;
    let api_key = chn["key"].as_str().unwrap_or("").to_string();
    let model = chn["model"].as_str().unwrap_or("").to_string();

    // 同会话已有别的请求在运行：拒绝并告知，不靠前端禁用按钮防重
    if let Some((other_req, other_run)) = agent_runtime::session_busy(&session_id, &request_id) {
        cx.ev(json!({"type":"error","code":"SESSION_BUSY","message":"本会话已有任务在运行，请等待完成或先停止","runId":other_run})).await;
        cx.ev(json!({"type":"done","status":"session_busy","runId":other_run}))
            .await;
        return Ok(Some(
            json!({"ok": false, "status": "session_busy", "runId": other_run, "requestId": other_req}),
        ));
    }

    // 幂等 run：终态重放直接返回回执，绝不重跑模型
    let run = agent_run::begin_run(
        db,
        &BeginRun {
            book_id: book_id.clone(),
            session_id: session_id.clone(),
            request_id: request_id.clone(),
            task: spec.task_id().into(),
            model: model.clone(),
            target_ch: Some(spec.ch()).filter(|c| *c > 0),
            max_tool_rounds: max_rounds,
            budget_tokens: budget,
        },
    )?;
    let run_id = run["id"].as_str().unwrap_or("").to_string();
    if run["status"].as_str().unwrap_or("running") != "running" {
        let receipt =
            json!({"ok": true, "runId": run_id, "status": run["status"], "replayed": true});
        cx.done(receipt.clone()).await;
        return Ok(Some(receipt));
    }
    // 同一 requestId 的重入（前端重试/双开/网络重发）：绝不进入第二个循环
    if !run["created"].as_bool().unwrap_or(false) {
        cx.ev(json!({"type":"done","status":"already_running","runId":run_id}))
            .await;
        return Ok(Some(
            json!({"ok": false, "status": "already_running", "runId": run_id}),
        ));
    }
    let cancel = chat::register_abort(&request_id);
    let _abort_guard = AbortGuard(request_id.clone());
    let _live = agent_runtime::register_live(&session_id, &request_id, &run_id, cancel.clone());

    // 计划与上下文冻结；必要依赖缺失 → 不调用模型，如实告知缺什么
    let prep = match agent_runtime::prepare(db, root, &book_id, &session_id, &spec, &model) {
        Ok(p) if p.blockers.is_empty() => p,
        Ok(p) => {
            let why = p.blockers.join("；");
            agent_run::finish_run(db, &run_id, "error", &why)?;
            cx.ev(json!({"type":"error","code":"CONTEXT_BLOCKED","message":why,"blockers":p.blockers})).await;
            cx.ev(json!({"type":"done","status":"error","runId":run_id}))
                .await;
            return Ok(Some(
                json!({"ok": false, "status": "error", "code": "CONTEXT_BLOCKED", "runId": run_id, "error": why}),
            ));
        }
        Err(e) => {
            agent_run::finish_run(db, &run_id, "error", &e.to_string())?;
            return Err(e);
        }
    };
    let plan_hash = prep.plan["planHash"].as_str().unwrap_or("").to_string();
    let mode = if spec.mode == agent_runtime::Mode::Direct {
        "direct"
    } else {
        "agent"
    };
    let _ = agent_run::set_plan(db, &run_id, &plan_hash, &prep.manifest_id, mode);

    // 用户消息落库（与 chat_stream 同构）
    let user_message_id = uuid::Uuid::new_v4().to_string();
    let now = stats::now_ms();
    let skill_ids: Vec<Value> = prep.plan["skills"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| s["id"].clone())
        .collect();
    let ctx_json = json!({
        "requestId": request_id, "runId": run_id, "agent": true, "task": spec.task_id(),
        "target": spec.target, "skills": skill_ids, "planHash": plan_hash, "files": spec.files, "mode": mode,
    })
    .to_string();
    db.exec(
        "INSERT INTO messages(id,session_id,role,content,context_json,steps_json,result_json,created_at,interrupted) VALUES(?,?,?,?,?,NULL,NULL,?,0)",
        &[&user_message_id as &dyn rusqlite::ToSql, &session_id, &"user", &message, &ctx_json, &now],
    )?;
    db.exec(
        "UPDATE sessions SET msg_count=(SELECT COUNT(*) FROM messages WHERE session_id=?1), updated_at=?2 WHERE id=?1",
        &[&session_id as &dyn rusqlite::ToSql, &now],
    )?;

    // 心跳：工具轮可能静默数十秒，防客户端判流已死
    struct HbGuard(tokio::task::JoinHandle<()>);
    impl Drop for HbGuard {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let hb_tx = tx.clone();
    let _hb = HbGuard(tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            let _ = hb_tx.try_send(format!(
                "{}\n",
                json!({"ch": "__hb__", "e": {"type": "progress", "chars": -1}})
            ));
        }
    }));

    let mut msgs: Vec<Value> = vec![json!({"role": "system", "content": prep.system})];
    let hist_rows = db
        .q_json(
            "SELECT id, role, content FROM messages WHERE session_id=?1 AND role IN ('user','assistant') ORDER BY created_at DESC, rowid DESC LIMIT 16",
            &[&session_id as &dyn rusqlite::ToSql],
        )
        .unwrap_or_default();
    msgs.extend(chat::chat_contract::history(&hist_rows, &user_message_id));
    msgs.push(json!({"role": "user", "content": message}));

    let catalog = spec.catalog();
    let tools_count = catalog
        .as_ref()
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let temp: f64 = molan_llm::get_setting(db, "temperature")
        .parse()
        .unwrap_or(0.7);
    let max_tokens: i64 = molan_llm::get_setting(db, "max_tokens")
        .parse()
        .unwrap_or(8192);
    cx.ev(json!({"type":"meta","task":spec.task_id(),"taskLabel":spec.task.map(|t| t.label()).unwrap_or("创作助手"),
        "model":model,"runId":run_id,"requestId":request_id,"tools":tools_count,"mode":mode,"planHash":plan_hash})).await;
    cx.ev(json!({"type":"plan","plan":molan_core::skill_resolver::public_view(&prep.plan)}))
        .await;
    cx.ev(json!({"type":"context","manifestId":prep.manifest_id,"blocks":prep.blocks}))
        .await;

    // 重试次数可配置（首次之外额外 N 次，0..=3）；只同模型退避，绝不换模型
    let extra = setting_i64(db, "agent_retry_extra", |n| (0..=3).contains(&n), 2) as usize;
    let retry = molan_llm::retry::RetryPolicy::with_extra_retries(extra);
    let message_id = uuid::Uuid::new_v4().to_string();
    let (mut retries, mut used_tokens, mut final_round) = (0u32, 0i64, false);
    let mut full = String::new();
    let mut steps: Vec<Value> = Vec::new();
    let mut artifacts: Vec<Value> = Vec::new();
    let mut status = "done";
    let mut err_text = String::new();
    let mut cache = ToolCache::default();

    loop {
        if cancel.is_cancelled() || chat::take_abort(&request_id) {
            status = "interrupted";
            err_text = "作者已停止".into();
            break;
        }
        if agent_run::budget_exhausted(db, &run_id)? {
            status = "budget_exhausted";
            err_text = "token 预算耗尽".into();
            cx.ev(json!({"type":"error","code":"BUDGET_EXHAUSTED","message":err_text}))
                .await;
            break;
        }
        if !final_round && agent_run::rounds_exhausted(db, &run_id)? {
            // 轮数用尽：给模型一次无工具的收尾轮，基于已有结果作答（而不是直接截断）
            final_round = true;
            cx.ev(json!({"type":"notice","code":"ROUNDS_EXHAUSTED","message":"工具轮数已达上限，模型将基于已有结果收尾"})).await;
            msgs.push(json!({"role":"user","content":"（系统）工具轮数已用尽：请基于已有工具结果直接给出最终回答，不要再调用工具。"}));
        }
        let tools = if final_round { None } else { catalog.clone() };
        let params = ChatParams {
            base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
            api_key: api_key.clone(),
            model: model.clone(),
            messages: msgs.clone(),
            temperature: if temp == 0.0 { 0.7 } else { temp },
            max_tokens: if max_tokens == 0 { 8192 } else { max_tokens },
            stream: true,
            no_thinking: false,
            reasoning_effort: "max".to_string(),
            log_tag: String::new(),
            log_book: book_id.clone(),
            cancel: Some(cancel.clone()),
        };
        let opts = StreamOpts {
            allow_missing_finish_reason: false,
            accept_tool_calls: tools.is_some(),
            tools,
            tool_choice: None,
        };
        let out = tool_exec::stream_round(&mut cx, params, opts, &mut metrics).await;
        full = out.full;
        let (res, round_usage, pending_calls) = (out.res, out.usage, out.calls);
        let est_p = agent_run::estimate_tokens(msgs_chars(&msgs));
        let est_c = agent_run::estimate_tokens(full.chars().count());
        used_tokens = agent_run::charge_usage_est(db, &run_id, round_usage.as_ref(), est_p, est_c)?;

        match res {
            Ok(molan_llm::CompletionState::ToolCalls) if !final_round => {
                let calls = pending_calls.unwrap_or_else(|| json!([]));
                let mut calls_arr: Vec<Value> = calls.as_array().cloned().unwrap_or_default();
                calls_arr.truncate(agent_tools::MAX_CALLS_PER_ROUND);
                if calls_arr.is_empty() {
                    status = "error";
                    err_text = "上游报告工具调用但没有调用内容".into();
                    break;
                }
                // 轨迹与对话一致：记录实际执行的（截断后的）调用及同轮文本
                agent_run::record_turn(
                    db,
                    &run_id,
                    "assistant",
                    &full,
                    &json!(calls_arr).to_string(),
                    "",
                )?;
                msgs.push(json!({"role": "assistant", "content": if full.is_empty() { Value::Null } else { json!(full) }, "tool_calls": calls_arr.clone()}));
                // 只读调用：缓存 + 同轮有限并发；有副作用的逐个顺序执行；结果按原顺序回填
                let io = (&cancel, tx, channel.as_str(), request_id.as_str());
                let (outs, aborted) = tool_exec::run_tools(
                    &cx,
                    db,
                    &book_id,
                    &spec,
                    &calls_arr,
                    &mut cache,
                    &mut metrics,
                    io,
                )
                .await;
                for o in outs {
                    let (call_id, fname) = (o.call_id, o.name);
                    let (payload, ev_status) = match &o.result {
                        Ok(v) => (v.clone(), "ok"),
                        Err(e) => (
                            json!({"error": agent_tools::sanitize(&api_key, e)}),
                            "error",
                        ),
                    };
                    let summary = agent_tools::tool_summary(&payload);
                    let artifact = agent_tools::artifact_of(&fname, &payload);
                    if ev_status == "ok" {
                        if let Some(view) = agent_runtime::adopt_tool_result(
                            db,
                            &book_id,
                            &session_id,
                            &run_id,
                            &message_id,
                            &fname,
                            &payload,
                            &model,
                        ) {
                            used_tokens = agent_runtime::charge_tool_usage(db, &run_id, &payload)?;
                            cx.ev(json!({"type":"artifact","artifact":view})).await;
                            artifacts.push(view["id"].clone());
                        }
                    }
                    steps.push(json!({"type":"tool","callId":call_id,"name":fname,"status":ev_status,"summary":summary,"artifact":artifact}));
                    cx.ev(json!({"type":"tool","callId":call_id,"name":fname,"status":ev_status,"summary":summary,"artifact":artifact})).await;
                    let content_str = agent_tools::truncate_chars(
                        &payload.to_string(),
                        agent_tools::MAX_RESULT_CHARS,
                    );
                    agent_run::record_turn(db, &run_id, "tool", &content_str, "", &call_id)?;
                    msgs.push(json!({"role":"tool","tool_call_id":call_id,"content":content_str}));
                }
                if aborted {
                    status = "interrupted";
                    err_text = "作者已停止".into();
                    break;
                }
                let round = agent_run::bump_round(db, &run_id)?;
                cx.step(round, &format!("完成第 {} 轮工具调用", round))
                    .await;
                retries = 0;
                continue;
            }
            Ok(molan_llm::CompletionState::ToolCalls) => {
                status = "error";
                err_text = "收尾轮仍请求工具调用，已停止".into();
                break;
            }
            Ok(_) => {
                if full.trim().is_empty() && artifacts.is_empty() {
                    // 空回复不是完成：有限重试，仍为空则如实失败
                    if retry.can_retry_transient(retries) {
                        retries += 1;
                        metrics.retries += 1;
                        cx.ev(json!({"type":"error","code":"RETRY","message":format!("模型返回空回复，同模型重试（{}/{}）", retries, retry.transient_delays_ms.len())})).await;
                        continue;
                    }
                    status = "error";
                    err_text = "模型返回了空回复（已重试），未产生任何内容".into();
                    cx.ev(json!({"type":"error","code":"EMPTY_OUTPUT","message":err_text}))
                        .await;
                    break;
                }
                agent_run::record_turn(db, &run_id, "assistant", &full, "", "")?;
                break;
            }
            Err(e) => {
                if molan_llm::is_cancelled_err(&e) || cancel.is_cancelled() {
                    status = "interrupted";
                    err_text = "作者已停止".into();
                    if !full.is_empty() {
                        agent_run::record_turn(db, &run_id, "assistant", &full, "", "")?;
                    }
                    break;
                }
                if looks_tools_unsupported(&e) {
                    status = "tools_unsupported";
                    err_text = "当前模型渠道不支持工具调用（tools）。可换用支持工具的渠道，或选择「直接生成」模式重试；系统不会自动切换模型。".into();
                    cx.ev(json!({"type":"error","code":"TOOLS_UNSUPPORTED","message":err_text}))
                        .await;
                    break;
                }
                if molan_llm::is_incomplete_err(&e) {
                    status = "interrupted";
                    err_text = "上游输出不完整（截断/限流/过滤），已保留残片".into();
                    if !full.is_empty() {
                        agent_run::record_turn(db, &run_id, "assistant", &full, "", "")?;
                    }
                    break;
                }
                if molan_llm::is_transient_error(&e.to_string())
                    && full.is_empty()
                    && retry.can_retry_transient(retries)
                {
                    let delay = retry.delay_for_retry(retries).unwrap_or(2000);
                    retries += 1;
                    metrics.retries += 1;
                    cx.ev(json!({"type":"error","code":"RETRY","message":format!("上游瞬态错误，{}ms 后同模型重试（{}/{}）", delay, retries, retry.transient_delays_ms.len())})).await;
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(delay)) => {}
                        _ = cancel.cancelled() => {
                            status = "interrupted";
                            err_text = "作者已停止".into();
                            break;
                        }
                    }
                    continue;
                }
                status = "error";
                err_text = agent_tools::sanitize(&api_key, &e);
                break;
            }
        }
    }

    let m = metrics.to_json();
    let _ = agent_run::set_metrics(db, &run_id, &m);
    agent_run::finish_run(db, &run_id, status, &err_text)?;
    // 最终文本 → 产物（中断的残片保留为可恢复草稿，不当完整稿）
    let lifecycle = match status {
        "done" => "generated",
        "interrupted" => "interrupted",
        _ => "failed",
    };
    if let Some(view) = agent_runtime::final_artifact(
        db,
        &book_id,
        &session_id,
        &run_id,
        &message_id,
        &spec,
        &prep.plan,
        &full,
        lifecycle,
        &model,
        &prep.manifest_id,
    ) {
        cx.ev(json!({"type":"artifact","artifact":view})).await;
        artifacts.push(view["id"].clone());
    }
    if status == "interrupted" {
        cx.ev(json!({"type":"interrupted","reason":err_text,"partialChars":full.chars().count()}))
            .await;
    } else if status != "done"
        && !err_text.is_empty()
        && status != "tools_unsupported"
        && status != "budget_exhausted"
    {
        cx.error(&err_text).await;
    }

    // assistant 消息：steps_json=工具轨迹，result_json=运行回执与产物 id
    let result_json = json!({
        "agent": true, "runId": run_id, "status": status, "usedTokens": used_tokens, "task": spec.task_id(),
        "planHash": plan_hash, "artifactIds": artifacts, "metrics": m,
        "error": if err_text.is_empty() { Value::Null } else { json!(err_text) },
    })
    .to_string();
    let now = stats::now_ms();
    db.exec(
        "INSERT INTO messages(id,session_id,role,content,context_json,steps_json,result_json,created_at,interrupted) SELECT ?1,?2,'assistant',?3,?4,?5,?6,?7,?8 WHERE EXISTS(SELECT 1 FROM sessions s JOIN books b ON b.id=s.book_id WHERE s.id=?2 AND b.deleted_at IS NULL) ON CONFLICT(id) DO UPDATE SET content=excluded.content,context_json=excluded.context_json,steps_json=excluded.steps_json,result_json=excluded.result_json,interrupted=excluded.interrupted",
        &[
            &message_id as &dyn rusqlite::ToSql, &session_id, &full, &ctx_json, &json!(steps).to_string(),
            &result_json, &now, &(if status == "interrupted" { 1i64 } else { 0i64 }) as &dyn rusqlite::ToSql,
        ],
    )?;
    db.exec(
        "UPDATE sessions SET msg_count=(SELECT COUNT(*) FROM messages WHERE session_id=?1), updated_at=?2 WHERE id=?1",
        &[&session_id as &dyn rusqlite::ToSql, &now],
    )?;
    // done.full 只含最终文本（与落库一致），不含工具轮之间的过程文字
    cx.full = full.clone();
    cx.done(json!({
        "messageId": message_id, "runId": run_id, "status": status, "model": model,
        "outputChars": full.chars().count(), "usedTokens": used_tokens, "artifactIds": artifacts, "metrics": m,
    }))
    .await;
    Ok(Some(
        json!({"ok": status == "done", "runId": run_id, "status": status, "messageId": message_id, "artifactIds": artifacts}),
    ))
}

#[cfg(test)]
#[path = "agent_loop_tests.rs"]
mod tests;
