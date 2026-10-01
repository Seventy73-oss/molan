// 主对话流：chat_stream 与编辑器内联助手（inline_chat / inline_assist）的事件封装 Ctx 与取消登记。
#[path = "chat_contract.rs"]
pub(crate) mod chat_contract;
#[path = "chat_snapshot.rs"]
mod chat_snapshot;
use super::super::AppState;
use super::{auto_book_context, auto_humanize, auto_match_skills};
use super::{auto_save_chat_output_checked, context_text, has_body_heading};
use super::{fallback, requested_chapter, resolve_book_style, stage_context};
use super::{skill_catalog, smart_archive};
use crate::handlers::{channel_id, chapter_num_from_name};
use anyhow::{anyhow, Result};
use molan_core::files;
use molan_core::stats;
use molan_llm::{ChatParams, LlmEvent};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

static ABORTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 优雅停机标志：置位后拒绝新 run 类请求；与用户主动停止区分（停机不走 ABORTS）。
static SHUTTING_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 优雅停机入口（main 的信号处理调用）：停止接单 + 取消全部在飞令牌。
/// 在飞流被掐断后走 interrupted 分支，服务端会把残稿自动存入正文待审（作者不在场也兜底）。
pub fn shutdown_cancel_all() {
    SHUTTING_DOWN.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Ok(g) = ABORT_TOKENS.lock() {
        for t in g.values() {
            t.cancel();
        }
    }
}

pub(crate) fn shutting_down() -> bool {
    SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst)
}

/// 在飞请求的取消令牌表：requestId -> CancellationToken。
/// 与 ABORTS 双轨并行——ABORTS 负责「分段边界不再发起下一段」，令牌负责「掐断当前在飞的那次 HTTP 调用」。
static ABORT_TOKENS: std::sync::LazyLock<
    Mutex<std::collections::HashMap<String, tokio_util::sync::CancellationToken>>,
> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// 登记一个在飞请求的取消令牌（同 id 覆盖旧值），返回 clone 供本次调用使用
pub(crate) fn register_abort(req_id: &str) -> tokio_util::sync::CancellationToken {
    let token = tokio_util::sync::CancellationToken::new();
    if let Ok(mut g) = ABORT_TOKENS.lock() {
        g.insert(req_id.to_string(), token.clone());
    }
    token
}

/// 注销：请求收尾时必须调用，否则令牌表无限增长（拿到旧 channel 的取消也会污染后续同 id 请求）
pub(crate) fn unregister_abort(req_id: &str) {
    if let Ok(mut g) = ABORT_TOKENS.lock() {
        g.remove(req_id);
    }
}

/// 查询并消费取消标记：任务被 abort 时返回 true（并自动清除，防止复跑同 id 误判）
pub(crate) fn take_abort(req_id: &str) -> bool {
    let mut g = ABORTS.lock().unwrap_or_else(|e| e.into_inner());
    match g.iter().position(|r| r == req_id) {
        Some(i) => {
            g.remove(i);
            true
        }
        None => false,
    }
}

/// abort_chat 入口：登记取消（供 mod.rs 调用）。
/// 两件事：① 集合置位（分段边界消费）；② 若该 id 有在飞的请求令牌则立即 cancel() →
/// 在飞的 reqwest 调用被 drop → 上游停止生成（= 停止计费）。
pub fn request_abort(req_id: &str) {
    let mut g = ABORTS.lock().unwrap_or_else(|e| e.into_inner());
    if !req_id.is_empty() && !g.iter().any(|r| r == req_id) {
        g.push(req_id.to_string());
    }
    drop(g);
    if let Ok(g) = ABORT_TOKENS.lock() {
        if let Some(t) = g.get(req_id) {
            t.cancel();
        }
    }
}

pub(crate) struct Ctx<'a> {
    tx: &'a tokio::sync::mpsc::Sender<String>,
    channel: String,
    pub(crate) chars: usize,
    pub(crate) full: String,
    snapshot: Option<chat_snapshot::Snapshot<'a>>,
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(tx: &'a tokio::sync::mpsc::Sender<String>, channel: &str) -> Self {
        Ctx {
            tx,
            channel: channel.to_string(),
            chars: 0,
            full: String::new(),
            snapshot: None,
        }
    }
    pub(crate) async fn ev(&self, obj: Value) {
        let _ = self
            .tx
            .send(format!("{}\n", json!({"ch": self.channel, "e": obj})))
            .await;
    }
    pub(crate) async fn delta(&mut self, text: &str) {
        self.chars += text.chars().count();
        self.full.push_str(text);
        if let Some(snapshot) = self.snapshot.as_mut() {
            snapshot.capture(&self.full);
        }
        self.ev(json!({"type": "delta", "text": text})).await;
    }
    pub(crate) async fn reasoning(&self, text: &str) {
        self.ev(json!({"type": "reasoning", "text": text})).await;
    }
    pub(crate) async fn progress(&self) {
        self.ev(json!({"type": "progress", "chars": self.chars}))
            .await;
    }
    pub(crate) async fn step(&self, index: i64, title: &str) {
        self.ev(json!({"type": "step", "index": index, "title": title}))
            .await;
    }
    pub(crate) async fn done(&self, extra: Value) {
        let mut o = json!({"type": "done", "full": self.full});
        if let (Some(dst), Some(src)) = (o.as_object_mut(), extra.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
        self.ev(o).await;
    }
    pub(crate) async fn error(&self, message: &str) {
        // 取消不是失败：带 code=CANCELLED，前端各消费方据此走「已停止」分支而非通用报错。
        let mut ev = json!({"type": "error", "message": message});
        if message.contains(molan_llm::CANCELLED_MSG) {
            ev["code"] = json!("CANCELLED");
        }
        self.ev(ev).await;
    }
    pub(crate) async fn result(&self, result: &Value) {
        self.ev(json!({"type": "result", "result": result})).await;
    }
}

// 真流式对话：SSE 解析一块就转发一块，不做收尾缓冲（对齐官方逐字输出）
pub(crate) async fn drain_chat_completion(
    params: molan_llm::ChatParams,
    cx: &mut Ctx<'_>,
) -> anyhow::Result<()> {
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    // 包装一层：流结束时先 drop sender 再返回，使下方排空循环能在缓冲清空后自然收到 None。
    // 直接 select! { sse => break } 有尾块丢失竞态：最后一次 poll 里"发送内容+返回结束"是原子的，
    // break 会把压在通道缓冲里的尾块整体丢掉——mock:// 一次性输出全丢（表现为空回复），
    // 真实流式渠道则随机吞掉回复结尾。
    let sse = {
        let tx = ev_tx;
        async move {
            let r = molan_llm::chat_completion_stream(params, &tx).await;
            drop(tx);
            r
        }
    };
    tokio::pin!(sse);
    let res = loop {
        tokio::select! {
            r = &mut sse => break r,
            ev = ev_rx.recv() => {
                if let Some(ev) = ev {
                    match ev {
                        LlmEvent::Delta(t) => cx.delta(&t).await,
                        LlmEvent::Reasoning(t) => cx.reasoning(&t).await,
                        LlmEvent::Meta(_) => {}
                        // Agent 工具传输层新增变体：旧对话流不消费，最小忽略臂。
                        LlmEvent::ToolCalls(_) => {}
                    }
                }
            }
        }
    };
    res?;
    // 流已结束、sender 已 drop：排空缓冲里剩余的事件（至少最后一块内容）
    while let Some(ev) = ev_rx.recv().await {
        match ev {
            LlmEvent::Delta(t) => cx.delta(&t).await,
            LlmEvent::Reasoning(t) => cx.reasoning(&t).await,
            LlmEvent::Meta(_) => {}
            // Agent 工具传输层新增变体：旧对话流不消费，最小忽略臂。
            LlmEvent::ToolCalls(_) => {}
        }
    }
    Ok(())
}

/// chat_stream：主对话流（原 dispatch 分支体原样搬入）。
pub(crate) async fn chat_stream(
    st: &Arc<AppState>,
    _cmd: &str,
    args: &Value,
    tx: &tokio::sync::mpsc::Sender<String>,
) -> Result<Option<Value>> {
    let db = &st.db;
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();
    let channel = channel_id(&a("onEvent"));
    if a("onEvent").is_null() {
        return Err(anyhow!("缺少事件通道"));
    }
    if shutting_down() {
        return Err(anyhow!("服务正在优雅停机，请稍后重试"));
    }
    let mut cx = Ctx::new(tx, &channel);
    let session_id = s("sessionId");
    let message = s("message");
    // 前端展示文本字段：老版发 displayMessage，新版 bundle 发 displayText（值取 displayText ?? null）。
    // 两个键都收，避免只认一个导致落库为空。
    let display_raw = {
        let d1 = s("displayMessage");
        if !d1.trim().is_empty() {
            d1
        } else {
            s("displayText")
        }
    };
    // 前端「技能反问后回复」可能只带展示文本不带 message——任一非空即算本轮内容
    let message = if message.trim().is_empty() {
        display_raw.clone()
    } else {
        message
    };
    // 落库内容：展示文本非空则用它（用户实际输入），否则退回兜底后的 message。
    // 修复：旧代码直接存 display_raw，普通对话（前端不传展示文本）会存成空消息。
    let display = if display_raw.trim().is_empty() {
        message.clone()
    } else {
        display_raw
    };
    let book_id = s("bookId");
    if session_id.is_empty() {
        return Err(anyhow!("未选择会话，请重新选择本书会话以保存消息"));
    }
    let session_rows = db.q_json(
        "SELECT book_id FROM sessions WHERE id=?1",
        &[&session_id as &dyn rusqlite::ToSql],
    )?;
    if session_rows.first().and_then(|row| row["bookId"].as_str()) != Some(book_id.as_str()) {
        return Err(anyhow!("会话不存在或不属于当前书，请刷新会话列表"));
    }
    // 仅客户端显式选择的任务/技能可授权对话自动存稿；自动匹配技能只用于提示词路由。
    let chosen_skills: Vec<Value> = chat_contract::selected_skills(args)
        .into_iter()
        .map(Value::String)
        .collect();
    // 对话自动路由：消息点名了技能卡 → 作为辅助挂载（手动勾选优先、去重；自动匹配不替代主技能）
    let mut sel = molan_core::skill_resolver::Selection::from_args(args);
    sel.auto_matched = auto_match_skills(db, &message)
        .into_iter()
        .filter(|n| !chosen_skills.iter().any(|v| v.as_str() == Some(n.as_str())))
        .collect();
    let mut skill_names = chosen_skills.clone();
    skill_names.extend(sel.auto_matched.iter().map(|n| json!(n)));

    let task_hint = chat_contract::task(args)?;
    let role = chat_contract::role(&task_hint);
    let auto_save_mode = book_auto_save_mode(db, &book_id);
    let allow_stream_save = chat_contract::allow_save(args, &task_hint, auto_save_mode);
    let user_message_id = uuid::Uuid::new_v4().to_string();
    let chn = if role.is_empty() {
        molan_llm::active_channel(db)
    } else {
        molan_llm::resolve_agent_channel(db, role)
    }
    .ok_or_else(|| {
        anyhow!("未配置可用的模型渠道。请在 设置→渠道管理 里添加你自己的 API（baseUrl + Key）")
    })?;
    if chn["baseUrl"].as_str().unwrap_or("").is_empty() {
        return Err(anyhow!(
            "未配置可用的模型渠道。请在 设置→渠道管理 里添加你自己的 API（baseUrl + Key）"
        ));
    }
    let ch_key = chn["key"].as_str().unwrap_or("").to_string();

    // 保存用户消息
    if !session_id.is_empty() {
        // files 缺省时补 []：写 null 会让前端 c.files.length 抛异常并白屏
        let ctx_files = match a("contextFiles") {
            Value::Array(_) => a("contextFiles"),
            _ => json!([]),
        };
        let ctx_json = json!({
            "requestId": a("requestId"), "runId": a("runId"),
            "skills": skill_names, "files": ctx_files,
            "webSearch": a("webSearch").as_bool().unwrap_or(false),
        })
        .to_string();
        let now = stats::now_ms();
        db.exec(
                "INSERT INTO messages(id,session_id,role,content,context_json,steps_json,result_json,created_at,interrupted) VALUES(?,?,?,?,?,NULL,NULL,?,0)",
                &[
                    &user_message_id as &dyn rusqlite::ToSql,
                    &session_id, &"user", &display, &ctx_json, &now,
                ],
            )?;
        db.exec(
            "UPDATE sessions SET msg_count=(SELECT COUNT(*) FROM messages WHERE session_id=?1), updated_at=?2 WHERE id=?1",
            &[&session_id as &dyn rusqlite::ToSql, &now],
        )?;
    }
    cx.ev(json!({"type": "webSearch", "status": if a("webSearch").as_bool().unwrap_or(false) { "unavailable" } else { "off" }})).await;

    // 组装 system
    let book_genre = if book_id.is_empty() {
        None
    } else {
        db.q_json(
            "SELECT genre FROM books WHERE id=?1",
            &[&book_id as &dyn rusqlite::ToSql],
        )
        .ok()
        .and_then(|v| {
            v.first()
                .and_then(|r| r["genre"].as_str().map(|s| s.to_string()))
        })
    };
    // N01：按任务路由技能（统一 SkillResolver + 冻结计划；文风/去味覆盖同源生效）
    let chat_task = task_hint.clone();
    let kind = molan_core::task_kind::TaskKind::parse(&chat_task)
        .unwrap_or(molan_core::task_kind::TaskKind::Chat);
    let plan = super::run_plan::build(db, &st.root, &book_id, kind, &sel)?;
    let style = super::run_plan::style_text(&plan);
    let skills = super::run_plan::skill_rows(&plan);
    let skill_debug: Vec<String> = skills
        .iter()
        .map(|s| {
            format!(
                "{}:{}",
                s["name"].as_str().unwrap_or(""),
                s["usageMode"].as_str().unwrap_or("support")
            )
        })
        .collect();
    let (mut context, context_files) = context_text(db, &book_id, &a("contextFiles"));
    let auto_ctx = auto_book_context(db, &book_id, &message);
    if !auto_ctx.is_empty() {
        context = if context.is_empty() {
            auto_ctx
        } else {
            format!("{}\n\n{}", auto_ctx, context)
        };
    }
    // P2 契约：目标章细纲自动注入（显式引用去重、aiOff 尊重）；meta 回显注入项供审计
    let target_ch = requested_chapter(&message).unwrap_or(0);
    let stage_ol = stage_context::append_target_outline(db, &book_id, target_ch, &mut context);
    let sys = super::build_system_with(
        db,
        &st.root,
        &book_id,
        book_genre.as_deref(),
        style.as_deref(),
        &skills,
        &message,
        &context,
        &super::run_plan::humanize_method(&plan),
    );
    // 附加技能/文风目录：AI 全程知道有哪些写法可参考
    let catalog = skill_catalog(db);
    let sys = if catalog.is_empty() {
        sys
    } else {
        format!(
            "{}

{}",
            sys, catalog
        )
    };
    // 元事件：把本次实际生效的技能（名字:usage）发给前端/调试，便于确认主辅读取是否接通。
    cx.ev(json!({"type": "meta", "task": chat_task, "role": role, "model": chn["model"], "allowSave": allow_stream_save, "effectiveSkills": skill_debug, "stageOutline": stage_ol, "planHash": plan["planHash"], "excludedSkills": plan["excluded"], "contextFiles": context_files}))
        .await;
    // 上下文清单（P0-3）：记录本轮实际注入的块与记忆覆盖度，只观测不阻断。
    // 后续任何"设定为什么没生效"都能回溯到这份清单（缺哪块、被预算裁掉几条）。
    {
        // 与实际注入保持一致：未点名章号时按最新正式章+1 记录覆盖度（见 auto_book_context_for_chapter 的 eff_target）。
        let manifest_target = match requested_chapter(&message) {
            Some(n) if n > 0 => n,
            _ if !book_id.is_empty() => super::auto_write::latest_chapter_num(db, &book_id) + 1,
            _ => 0,
        };
        let coverage = if manifest_target > 1 && !book_id.is_empty() {
            molan_core::continuity::chapter_context(db, &book_id, manifest_target)
                .ok()
                .map(|cx| {
                    json!({
                        "complete": cx["complete"], "missingCount": cx["missingCount"],
                        "stale": cx["stale"], "hidden": cx["hidden"],
                        "omittedEntries": cx["omittedEntries"],
                    })
                })
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let _ = molan_core::ctx_manifest::record(
            db,
            &book_id,
            &session_id,
            "chat_stream",
            manifest_target,
            chn["model"].as_str().unwrap_or(""),
            &context,
            coverage,
        );
    }

    // 消息序列：system + 历史（按「越近越完整」预算）+ 本条。
    // 修复：本轮 user 消息已先落库，不过滤会在历史里再出现一次（重复上下文 / 浪费预算）。
    let mut msgs: Vec<Value> = vec![json!({"role": "system", "content": sys})];
    if !session_id.is_empty() {
        let rows = db
                .q_json(
                    // rowid 作为同毫秒 tiebreak：created_at 相同也能得到稳定、真实的插入顺序
                    "SELECT id, role, content FROM messages WHERE session_id=?1 AND role IN ('user','assistant') ORDER BY created_at DESC, rowid DESC LIMIT 16",
                    &[&session_id as &dyn rusqlite::ToSql],
                )
                .unwrap_or_default();
        msgs.extend(chat_contract::history(&rows, &user_message_id));
    }
    msgs.push(json!({"role": "user", "content": message}));

    // temperature/max_tokens 设置
    let temp: f64 = molan_llm::get_setting(db, "temperature")
        .parse()
        .unwrap_or(0.7);
    let max_tokens: i64 = molan_llm::get_setting(db, "max_tokens")
        .parse()
        .unwrap_or(8192);
    // 思考 max 开启：思考本身占输出预算，主对话至少留 16k 给思考+正文
    // pro 系模型 reasoning 计入 max_tokens：16384 写长章必截断，抬到 32768 + 截断自动续写兜底
    let max_tokens = max_tokens.max(32768);

    // 取消登记：带 requestId 的主对话注册令牌，abort_chat 时能立即掐断在飞的 LLM 调用。
    // RAII：令牌从登记起一直有效到本轮真正收尾（humanize / 文件 commit 都在保护期内），
    // 由 guard 的 Drop 注销；不再在 LLM 段结束后提前 unregister（那会让后处理阶段取消失效）。
    let req_id = s("requestId");
    // 缺 requestId（脚本通道/烟测）时自动生成 key：保证优雅停机能取消**所有**在飞流，
    // 而不是只取消带了 requestId 的主对话。
    let abort_key = if req_id.is_empty() {
        format!("auto-{}", uuid::Uuid::new_v4())
    } else {
        req_id.clone()
    };
    struct AbortGuard<'g> {
        key: &'g str,
    }
    impl<'g> Drop for AbortGuard<'g> {
        fn drop(&mut self) {
            unregister_abort(self.key);
        }
    }
    let msg_id = uuid::Uuid::new_v4().to_string();
    cx.snapshot = Some(chat_snapshot::Snapshot::new(
        db,
        &msg_id,
        &session_id,
        &req_id,
    ));
    let cancel_tok = Some(register_abort(&abort_key));
    let _abort_guard = AbortGuard {
        key: abort_key.as_str(),
    };
    let mut params = ChatParams {
        base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: ch_key,
        model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
        messages: msgs,
        temperature: if temp == 0.0 { 0.7 } else { temp },
        max_tokens: if max_tokens == 0 { 8192 } else { max_tokens },
        stream: true,
        no_thinking: false,
        // 主对话：思考强度开到最大（对齐官方「极致 max」档）
        reasoning_effort: "max".to_string(),
        // 主对话是流式路径，用量不落账
        log_tag: String::new(),
        log_book: book_id.to_string(),
        cancel: cancel_tok.clone(),
    };

    // 进度心跳：**全程**有效（含去味/落盘阶段）。
    // 去味是第二次 LLM 调用，可能静默几十秒；以前心跳在主流结束后就停了，
    // 客户端会判流已死而断开，服务端随之取消——完整产出被当「中断」丢弃（真实事故）。
    // RAII 守卫：函数返回（含 `?` 提前返回）即自动停止，不留后台任务。
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
    // 两类自愈：
    // 1) 瞬态错误（502/503/限流/网络抖动）且尚未输出任何字：退避重试，避免「一次抖动即死」；
    // 2) finish_reason=length 截断且已有产出：自动发「继续」续写拼接（最多 3 次），
    //    拼接完成后按完整产出走落盘——长章节不再因 token 上限被当「中断」丢弃。
    //    已有产出时绝不瞬态重试（会把重复内容混进已流出的文本）。
    let mut run_res: anyhow::Result<()>;
    let mut attempt = 0u32;
    let mut cont_count = 0u32;
    // 重试/续写数值集中在 molan-llm::retry::RetryPolicy（与生产现状一致，可单测）
    // 作者未选择时不自动继续生成额外文本；仅无产出时保留瞬态重试。
    let retry_cfg = molan_llm::get_setting(db, "agent_retry_extra");
    let extra_n: usize = retry_cfg.parse().unwrap_or(2).min(3); // §10：额外 N 次可配（含 0）
    let mut retry_policy = molan_llm::retry::RetryPolicy::with_extra_retries(extra_n);
    if a("autoContinue").as_bool() != Some(true) {
        retry_policy.continuation_max = 0;
    }
    loop {
        match drain_chat_completion(params.clone(), &mut cx).await {
            Ok(()) => {
                run_res = Ok(());
                break;
            }
            Err(e) => {
                let msg = format!("{}", e);
                if molan_llm::is_cancelled_err(&e) {
                    run_res = Err(e);
                    break;
                }
                let is_len_trunc = msg.contains("finish_reason=length");
                // 中途失败但已有产出：自动续写拼接（长度截断 / 上游 429·5xx 抖动 / 裸断流），
                // 最多 3 次；拼完按完整产出走落盘——长章节不再因一次抖动被当「中断」丢弃。
                if molan_llm::is_resumable_err(&e)
                    && !cx.full.is_empty()
                    && retry_policy.can_continue(cont_count)
                {
                    cont_count += 1;
                    let why = if is_len_trunc {
                        "触长度上限"
                    } else {
                        "上游中断"
                    };
                    tracing::info!(
                        "输出{}，自动续写 {}/3：{}",
                        why,
                        cont_count,
                        &msg.chars().take(120).collect::<String>()
                    );
                    cx.ev(json!({"type": "cont", "n": cont_count, "why": why}))
                        .await;
                    if !is_len_trunc {
                        // 抖动续写前稍等，避免立刻撞上同一波限流
                        tokio::time::sleep(std::time::Duration::from_millis(
                            retry_policy.continuation_backoff_ms,
                        ))
                        .await;
                    }
                    let tail: String = cx
                        .full
                        .chars()
                        .rev()
                        .take(200)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    params
                        .messages
                        .push(json!({"role": "assistant", "content": cx.full.clone()}));
                    params.messages.push(json!({
                        "role": "user",
                        "content": format!(
                            "上文输出被{}中断。请从「{}」之后逐字继续写下去：不要重复已写内容，不要解释，不要重新开头，直接接续：",
                            if is_len_trunc { "长度上限" } else { "上游网络/限流" },
                            tail
                        )
                    }));
                    continue;
                }
                let can_retry = retry_policy.can_retry_transient(attempt)
                    && cx.full.is_empty()
                    && molan_llm::is_transient_error(&msg);
                run_res = Err(e);
                if !can_retry {
                    break;
                }
                let delay_ms = retry_policy.delay_for_retry(attempt).unwrap_or(2000);
                attempt += 1;
                tracing::info!(
                    "流式瞬态错误，{}ms 后重试 {}/{}：{}",
                    delay_ms,
                    attempt,
                    retry_policy.transient_delays_ms.len(),
                    &msg.chars().take(120).collect::<String>()
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }
    }
    // （心跳由 _hb 守卫在函数返回时停止；主流结束后仍需覆盖去味阶段）
    // 用户点了「停止」：不是错误，不向前端发 {"err"}；已收到的部分内容照常落库（interrupted=1）。
    // 截断（F14：length / 裸 EOF / content_filter / tool_calls）也不是完成：
    // 同样按中断处理——只留消息记录，绝不当完整正文、不 humanize、不落盘。
    let mut incomplete_reason: Option<String> = None;
    let interrupted = match &run_res {
        Ok(()) => false,
        Err(e) => {
            if molan_llm::is_cancelled_err(e) {
                true
            } else if molan_llm::is_incomplete_err(e) {
                incomplete_reason = Some(format!("{}", e));
                true
            } else if !cx.full.is_empty() {
                // 中途 429/502 等已有文本：保留残稿供作者决定，不因错误丢弃整个回复。
                incomplete_reason = Some(format!("上游中断，已保留未完成文本：{}", e));
                true
            } else if molan_llm::is_transient_error(&format!("{}", e)) {
                // P0/P3b：零输出瞬态故障——先试显式回退模型；无回退或回退失败时
                // limit() 已发 UPSTREAM_LIMIT 结构化事件并返回 true 收口（不抛裸 err 行）。
                if fallback::limit(db, role, &chn, &params, &mut cx, e).await? {
                    return Ok(None);
                }
                false
            } else {
                run_res?;
                false
            }
        }
    };

    let mut full = cx.full.clone();
    // 明确告知前端：本次是中断文本，不是可用成果。
    if interrupted {
        fallback::emit_interrupted(&mut cx, &incomplete_reason, &chn, role).await;
    }
    // 空输出守卫：非取消却零产出（真实渠道偶发）——不存空消息污染会话，直接向前端报错
    if full.is_empty() && !interrupted {
        cx.error("模型返回了空内容，请重试或更换渠道").await;
        return Ok(Some(json!({"ok": false, "empty": true})));
    }
    // 本次产出是否会被当作「正文」——与下面的落盘判定保持一致
    let want_ch = requested_chapter(&message);
    let is_outline_cmd = task_hint == "outline";
    let is_body_cmd = task_hint == "body";
    let is_body_output = is_body_cmd && (has_body_heading(&full) || full.chars().count() >= 800);
    // 生成正文后的自动审核去AI味：读本书「自动去味」设置（默认官方标准），
    // 机器门评分 → 未过线自动修（最多 2 轮）。
    // 中断/截断的产出绝不当成果：不 humanize、不落盘、不 saved。
    // 只把中断文本留在聊天记录里（interrupted=1），由作者决定是否取回。
    // 取消请求判定：令牌（本次 turn 全程有效）或 ABORTS 集合任一置位即为「已请求停止」。
    let abort_requested = |tok: &Option<tokio_util::sync::CancellationToken>| -> bool {
        tok.as_ref().map(|t| t.is_cancelled()).unwrap_or(false)
    };
    let mut interrupted = interrupted;
    let mut save_errors: Vec<String> = Vec::new();
    let mut saved_files: Vec<String> = if !interrupted {
        // 生成正文后的自动审核去AI味：读本书「自动去味」设置（默认官方标准），
        // 机器门评分 → 未过线自动修（最多 2 轮）。
        // auto_humanize 内部读的是全局 AUTO_CANCEL（别的任务），拿不到本轮 requestId 令牌，
        // 所以这里用 select! 与本轮令牌赛跑：停止即 drop 该 future，不再等它跑完。
        if a("writeIntent").as_str() == Some("explicit-task")
            && !book_id.is_empty()
            && full.chars().count() >= 300
            && is_body_output
        {
            let hz_plan = super::run_plan::humanize_stage(db, &st.root, &book_id, args);
            let humanize = auto_humanize(db, &book_id, &full, &hz_plan, "chapter");
            let res = match cancel_tok.clone() {
                Some(tok) => tokio::select! {
                    biased;
                    _ = tok.cancelled() => None,
                    r = humanize => Some(r),
                },
                None => Some(humanize.await),
            };
            match res {
                Some((hb, rep)) => {
                    if hb != full {
                        full = hb;
                        cx.full = full.clone();
                    }
                    cx.ev(json!({"type": "deai", "score": rep["score"], "passed": rep["passed"],
                            "method": rep["method"], "rounds": rep["rounds"],
                            "violations": rep["violations"].as_array().map(|x| x.len()).unwrap_or(0)}))
                            .await;
                    // 章节状态机：去味后文本指纹（只观测不阻断）
                    if let Some(ch) = want_ch {
                        let _ = molan_core::chapter_state::record_humanized(
                            db,
                            &book_id,
                            ch,
                            &molan_core::continuity::content_hash(&full),
                        );
                    }
                }
                None => {
                    // 去味（第二次 LLM 调用）被中断/取消，但**正文已经完整生成**：
                    // 保留原稿继续落盘，绝不因一次润色失败把整章丢掉。
                    tracing::warn!(
                        "去味步骤被中断，保留原稿并继续落盘（{}字）",
                        full.chars().count()
                    );
                }
            }
        }
        // humanize 之后、落盘之前再同步确认一次：停止优先于未提交成果。
        if abort_requested(&cancel_tok) || take_abort(&req_id) {
            interrupted = true;
        }
        if interrupted {
            // 已请求停止：不写任何文件，只留中断文本。
            Vec::new()
        } else {
            // 落盘失败必须显式报错，绝不 emit saved 假成功。
            // 选区改写绝不原位直写（客户端参数不构成审批授权）；统一由前端「写回原文」走 dw_create_proposal 提案审批。
            let mut files_saved: Vec<String> = Vec::new();
            let ct = cancel_tok.clone();
            let rid = req_id.clone();
            if allow_stream_save && (is_outline_cmd || is_body_cmd) {
                match auto_save_chat_output_checked(
                    db,
                    &book_id,
                    &full,
                    want_ch,
                    is_outline_cmd,
                    || {
                        if abort_requested(&ct) || take_abort(&rid) {
                            return Err(anyhow!("已请求停止，放弃落盘"));
                        }
                        Ok(())
                    },
                ) {
                    Ok((saved, skipped)) => {
                        files_saved.extend(saved);
                        save_errors.extend(skipped);
                    }
                    Err(e) => save_errors.push(format!("结构化产出未完整保存：{}", e)),
                }
            }
            // 落盘兜底：明确的正文写作任务（技能/指令点名写第N章）但输出无「第X章」标题时，
            // 存为待审 第N章.md（book_id 为空时严禁落盘）。
            if files_saved.is_empty()
                && allow_stream_save
                && full.chars().count() >= 800
                && !book_id.is_empty()
                && is_body_cmd
                && want_ch.is_some()
            {
                let target = want_ch.unwrap_or_default();
                let fname = format!("第{}章.md", target);
                // 兜底正文同样只进入待审；即使点名章节，也不自动覆盖正式稿。
                let has_review =
                    files::read_file(db, &book_id, molan_core::db::REVIEW_GROUP, &fname).is_some();
                if has_review {
                    save_errors.push(format!(
                        "第{}章已有待审稿，未覆盖；请先处理审批队列",
                        target
                    ));
                } else if abort_requested(&cancel_tok) || take_abort(&req_id) {
                    save_errors.push("已请求停止，放弃落盘".to_string());
                    interrupted = true;
                } else {
                    let ct2 = cancel_tok.clone();
                    let rid2 = req_id.clone();
                    // 写盘 + 待审登记 + 章节状态在同一把锁内完成（chapter_commit）
                    match molan_core::chapter_commit::submit_pending(
                        db,
                        &book_id,
                        target,
                        &full,
                        "chat",
                        move || {
                            if abort_requested(&ct2) || take_abort(&rid2) {
                                return Err(anyhow!("已请求停止，放弃落盘"));
                            }
                            Ok(())
                        },
                    ) {
                        Err(e) => save_errors.push(format!("第{}章待审：{}", target, e)),
                        Ok(_) => files_saved.push(format!(
                            "{} / {}",
                            molan_core::db::REVIEW_GROUP,
                            fname
                        )),
                    }
                }
            }
            // 细纲指令兜底：输出没带可识别的「细纲_第N章」标题时，按消息里的章号写
            // 「细纲/细纲_第N章.md」（不覆盖已有细纲）。否则细纲产出会被整段丢弃（全链路 S2）。
            if files_saved.is_empty() && allow_stream_save && is_outline_cmd && !book_id.is_empty()
            {
                if let Some(ch) = want_ch {
                    let fname = format!("细纲_第{}章.md", ch);
                    let existing = files::read_file(db, &book_id, "细纲", &fname)
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if existing.is_empty() && full.trim().chars().count() >= 200 {
                        let ct2 = cancel_tok.clone();
                        let rid2 = req_id.clone();
                        match files::write_ai_file_checked(
                            db,
                            &book_id,
                            "细纲",
                            &fname,
                            full.trim(),
                            move || {
                                if abort_requested(&ct2) || take_abort(&rid2) {
                                    return Err(anyhow!("已请求停止，放弃落盘"));
                                }
                                Ok(())
                            },
                        ) {
                            Ok(()) => {
                                tracing::info!("细纲兜底落盘：{}", fname);
                                let _ = molan_core::chapter_state::record_outline(
                                    db,
                                    &book_id,
                                    ch,
                                    &molan_core::continuity::content_hash(full.trim()),
                                );
                                files_saved.push(format!("细纲 / {}", fname));
                            }
                            Err(e) => {
                                tracing::warn!("细纲兜底落盘失败（{}）：{}", fname, e);
                                save_errors.push(format!("细纲兜底落盘失败（{}）：{}", fname, e));
                            }
                        }
                    }
                }
            }
            files_saved
        }
    } else {
        Vec::new()
    };
    // 优雅停机兜底：被 SIGTERM 掐断的在途流（含去味阶段被停机取消的情形），
    // 服务端自动把**正文任务**的残稿存正文待审并登记队列（作者不在场也兜底）；
    // 与用户主动停止区分（停机不写 ABORTS，故 take_abort 为 false 才兜底）。
    // 停机兜底同样受授权门约束：只有显式写作任务的残稿才可能被处理，普通对话绝不自动写书。
    // 授权门之后还要按任务类型分流：细纲/大纲的残稿绝不冒充正文（截断细纲两头都不落盘），
    // 而每一条「没落盘」的分支都必须进 save_errors——静默丢弃等于让作者以为稿已保住。
    if saved_files.is_empty()
        && interrupted
        && shutting_down()
        && !take_abort(&req_id)
        && (is_body_cmd || is_outline_cmd)
        && allow_stream_save
    {
        let raw = cx.full.clone();
        // 先定章号：残稿标题里的「# 第N章」优先，其次本轮消息点名的章号；
        // 两者都没有就是定位失败，绝不猜 cur_max+1（那会把无关章节写进别人的槽）。
        let ch = raw
            .lines()
            .find_map(|l| {
                let t = l.trim();
                if t.starts_with('#') {
                    chapter_num_from_name(t.trim_start_matches('#').trim())
                } else {
                    None
                }
            })
            .or_else(|| requested_chapter(&message))
            .unwrap_or(0);
        if book_id.is_empty() {
            save_errors.push("停机残稿未落盘：本轮未关联书籍（只留聊天记录）".to_string());
        } else if raw.chars().count() < 400 {
            save_errors.push(format!(
                "停机残稿未落盘：残稿仅 {}字（<400），不足以成章（只留聊天记录）",
                raw.chars().count()
            ));
        } else if ch <= 0 {
            save_errors.push(
                "停机残稿未落盘：无法定位章号（输出无 # 第N章 标题、消息也未点名章号），不猜章号"
                    .to_string(),
            );
        } else if is_outline_cmd {
            // 细纲/大纲任务的残稿只能是细纲：绝不冒充正文进审批队列；
            // 截断的细纲也不写「细纲」组（半成品会污染后续章节规划）——只报错，让作者重跑。
            save_errors.push(format!(
                "停机中断的细纲残稿（{}字）未落盘：截断细纲不进细纲组，更不进正文待审；请重跑第{}章细纲",
                raw.chars().count(),
                ch
            ));
        } else {
            let fname = format!("第{}章.md", ch);
            let review_cur = files::read_file(db, &book_id, molan_core::db::REVIEW_GROUP, &fname)
                .unwrap_or_default();
            if !review_cur.trim().is_empty() {
                save_errors.push(format!("停机残稿未落盘：第{}章已有待审稿，未覆盖", ch));
            } else if let Err(e) = molan_core::chapter_commit::submit_pending(
                db,
                &book_id,
                ch,
                &raw,
                "chat:interrupted",
                || Ok(()),
            ) {
                save_errors.push(format!("停机残稿落盘失败：{}", e));
            } else {
                tracing::info!("停机残稿已存正文待审：{}", fname);
                let _ = molan_core::chapter_state::record_interrupted(
                    db,
                    &book_id,
                    ch,
                    "优雅停机中断，残稿已入待审",
                );
                saved_files.push(format!("{} / {}", molan_core::db::REVIEW_GROUP, fname));
            }
        }
    }
    // 中断且未能落盘：状态机如实记 INTERRUPTED（有章号可定位时）
    if interrupted && saved_files.is_empty() {
        if let Some(ch) = want_ch {
            let _ = molan_core::chapter_state::record_interrupted(
                db,
                &book_id,
                ch,
                "生成中断，产出未落盘",
            );
        }
    }
    // 只有真正写盘成功才发 saved；中断/失败一律不发（不假成功）。
    if !saved_files.is_empty() {
        cx.ev(json!({"type": "saved", "files": saved_files, "pending": true}))
            .await;
    }
    let is_quote_rewrite = message.contains("【引用选区");
    if saved_files.is_empty() && !interrupted && (full.chars().count() >= 120 || is_quote_rewrite) {
        let (reason, manual_save_hint) = if is_quote_rewrite {
            ("选区改写产出不自动整章落盘（按契约仅整章结构自动落盘），可用「写回原文」按钮生成提案".to_string(), true)
        } else if book_id.is_empty() {
            ("未关联书籍，跳过落盘".to_string(), false)
        } else if auto_save_mode != "auto" && (is_body_cmd || is_outline_cmd) {
            (format!("本书「对话自动写入」为「{}」：产出只留在聊天，需要时点「保存到书籍目录」手动写入", auto_save_mode), true)
        } else if !is_body_cmd && !is_outline_cmd {
            ("普通对话不自动写书：任何策略下都只出预览；请点击「保存到书籍目录」选择目标分组后手动写入".to_string(), true)
        } else if !has_body_heading(&full) && !is_body_cmd {
            ("输出正文不含「# 第N章」标题结构，按契约整章结构才可自动落盘；片段请走「写回原文」提案通道".to_string(), false)
        } else if is_body_cmd && !has_body_heading(&full) && want_ch.is_none() {
            ("无法定位目标章（输出无 # 第N章 标题、指令也未点名章号），未尝试落盘；请指明章号或点击「保存到书籍目录」".to_string(), false)
        } else {
            ("同名正文/正文待审稿已存在或写入被拒，未覆盖现有稿；请先处理审批队列，或点「保存到书籍目录」手动保存".to_string(), false)
        };
        if !(manual_save_hint && auto_save_mode == "auto" && (is_outline_cmd || is_body_cmd)) {
            cx.ev(json!({"type": "save_skipped", "reasons": [reason]}))
                .await;
        }
    }
    if !save_errors.is_empty() {
        cx.ev(json!({"type": "save_skipped", "reasons": save_errors}))
            .await;
    }
    // 单章审核后关（T4b）：显式写作任务的正文待审**落盘成功后**，对已落盘内容做一次剧情审核。
    // 顺序：写前=文风卡约束 → 正文 → 去味（上方 auto_humanize 门，既有）→ 待审落盘 → 审核（此处）。
    // 审核对象 = 实际写入待审的文件正文，bodyHash 与之一一对应（再改正文即作废）。
    // 审核失败/无渠道一律 ok:false + reason（fail-closed）：不阻断已完成的保存，也不冒充通过。
    let mut review_summary: Option<Value> = None;
    if !interrupted
        && is_body_cmd
        && a("writeIntent").as_str() == Some("explicit-task")
        && !book_id.is_empty()
        && full.chars().count() >= 300
    {
        let rv_plan = super::run_plan::review_stage(db, &st.root, &book_id, args);
        if let Some((ev, summary)) = super::chapter_review::review_pending_saved(
            db,
            &book_id,
            want_ch,
            &saved_files,
            cancel_tok.clone(),
            &rv_plan,
        )
        .await
        {
            cx.ev(ev).await;
            review_summary = Some(summary);
        }
    }
    let mut parseable = molan_llm::extract_json(&full);
    // 中断文本不是成果：不解析 bookSetup、不建书、不落盘（只留聊天记录）。
    if !interrupted {
        if let Some(p) = parseable.as_mut() {
            if p.is_object() {
                // 定档建书**只出预览卡**：服务端绝不代作者落库/改名/写文件。
                // 书名与建书档案必须由作者在前端确认后，经 IPC apply_book_setup 提交
                // （那是唯一写入路径，且自带消息刷新）。这里只把模型给的 bookSetup 原样
                // 回传，并强制标记 saved=false，避免前端误渲染成「已创建」完成态。
                mark_book_setup_preview(p);
                cx.result(p).await;
            }
        }
    }
    // 兜底预览：共创会话里作者已定档（回复「定/开写/确认」等）但模型没按约定输出 bookSetup JSON 时，
    // 从本轮或最近一条档案正文里拼一张 bookSetup 预览卡，交作者确认。
    // 与上面模型输出的 bookSetup 一样：**只预览，绝不落库/改名/写文件**（saved 恒为 false）。
    // 真正的写入只能由作者在前端点确认后，走 IPC apply_book_setup。
    let has_book_setup = parseable
        .as_ref()
        .map(|p| p.get("bookSetup").is_some())
        .unwrap_or(false);
    let is_confirm = is_book_setup_confirm(&message);
    let is_cocreate = skill_names
        .iter()
        .filter_map(|v| v.as_str())
        .any(|s| s == "新书共创");
    if !interrupted && !has_book_setup && is_confirm && is_cocreate && !book_id.is_empty() {
        let mut archive = String::new();
        if full.chars().count() >= 1200 {
            archive = full.clone();
        } else {
            // 本轮回复太短：往前找最近的档案（含「建书档案」标记或足够长的助手消息）
            let rows = db
                    .q_json(
                        "SELECT content FROM messages WHERE session_id=?1 AND role='assistant' AND content != '' ORDER BY created_at DESC LIMIT 6",
                        &[&session_id as &dyn rusqlite::ToSql],
                    )
                    .unwrap_or_default();
            for r in rows {
                let c = r["content"].as_str().unwrap_or("");
                if c.contains("建书档案") || c.chars().count() >= 1500 {
                    archive = c.to_string();
                    break;
                }
            }
        }
        if !archive.is_empty() {
            // 书名取档案里第一个《…》，找不到则留空——只用于预览展示，不改库
            let title = extract_book_title_from_archive(&archive).unwrap_or_default();
            let result = json!({
                "bookSetup": {
                    "titles": [if title.is_empty() { json!("") } else { json!(title) }],
                    "files": [{"group": "设定", "name": "建书档案.md", "content": archive}],
                    "saved": false,
                }
            });
            parseable = Some(result.clone());
            cx.result(&result).await;
        }
    }
    // 审核摘要并入本轮 result_json（前端消息卡可回看审核结论；不覆盖模型原有字段）
    if let Some(rv) = review_summary {
        if let Some(m) = parseable.get_or_insert_with(|| json!({})).as_object_mut() {
            m.insert("review".to_string(), rv);
        }
    }
    // 保存助手消息
    let result_json = parseable
        .as_ref()
        .map(|p| p.to_string())
        .unwrap_or_default();
    // 与 user 消息保持同构：前端渲染助手消息时会读 context.files（缺字段/为 null 会整页崩）
    let ctx_json = json!({"requestId": a("requestId"), "runId": a("runId"), "skills": skill_names, "files": json!([])}).to_string();
    cx.snapshot = None; // Flush pending recovery text before writing the final result.
    let now = stats::now_ms();
    if !session_id.is_empty() {
        db.exec(
                "INSERT INTO messages(id,session_id,role,content,context_json,steps_json,result_json,created_at,interrupted) SELECT ?1,?2,?3,?4,?5,NULL,?6,?7,?8 WHERE EXISTS(SELECT 1 FROM sessions s JOIN books b ON b.id=s.book_id WHERE s.id=?2 AND b.deleted_at IS NULL) ON CONFLICT(id) DO UPDATE SET content=excluded.content,context_json=excluded.context_json,result_json=excluded.result_json,interrupted=excluded.interrupted",
                &[
                    &msg_id as &dyn rusqlite::ToSql, &session_id, &"assistant",
                    &full, &ctx_json, &result_json, &now,
                    &(if interrupted { 1i64 } else { 0i64 }) as &dyn rusqlite::ToSql,
                ],
            )?;
        db.exec(
            "UPDATE sessions SET msg_count=(SELECT COUNT(*) FROM messages WHERE session_id=?1), updated_at=?2 WHERE id=?1",
            &[&session_id as &dyn rusqlite::ToSql, &now],
        )?;
        stats::refresh_words(db, &book_id);
    }
    cx.done(json!({"messageId": msg_id, "model": chn["model"], "reasoningChars": 0, "outputChars": full.chars().count()})).await;
    Ok(Some(json!({"ok": true, "messageId": msg_id})))
}

/// inline_chat / inline_assist：编辑器内联助手（原 dispatch 分支体原样搬入）。
pub(crate) async fn inline_chat(
    st: &Arc<AppState>,
    cmd: &str,
    args: &Value,
    tx: &tokio::sync::mpsc::Sender<String>,
) -> Result<Option<Value>> {
    let db = &st.db;
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();
    let channel = channel_id(&a("onEvent"));
    let mut cx = Ctx::new(tx, &channel);
    let mode0 = s("mode");
    // 审稿维度走 review 智能体渠道（更强的模型，JSON 遵循度更高）；其余走活跃渠道
    let chn = if matches!(
        mode0.as_str(),
        "preview" | "pace" | "consistency" | "review-skill"
    ) {
        molan_llm::resolve_agent_channel(db, "review")
            .or_else(|| molan_llm::active_channel(db))
            .ok_or_else(|| anyhow!("未配置可用的模型渠道"))?
    } else {
        molan_llm::active_channel(db).ok_or_else(|| anyhow!("未配置可用的模型渠道"))?
    };
    let mode = mode0;
    let book_id = s("bookId");
    let instruction = s("instruction");
    // 输入文本：选区工具条（inline_chat）发 selection；底部工具条（inline_assist）发 docContext
    let mut input = {
        let v = s("selection");
        if v.is_empty() {
            s("text")
        } else {
            v
        }
    };
    if input.is_empty() {
        input = s("docContext");
    }
    let body_only = matches!(
        mode.as_str(),
        "deai" | "preview" | "pace" | "consistency" | "review-skill"
    );
    // 起名只需要作者描述；其余模式都要有正文/选区
    if input.trim().is_empty() && mode != "name" {
        return Err(anyhow!(if cmd == "inline_assist" || body_only {
            "这一章还没有正文"
        } else {
            "没有选中文字"
        }));
    }

    // ── 审查：本地机器门，不调模型 ──
    if mode == "review" {
        let report = molan_core::deai::score_text(&input);
        let mut md = String::from("## 去AI味审查报告\n\n");
        let score = report["score"].as_i64().unwrap_or(0);
        md.push_str(&format!(
            "**AI味评分：{} / 100**（通过线 ≤32，越低越像真人）——{}\n\n",
            score,
            if report["passed"].as_bool().unwrap_or(false) {
                "✅ 过线"
            } else {
                "⚠️ 超线，建议处理"
            }
        ));
        if let Some(arr) = report["violations"].as_array() {
            if arr.is_empty() {
                md.push_str("未检出 AI 写作特征。\n");
            }
            for v in arr {
                // tier5 结构类指标不是「处数」，只给结论与改法
                if v["tier"].as_i64() == Some(5) {
                    md.push_str(&format!(
                        "- **{}**：{}\n",
                        v["rule"].as_str().unwrap_or("?"),
                        v["advice"].as_str().unwrap_or("")
                    ));
                } else {
                    md.push_str(&format!(
                        "- **{}**（{}处）：{}\n",
                        v["rule"].as_str().unwrap_or("?"),
                        v["count"].as_i64().unwrap_or(0),
                        v["advice"].as_str().unwrap_or("")
                    ));
                }
            }
        }
        let m = report["metrics"].clone();
        md.push_str(&format!(
            "\n---\n字数 {} · 段落 {} · 平均段长 {} 字\n",
            m["chars"].as_i64().unwrap_or(0),
            m["paras"].as_i64().unwrap_or(0),
            m["avg_para"].as_f64().unwrap_or(0.0)
        ));
        for chunk in split_chunks(&md, 220) {
            cx.delta(&chunk).await;
        }
        cx.done(json!({})).await;
        return Ok(Some(json!({"ok": true, "report": md, "score": score})));
    }

    let mut sys = String::new();
    let mut user = String::new();
    let mut no_thinking = false;
    let mut temperature = 0.7f64;
    let mut max_tokens: i64 = molan_llm::get_setting(db, "max_tokens")
        .parse()
        .unwrap_or(8192i64);

    match mode.as_str() {
        // ── 去AI味（选区与整章共用）。输出必须是纯正文：前端「替换」把结果原样写回文档 ──
        "deai" => {
            // humanizeOverride 三态（与批量路径 resolve_humanize 同一实现，语义不再分叉）：
            //   ""       = 回落书级 book_humanize__<book>，再缺省 official:standard
            //   "none"   = 明确关闭（不注入任何方法提示词）
            //   "official:*" / "skill:<id>" = 显式指定
            let (_method, method_prompt) =
                super::auto_write::resolve_humanize(db, &book_id, a("humanizeOverride").as_str());
            let report = molan_core::deai::score_text(&input);
            sys.push_str(
                    "你是网文成章质检引擎（去AI味整合门）。对给定正文做去AI味改写。\n\
                     【保真红线（最高优先级）】人物、事实、数字、时间、因果、信息点一项不能丢；原文的每个动作、每句对白都要有对应表达（可换说法，不可删内容）；只改「怎么说」不改「说什么」。\n\
                     【禁止】重写未命中的段落、调整段落结构、增删剧情、美化文采、修正错字与标点瑕疵（瑕疵=真人指纹）。\n\
                     【输出】只输出改写后的正文全文（保留原有章节标题行），不加解释、不加报告、不加代码块。\n\n",
                );
            if !method_prompt.is_empty() {
                sys.push_str(&method_prompt);
                sys.push_str("\n\n");
            }
            // 编辑器里选中的往往只是几句话，而多数去味方法（含用户技能）是按整章写的，
            // 会因「输入不是完整章节」而拒答。这里显式声明输入形态，优先级高于方法提示词。
            let is_fragment = !has_body_heading(&input) && input.chars().count() < 1500;
            if is_fragment {
                sys.push_str(
                        "【本次输入是节选片段（最高优先级，覆盖上文任何「需要整章/完整正文」的要求）】\n\
                         作者在编辑器里只选中了正文中的一小段，这就是全部输入，没有更多上下文。\n\
                         只改写这段文字本身：保持原意、人称、时态与信息量，长度与原文相当。\n\
                         禁止输出「未提供正文」「请粘贴完整章节」「无法执行」等任何索要材料的话；禁止解释、报告、代码块、标题行；只输出改写后的这段文字。\n\n",
                    );
            }
            let viols = report["violations"].as_array().cloned().unwrap_or_default();
            if !viols.is_empty() {
                sys.push_str("【机器门检出（优先修复）】\n");
                for v in &viols {
                    sys.push_str(&format!(
                        "- {}（{}处）：{}\n",
                        v["rule"].as_str().unwrap_or("?"),
                        v["count"].as_i64().unwrap_or(0),
                        v["advice"].as_str().unwrap_or("")
                    ));
                    if let Some(ms) = v["matches"].as_array() {
                        let list = ms
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(|x| format!("「{}」", x))
                            .collect::<Vec<_>>()
                            .join("");
                        if !list.is_empty() {
                            sys.push_str(&format!(
                                "    命中原文（必须逐一改掉或删除）：{}\n",
                                list
                            ));
                        }
                    }
                }
                sys.push_str("\n【死命令】上面列出的「命中原文」必须逐一从正文消失——换成符合上下文的具体动作/细节/台词；直接引语必须保留引号与说话人，严禁改成「问道…」「回答说…」式转述。\n");
                sys.push('\n');
            }
            sys.push_str(&format!(
                "【当前机器评分】{}/100（目标 ≤32）",
                report["score"].as_i64().unwrap_or(0)
            ));
            user = if is_fragment {
                format!(
                    "【原文（作者选中的片段，只改写这一段）】\n{}",
                    input.chars().take(16000).collect::<String>()
                )
            } else {
                format!(
                    "【原文】\n{}",
                    input.chars().take(16000).collect::<String>()
                )
            };
            no_thinking = true;
            temperature = 0.6;
            max_tokens = max_tokens.max(16384);
        }
        // ── 底部工具条 · 写作组：每行一条候选（前端拆行，点击插到光标处）──
        "stuck" => {
            sys = "你是网文责编。作者写到一半卡住了，读当前草稿，就地给出 4~5 条可以直接接着写的走向。\n每行一条，独立成行；不要序号、不要标题、不要解释；每条具体到人物、动作与悬念，一句到两句为止。".to_string();
            user = format!(
                "【当前草稿（写到光标处停了）】\n{}",
                input.chars().take(4000).collect::<String>()
            );
            no_thinking = true;
            temperature = 0.9;
        }
        "name" => {
            sys = "你是网文起名师。按作者描述与本书题材气质，起 6~8 个候选名字。\n每行一个，格式：名字——寓意与气质（一句话）。不要序号、不要其他解释。".to_string();
            user = format!(
                "【要起名的对象】{}",
                if instruction.trim().is_empty() {
                    "本章新出场人物"
                } else {
                    instruction.trim()
                }
            );
            no_thinking = true;
            temperature = 0.95;
        }
        "idea" => {
            sys = "你是网文策划。按作者给的引子，结合本章已有内容，发散 4~6 条可落笔的点子。\n每行一条，具体到冲突、人物与钩子；不要序号与解释。".to_string();
            user = format!(
                "【引子】{}\n\n【本章内容】\n{}",
                if instruction.trim().is_empty() {
                    "给这一章加个变数"
                } else {
                    instruction.trim()
                },
                input.chars().take(4000).collect::<String>()
            );
            no_thinking = true;
            temperature = 0.95;
        }
        // ── 审这一章 · 三维度：markdown 意见 + 末尾 ```json 结构化数据（前端渲染卡片）──
        "preview" => {
            sys = "你是网文读者反馈模拟器。通读整章，模拟 4~6 位不同画像的真实网文读者逐段反应，再给编辑解读。\n输出两部分：\n1) 先写 markdown 简评（## 读者反应，不写代码块）；\n2) 最后单独用 ```json 围栏输出结构化数据，字段严格如下（chaseRate 为 0~1 小数，atPara 是从 1 数的段落序号可为 null）：\n{\"verdict\":\"will_chase|maybe_drop|must_fix\",\"chaseRate\":0.7,\"oneLine\":\"一句话总评\",\"comments\":[{\"id\":\"r1\",\"avatar\":\"一个emoji\",\"name\":\"读者绰号\",\"persona\":\"读者画像\",\"mood\":\"love|urge|want_skip|confused|almost_drop\",\"text\":\"读者口吻的评论\",\"likes\":123,\"atPara\":5,\"quote\":\"引发反应的原文短句\",\"interpret\":{\"severity\":\"info|warn|risk\",\"means\":\"编辑解读\",\"fix\":\"怎么改\"}}]}".to_string();
            user = format!(
                "【本章正文】\n{}",
                input.chars().take(16000).collect::<String>()
            );
            max_tokens = max_tokens.max(8192);
        }
        "pace" => {
            sys = "你是网文节奏编辑。给整章画张力曲线并给可执行改法。\n输出两部分：\n1) 先写 markdown 简评（## 节奏诊断，不写代码块）；\n2) 最后单独用 ```json 围栏输出：{\"verdict\":\"tight|ok|draggy\",\"readThrough\":0.6,\"oneLine\":\"一句话总评\",\"points\":[{\"pos\":30,\"tension\":70,\"kind\":\"setup|beat|hook|drag\",\"label\":\"这一段在干嘛\",\"quote\":\"原文短句\"}],\"cuts\":[{\"title\":\"改法名\",\"action\":\"具体怎么动刀\",\"quote\":\"示例句\"}]}\npoints 按章内顺序 6~12 个（pos/tension 均 0~100），cuts 1~3 条。".to_string();
            user = format!(
                "【本章正文】\n{}",
                input.chars().take(16000).collect::<String>()
            );
            max_tokens = max_tokens.max(8192);
        }
        "consistency" => {
            sys = "你是网文校对编辑。对照本书设定与前文，找整章的设定/时间线/人物/物品/称谓矛盾。\n输出两部分：\n1) 先写 markdown 简评（## 一致性，不写代码块）；\n2) 最后单独用 ```json 围栏输出：{\"verdict\":\"clean|minor|conflict\",\"oneLine\":\"一句话总评\",\"conflicts\":[{\"id\":\"c1\",\"severity\":\"info|warn|risk\",\"kind\":\"设定|时间线|人物|物品|称谓\",\"title\":\"矛盾点\",\"current\":{\"note\":\"本章的说法\",\"quote\":\"原文短句\"},\"prior\":{\"source\":\"出处（第N章/设定表）\",\"note\":\"前文的说法\",\"quote\":\"原文短句\"},\"suggestion\":\"怎么改\"}]}\n没有矛盾时 conflicts 为空数组。".to_string();
            let archive = files::read_file(db, &book_id, "设定", "建书档案.md").unwrap_or_default();
            let people = files::read_file(db, &book_id, "设定", "人物表.md").unwrap_or_default();
            user = format!(
                "【设定参照】\n{}\n{}\n\n【本章正文】\n{}",
                smart_archive(&archive, 1200),
                people.chars().take(800).collect::<String>(),
                input.chars().take(16000).collect::<String>()
            );
            max_tokens = max_tokens.max(8192);
        }
        // ── 审这一章 · 自定义审校视角（技能广场里的 review 技能）──
        "review-skill" => {
            let sid = s("reviewSkillId");
            let tpl = if sid.is_empty() {
                String::new()
            } else {
                db.q_json(
                    "SELECT prompt_template FROM skills WHERE id=?1",
                    &[&sid as &dyn rusqlite::ToSql],
                )
                .ok()
                .and_then(|v| {
                    v.first()
                        .and_then(|r| r["promptTemplate"].as_str().map(|x| x.to_string()))
                })
                .unwrap_or_default()
            };
            sys = format!(
                    "你是网文审稿编辑。按下面的审校视角对整章出审稿报告：用 markdown，## 分节，每条意见尽量引用原文短句；无问题的维度写「未见明显问题」。\n\n{}",
                    tpl
                );
            user = format!(
                "【本章正文】\n{}",
                input.chars().take(16000).collect::<String>()
            );
            max_tokens = max_tokens.max(8192);
        }
        // ── 选区工具条 · 改写组（inline_chat）──
        "rewrite" => {
            sys = "你是资深网文编辑。改写给定选段：换句法（长句重组/主谓调整/叙述角度切换）、「汇报式」改「现场式」，保住情节事实不动。只输出改写结果，不要解释。".to_string();
        }
        "expand" => {
            sys = "你是资深网文编辑。扩写给定选段：补具体动作、细节与人物反应，不新增情节、不注水描写。扩写幅度约 1.5~2 倍。只输出扩写结果，不要解释。".to_string();
        }
        "condense" => {
            sys = "你是资深网文编辑。精简给定选段：删冗余修饰与重复信息，保留全部情节事实与关键细节。只输出精简结果，不要解释。".to_string();
        }
        "polish" => {
            sys = "你是资深网文编辑。润色给定选段：提升文字质感与节奏，不改剧情不改人设，风格贴住上下文。只输出润色结果，不要解释。".to_string();
        }
        "scene" => {
            sys = "你是资深网文编辑。增强给定选段的场景感：补感官细节（视/听/触/嗅）与空间调度，不改情节。只输出增强结果，不要解释。".to_string();
        }
        "tone" => {
            sys = "你是资深网文编辑。按用户给的基调改写给定选段，保住情节事实。只输出结果，不要解释。".to_string();
        }
        _ => {
            sys = "你是资深网文编辑。按用户指令处理给定选段，保住情节事实。只输出结果，不要解释。"
                .to_string();
        }
    }

    // 选区改写组兜底 user 组装（专属分支已赋值的不动）
    if user.is_empty() {
        user = format!(
            "【指令】{}\n\n【选中原文】\n{}",
            if instruction.is_empty() {
                "按要求处理"
            } else {
                instruction.as_str()
            },
            input.chars().take(8000).collect::<String>()
        );
    }
    // 审稿维度：强制要求末尾 json（弱模型常漏）
    if matches!(mode.as_str(), "preview" | "pace" | "consistency") {
        sys.push_str("\n\n【硬性要求】回答的最后必须是一个 ```json 代码块（结构化数据），缺失视为无效报告；代码块之前是给人看的 markdown 意见。");
    }
    // 本书文风注入（去味/候选/审稿都贴本书语感）
    if !book_id.is_empty() {
        let genre = db
            .q_json(
                "SELECT genre FROM books WHERE id=?1",
                &[&book_id as &dyn rusqlite::ToSql],
            )
            .ok()
            .and_then(|v| {
                v.first()
                    .and_then(|r| r["genre"].as_str().map(|x| x.to_string()))
            });
        if let Some(bst) = resolve_book_style(db, &st.root, &book_id, genre.as_deref()) {
            if !bst.is_empty() {
                sys.push_str("\n\n【本书文风】\n");
                sys.push_str(&bst.chars().take(600).collect::<String>());
            }
        }
    }

    let params = ChatParams {
        base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: chn["key"].as_str().unwrap_or("").to_string(),
        model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
        messages: vec![
            json!({"role": "system", "content": sys}),
            json!({"role": "user", "content": user}),
        ],
        temperature,
        max_tokens,
        stream: true,
        no_thinking,
        reasoning_effort: String::new(),
        // 内联助手是流式路径，用量不落账
        log_tag: String::new(),
        log_book: book_id.to_string(),
        // 内联助手无 requestId 取消通道（拆分/重写等短任务），不参与取消
        cancel: None,
    };
    // 内联助手：短任务；有 requestId 时同样登记取消（空结果同样是失败，不能报 ok）。
    let req_id = s("requestId");
    let inline_cancel = if req_id.is_empty() {
        None
    } else {
        Some(register_abort(&req_id))
    };
    struct AbortGuard<'g> {
        req_id: &'g str,
    }
    impl<'g> Drop for AbortGuard<'g> {
        fn drop(&mut self) {
            if !self.req_id.is_empty() {
                unregister_abort(self.req_id);
            }
        }
    }
    let _abort_guard = AbortGuard {
        req_id: req_id.as_str(),
    };
    let mut p1 = params.clone();
    p1.cancel = inline_cancel;
    if let Err(e) = drain_chat_completion(p1, &mut cx).await {
        if molan_llm::is_cancelled_err(&e) {
            // 取消是正常停止语义：走事件通道（带 code=CANCELLED），不作为请求失败行抛出，
            // 否则前端 .catch 归一化丢掉 code，会把内部哨兵原文显示给用户。
            cx.error(molan_llm::CANCELLED_MSG).await;
            return Ok(None);
        }
        return Err(e);
    }
    let mut full = cx.full.clone();
    // 去AI味兜底：选区很短时模型偶发返回空（思考全占、正文没吐）——清空流缓冲，换极简提示词重试一次
    if mode == "deai" && full.trim().is_empty() {
        let mut p2 = params.clone();
        p2.cancel = if req_id.is_empty() {
            None
        } else {
            Some(register_abort(&req_id))
        };
        p2.messages = vec![
            json!({"role": "system", "content": "你是网文去AI味编辑。只改写作者给的这段文字：换掉模板腔、套话与机械句式，保持原意、人称与篇幅相当，直接输出改写后的文字；不要任何说明、标题、编号或代码块。"}),
            json!({"role": "user", "content": format!("【原文】\n{}", input)}),
        ];
        p2.temperature = 0.7;
        cx.full.clear();
        cx.chars = 0;
        if drain_chat_completion(p2, &mut cx).await.is_ok() {
            let retry = cx.full.clone();
            if !retry.trim().is_empty() {
                full = retry;
            }
        }
    }
    // 空输出守卫：内联助手空结果就是失败，绝不 done+ok 让前端执行空替换。
    if full.trim().is_empty() {
        cx.error("模型返回了空内容，请重试或更换渠道").await;
        return Err(anyhow!("模型返回了空内容，未做任何替换"));
    }
    // 内联改写（deai/rewrite/expand/condense/polish/scene/tone）**不在服务端落盘**：
    // 前端 inline_chat 只传 selection/docContext（经核实无 fileName/fileGroup），
    // 结果由编辑器把它替换回选区。若按 selection 去 CAS 整篇文件，必然「全文 != 选区」而失败，
    // 且一旦匹配就会把整篇正文覆盖成选段——两者都错，故此处不做任何文件写入。
    // 去AI味：回传机器门前后分数，前端在面板上显示「X 分 → Y 分 / 未改写」
    if mode == "deai" {
        let before = molan_core::deai::score_text(&input);
        let after = molan_core::deai::score_text(&full);
        cx.result(&json!({"deai": {
            "scoreBefore": before["score"],
            "scoreAfter": after["score"],
            "passedBefore": before["passed"],
            "passedAfter": after["passed"],
            "changed": full.trim() != input.trim(),
            "charsBefore": input.chars().count(),
            "charsAfter": full.chars().count(),
        }}))
        .await;
    }
    // 审稿维度：服务端从流式全文提取 ```json 块，包成前端期望的 {key: {...}} 结构主动发 result 事件
    if matches!(mode.as_str(), "preview" | "pace" | "consistency") {
        if let Some(obj) = extract_json_block(&full) {
            let wrap_key = match mode.as_str() {
                "preview" => "readerReview",
                "pace" => "pacePlan",
                _ => "consistency",
            };
            // 模型可能已带包装键，也可能直接给内层对象——两种都归一化
            let payload = obj.get(wrap_key).cloned().unwrap_or(obj);
            cx.result(&json!({ wrap_key: payload })).await;
        }
    }
    cx.done(json!({})).await;
    Ok(Some(json!({"ok": true, "report": full})))
}

// 把报告按块切开流式下发（前端按 delta 渲染）
// （旧的 parse_quote_selection/splice_once 文本搜索式选区替换已删除：选区写回统一走
//  DocumentWriteService replace_range，按基线 hash + UTF-16 偏移 + 原文锚点精确替换。）
fn split_chunks(s: &str, n: usize) -> Vec<String> {
    let cs: Vec<char> = s.chars().collect();
    if cs.len() <= n {
        return vec![s.to_string()];
    }
    (0..cs.len())
        .step_by(n)
        .map(|i| cs[i..(i + n).min(cs.len())].iter().collect())
        .collect()
}

// 审稿维度：服务端从流式全文提取 json 块，主动发 result 事件（前端卡片渲染不依赖其自解析）
fn extract_json_block(text: &str) -> Option<Value> {
    let i = text.find("```json")?;
    let rest = &text[i + 7..];
    let end = rest.find("```")?;
    serde_json::from_str::<Value>(rest[..end].trim()).ok()
}

/// 定档建书预览：把模型输出的 bookSetup 归一成「未保存」态。
/// 服务端绝不代作者写库/写文件——只有作者在前端确认后走 IPC apply_book_setup 才落库。
/// saved 恒为 false，防止前端把预览误渲染成「已创建」完成态。
fn mark_book_setup_preview(p: &mut Value) {
    if let Some(bs) = p.get_mut("bookSetup").filter(|v| v.is_object()) {
        if let Some(o) = bs.as_object_mut() {
            o.insert("saved".to_string(), json!(false));
            o.remove("destination");
            o.remove("savedFiles");
        }
    }
}

/// 作者「定档」确认语：命中即视为同意建书，但服务端只据此生成预览，不自动落库。
fn is_book_setup_confirm(message: &str) -> bool {
    let trimmed = message.trim();
    trimmed == "定"
        || [
            "开写",
            "开始写",
            "就这个",
            "定稿",
            "按这个定",
            "定了",
            "确认",
        ]
        .iter()
        .any(|k| message.contains(k))
        || (trimmed.chars().count() <= 6
            && ["定", "好", "行", "可以", "中", "成"]
                .iter()
                .any(|k| trimmed.starts_with(k)))
}

/// 从建书档案正文里取第一个《…》作为预览书名（不落库，仅展示）。
/// 超过 30 字或明显不是书名（括号跨度过大）时返回 None。
fn extract_book_title_from_archive(archive: &str) -> Option<String> {
    match (archive.find('《'), archive.find('》')) {
        (Some(b), Some(e)) if e > b && e - b <= 90 => {
            let t = archive[b + 3..e].trim();
            if !t.is_empty() && t.chars().count() <= 30 {
                return Some(t.to_string());
            }
            None
        }
        _ => None,
    }
}

// Task normalization, intent authorization, and history ordering live in chat_contract.

/// 是否应跳过历史里的这一条：本轮 user 消息在组装 system 之前就已落库，
/// 若不过滤会在 prompt 里重复出现一次（浪费预算、也可能误导模型）。
/// 只跳过「最近一条 + 内容等于本轮展示文本或正文」的 user 行。
#[cfg(test)]
fn is_echo_user_row(i: usize, role: &str, content: &str, display: &str, message: &str) -> bool {
    i == 0 && role == "user" && (content == display || content == message)
}

/// 历史预算：越近（i 越小）保留越多。旧实现先 rev() 再按 i<4 给 4000 字，
/// 实际把最大预算给了最旧的 4 条，方向是反的。
#[cfg(test)]
fn history_budget_cap(i: usize) -> usize {
    if i < 4 {
        4000
    } else {
        1500
    }
}

fn book_auto_save_mode(db: &molan_core::db::Db, book_id: &str) -> &'static str {
    let v = molan_llm::get_setting(db, &format!("book_auto_save__{}", book_id));
    match v.trim().to_ascii_lowercase().as_str() {
        "auto" => "auto",
        "off" => "off",
        _ => "ask",
    }
}

#[cfg(test)]
fn stream_save_allowed(mode: &str, is_body_cmd: bool, is_outline_cmd: bool) -> bool {
    mode == "auto" && (is_body_cmd || is_outline_cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_error_event_carries_code_for_friendly_ui_branch() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        {
            let cx = Ctx::new(&tx, "1");
            cx.error("上游 500").await;
            cx.error(molan_llm::CANCELLED_MSG).await;
        }
        let parse = |line: String| serde_json::from_str::<Value>(line.trim()).unwrap()["e"].clone();
        let ordinary = parse(rx.recv().await.unwrap());
        assert!(ordinary.get("code").is_none());
        let cancelled = parse(rx.recv().await.unwrap());
        assert_eq!(cancelled["code"], "CANCELLED");
        assert!(cancelled["message"]
            .as_str()
            .unwrap()
            .contains(molan_llm::CANCELLED_MSG));
    }

    #[test]
    fn echo_user_row_is_skipped_once() {
        // 最近一条 user 且内容等于本轮展示文本 -> 跳过
        assert!(is_echo_user_row(0, "user", "写第3章", "写第3章", "写第3章"));
        // 内容等于 message 但 display 是别的写法 -> 仍跳过
        assert!(is_echo_user_row(
            0,
            "user",
            "写第3章",
            "【技能】写第3章",
            "写第3章"
        ));
        // 不是最近一条 -> 保留（历史里的真实旧消息）
        assert!(!is_echo_user_row(
            1,
            "user",
            "写第3章",
            "写第3章",
            "写第3章"
        ));
        // assistant 行 -> 保留
        assert!(!is_echo_user_row(
            0,
            "assistant",
            "写第3章",
            "写第3章",
            "写第3章"
        ));
        // 内容不同 -> 保留
        assert!(!is_echo_user_row(
            0,
            "user",
            "别的内容",
            "写第3章",
            "写第3章"
        ));
    }

    #[test]
    fn history_budget_favours_recent_rows() {
        assert_eq!(history_budget_cap(0), 4000);
        assert_eq!(history_budget_cap(3), 4000);
        assert_eq!(history_budget_cap(4), 1500);
        assert!(history_budget_cap(0) > history_budget_cap(9));
    }

    #[test]
    fn book_setup_preview_never_claims_saved() {
        // 模型若自己带了 saved=true，预览必须强制改回 false——服务端不代作者落库。
        let mut p = json!({
            "bookSetup": {"titles": ["测试书名"], "files": [], "saved": true,
                "destination": {"bookId": "forged"}, "savedFiles": [{"index": 0}]}
        });
        mark_book_setup_preview(&mut p);
        assert_eq!(p["bookSetup"]["saved"], json!(false));
        assert!(p["bookSetup"].get("destination").is_none());
        assert!(p["bookSetup"].get("savedFiles").is_none());
        // 没有 saved 字段时也要补上 false（前端据此判定为待确认预览）。
        let mut p2 = json!({"bookSetup": {"titles": ["甲"]}});
        mark_book_setup_preview(&mut p2);
        assert_eq!(p2["bookSetup"]["saved"], json!(false));
        // 没有 bookSetup 的普通 JSON 不受影响。
        let mut p3 = json!({"deai": {"score": 10}});
        mark_book_setup_preview(&mut p3);
        assert!(p3.get("bookSetup").is_none());
        // 非对象 bookSetup（异常输出）不 panic、不被篡改。
        let mut p4 = json!({"bookSetup": "oops"});
        mark_book_setup_preview(&mut p4);
        assert_eq!(p4["bookSetup"], json!("oops"));
    }

    #[test]
    fn book_setup_confirm_words_are_recognised() {
        // 精确「定」
        assert!(is_book_setup_confirm("定"));
        assert!(is_book_setup_confirm("  定  "));
        // 含关键词
        assert!(is_book_setup_confirm("那就开写吧"));
        assert!(is_book_setup_confirm("确认"));
        // 短消息以「好/行/可以…」开头
        assert!(is_book_setup_confirm("好的"));
        assert!(is_book_setup_confirm("行"));
        assert!(is_book_setup_confirm("可以"));
        // 普通创作请求不是确认（绝不能触发建书预览）
        assert!(!is_book_setup_confirm("写第3章"));
        assert!(!is_book_setup_confirm("帮我改一下这段"));
        assert!(!is_book_setup_confirm(""));
    }

    #[test]
    fn book_auto_save_gate_and_setting_table() {
        assert!(stream_save_allowed("auto", true, false));
        assert!(stream_save_allowed("auto", false, true));
        assert!(!stream_save_allowed("auto", false, false));
        assert!(!stream_save_allowed("ask", true, false));
        assert!(!stream_save_allowed("off", true, false));
        let dir = tempfile::tempdir().unwrap();
        let db = molan_core::db::Db::open(dir.path(), None).unwrap();
        let sql =
            "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2";
        let set = |v: &str| db.exec(sql, &[&"book_auto_save__b1", &v]).unwrap();
        set("auto");
        assert_eq!(book_auto_save_mode(&db, "b1"), "auto");
        set("OFF");
        assert_eq!(book_auto_save_mode(&db, "b1"), "off");
        for bad in ["", "yes", "true"] {
            set(bad);
            assert_eq!(book_auto_save_mode(&db, "b1"), "ask");
        }
        set("off");
        assert_eq!(book_auto_save_mode(&db, "b2"), "ask");
    }

    #[test]
    fn archive_title_extraction_is_bounded() {
        assert_eq!(
            extract_book_title_from_archive("《长夜行》\n\n【题材】玄幻"),
            Some("长夜行".to_string())
        );
        // 没有书名号 -> 无标题
        assert_eq!(extract_book_title_from_archive("随便一段档案正文"), None);
        // 只有左书名号 -> 无标题
        assert_eq!(extract_book_title_from_archive("《未闭合"), None);
        // 超过 30 字 -> 不作为书名
        let long = format!("《{}》", "长".repeat(31));
        assert_eq!(extract_book_title_from_archive(&long), None);
    }
}
