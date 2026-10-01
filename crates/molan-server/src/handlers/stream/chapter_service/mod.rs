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
use molan_core::task_kind::TaskKind;
use molan_core::{continuity, files, outline_confirm, pipeline};
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

/// 起草选项：本轮特别要求 + 本次技能覆盖（§8.4 三作用域：作品默认/阶段默认/本次临时）。
pub(crate) struct DraftOpts<'a> {
    pub instruction: &'a str,
    pub skill_ids: &'a [String],
    /// 新版技能选择（本次主/辅/文风/去味）；None 时按旧 skillIds 混合列表解析。
    pub selection: Option<&'a molan_core::skill_resolver::Selection>,
}

/// 单章正文起草：前置检查 → 生成 → 去AI味 → 落「正文待审」+ 队列/溯源记账 → 剧情审核 → 回执。
/// 返回 body_draft 工件（MANUAL-LINE-CONTRACT §2）；任何失败都不落盘、返回中文原因。
pub(crate) async fn draft_chapter(
    db: &Db,
    root: &std::path::Path,
    book_id: &str,
    ch: i64,
    opts: &DraftOpts<'_>,
    cancel: CancellationToken,
    emit: &Emit<'_>,
) -> Result<Value> {
    let t1 = std::time::Instant::now();
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
    let _ = &genre;
    // 技能三作用域（§6.3）：本次临时覆盖只影响本轮，不改书级/阶段默认绑定；计划在此冻结
    let legacy_sel = molan_core::skill_resolver::Selection {
        legacy: opts.skill_ids.to_vec(),
        ..Default::default()
    };
    let sel = opts.selection.unwrap_or(&legacy_sel);
    let plan = super::run_plan::build(db, root, book_id, TaskKind::Body, sel)?;
    // 去AI味子阶段按自己的任务解析，与正文计划同时冻结
    let hz_plan = super::run_plan::stage_plan(db, root, book_id, TaskKind::Humanize, sel);
    let rv_plan = super::run_plan::stage_plan(db, root, book_id, TaskKind::Review, sel);
    let skills = super::run_plan::skill_rows(&plan);
    emit.ev(json!({"type":"plan","plan": molan_core::skill_resolver::public_view(&plan)}))
        .await;
    let mut context = super::auto_book_context_for_chapter(db, book_id, ch, false);
    if context.is_empty() {
        context = format!(
            "【提醒】第{}章尚无前文与资料，请按建书档案与本章细纲合理开篇。",
            ch
        );
    }
    super::stage_context::append_target_outline(db, book_id, ch, &mut context);
    let msg = if opts.instruction.trim().is_empty() {
        format!("写第{}章正文。", ch)
    } else {
        format!(
            "写第{}章正文。本轮特别要求：{}",
            ch,
            opts.instruction.trim()
        )
    };
    let mut sys = super::run_plan::system_prompt(db, root, book_id, &plan, &msg, &context);
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
    // 上下文清单 + 技能快照（§6/§8.4）：实际注入与所用技能版本可追责
    let skill_snap = json!(skills
        .iter()
        .map(|sk| json!({"id": sk["id"], "name": sk["name"], "rev": sk["rev"], "planHash": plan["planHash"]}))
        .collect::<Vec<_>>());
    if let Ok(mid) = molan_core::ctx_manifest::record(
        db,
        book_id,
        "",
        "manual_draft",
        ch,
        &model,
        &context,
        coverage,
    ) {
        let _ = molan_core::ctx_manifest::attach_skills(db, &mid, &skill_snap);
    }

    let t2 = std::time::Instant::now();
    emit.ev(json!({"type":"step","index":2,"title":"生成正文（按已确认细纲）"}))
        .await;
    let temp: f64 = molan_llm::get_setting(db, "temperature")
        .parse()
        .unwrap_or(0.7);
    let max_tokens: i64 = molan_llm::get_setting(db, "max_tokens")
        .parse()
        .unwrap_or(8192);
    let prompt_chars = sys.chars().count() + msg.chars().count();
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

    let t3 = std::time::Instant::now();
    emit.ev(json!({"type":"step","index":3,"title":"去AI味（按本书设置）"}))
        .await;
    let (mut final_body, humanize_report) =
        super::auto_humanize(db, book_id, &body, &hz_plan, "editor").await;
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

    let t4 = std::time::Instant::now();
    emit.ev(json!({"type":"step","index":4,"title":"落盘「正文待审」+ 审批队列登记"}))
        .await;
    let name = format!("第{}章.md", ch);
    let frozen2 = frozen.clone();
    let cancel2 = cancel.clone();
    let oname2 = outline_name.clone();
    let book2 = book_id.to_string();
    // §8 保留原稿与差异：先提交去味前原稿（写盘 + 待审登记同锁完成，杜绝孤儿稿），
    // 再用 CAS 覆盖为去味稿——版本快照因此保留原稿；CAS 保证覆盖前无人改过。
    molan_core::chapter_commit::submit_pending(db, book_id, ch, &body, "", move || {
        // 锁内复核：取消 / 细纲确认仍有效 / 依赖指纹未变——任一失效即拒绝落盘。
        if cancel2.is_cancelled() {
            bail!("已停止（作者取消）");
        }
        if !outline_confirm::is_confirmed(db, &book2, ch, &oname2) {
            bail!("细纲确认在生成期间失效，未落盘");
        }
        let now_fp = continuity::input_fingerprint(db, &book2, frozen2.0)?;
        if now_fp != frozen2.1 {
            bail!("依赖（设定/前文）在生成期间被修改，未落盘");
        }
        Ok(())
    })?;
    if final_body != body {
        // 去味稿替换待审稿：同锁 CAS + 重新登记 + 统一回执（章节提交服务）
        molan_core::chapter_commit::rewrite_pending(
            db,
            book_id,
            &name,
            &body,
            &final_body,
            "replace",
            "chapter_service.humanize",
        )?;
        let _ = molan_core::chapter_state::record_save(
            db,
            book_id,
            ch,
            molan_core::db::REVIEW_GROUP,
            &continuity::content_hash(&final_body),
        );
    }
    let hash = continuity::content_hash(&final_body);
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

    let t5 = std::time::Instant::now();
    emit.ev(json!({"type":"step","index":5,"title":"剧情/设定审核（报告绑定落盘 hash）"}))
        .await;
    let review = super::chapter_review::review_pending_saved(
        db,
        book_id,
        Some(ch),
        &[format!("{} / {}", molan_core::db::REVIEW_GROUP, name)],
        Some(cancel.clone()),
        &rv_plan,
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
        // 正文生成调用的提示字数：Agent 内起草时按「提示 + 产出」估算计入本次运行预算
        "promptChars": prompt_chars,
        // 各步骤耗时（前置检查 / 生成 / 去味 / 落盘待审 / 审稿），供排查慢在哪一步
        "timings": {
            "precheckMs": (t2 - t1).as_millis() as u64, "generateMs": (t3 - t2).as_millis() as u64,
            "humanizeMs": (t4 - t3).as_millis() as u64, "writeMs": (t5 - t4).as_millis() as u64,
            "reviewMs": t5.elapsed().as_millis() as u64, "totalMs": t1.elapsed().as_millis() as u64,
        },
        "stagePlans": {"humanize": hz_plan["planHash"], "review": rv_plan["planHash"]},
        "planHash": plan["planHash"],
        "skillIds": skill_snap
            .as_array()
            .map(|a| a.iter().filter_map(|v| v["id"].as_str()).collect::<Vec<_>>())
            .unwrap_or_default(),
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
    // 本次技能覆盖（§6.3 临时作用域）：面板/IPC 与 Agent 工具同权
    let skill_ids: Vec<String> = a("skillIds")
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let instruction = s("instruction");
    let selection = args
        .get("skillSelection")
        .is_some()
        .then(|| molan_core::skill_resolver::Selection::from_args(args));
    let opts = DraftOpts {
        instruction: &instruction,
        skill_ids: &skill_ids,
        selection: selection.as_ref(),
    };
    match draft_chapter(db, &st.root, &book_id, ch, &opts, cancel.clone(), &emit).await {
        Ok(receipt) => {
            // 面板按钮也形成同一种产物卡片（与 Agent 工具路径一致）：带 sessionId 时归入该会话
            let sid = s("sessionId");
            let owned = !sid.is_empty()
                && db
                    .q_json(
                        "SELECT 1 FROM sessions WHERE id=?1 AND book_id=?2",
                        &[&sid as &dyn rusqlite::ToSql, &book_id],
                    )
                    .map(|r| !r.is_empty())
                    .unwrap_or(false);
            let model = receipt["model"].as_str().unwrap_or("").to_string();
            let session = if owned { sid.as_str() } else { "" };
            if let Some(view) = super::agent_runtime::adopt_tool_result(
                db,
                &book_id,
                session,
                "",
                "",
                "draft_chapter_body",
                &receipt,
                &model,
            ) {
                emit.ev(json!({"type":"artifact","artifact":view})).await;
            }
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
    // 与 approve_chapter IPC 同一实现（重入核对 + saga + 审阅版本绑定）。
    let r = molan_core::chapter_commit::approve(
        db,
        book_id,
        "",
        ch,
        Some(expected_hash),
        "agent_manual",
    )?;
    let mut out = json!({
        "ok": true, "kind": "body_finalized", "ch": ch, "finalName": r["finalName"], "hash": r["hash"],
    });
    if r["alreadyApproved"] == json!(true) {
        out["alreadyApproved"] = json!(true);
        out["verified"] = r["verified"].clone();
    } else {
        out["memorySync"] = json!("queued");
        out["note"] = json!("已定稿；故事记忆已排队同步（进度/失败见生产线状态）");
    }
    Ok(out)
}

/// 重启恢复（§8.6）：启动时把全书「已定稿但记忆仍 pending」的章补发抽取。
/// 与 agent_turn 收尾共用 sweep_pending_memory（幂等 + 在飞去重）。
pub(crate) fn spawn_boot_memory_sweep(st: Arc<AppState>) {
    tokio::spawn(async move {
        let books = st
            .db
            .q_json(
                "SELECT DISTINCT book_id FROM memory_job WHERE status='pending'",
                &[],
            )
            .unwrap_or_default();
        for b in books {
            if let Some(id) = b["bookId"].as_str() {
                sweep_pending_memory(&st, id).await;
            }
        }
    });
}

/// 在飞记忆同步去重：sweep/工具/按钮并发时同一 (book,ch) 只 spawn 一次。
static SYNC_INFLIGHT: std::sync::LazyLock<Mutex<HashSet<(String, i64)>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

/// 按章序串行同步（后章校验依赖前章记忆完整，并行会让后章误判失败）。
fn spawn_memory_sync(st: Arc<AppState>, book_id: String, chs: Vec<i64>) {
    let chs: Vec<i64> = {
        let mut g = SYNC_INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        chs.into_iter()
            .filter(|ch| g.insert((book_id.clone(), *ch)))
            .collect()
    };
    if chs.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for ch in chs {
            // post_approved_chapter 幂等（同 hash 有效记忆直接返回）；失败如实写 memory_job failed。
            if let Err(e) = super::post_approved_chapter(&st, &book_id, ch).await {
                let _ = continuity::mark_memory_error(&st.db, &book_id, ch, &e.to_string());
            }
            if let Ok(mut g) = SYNC_INFLIGHT.lock() {
                g.remove(&(book_id.clone(), ch));
            }
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
    let chs: Vec<i64> = latest
        .into_iter()
        .filter(|(ch, status)| {
            status == "pending"
                && files::read_file(&st.db, book_id, "正文", &format!("第{}章.md", ch))
                    .is_some_and(|t| !t.trim().is_empty())
        })
        .map(|(ch, _)| ch)
        .collect();
    spawn_memory_sync(Arc::clone(st), book_id.to_string(), chs);
}

#[cfg(test)]
mod tests;
