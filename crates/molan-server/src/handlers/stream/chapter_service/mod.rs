//! 手动线：单章正文起草服务（HANDOFF §5.4/§5.5，MANUAL-LINE-CONTRACT §1.2）。
//!
//! 同一服务被两个入口共用（§4「对话与按钮走同一后端动作」）：
//! - IPC `draft_chapter`（流式；面板按钮/快捷动作）；
//! - Agent 工具 `draft_chapter_body`（对话内明确指令）。
//!
//! 铁律：
//! - 正文永远先进「正文待审」+ 审批队列，绝不直接写「正文」；
//! - 起草前置门：目标章明确、细纲已被作者确认且未失效、无既有稿件、前序记忆无阻塞；
//! - 生成前冻结依赖指纹，落盘前锁内复核（取消/指纹/细纲确认任一失效即拒绝落盘）；
//! - 审核报告绑定落盘内容的 hash；草稿 ≠ 定稿，定稿只发生在作者明确动作里。
use super::chat;
use crate::AppState;
use anyhow::{anyhow, bail, Result};
use molan_core::db::Db;
use molan_core::{continuity, files, outline_confirm, pipeline, stats};
use molan_llm::{ChatParams, StreamOpts};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

/// 事件发送器：IPC 流式与 Agent 工具两条路径都经它发进度/步骤（Agent 路径外层还有心跳）。
pub(crate) struct Emit<'a> {
    tx: Option<&'a Sender<String>>,
    channel: String,
}

impl<'a> Emit<'a> {
    pub(crate) fn new(tx: &'a Sender<String>, channel: &str) -> Self {
        Emit {
            tx: Some(tx),
            channel: channel.to_string(),
        }
    }
    async fn ev(&self, e: Value) {
        if let Some(tx) = self.tx {
            let _ = tx
                .send(format!("{}\n", json!({"ch": self.channel, "e": e})))
                .await;
        }
    }
}

/// 起草前置检查（§5.4）。返回 (细纲名, 细纲确认hash, 冻结的依赖指纹)。
fn preflight(db: &Db, book_id: &str, ch: i64) -> Result<(String, String, (i64, String))> {
    if !(1..=1_000_000).contains(&ch) {
        bail!("章号超出范围：{}", ch);
    }
    if !files::valid_book_id(db, book_id) {
        bail!("书籍不存在或已删除");
    }
    let name = format!("第{}章.md", ch);
    // 手动线逐章推进（§5.7）：上一章没有正式稿就没有承接锚点，不写下一章。
    if ch > 1 {
        let prev_body = files::read_file(db, book_id, "正文", &format!("第{}章.md", ch - 1))
            .unwrap_or_default();
        if prev_body.trim().is_empty() {
            bail!(
                "第{}章尚无正式正文（手动线逐章推进）：请先定稿第{}章",
                ch,
                ch - 1
            );
        }
    }
    // 细纲必须「已被作者确认」且当前文件与确认版本一致（§5.3/§5.4）。
    let Some(oname) = super::stage_context::target_outline_name(db, book_id, ch) else {
        bail!(
            "第{}章还没有细纲：先起草细纲并由作者确认入库，再起草正文",
            ch
        );
    };
    let ost = outline_confirm::status_for(db, book_id, ch, &oname);
    match ost["status"].as_str().unwrap_or("") {
        "confirmed" => {}
        "stale" => bail!(
            "第{}章细纲在确认后被修改（原确认已失效）：请作者重新查看并确认入库；已生成的稿件需复核",
            ch
        ),
        other => bail!(
            "第{}章细纲未确认（当前状态：{}）：手动线要求作者先确认细纲入库，再起草正文",
            ch,
            if other.is_empty() { "无文件" } else { other }
        ),
    }
    let outline_hash = ost["hash"].as_str().unwrap_or("").to_string();
    // 不覆盖、不造第二份草稿。
    let pending_now =
        files::read_file(db, book_id, molan_core::db::REVIEW_GROUP, &name).unwrap_or_default();
    if !pending_now.trim().is_empty() {
        bail!("第{}章已有待审稿：请先审阅定稿或退回，再重新起草", ch);
    }
    let formal = files::read_file(db, book_id, "正文", &name).unwrap_or_default();
    if !formal.trim().is_empty() {
        bail!(
            "第{}章已有正式稿：不重复生成；要修改请走修订流程（提案/审阅）",
            ch
        );
    }
    // 记忆阻塞（与 pipeline blockers 同源，F4 语义）：更早章记忆未同步不得续写。
    let tree = files::scan_tree(db, book_id);
    let queue = Value::Array(
        db.q_json(
            "SELECT ch, review_file, status FROM pending_chapter WHERE book_id=?1",
            &[&book_id as &dyn rusqlite::ToSql],
        )
        .unwrap_or_default(),
    );
    let mut state = pipeline::derive_state_with_memory(db, book_id, &tree, &queue);
    outline_confirm::annotate(db, book_id, &mut state);
    if let Some(bs) = state["blockers"].as_array() {
        for b in bs {
            if b["type"].as_str() == Some("memory") {
                if let Some(bch) = b["chapter"].as_i64() {
                    if bch < ch {
                        bail!(
                            "第{}章记忆未同步（{}）：请先处理记忆同步，再起草第{}章正文",
                            bch,
                            b["status"].as_str().unwrap_or("待处理"),
                            ch
                        );
                    }
                }
            }
        }
    }
    // 冻结依赖指纹（与 auto_write 同法）：生成期间设定/前文变化 → 落盘前复核拒绝。
    let fp = continuity::input_fingerprint(db, book_id, ch + 1)?;
    Ok((oname, outline_hash, (ch + 1, fp)))
}

/// 单章正文起草：前置检查 → 生成 → 去AI味 → 落「正文待审」+ 队列/溯源记账 → 剧情审核 → 回执。
/// 返回 body_draft 工件（MANUAL-LINE-CONTRACT §2）；任何失败都不落盘、返回中文原因。
pub(crate) async fn draft_chapter(
    db: &Db,
    root: &std::path::Path,
    book_id: &str,
    ch: i64,
    instruction: &str,
    cancel: CancellationToken,
    emit: &Emit<'_>,
) -> Result<Value> {
    emit.ev(json!({"type":"step","index":1,"title":"前置检查（细纲确认/既有稿件/记忆阻塞）"}))
        .await;
    let (outline_name, outline_hash, frozen) = preflight(db, book_id, ch)?;

    let chn = molan_llm::resolve_agent_channel(db, "chapter").ok_or_else(|| {
        anyhow!("未配置可用的模型渠道。请在 设置→渠道管理 里添加你自己的 API（baseUrl + Key）")
    })?;
    if chn["baseUrl"].as_str().unwrap_or("").is_empty() {
        bail!("未配置可用的模型渠道。请在 设置→渠道管理 里添加你自己的 API（baseUrl + Key）");
    }
    let model = chn["model"].as_str().unwrap_or("").to_string();
    emit.ev(json!({"type":"meta","task":"draft_chapter","ch":ch,"model":model}))
        .await;

    // 上下文四层（§7）：自动书籍上下文（档案/人物/前情/上章结尾/本章细纲）+ 显式补注目标细纲（按名去重）。
    let genre = db
        .q_json(
            "SELECT genre FROM books WHERE id=?1",
            &[&book_id as &dyn rusqlite::ToSql],
        )
        .ok()
        .and_then(|v| {
            v.first().and_then(|r| {
                r["genre"]
                    .as_str()
                    .filter(|g| !g.is_empty())
                    .map(str::to_string)
            })
        });
    let style = super::resolve_book_style(db, root, book_id, genre.as_deref());
    let skills = super::effective_skills(db, book_id, "body", &[]);
    let mut context = super::auto_book_context_for_chapter(db, book_id, ch, false);
    if context.is_empty() {
        context = format!(
            "【提醒】第{}章尚无前文与资料，请按建书档案与本章细纲合理开篇。",
            ch
        );
    }
    super::stage_context::append_target_outline(db, book_id, ch, &mut context);
    let msg = if instruction.trim().is_empty() {
        format!("写第{}章正文。", ch)
    } else {
        format!("写第{}章正文。本轮特别要求：{}", ch, instruction.trim())
    };
    let mut sys = super::build_system(
        db,
        root,
        book_id,
        genre.as_deref(),
        style.as_deref(),
        &skills,
        &msg,
        &context,
    );
    for role in ["character", "editor"] {
        if let Ok(extra) = molan_core::deepwrite::agent_instructions(db, book_id, role) {
            if !extra.is_empty() {
                sys.push_str("\n\n");
                sys.push_str(&extra);
            }
        }
    }
    let bound = super::deepwrite_bound_skills(db, book_id, "body", &sys);
    if !bound.is_empty() {
        sys.push_str("\n\n");
        sys.push_str(&bound);
    }
    sys.push_str(&format!(
        "\n\n【硬约束】只写第{0}章正文：首行是章节标题（第{0}章 …），不复述细纲、不输出任何解释、不写后续章节。",
        ch
    ));

    // 记账（只观测不阻断）：状态机 + 上下文清单（真实注入审计，§7）。
    let _ = molan_core::chapter_state::record_generating(db, book_id, ch, "manual-draft", &model);
    let coverage = if ch > 1 {
        continuity::chapter_context(db, book_id, ch)
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
        book_id,
        "",
        "manual_draft",
        ch,
        &model,
        &context,
        coverage,
    );

    emit.ev(json!({"type":"step","index":2,"title":"生成正文（按已确认细纲）"}))
        .await;
    let temp: f64 = molan_llm::get_setting(db, "temperature")
        .parse()
        .unwrap_or(0.7);
    let max_tokens: i64 = molan_llm::get_setting(db, "max_tokens")
        .parse()
        .unwrap_or(8192);
    let params = ChatParams {
        base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: chn["key"].as_str().unwrap_or("").to_string(),
        model: model.clone(),
        messages: vec![
            json!({"role": "system", "content": sys}),
            json!({"role": "user", "content": msg}),
        ],
        temperature: if temp == 0.0 { 0.7 } else { temp },
        // 长章预算（与主对话同策略）：思考占输出预算，太小必截断
        max_tokens: max_tokens.max(32768),
        stream: true,
        no_thinking: false,
        reasoning_effort: "max".to_string(),
        log_tag: "manual_chapter".to_string(),
        log_book: book_id.to_string(),
        cancel: Some(cancel.clone()),
    };
    // 流式收集 + 节流进度：正文不进对话流（落盘为准），只报字数进度。
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let sse = async move {
        let r = molan_llm::chat_completion_stream_opts(params, &ev_tx, StreamOpts::default()).await;
        drop(ev_tx);
        r
    };
    tokio::pin!(sse);
    let mut body = String::new();
    let mut last_prog = 0usize;
    let res = loop {
        tokio::select! {
            r = &mut sse => break r,
            ev = ev_rx.recv() => {
                if let Some(molan_llm::LlmEvent::Delta(t)) = ev {
                    body.push_str(&t);
                    let n = body.chars().count();
                    if n >= last_prog + 200 {
                        last_prog = n;
                        emit.ev(json!({"type":"progress","chars":n})).await;
                    }
                }
            }
        }
    };
    while let Some(ev) = ev_rx.recv().await {
        if let molan_llm::LlmEvent::Delta(t) = ev {
            body.push_str(&t);
        }
    }
    match res {
        Ok(molan_llm::CompletionState::Done) | Ok(molan_llm::CompletionState::FinishStop) => {}
        Ok(_) => bail!("上游完成状态不可信（无明确终止标记），本次不落盘"),
        Err(e) => {
            if molan_llm::is_cancelled_err(&e) || cancel.is_cancelled() {
                bail!("已停止（作者取消），未落盘");
            }
            if molan_llm::is_incomplete_err(&e) {
                bail!("上游输出不完整（截断/限流/过滤），本次不落盘");
            }
            return Err(e);
        }
    }
    let body = body.trim().to_string();
    if body.chars().count() < 200 {
        bail!(
            "生成正文过短（{}字），疑似上游异常，未落盘",
            body.chars().count()
        );
    }
    // 首行不是章节标题则补齐（文件名仍是唯一权威章号来源）。
    let first_line = body.lines().next().unwrap_or("");
    let body = if super::is_chapter_head(first_line) {
        body
    } else {
        format!("# 第{}章\n\n{}", ch, body)
    };

    emit.ev(json!({"type":"step","index":3,"title":"去AI味（按本书设置）"}))
        .await;
    let (mut final_body, humanize_report) =
        super::auto_humanize(db, book_id, &body, None, "editor").await;
    // 收缩保护：去味稿掉字过多视为损坏，保留原稿并如实标注（§8 保留原稿与差异）。
    let mut humanize_brief = json!({
        "passed": humanize_report["passed"],
        "score": humanize_report["score"],
    });
    if let Some(sk) = humanize_report.get("skipped") {
        humanize_brief["skipped"] = sk.clone();
    }
    if final_body.chars().count() * 10 < body.chars().count() * 6 {
        humanize_brief["reverted"] = json!("去味稿字数收缩过多，已保留原稿");
        final_body = body.clone();
    }

    emit.ev(json!({"type":"step","index":4,"title":"落盘「正文待审」+ 审批队列登记"}))
        .await;
    let name = format!("第{}章.md", ch);
    let frozen2 = frozen.clone();
    let cancel2 = cancel.clone();
    let oname2 = outline_name.clone();
    let book2 = book_id.to_string();
    files::write_ai_file_checked(
        db,
        book_id,
        molan_core::db::REVIEW_GROUP,
        &name,
        &final_body,
        move || {
            // 锁内复核：取消 / 细纲确认仍有效 / 依赖指纹未变——任一失效即拒绝落盘。
            if cancel2.is_cancelled() {
                bail!("已停止（作者取消）");
            }
            if !outline_confirm::is_confirmed(db, &book2, ch, &oname2) {
                bail!("细纲确认在生成期间失效，未落盘");
            }
            let now_fp = continuity::input_fingerprint(db, &book2, frozen2.0)?;
            if now_fp != frozen2.1 {
                bail!("依赖（设定/前文/技能）在生成期间被修改，未落盘");
            }
            Ok(())
        },
    )?;
    let hash = continuity::content_hash(&final_body);
    // 队列登记失败必须报错：文件已写但队列没有 = 作者看不到这篇稿。
    crate::handlers::register_review_queue(db, book_id, &name, &final_body)?;
    let _ = molan_core::chapter_state::record_save(
        db,
        book_id,
        ch,
        molan_core::db::REVIEW_GROUP,
        &hash,
    );
    // 溯源与依赖（与 auto_write 同法；task_id 固定 manual-draft）。
    if let Err(e) = continuity::record_draft_origin(db, book_id, ch, &name, &hash, "manual-draft") {
        tracing::warn!("第{}章 草稿来源登记失败：{}", ch, e);
    }
    if ch > 1 {
        if let Some(pb) = files::read_file(db, book_id, "正文", &format!("第{}章.md", ch - 1)) {
            if let Err(e) = continuity::record_draft_dependency(
                db,
                book_id,
                ch,
                ch - 1,
                &continuity::content_hash(&pb),
                "manual-draft",
            ) {
                tracing::warn!("第{}章 待审依赖登记失败：{}", ch, e);
            }
        }
    }

    emit.ev(json!({"type":"step","index":5,"title":"剧情/设定审核（报告绑定落盘 hash）"}))
        .await;
    let review = super::chapter_review::review_pending_saved(
        db,
        book_id,
        Some(ch),
        &[format!("{} / {}", molan_core::db::REVIEW_GROUP, name)],
        Some(cancel.clone()),
    )
    .await;
    let review_summary = match review {
        Some((ev, summary)) => {
            emit.ev(ev).await;
            Some(summary)
        }
        None => None,
    };
    match &review_summary {
        Some(s) if s["ok"].as_bool().unwrap_or(false) => {
            let _ =
                molan_core::chapter_state::record_reviewed(db, book_id, ch, "手动线起草审核通过");
        }
        Some(s) => {
            let _ = molan_core::chapter_state::record_review_failed(
                db,
                book_id,
                ch,
                s["note"].as_str().unwrap_or("审核未通过"),
            );
        }
        None => {}
    }

    Ok(json!({
        "ok": true, "kind": "body_draft", "ch": ch, "name": name,
        "group": molan_core::db::REVIEW_GROUP,
        "hash": hash, "chars": final_body.chars().count(),
        "outline": {"name": outline_name, "hash": outline_hash},
        "review": review_summary, "humanize": humanize_brief, "model": model,
        "note": "草稿已进入「正文待审」并登记审批队列；不是定稿。请审阅后定稿或退回。",
    }))
}

/// IPC `draft_chapter`（流式）：面板按钮/快捷动作入口，与 Agent 工具共用 draft_chapter。
pub(crate) async fn draft_chapter_stream(
    st: &Arc<AppState>,
    _cmd: &str,
    args: &Value,
    tx: &Sender<String>,
) -> Result<Option<Value>> {
    let db = &st.db;
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();
    if a("onEvent").is_null() {
        return Err(anyhow!("缺少事件通道"));
    }
    if chat::shutting_down() {
        return Err(anyhow!("服务正在优雅停机，请稍后重试"));
    }
    let book_id = s("bookId");
    if book_id.is_empty() {
        return Err(anyhow!("缺少 bookId"));
    }
    let ch = a("ch").as_i64().unwrap_or(0);
    let channel = crate::handlers::channel_id(&a("onEvent"));
    let emit = Emit::new(tx, &channel);
    // 取消登记（与 chat_stream 同法）：abort_chat({requestId}) 可掐断在飞生成。
    let req = s("requestId");
    let key = if req.trim().is_empty() {
        format!("auto-{}", uuid::Uuid::new_v4())
    } else {
        req.clone()
    };
    let cancel = chat::register_abort(&key);
    struct AbortGuard<'g>(&'g str);
    impl Drop for AbortGuard<'_> {
        fn drop(&mut self) {
            chat::unregister_abort(self.0);
        }
    }
    let _guard = AbortGuard(&key);
    match draft_chapter(
        db,
        &st.root,
        &book_id,
        ch,
        &s("instruction"),
        cancel.clone(),
        &emit,
    )
    .await
    {
        Ok(receipt) => {
            emit.ev(json!({"type":"done","receipt":receipt.clone()}))
                .await;
            Ok(Some(receipt))
        }
        Err(e) => {
            let cancelled = molan_llm::is_cancelled_err(&e) || cancel.is_cancelled();
            let msg = if cancelled {
                "已停止（作者取消），未落盘".to_string()
            } else {
                e.to_string()
            };
            let mut ev = json!({"type":"error","message":msg});
            if cancelled {
                ev["code"] = json!("CANCELLED");
            }
            emit.ev(ev).await;
            Err(anyhow!(msg))
        }
    }
}

/// 定稿（Agent 工具 finalize_chapter_draft 的执行体；按钮路径走既有 approve_chapter IPC，
/// 两者核心动作同为 files::approve_pending_chapter —— 同一后端动作）。
/// expected_hash 非空时绑定校验：审阅后稿子被改过 → 拒绝定稿（§5.5 报告/批准绑定版本）。
pub(crate) fn finalize_draft(
    db: &Db,
    book_id: &str,
    ch: i64,
    expected_hash: &str,
) -> Result<Value> {
    if !(1..=1_000_000).contains(&ch) {
        bail!("章号超出范围：{}", ch);
    }
    if !files::valid_book_id(db, book_id) {
        bail!("书籍不存在或已删除");
    }
    let name = format!("第{}章.md", ch);
    let rows = db
        .q_json(
            "SELECT review_file,status FROM pending_chapter WHERE book_id=?1 AND ch=?2",
            &[
                &book_id as &dyn rusqlite::ToSql,
                &ch as &dyn rusqlite::ToSql,
            ],
        )
        .unwrap_or_default();
    let (review_file, status) = rows
        .first()
        .map(|r| {
            (
                r["reviewFile"].as_str().unwrap_or("").to_string(),
                r["status"].as_str().unwrap_or("").to_string(),
            )
        })
        .unwrap_or_default();
    if status == "approved" {
        // 幂等重入：正式稿与批准回执一致才算「已定稿」；不一致绝不静默。
        let formal = files::read_file(db, book_id, "正文", &name);
        let receipt = continuity::approved_hash(db, book_id, &name).unwrap_or(None);
        return match (formal.as_deref(), receipt.as_deref()) {
            (Some(f), Some(r)) if continuity::content_hash(f) == r => Ok(json!({
                "ok": true, "alreadyApproved": true, "verified": "approved-receipt-hash-match",
                "kind": "body_finalized", "ch": ch, "finalName": name, "hash": r,
            })),
            (Some(_), Some(_)) => Err(anyhow!(
                "第{}章已批准，但正式稿与批准回执不一致（批准后被改动过）：请人工核对，系统不静默处理",
                ch
            )),
            _ => Err(anyhow!(
                "队列显示第{}章已批准，但正式稿或批准回执缺失：请人工核对",
                ch
            )),
        };
    }
    if status != "pending" || review_file.is_empty() {
        bail!(
            "第{}章没有待审稿，无法定稿（手动线只定稿「正文待审」中的草稿）",
            ch
        );
    }
    let pending_text = files::read_file(db, book_id, molan_core::db::REVIEW_GROUP, &review_file)
        .ok_or_else(|| anyhow!("待审文件不存在：{}", review_file))?;
    let cur = continuity::content_hash(&pending_text);
    let exp = expected_hash.trim();
    if !exp.is_empty() && exp != cur {
        let brief = |h: &str| -> String { h.chars().take(8).collect() };
        bail!(
            "待审稿在你审阅后被修改（期望 {}…，当前 {}…）：本次定稿失效，请重新审阅",
            brief(exp),
            brief(&cur)
        );
    }
    // 与 approve_chapter IPC 相同的核心批准动作（saga：队列+批准回执同事务；依赖检查在内）。
    let final_name = files::approve_pending_chapter(db, book_id, &review_file)?;
    stats::refresh_words(db, book_id);
    let _ = molan_core::chapter_state::record_approved(
        db,
        book_id,
        ch,
        json!({"finalName": final_name, "by": "agent_manual"}),
    );
    // 记忆排队由审批 saga 预置（note_file_change → memory_job pending）；
    // 实际抽取由 agent_turn 收尾的 sweep_pending_memory 补发（对话路径没有 st，不能就地 spawn）。
    Ok(json!({
        "ok": true, "kind": "body_finalized", "ch": ch, "finalName": final_name, "hash": cur,
        "memorySync": "queued",
        "note": "已定稿；故事记忆已排队同步（进度/失败见生产线状态）",
    }))
}

/// 在飞记忆同步去重：sweep/工具/按钮并发时同一 (book,ch) 只 spawn 一次。
static SYNC_INFLIGHT: std::sync::LazyLock<Mutex<HashSet<(String, i64)>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

fn spawn_memory_sync(st: Arc<AppState>, book_id: String, ch: i64) {
    let key = (book_id.clone(), ch);
    {
        let mut g = SYNC_INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        if !g.insert(key.clone()) {
            return;
        }
    }
    tokio::spawn(async move {
        // post_approved_chapter 幂等（同 hash 有效记忆直接返回）；失败如实写 memory_job failed。
        if let Err(e) = super::post_approved_chapter(&st, &book_id, ch).await {
            let _ = continuity::mark_memory_error(&st.db, &book_id, ch, &e.to_string());
        }
        if let Ok(mut g) = SYNC_INFLIGHT.lock() {
            g.remove(&key);
        }
    });
}

/// agent_turn 收尾补发：把「已定稿但记忆仍 pending」的章交给 post_approved_chapter。
/// 只认每章 memory_job 最新一行（与 pipeline 同语义），且正式稿必须存在——绝不碰未定稿章。
pub(crate) async fn sweep_pending_memory(st: &Arc<AppState>, book_id: &str) {
    if book_id.is_empty() || !files::valid_book_id(&st.db, book_id) {
        return;
    }
    let rows = st
        .db
        .q_json(
            "SELECT ch,status FROM memory_job WHERE book_id=?1 ORDER BY rowid",
            &[&book_id as &dyn rusqlite::ToSql],
        )
        .unwrap_or_default();
    let mut latest: BTreeMap<i64, String> = BTreeMap::new();
    for r in rows {
        if let Some(ch) = r["ch"].as_i64() {
            latest.insert(ch, r["status"].as_str().unwrap_or("").to_string());
        }
    }
    for (ch, status) in latest {
        if status != "pending" {
            continue;
        }
        let name = format!("第{}章.md", ch);
        let has_formal = files::read_file(&st.db, book_id, "正文", &name)
            .map(|t| !t.trim().is_empty())
            .unwrap_or(false);
        if has_formal {
            spawn_memory_sync(Arc::clone(st), book_id.to_string(), ch);
        }
    }
}

#[cfg(test)]
mod tests;
