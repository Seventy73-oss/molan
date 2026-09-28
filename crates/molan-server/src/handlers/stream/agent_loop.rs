//! Agent 对话主循环（交接文档附录 A1/A2.2）：独立命令 agent_turn，不改 chat_stream。
//!
//! 循环有限且可恢复：工具轮数上限 + token 预算 + 取消令牌贯穿每轮与每个工具执行前；
//! 运行态落 molan_core::agent_run（幂等键 session+request，重放返回回执不重跑）；
//! 工具轨迹写 messages.steps_json（books.rs 已回传前端，无需改表）。
//!
//! 错误语义（用户约定：上游问题作者自理，系统只做同模型有限重试）：
//! - 取消 → interrupted（保留残片，不冒充完成）；
//! - 瞬态且零输出 → RetryPolicy 同模型退避重试（计入本 run，不与底层重试相乘）；
//! - 不完整（截断/过滤/裸EOF/工具参数坏）→ interrupted + 残片保留；
//! - 渠道不支持 tools → 结构化 TOOLS_UNSUPPORTED，绝不自动换模型或降级伪装；
//! - 预算/轮数耗尽 → budget_exhausted，run 状态可恢复。
//!
//! 事件协议（NDJSON {"ch":..,"e":{..}}）：meta / delta / reasoning / step /
//! tool{callId,name,status:running|ok|error,summary} / error{code} / interrupted / done。
use super::agent_tools;
use super::chat::{self, Ctx};
use crate::handlers::channel_id;
use anyhow::{anyhow, Result};
use molan_core::agent_run::{self, BeginRun, BUDGET_TOKENS_DEFAULT, MAX_TOOL_ROUNDS_DEFAULT};
use molan_core::db::Db;
use molan_core::stats;
use molan_llm::{ChatParams, LlmEvent, StreamOpts};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// session -> 在飞 agent run 的取消令牌别名表：前端只有 sessionId 时也能定向停止
/// （chat.rs 的 ABORT_TOKENS 按 requestId 索引，无 requestId 的历史调用无法取消）。
static SESSION_TOKENS: std::sync::LazyLock<Mutex<HashMap<String, CancellationToken>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn session_abort_key(session_id: &str) -> String {
    format!("session:{}", session_id)
}

/// abort_chat 兜底：requestId 为空且带 sessionId 时，取消该会话**在飞**的 agent run。
///
/// F1：只有 SESSION_TOKENS 里确实有该会话的在飞令牌时才登记取消标记。无在飞 run 时
/// 直接返回——否则 `chat::request_abort(session:<sid>)` 会把标记永久留在全局 ABORTS 里
/// （只有 run 循环内的 take_abort 会消费它），毒化该会话的下一条 agent_turn：
/// 循环第一行命中该标记，零模型调用、零输出直接判 interrupted，表现为"发了消息 AI 什么都不回"。
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
    // 先取在飞令牌：没有在飞 run 就没有消费者，登记标记等于泄漏。
    let inflight = SESSION_TOKENS
        .lock()
        .ok()
        .and_then(|g| g.get(&sid).cloned());
    let Some(token) = inflight else {
        return;
    };
    chat::request_abort(&session_abort_key(&sid));
    token.cancel();
}

/// RAII：run 收尾（含 panic/提前返回）必须注销双键，防止令牌表泄漏与串号取消。
struct AgentGuard {
    req: String,
    sid: String,
}
impl Drop for AgentGuard {
    fn drop(&mut self) {
        chat::unregister_abort(&self.req);
        if let Ok(mut g) = SESSION_TOKENS.lock() {
            g.remove(&self.sid);
        }
    }
}

/// dispatch_stream 入口（st 仅作 db 载体；核心逻辑在 run_agent_turn，便于直测）。
pub(crate) async fn agent_turn(
    st: &Arc<super::super::AppState>,
    _cmd: &str,
    args: &Value,
    tx: &tokio::sync::mpsc::Sender<String>,
) -> Result<Option<Value>> {
    run_agent_turn(&st.db, args, tx).await
}

fn agent_system_prompt(db: &Db, book_id: &str) -> String {
    let meta = db
        .q_json(
            "SELECT title, genre FROM books WHERE id=?1",
            &[&book_id as &dyn rusqlite::ToSql],
        )
        .unwrap_or_default();
    let title = meta
        .first()
        .and_then(|r| r["title"].as_str())
        .unwrap_or("未命名");
    let genre = meta
        .first()
        .and_then(|r| r["genre"].as_str())
        .filter(|g| !g.is_empty())
        .unwrap_or("未设定");
    format!(
        "你是墨澜工坊的创作 Agent，在书《{}》（题材：{}）范围内工作。\n你有工具可查询书的真实状态与文件、创建变更提案；工具结果是唯一事实来源，不得虚构文件内容、章节状态或回执。\n行为准则：\n1) 行动前先用 get_pipeline_state / scan_book_tree 查状态，用 get_chapter_context 查前文记忆；\n2) 正文与细纲的创作、定稿由作者通过写作流程与审批队列完成；你只做分析、规划与提案（create_change_proposal 产生 pending 提案，作者接受才生效），绝不声称已写入或已批准；\n3) 引用文件时给出 group/name 与关键原文，不确定就明说；\n4) 用与作者相同的语言回答，简洁、面向下一步行动。",
        title, genre
    )
}

/// 上游是否明示不支持 tools（400/参数错误 + tool 关键词）。判定保守：
/// 拿不准就按普通 error 处理，绝不假装成功。
fn looks_tools_unsupported(e: &anyhow::Error) -> bool {
    let m = e.to_string().to_lowercase();
    (m.contains("400") || m.contains("bad request") || m.contains("invalid_request_error"))
        && (m.contains("tool") || m.contains("function calling"))
}

fn sanitize(api_key: &str, e: &anyhow::Error) -> String {
    let msg = e.to_string();
    if api_key.len() >= 8 && msg.contains(api_key) {
        msg.replace(api_key, "***")
    } else {
        msg
    }
}

pub(crate) async fn run_agent_turn(
    db: &Db,
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
    let session_id = s("sessionId");
    let book_id = s("bookId");
    let message = {
        let m = s("message");
        if m.is_empty() {
            s("displayText")
        } else {
            m
        }
    };
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

    let request_id = {
        let r = s("requestId");
        if r.is_empty() {
            format!("auto-{}", uuid::Uuid::new_v4())
        } else {
            r
        }
    };
    let max_rounds: i64 = molan_llm::get_setting(db, "agent_max_tool_rounds")
        .parse()
        .ok()
        .filter(|n: &i64| (1..=32).contains(n))
        .unwrap_or(MAX_TOOL_ROUNDS_DEFAULT);
    let budget: i64 = molan_llm::get_setting(db, "agent_budget_tokens")
        .parse()
        .ok()
        .filter(|n: &i64| *n >= 0)
        .unwrap_or(BUDGET_TOKENS_DEFAULT);
    let chn = molan_llm::resolve_agent_channel(db, "chat")
        .or_else(|| molan_llm::active_channel(db))
        .ok_or_else(|| {
            anyhow!("未配置可用的模型渠道。请在 设置→渠道管理 里添加你自己的 API（baseUrl + Key）")
        })?;
    if chn["baseUrl"].as_str().unwrap_or("").is_empty() {
        return Err(anyhow!(
            "未配置可用的模型渠道。请在 设置→渠道管理 里添加你自己的 API（baseUrl + Key）"
        ));
    }
    let api_key = chn["key"].as_str().unwrap_or("").to_string();
    let model = chn["model"].as_str().unwrap_or("").to_string();

    // 幂等 run：终态重放直接返回回执，绝不重跑模型
    let run = agent_run::begin_run(
        db,
        &BeginRun {
            book_id: book_id.clone(),
            session_id: session_id.clone(),
            request_id: request_id.clone(),
            task: "agent".into(),
            model: model.clone(),
            target_ch: None,
            max_tool_rounds: max_rounds,
            budget_tokens: budget,
        },
    )?;
    let run_id = run["id"].as_str().unwrap_or("").to_string();
    let created = run["created"].as_bool().unwrap_or(false);
    let run_status = run["status"].as_str().unwrap_or("running").to_string();
    if run_status != "running" {
        let receipt =
            json!({"ok": true, "runId": run_id, "status": run["status"], "replayed": true});
        cx.done(receipt.clone()).await;
        return Ok(Some(receipt));
    }
    // F6：既有 running 行 + 非本次新建 = 同一 requestId 的重入（前端重试/双开/网络重发）。
    // 绝不进入循环、绝不插第二条 user 消息：同一 run 只能有一个循环，否则 agent_turn 轮次
    // 交错、record_turn 非原子取号撞主键、同 run 落两条 assistant 消息。
    if !created {
        let receipt = json!({"ok": false, "status": "already_running", "runId": run_id});
        cx.ev(json!({"type":"done","status":"already_running","runId":run_id}))
            .await;
        return Ok(Some(receipt));
    }

    // 用户消息落库（与 chat_stream 同构；files 缺省 [] 防前端白屏）
    let user_message_id = uuid::Uuid::new_v4().to_string();
    let now = stats::now_ms();
    let ctx_json =
        json!({"requestId": request_id, "runId": run_id, "agent": true, "skills": [], "files": []})
            .to_string();
    db.exec(
        "INSERT INTO messages(id,session_id,role,content,context_json,steps_json,result_json,created_at,interrupted) VALUES(?,?,?,?,?,NULL,NULL,?,0)",
        &[
            &user_message_id as &dyn rusqlite::ToSql,
            &session_id, &"user", &message, &ctx_json, &now,
        ],
    )?;
    db.exec(
        "UPDATE sessions SET msg_count=(SELECT COUNT(*) FROM messages WHERE session_id=?1), updated_at=?2 WHERE id=?1",
        &[&session_id as &dyn rusqlite::ToSql, &now],
    )?;

    // 取消双登记：requestId（chat 注册表）+ session 别名表；边界同时消费两个标记
    let cancel = chat::register_abort(&request_id);
    if let Ok(mut g) = SESSION_TOKENS.lock() {
        g.insert(session_id.clone(), cancel.clone());
    }
    let _guard = AgentGuard {
        req: request_id.clone(),
        sid: session_id.clone(),
    };
    let session_key = session_abort_key(&session_id);

    // 心跳：工具轮可能静默数十秒，防客户端判流已死（对齐 chat_stream 的 RAII 心跳）
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

    // system + 历史（近16条，越近越完整）+ 本条
    let sys = agent_system_prompt(db, &book_id);
    let mut msgs: Vec<Value> = vec![json!({"role": "system", "content": sys})];
    let hist_rows = db
        .q_json(
            "SELECT id, role, content FROM messages WHERE session_id=?1 AND role IN ('user','assistant') ORDER BY created_at DESC, rowid DESC LIMIT 16",
            &[&session_id as &dyn rusqlite::ToSql],
        )
        .unwrap_or_default();
    msgs.extend(chat::chat_contract::history(&hist_rows, &user_message_id));
    msgs.push(json!({"role": "user", "content": message}));

    let catalog = agent_tools::tool_catalog();
    let tools_count = catalog.as_array().map(|v| v.len()).unwrap_or(0);
    let temp: f64 = molan_llm::get_setting(db, "temperature")
        .parse()
        .unwrap_or(0.7);
    let max_tokens: i64 = molan_llm::get_setting(db, "max_tokens")
        .parse()
        .unwrap_or(8192);
    cx.ev(json!({"type":"meta","task":"agent","model":model,"runId":run_id,"tools":tools_count}))
        .await;

    let retry = molan_llm::retry::RetryPolicy::default();
    let mut retries: u32 = 0;
    let mut full = String::new();
    let mut steps: Vec<Value> = Vec::new();
    let mut status = "done";
    let mut err_text = String::new();
    let mut used_tokens = 0i64;

    loop {
        if cancel.is_cancelled() || chat::take_abort(&request_id) || chat::take_abort(&session_key)
        {
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
        if agent_run::rounds_exhausted(db, &run_id)? {
            status = "budget_exhausted";
            err_text = "工具轮数已达上限".into();
            cx.ev(json!({"type":"error","code":"ROUNDS_EXHAUSTED","message":err_text}))
                .await;
            break;
        }
        full.clear();
        let mut round_usage: Option<Value> = None;
        let mut pending_calls: Option<Value> = None;
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
            // 用量走 agent_run 账本，不再落 llm_call_log（避免双记账）
            log_tag: String::new(),
            log_book: book_id.clone(),
            cancel: Some(cancel.clone()),
        };
        let opts = StreamOpts {
            allow_missing_finish_reason: false,
            tools: Some(catalog.clone()),
            tool_choice: None,
            accept_tool_calls: true,
        };
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let sse = {
            let tx2 = ev_tx;
            async move {
                let r = molan_llm::chat_completion_stream_opts(params, &tx2, opts).await;
                drop(tx2);
                r
            }
        };
        tokio::pin!(sse);
        // 自建 drain：必须捕获 Meta（用量），chat.rs 的 drain 会丢弃
        let res = loop {
            tokio::select! {
                r = &mut sse => break r,
                ev = ev_rx.recv() => {
                    if let Some(ev) = ev {
                        match ev {
                            LlmEvent::Delta(t) => { full.push_str(&t); cx.delta(&t).await; }
                            LlmEvent::Reasoning(t) => cx.reasoning(&t).await,
                            LlmEvent::Meta(m) => { if m.is_some() { round_usage = m; } }
                            LlmEvent::ToolCalls(v) => { pending_calls = Some(v); }
                        }
                    }
                }
            }
        };
        while let Some(ev) = ev_rx.recv().await {
            match ev {
                LlmEvent::Delta(t) => {
                    full.push_str(&t);
                    cx.delta(&t).await;
                }
                LlmEvent::Reasoning(t) => cx.reasoning(&t).await,
                LlmEvent::Meta(m) => {
                    if m.is_some() {
                        round_usage = m;
                    }
                }
                LlmEvent::ToolCalls(v) => {
                    pending_calls = Some(v);
                }
            }
        }
        used_tokens = agent_run::charge_usage(db, &run_id, round_usage.as_ref())?;

        match res {
            Ok(molan_llm::CompletionState::ToolCalls) => {
                let calls = pending_calls.unwrap_or_else(|| json!([]));
                let mut calls_arr: Vec<Value> = calls
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                if calls_arr.len() > agent_tools::MAX_CALLS_PER_ROUND {
                    calls_arr.truncate(agent_tools::MAX_CALLS_PER_ROUND);
                }
                if calls_arr.is_empty() {
                    status = "error";
                    err_text = "上游报告工具调用但没有调用内容".into();
                    break;
                }
                agent_run::record_turn(db, &run_id, "assistant", "", &calls.to_string(), "")?;
                msgs.push(
                    json!({"role": "assistant", "content": null, "tool_calls": calls_arr.clone()}),
                );
                let mut aborted = false;
                for call in &calls_arr {
                    if cancel.is_cancelled()
                        || chat::take_abort(&request_id)
                        || chat::take_abort(&session_key)
                    {
                        aborted = true;
                        break;
                    }
                    let call_id = call["id"].as_str().unwrap_or("").to_string();
                    let fname = call["function"]["name"].as_str().unwrap_or("").to_string();
                    let fargs: Value = serde_json::from_str(
                        call["function"]["arguments"].as_str().unwrap_or("{}"),
                    )
                    .unwrap_or_else(|_| json!({}));
                    cx.ev(json!({"type":"tool","callId":call_id,"name":fname,"status":"running"}))
                        .await;
                    let outcome = agent_tools::dispatch_tool(db, &book_id, &fname, &fargs);
                    let (payload, ev_status, summary) = match &outcome {
                        Ok(v) => (v.clone(), "ok", agent_tools::tool_summary(v)),
                        Err(e) => (
                            json!({"error": sanitize(&api_key, e)}),
                            "error",
                            agent_tools::tool_summary(&json!({"error": sanitize(&api_key, e)})),
                        ),
                    };
                    steps.push(
                        json!({"type":"tool","name":fname,"status":ev_status,"summary":summary}),
                    );
                    cx.ev(json!({"type":"tool","callId":call_id,"name":fname,"status":ev_status,"summary":summary})).await;
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
            Ok(_) => {
                // Done / FinishStop / Unverified（allow_missing=false 下 Unverified 不出现）
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
                    err_text = "当前模型渠道不支持工具调用（tools）。请在设置中换用支持工具的渠道；系统不会自动切换模型。".into();
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
                    cx.ev(json!({"type":"interrupted","reason":"incomplete","partialChars":full.chars().count()})).await;
                    break;
                }
                if molan_llm::is_transient_error(&e.to_string())
                    && full.is_empty()
                    && retry.can_retry_transient(retries)
                {
                    let delay = retry.delay_for_retry(retries).unwrap_or(2000);
                    retries += 1;
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
                err_text = sanitize(&api_key, &e);
                break;
            }
        }
    }

    agent_run::finish_run(db, &run_id, status, &err_text)?;
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

    // assistant 消息落库：steps_json=工具轨迹，result_json=运行回执（books.rs 原样回传前端）
    let msg_id = uuid::Uuid::new_v4().to_string();
    let result_json = json!({
        "agent": true, "runId": run_id, "status": status,
        "usedTokens": used_tokens,
        "error": if err_text.is_empty() { Value::Null } else { json!(err_text) },
    })
    .to_string();
    let steps_json = json!(steps).to_string();
    let now = stats::now_ms();
    db.exec(
        "INSERT INTO messages(id,session_id,role,content,context_json,steps_json,result_json,created_at,interrupted) SELECT ?1,?2,'assistant',?3,?4,?5,?6,?7,?8 WHERE EXISTS(SELECT 1 FROM sessions s JOIN books b ON b.id=s.book_id WHERE s.id=?2 AND b.deleted_at IS NULL) ON CONFLICT(id) DO UPDATE SET content=excluded.content,context_json=excluded.context_json,steps_json=excluded.steps_json,result_json=excluded.result_json,interrupted=excluded.interrupted",
        &[
            &msg_id as &dyn rusqlite::ToSql,
            &session_id,
            &full,
            &ctx_json,
            &steps_json,
            &result_json,
            &now,
            &(if status == "interrupted" { 1i64 } else { 0i64 }) as &dyn rusqlite::ToSql,
        ],
    )?;
    db.exec(
        "UPDATE sessions SET msg_count=(SELECT COUNT(*) FROM messages WHERE session_id=?1), updated_at=?2 WHERE id=?1",
        &[&session_id as &dyn rusqlite::ToSql, &now],
    )?;
    cx.done(json!({
        "messageId": msg_id, "runId": run_id, "status": status, "model": model,
        "outputChars": full.chars().count(), "usedTokens": used_tokens,
    }))
    .await;
    Ok(Some(
        json!({"ok": status == "done", "runId": run_id, "status": status, "messageId": msg_id}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Db, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        // mock 渠道（tools 协议见 molan-llm agent_transport）
        db.exec(
            "INSERT INTO settings(key,value) VALUES('channels','[{\"id\":\"mock\",\"label\":\"Mock\",\"baseUrl\":\"mock://\",\"model\":\"mock-model\"}]')",
            &[],
        )
        .unwrap();
        db.exec(
            "INSERT INTO settings(key,value) VALUES('active_channel','mock')",
            &[],
        )
        .unwrap();
        let book = molan_core::books::create_book(&db, "agent-test", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        let session = molan_core::books::create_session(&db, &book, "工作会话").unwrap();
        let sid = session["id"].as_str().unwrap().to_string();
        (dir, db, book, sid)
    }

    fn args(book: &str, sid: &str, msg: &str, req: &str) -> Value {
        json!({"onEvent": "__CHANNEL__:1", "sessionId": sid, "bookId": book, "message": msg, "requestId": req})
    }

    /// 同任务并发：join!(运行, 收集)。runner 结束后 drop(tx) 关闭通道，收集自然收尾。
    async fn collect(db: &Db, args: &Value) -> (Result<Option<Value>>, Vec<Value>) {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(256);
        let a = args.clone();
        let runner = async {
            let r = run_agent_turn(db, &a, &tx).await;
            drop(tx);
            r
        };
        let collector = async {
            let mut events = Vec::new();
            while let Some(line) = rx.recv().await {
                if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                    events.push(v["e"].clone());
                }
            }
            events
        };
        tokio::join!(runner, collector)
    }

    fn run_row(db: &Db, req: &str) -> Value {
        db.q_json(
            "SELECT * FROM agent_run WHERE request_id=?1",
            &[&req as &dyn rusqlite::ToSql],
        )
        .unwrap()
        .remove(0)
    }

    #[tokio::test]
    async fn tool_round_then_text_completes() {
        let (_d, db, book, sid) = fixture();
        molan_core::files::write_file(&db, &book, "设定", "世界观.md", "修真体系").unwrap();
        let (res, events) =
            collect(&db, &args(&book, &sid, "[call:scan_book_tree]", "req-1")).await;
        let out = res.unwrap().unwrap();
        assert_eq!(out["status"], "done");
        // 事件序列：tool running → tool ok → done
        let tools: Vec<&Value> = events.iter().filter(|e| e["type"] == "tool").collect();
        assert!(tools
            .iter()
            .any(|t| t["status"] == "running" && t["name"] == "scan_book_tree"));
        assert!(tools.iter().any(|t| t["status"] == "ok"));
        assert!(events
            .iter()
            .any(|e| e["type"] == "done" && e["status"] == "done"));
        // run 账本：1 轮工具、终态 done、轨迹 assistant(tool_calls)+tool+assistant(text)
        let row = run_row(&db, "req-1");
        assert_eq!(row["status"], "done");
        assert_eq!(row["toolRound"].as_i64().unwrap(), 1);
        let turns = agent_run::list_turns(&db, row["id"].as_str().unwrap()).unwrap();
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0]["role"], "assistant");
        assert!(!turns[0]["toolCallsJson"].as_str().unwrap().is_empty());
        assert_eq!(turns[1]["role"], "tool");
        assert!(turns[1]["content"].as_str().unwrap().contains("设定"));
        assert_eq!(turns[2]["role"], "assistant");
        assert!(!turns[2]["content"].as_str().unwrap().is_empty());
        // 消息：user + assistant（steps_json 轨迹非空）
        let msgs = db
            .q_json(
                "SELECT role, steps_json FROM messages WHERE session_id=?1 ORDER BY created_at, rowid",
                &[&sid as &dyn rusqlite::ToSql],
            )
            .unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        let steps: Value =
            serde_json::from_str(msgs[1]["stepsJson"].as_str().unwrap_or("[]")).unwrap();
        assert_eq!(steps.as_array().unwrap().len(), 1);
        assert_eq!(steps[0]["name"], "scan_book_tree");
    }

    #[tokio::test]
    async fn plain_text_without_tools_completes() {
        let (_d, db, book, sid) = fixture();
        let (res, events) = collect(
            &db,
            &args(&book, &sid, "你好，介绍一下这本书的状态", "req-2"),
        )
        .await;
        let out = res.unwrap().unwrap();
        assert_eq!(out["status"], "done");
        assert!(!events.iter().any(|e| e["type"] == "tool"));
        let row = run_row(&db, "req-2");
        assert_eq!(row["toolRound"].as_i64().unwrap(), 0);
    }

    #[tokio::test]
    async fn replay_same_request_returns_receipt_without_rerun() {
        let (_d, db, book, sid) = fixture();
        let a = args(&book, &sid, "[call:scan_book_tree]", "req-3");
        let (r1, _) = collect(&db, &a).await;
        r1.unwrap().unwrap();
        let (r2, events2) = collect(&db, &a).await;
        let out2 = r2.unwrap().unwrap();
        assert_eq!(out2["replayed"], true, "终态重放必须直接返回回执");
        assert!(events2.iter().any(|e| e["type"] == "done"));
        let n = db
            .q_json(
                "SELECT COUNT(*) AS c FROM agent_run WHERE request_id='req-3'",
                &[],
            )
            .unwrap();
        assert_eq!(n[0]["c"].as_i64().unwrap(), 1);
        let row = run_row(&db, "req-3");
        assert_eq!(row["toolRound"].as_i64().unwrap(), 1, "重放不得追加轮次");
    }

    #[tokio::test]
    async fn foreign_session_and_bad_book_rejected() {
        let (_d, db, book, sid) = fixture();
        let other = molan_core::books::create_book(&db, "别的书", "都市", "第一人称");
        let other_id = other["id"].as_str().unwrap();
        let bad = args(other_id, &sid, "你好", "req-4");
        let (res, _) = collect(&db, &bad).await;
        assert!(res.is_err(), "会话不属于该书必须拒绝");
        let nobook = args("no-such-book", &sid, "你好", "req-5");
        let (res2, _) = collect(&db, &nobook).await;
        assert!(res2.is_err(), "书籍不存在必须拒绝");
        let _ = book;
    }

    #[tokio::test]
    async fn pre_cancelled_request_stops_before_model() {
        let (_d, db, book, sid) = fixture();
        chat::request_abort("req-6");
        let (res, _) = collect(&db, &args(&book, &sid, "[call:scan_book_tree]", "req-6")).await;
        let out = res.unwrap().unwrap();
        assert_eq!(out["status"], "interrupted");
        let row = run_row(&db, "req-6");
        assert_eq!(row["status"], "interrupted");
        assert_eq!(
            row["toolRound"].as_i64().unwrap(),
            0,
            "取消后不得再发起模型调用"
        );
    }

    #[tokio::test]
    async fn rounds_limit_stops_marker_loop() {
        let (_d, db, book, sid) = fixture();
        db.exec(
            "INSERT INTO settings(key,value) VALUES('agent_max_tool_rounds','1')",
            &[],
        )
        .unwrap();
        // mock 的 [call:] 标记会留在历史 user 消息里：无轮数上限将循环到预算耗尽
        let (res, events) =
            collect(&db, &args(&book, &sid, "[call:scan_book_tree]", "req-7")).await;
        let out = res.unwrap().unwrap();
        assert_eq!(out["status"], "budget_exhausted");
        assert!(events
            .iter()
            .any(|e| e["code"].as_str() == Some("ROUNDS_EXHAUSTED")));
        let row = run_row(&db, "req-7");
        assert_eq!(row["toolRound"].as_i64().unwrap(), 1);
        assert_eq!(row["maxToolRounds"].as_i64().unwrap(), 1);
        assert_eq!(row["budgetTokens"].as_i64().unwrap(), BUDGET_TOKENS_DEFAULT);
    }

    #[tokio::test]
    async fn failing_tool_is_reported_and_loop_continues() {
        let (_d, db, book, sid) = fixture();
        // read_book_file 缺参数 → 结构化错误回灌，第二轮 mock 出文本收敛
        let (res, events) =
            collect(&db, &args(&book, &sid, "[call:read_book_file]", "req-8")).await;
        let out = res.unwrap().unwrap();
        assert_eq!(out["status"], "done");
        assert!(events.iter().any(|e| e["type"] == "tool"
            && e["status"] == "error"
            && e["name"] == "read_book_file"));
        let row = run_row(&db, "req-8");
        let turns = agent_run::list_turns(&db, row["id"].as_str().unwrap()).unwrap();
        let tool_turn = turns.iter().find(|t| t["role"] == "tool").unwrap();
        assert!(tool_turn["content"].as_str().unwrap().contains("error"));
    }

    #[test]
    fn session_abort_alias_cancels_token() {
        let token = CancellationToken::new();
        if let Ok(mut g) = SESSION_TOKENS.lock() {
            g.insert("s-abort".to_string(), token.clone());
        }
        abort_by_args(&json!({"sessionId": "s-abort"}));
        assert!(token.is_cancelled());
        assert!(chat::take_abort("session:s-abort"));
        if let Ok(mut g) = SESSION_TOKENS.lock() {
            g.remove("s-abort");
        }
        // 带 requestId 时不走 session 兜底
        abort_by_args(&json!({"requestId": "r", "sessionId": "s-x"}));
        assert!(!chat::take_abort("session:s-x"));
    }

    /// F1 单元锁：无在飞 run 时 abort_by_args 不得把 session 标记推进全局 ABORTS。
    #[test]
    fn f1_idle_session_abort_leaves_no_marker() {
        // 空闲会话（SESSION_TOKENS 无该 sid）：什么都不登记
        abort_by_args(&json!({"sessionId": "s-idle"}));
        assert!(
            !chat::take_abort("session:s-idle"),
            "无在飞 run 时不得登记取消标记（否则毒化下一条 agent_turn）"
        );
        // 再调一次仍无残留（不是"吞一次"而是"根本没登记"）
        abort_by_args(&json!({"sessionId": "s-idle"}));
        assert!(!chat::take_abort("session:s-idle"));
        // 空 sessionId / 带 requestId 同样不登记
        abort_by_args(&json!({"sessionId": "  "}));
        abort_by_args(&json!({"requestId": "r1", "sessionId": "s-idle2"}));
        assert!(!chat::take_abort("session:s-idle2"));
    }

    /// F1 端到端：空闲会话被 abort 后，下一条 agent_turn 必须正常 done（零输出事故回归锁）。
    #[tokio::test]
    async fn f1_abort_on_idle_session_does_not_poison_next_turn() {
        let (_d, db, book, sid) = fixture();
        // 此刻没有任何在飞 run（等同前端点了停止、但 agent 并未在跑）
        abort_by_args(&json!({"sessionId": sid}));
        let (res, events) = collect(
            &db,
            &args(&book, &sid, "你好，介绍一下这本书的状态", "req-f1"),
        )
        .await;
        let out = res.unwrap().unwrap();
        assert_eq!(
            out["status"], "done",
            "空闲会话 abort 后下一条必须正常完成：{:?}",
            out
        );
        // 必须真的调了模型：有 delta 输出，run 也不是 interrupted
        assert!(
            events.iter().any(|e| e["type"] == "delta"),
            "必须零标记泄漏、正常产生输出：{:?}",
            events
        );
        let row = run_row(&db, "req-f1");
        assert_eq!(row["status"], "done");
        // 再下一条也正常（确认不是"只吞一次"而是没登记）
        let (res2, _) = collect(&db, &args(&book, &sid, "再来一条", "req-f1b")).await;
        assert_eq!(res2.unwrap().unwrap()["status"], "done");
    }

    /// F1 反向锁：确有在飞 run 时，session 兜底仍必须生效（修复不能削弱真实取消能力）。
    #[tokio::test]
    async fn f1_abort_with_inflight_run_still_cancels() {
        let (_d, _db, _book, sid) = fixture();
        // 模拟在飞 run：登记该会话的取消令牌（run_agent_turn 循环内做的同一件事）
        let token = CancellationToken::new();
        if let Ok(mut g) = SESSION_TOKENS.lock() {
            g.insert(sid.clone(), token.clone());
        }
        abort_by_args(&json!({"sessionId": sid}));
        assert!(token.is_cancelled(), "在飞 run 必须被取消");
        assert!(
            chat::take_abort(&session_abort_key(&sid)),
            "在飞 run 时仍要登记标记供循环边界消费"
        );
        if let Ok(mut g) = SESSION_TOKENS.lock() {
            g.remove(&sid);
        }
        // 令牌注销后（run 已收尾）再 abort：不再登记，避免泄漏
        abort_by_args(&json!({"sessionId": sid}));
        assert!(!chat::take_abort(&session_abort_key(&sid)));
    }

    // ---------- F6：同 requestId running 期重入不得进入循环 ----------

    /// 顺序双发：第二次得到 already_running，不重跑模型、不插第二条 user 消息。
    #[tokio::test]
    async fn f6_sequential_reentry_same_request_is_rejected_as_running() {
        let (_d, db, book, sid) = fixture();
        // 预置一条既有 running 行（模拟前一次请求已启动、尚未到终态）
        let run = agent_run::begin_run(
            &db,
            &BeginRun {
                book_id: book.clone(),
                session_id: sid.clone(),
                request_id: "req-f6".into(),
                task: "agent".into(),
                model: "mock-model".into(),
                target_ch: None,
                max_tool_rounds: MAX_TOOL_ROUNDS_DEFAULT,
                budget_tokens: BUDGET_TOKENS_DEFAULT,
            },
        )
        .unwrap();
        let run_id = run["id"].as_str().unwrap().to_string();
        // 模拟"前一次请求已进入循环并写完自己的 user 消息"（run_agent_turn 落库的同一行）
        db.exec(
            "INSERT INTO messages(id,session_id,role,content,context_json,steps_json,result_json,created_at,interrupted) VALUES(?1,?2,'user','前一次在飞的请求',NULL,NULL,NULL,0,0)",
            &[
                &uuid::Uuid::new_v4().to_string() as &dyn rusqlite::ToSql,
                &sid,
            ],
        )
        .unwrap();
        let (res, events) =
            collect(&db, &args(&book, &sid, "[call:scan_book_tree]", "req-f6")).await;
        let out = res.unwrap().unwrap();
        assert_eq!(out["ok"], false, "重入不得报成功：{:?}", out);
        assert_eq!(out["status"], "already_running");
        assert_eq!(out["runId"], run_id.as_str());
        // 结构化冲突事件：done{status:already_running, runId}
        let done = events
            .iter()
            .find(|e| e["type"] == "done")
            .expect("必须发 done 事件");
        assert_eq!(done["status"], "already_running");
        assert_eq!(done["runId"], run_id.as_str());
        // 绝不进入循环：无 meta/tool/delta，模型一次都没调
        assert!(
            !events.iter().any(|e| e["type"] == "meta"),
            "不得进入循环（无 meta）：{:?}",
            events
        );
        assert!(!events.iter().any(|e| e["type"] == "tool"));
        assert!(!events.iter().any(|e| e["type"] == "delta"));
        // messages 表仍只有前一次那一条 user：本次重入不插 user、不插 assistant
        let msgs = db
            .q_json(
                "SELECT role, content FROM messages WHERE session_id=?1",
                &[&sid as &dyn rusqlite::ToSql],
            )
            .unwrap();
        assert_eq!(msgs.len(), 1, "不得插第二条消息：{:?}", msgs);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "前一次在飞的请求");
        // run 账本不被污染：轮次仍 0、状态仍 running、只有一条 run
        let row = run_row(&db, "req-f6");
        assert_eq!(row["status"], "running");
        assert_eq!(row["toolRound"].as_i64().unwrap(), 0);
        let n = db
            .q_json(
                "SELECT COUNT(*) AS c FROM agent_run WHERE request_id='req-f6'",
                &[],
            )
            .unwrap();
        assert_eq!(n[0]["c"].as_i64().unwrap(), 1);
        // 真双发（join!）：同 requestId 并发重入也必须只跑一次。
        // 结果允许 done（抢到执行权）/ already_running（被拒）/ replayed（抢到时已到终态），
        // 但不变式是硬的：该 requestId 只落一条 user、只落一条 assistant、只跑一轮循环。
        let a1 = args(&book, &sid, "并发一", "req-f6c");
        let a2 = args(&book, &sid, "并发二", "req-f6c");
        let (a, b) = tokio::join!(collect(&db, &a1), collect(&db, &a2));
        let ra = a.0.unwrap().unwrap();
        let rb = b.0.unwrap().unwrap();
        let statuses = [
            ra["status"].as_str().unwrap_or("").to_string(),
            rb["status"].as_str().unwrap_or("").to_string(),
        ];
        assert!(
            statuses
                .iter()
                .all(|s| s == "done" || s == "already_running"),
            "并发结果必须收敛为 done/already_running：{:?}",
            statuses
        );
        // 恰好一次真跑：runId 一致，且该 run 的轮次只被一个循环写过
        assert_eq!(ra["runId"], rb["runId"], "同 requestId 必须同一个 run");
        let cid = ra["runId"].as_str().unwrap().to_string();
        let cid_turns = agent_run::list_turns(&db, &cid).unwrap();
        let assistant_turns = cid_turns
            .iter()
            .filter(|t| t["role"].as_str() == Some("assistant"))
            .count();
        assert_eq!(
            assistant_turns, 1,
            "并发重入不得让同一 run 被跑两遍（assistant 轮次应恰好 1）：{:?}",
            cid_turns
        );
        let users_c = db
            .q_json(
                "SELECT COUNT(*) AS c FROM messages WHERE session_id=?1 AND role='user'",
                &[&sid as &dyn rusqlite::ToSql],
            )
            .unwrap();
        assert_eq!(
            users_c[0]["c"].as_i64().unwrap(),
            2,
            "同 requestId 并发只能多出 1 条 user（前一次 1 条 + 本次 1 条）"
        );
        let turns = agent_run::list_turns(&db, run_id.as_str()).unwrap();
        assert!(turns.is_empty(), "既有 running 行不得被重入者追加轮次");
    }

    /// 终态重放与 running 重入必须区分：终态走 replayed 回执，running 走 already_running。
    #[tokio::test]
    async fn f6_replayed_terminal_run_still_returns_receipt() {
        let (_d, db, book, sid) = fixture();
        let a = args(&book, &sid, "[call:scan_book_tree]", "req-f6d");
        let (r1, _) = collect(&db, &a).await;
        assert_eq!(r1.unwrap().unwrap()["status"], "done");
        let (r2, events2) = collect(&db, &a).await;
        let out2 = r2.unwrap().unwrap();
        assert_eq!(out2["replayed"], true, "终态仍走重放回执");
        assert_ne!(out2["status"], "already_running");
        assert!(events2.iter().any(|e| e["type"] == "done"));
    }

    #[test]
    fn tools_unsupported_classifier_is_conservative() {
        let e1 = anyhow!("HTTP 400 Bad Request: invalid_request_error unknown parameter tools");
        assert!(looks_tools_unsupported(&e1));
        let e2 = anyhow!("HTTP 500 internal error");
        assert!(!looks_tools_unsupported(&e2));
        let e3 = anyhow!("400 bad prompt content");
        assert!(!looks_tools_unsupported(&e3), "没有 tool 关键词不得误判");
    }
}
