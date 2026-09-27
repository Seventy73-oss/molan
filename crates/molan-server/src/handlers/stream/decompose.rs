// 拆解与导入分析（长任务）：风格蒸馏 / 拆解 / 导入通读分析 / 场景正文生成 / 会话标题 / 风格样例。
use super::super::AppState;
use super::auto_write::auto_humanize;
use super::chat::{drain_chat_completion, take_abort, Ctx};
use super::ANTI_AI_STRUCTURE;
use super::{book_prefs_block, genre_key, prompts, resolve_book_style};
use crate::handlers::channel_id;
use anyhow::anyhow;
use molan_core::files;
use molan_core::stats;
use molan_llm::{chat_once, chat_once_retry_n, ChatParams};
use serde_json::{json, Value};
use std::sync::Arc;

pub(crate) async fn import_analyze(
    st: &Arc<AppState>,
    _cmd: &str,
    args: &Value,
    tx: &tokio::sync::mpsc::Sender<String>,
) -> anyhow::Result<Option<Value>> {
    let db = &st.db;
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();
    let channel = channel_id(&a("onEvent"));
    if a("onEvent").is_null() {
        return Err(anyhow!("缺少事件通道"));
    }
    let cx = Ctx::new(tx, &channel);
    let book_id = s("bookId");
    if book_id.is_empty() {
        cx.error("缺少 bookId").await;
        return Ok(Some(json!({"ok": true})));
    }
    let book_title = db
        .q_json(
            "SELECT title FROM books WHERE id=?1",
            &[&book_id as &dyn rusqlite::ToSql],
        )
        .ok()
        .and_then(|v| {
            v.first()
                .and_then(|r| r["title"].as_str().map(|s| s.to_string()))
        })
        .unwrap_or_default();
    // 收集正文/参考内容：自动全书分析必须排除作者标记 aiOff 的文件（N06 / F12）。
    // 显式引用走 context_text 的另一条路径，不受此处影响。
    let tree = files::scan_tree(db, &book_id);
    let mut docs: Vec<(String, String)> = Vec::new();
    let mut hidden: Vec<String> = Vec::new();
    if let Some(arr) = tree.as_array() {
        for g in arr {
            let dir = g["dir"].as_str().unwrap_or("");
            if dir != "正文" && dir != "参考" {
                continue;
            }
            if let Some(fs) = g["files"].as_array() {
                for f in fs {
                    let name = f["name"].as_str().unwrap_or("").to_string();
                    if files::file_flag(db, &book_id, dir, &name, "aiOff") {
                        hidden.push(format!("{}/{}", dir, name));
                        continue;
                    }
                    let c = files::read_file(db, &book_id, dir, &name).unwrap_or_default();
                    if !c.trim().is_empty() {
                        docs.push((name, c));
                    }
                }
            }
        }
    }
    if docs.is_empty() {
        let msg = "该书正文/参考目录里没有可分析的导入内容";
        cx.error(msg).await;
        // 无内容不是成功（契约 N15：不假 ok）
        return Ok(Some(
            json!({"ok": false, "complete": false, "error": msg, "hidden": hidden}),
        ));
    }
    let p = prompts(&st.root);
    let chn = molan_llm::resolve_agent_channel(db, "distill");
    let use_llm = chn
        .as_ref()
        .map(|c| {
            !c["baseUrl"].as_str().unwrap_or("").is_empty()
                && !c["key"].as_str().unwrap_or("").is_empty()
        })
        .unwrap_or(false);
    // 不再按字节截断（N06）：byte 切片会落在多字节字符中间触发 panic。
    // 保留完整 docs，按“字符”预算分批，覆盖来源可核对。
    let full_text = docs
        .iter()
        .map(|(n, c)| format!("## 章节标题：{}\n{}", n, c))
        .collect::<Vec<_>>()
        .join("\n\n");
    let total_chars = full_text.chars().count();
    let doc_count = docs.len();
    let base_url = chn
        .as_ref()
        .map(|c| c["baseUrl"].as_str().unwrap_or("").to_string())
        .unwrap_or_default();
    let api_key = chn
        .as_ref()
        .map(|c| c["key"].as_str().unwrap_or("").to_string())
        .unwrap_or_default();
    let model = chn
        .as_ref()
        .map(|c| c["model"].as_str().unwrap_or("").to_string())
        .unwrap_or_default();

    let call = |sys: String, user: String, max_tokens: i64| {
        let base_url = base_url.clone();
        let api_key = api_key.clone();
        let model = model.clone();
        async move {
            chat_once(ChatParams {
                base_url,
                api_key,
                model,
                messages: vec![
                    json!({"role": "system", "content": sys}),
                    json!({"role": "user", "content": user}),
                ],
                max_tokens,
                ..Default::default()
            })
            .await
        }
    };

    // 分批预算（字符数，非字节）：单批 12000 字符，覆盖全书；记录每批来源与失败。
    const SEG_CHARS: usize = 12000;
    const MAP_TOTAL_BUDGET: usize = 240000; // 单次导入分析的字数上限
    let seg_total = total_chars.div_ceil(SEG_CHARS).max(1);
    let mut segs: Vec<String> = Vec::new();
    for k in 0..seg_total {
        let seg: String = full_text
            .chars()
            .skip(k * SEG_CHARS)
            .take(SEG_CHARS)
            .collect();
        if !seg.trim().is_empty() {
            segs.push(seg);
        }
    }
    // 预算超限必须显式标记，不能静默裁掉后仍称"全书完成"（review #3）
    let budget_exceeded = total_chars > MAP_TOTAL_BUDGET;
    // 1 通读：逐批 map，记录成功/失败批号，失败不静默当完成
    let mut maps: Vec<String> = Vec::new();
    let mut map_failed: Vec<usize> = Vec::new();
    if use_llm {
        for (i, seg) in segs.iter().enumerate() {
            cx.step(1, &format!("通读全书 · 第 {}/{} 段", i + 1, segs.len()))
                .await;
            match call(
                p["import_map"].as_str().unwrap_or("").to_string(),
                seg.clone(),
                2048,
            )
            .await
            {
                Ok(r) if !r.trim().is_empty() => maps.push(r),
                Ok(_) => map_failed.push(i + 1),
                Err(e) => {
                    tracing::warn!("导入分析第 {} 段失败：{}", i + 1, e);
                    map_failed.push(i + 1);
                }
            }
        }
    }
    // 每个阶段独立记录成功；全部成功才算完成（review #4）
    let mut stages_ok = true;
    // 产出为「提议」：唯一文件名（uuid），绝不覆盖作者已改的同名文件（review #2）
    let proposal_id = uuid::Uuid::new_v4().to_string();
    let short = &proposal_id[..8];

    // 2 汇总设定：真实写盘，失败必须上报（不假成功）
    cx.step(2, "汇总重建设定（世界观 / 人物 / 大纲）").await;
    if use_llm && maps.iter().any(|m| !m.is_empty()) {
        let joined: String = maps.join("\n\n").chars().take(MAP_TOTAL_BUDGET).collect();
        match call(
            p["import_reduce"].as_str().unwrap_or("").to_string(),
            joined,
            3000,
        )
        .await
        {
            Ok(setting) => {
                let body = unwrap_import_json(&setting);
                if body.trim().is_empty() {
                    stages_ok = false;
                    cx.error("导入分析：设定汇总返回空内容，未写入（可重跑）")
                        .await;
                } else {
                    let fname = format!("导入分析_设定_{}.md", short);
                    files::write_file_new(db, &book_id, "参考", &fname, &body)?;
                }
            }
            Err(e) => {
                stages_ok = false;
                cx.error(&format!("导入分析：设定汇总失败（{}），未写入，可重跑", e))
                    .await;
            }
        }
    } else if use_llm {
        stages_ok = false;
    }
    // 3 章节摘要
    cx.step(3, "整理章节摘要").await;
    if use_llm {
        let head: String = full_text.chars().take(MAP_TOTAL_BUDGET).collect();
        match call(
            p["import_summary"].as_str().unwrap_or("").to_string(),
            head,
            3000,
        )
        .await
        {
            Ok(sum) => {
                let body = unwrap_import_json(&sum);
                if body.trim().is_empty() {
                    stages_ok = false;
                    cx.error("导入分析：章节摘要返回空内容，未写入（可重跑）")
                        .await;
                } else {
                    let fname = format!("导入分析_章节摘要_{}.md", short);
                    files::write_file_new(db, &book_id, "参考", &fname, &body)?;
                }
            }
            Err(e) => {
                stages_ok = false;
                cx.error(&format!("导入分析：章节摘要失败（{}），未写入，可重跑", e))
                    .await;
            }
        }
    } else {
        stages_ok = false;
    }
    // 4 蒸馏风格：导入蒸馏一律作为**新文风卡提议**，同名已有则加 uuid 后缀，
    // 绝不覆盖作者已有同名 style（review #2）
    cx.step(4, "蒸馏原作风格").await;
    if use_llm {
        let head: String = full_text.chars().take(MAP_TOTAL_BUDGET).collect();
        match call(
            "你是资深网文编辑与文体分析师。".to_string(),
            format!(
                "题目《{}》\n\n{}\n\n【文本】\n{}",
                if book_title.is_empty() {
                    "导入小说"
                } else {
                    &book_title
                },
                p["distill"].as_str().unwrap_or(""),
                head
            ),
            3000,
        )
        .await
        {
            Ok(style) => {
                let base = if !book_title.is_empty() && book_title != "未命名新书" {
                    book_title.clone()
                } else {
                    docs.first()
                        .map(|(n, _)| {
                            n.trim_end_matches(".md")
                                .trim_end_matches(".txt")
                                .split_once('_')
                                .map(|(a, _)| a.to_string())
                                .unwrap_or_else(|| n.clone())
                        })
                        .unwrap_or_else(|| "导入小说".into())
                };
                if style.trim().is_empty() {
                    stages_ok = false;
                    cx.error("导入分析：风格蒸馏返回空内容，未写入").await;
                } else {
                    // 冲突时改名，不覆盖作者同名风格卡
                    let base_name = format!("《{}》原作风格", base);
                    let exists = !db
                        .q_json(
                            "SELECT id FROM skills WHERE name=?1 AND kind='style'",
                            &[&base_name as &dyn rusqlite::ToSql],
                        )
                        .unwrap_or_default()
                        .is_empty();
                    let name = if exists {
                        format!("{}（导入{}）", base_name, short)
                    } else {
                        base_name
                    };
                    if save_style_card(db, &name, &base, style.trim()).is_none() {
                        stages_ok = false;
                        cx.error("导入分析：风格卡写入失败").await;
                    }
                }
            }
            Err(e) => {
                stages_ok = false;
                cx.error(&format!("导入分析：风格蒸馏失败（{}）", e)).await;
            }
        }
    } else {
        stages_ok = false;
    }
    // 完成标记：仅当**所有批次成功 + 无预算超限 + 所有阶段成功**才写 import_analyzed；
    // 否则明确 partial，绝不把部分覆盖说成全书完成（N06 / review #3 #4）。
    let covered_chars: usize = segs.iter().map(|s| s.chars().count()).sum();
    let complete = use_llm && map_failed.is_empty() && !budget_exceeded && stages_ok;
    if complete {
        let _ = db.exec(
                "INSERT INTO settings(key,value) VALUES(?,'1') ON CONFLICT(key) DO UPDATE SET value='1'",
                &[&format!("import_analyzed__{}", book_id) as &dyn rusqlite::ToSql],
            );
    }
    let mut summary = json!({
        "ok": complete,
        "complete": complete,
        "partial": !complete,
        "segments": segs.len(),
        "failedSegments": map_failed,
        "budgetExceeded": budget_exceeded,
        "stagesOk": stages_ok,
        "totalChars": total_chars,
        "coveredChars": covered_chars,
        "docCount": doc_count,
        "hidden": hidden,
        "proposalId": proposal_id,
    });
    if !complete {
        let reasons: Vec<String> = [
            if !use_llm {
                Some("未配置可用模型渠道".to_string())
            } else {
                None
            },
            if !map_failed.is_empty() {
                Some(format!("失败批次：{:?}", map_failed))
            } else {
                None
            },
            if budget_exceeded {
                Some(format!(
                    "超出单次预算（{}字 > {}）",
                    total_chars, MAP_TOTAL_BUDGET
                ))
            } else {
                None
            },
            if !stages_ok {
                Some("部分阶段未产出有效内容".to_string())
            } else {
                None
            },
        ]
        .into_iter()
        .flatten()
        .collect();
        let msg = format!(
                "导入分析未完整完成（{}）。已产出的为**提议**（参考/导入分析_*_{}.md），请作者确认后再并入正式设定；可重跑。",
                if reasons.is_empty() { "原因未知".to_string() } else { reasons.join("；") },
                short
            );
        cx.error(&msg).await;
        summary["warning"] = json!(msg);
    }
    cx.done(summary.clone()).await;
    Ok(Some(summary))
}

/// gen_session_title：会话标题生成（原 dispatch 分支体原样搬入）。
pub(crate) async fn gen_session_title(
    st: &Arc<AppState>,
    _cmd: &str,
    args: &Value,
    _tx: &tokio::sync::mpsc::Sender<String>,
) -> anyhow::Result<Option<Value>> {
    let db = &st.db;
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();
    let chn = molan_llm::resolve_agent_channel(db, "summary");
    let question = s("question");
    let reply = s("reply");
    let fallback = {
        let compact = reply.split_whitespace().collect::<Vec<_>>().join(" ");
        let a: String = compact.chars().take(10).collect();
        let b: String = question.chars().take(12).collect();
        if !a.is_empty() {
            a
        } else {
            b
        }
    };
    if let Some(chn) = chn {
        if let Ok(title) = chat_once(ChatParams {
                base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
                api_key: chn["key"].as_str().unwrap_or("").to_string(),
                model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
                messages: vec![
                    json!({"role": "system", "content": "用不超过 16 个字概括下面的会话主题，只输出标题本身："}),
                    json!({"role": "user", "content": question.chars().take(500).collect::<String>()}),
                ],
                temperature: 0.3,
                max_tokens: 64,
                ..Default::default()
            })
            .await
            {
                let t: String = title
                    .chars()
                    .filter(|c| !matches!(c, '“' | '”' | '"' | '\'' | '\n'))
                    .collect();
                let t = t.trim();
                if !t.is_empty() {
                    return Ok(Some(json!(t)));
                }
            }
    }
    Ok(Some(json!(fallback)))
}

/// decompose_novel：小说拆解（原 dispatch 分支体原样搬入）。
pub(crate) async fn decompose_novel(
    st: &Arc<AppState>,
    _cmd: &str,
    args: &Value,
    tx: &tokio::sync::mpsc::Sender<String>,
) -> anyhow::Result<Option<Value>> {
    let db = &st.db;
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();
    let channel = channel_id(&a("onEvent"));
    let mut cx = Ctx::new(tx, &channel);
    let req_id = s("requestId");
    let book_id = s("bookId");
    let book_name = s("name");
    let chn = molan_llm::resolve_agent_channel(db, "distill")
        .ok_or_else(|| anyhow!("未配置可用的模型渠道"))?;
    let mut sample = String::new();
    if let Some(arr) = a("texts").as_array() {
        sample = arr
            .iter()
            .filter_map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    if sample.is_empty() {
        sample = s("text");
    }
    if sample.is_empty() {
        if let Some(arr) = a("chapters").as_array() {
            sample = arr
                .iter()
                .filter_map(|t| t.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
        }
    }
    if sample.trim().is_empty() {
        return Err(anyhow!("没有可拆解的文本（请传入 text 或先抓取正文）"));
    }
    // 断点记录：开始前先落库（abort 后 pending_decompose 能查到，前端可「从断点继续」）
    let pend_key = format!("decompose_pending__{}", book_id);
    let save_pending = |text: &str| {
        let _ = db.exec(
                "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                &[&pend_key as &dyn rusqlite::ToSql, &json!({"name": book_name, "text": text}).to_string()],
            );
    };
    save_pending(&sample);
    // 取消登记：本任务开始前清掉同 id 残留标记（上一轮异常退出可能没消费）
    take_abort(&req_id);
    let p = prompts(&st.root);
    let base_url = chn["baseUrl"].as_str().unwrap_or("").to_string();
    let api_key = chn["key"].as_str().unwrap_or("").to_string();
    let model = chn["model"].as_str().unwrap_or("deepseek-chat").to_string();
    let sys = json!({"role": "system", "content": "你是小说结构拆解师。"});
    const SEG: usize = 24000;
    let chars: Vec<char> = sample.chars().collect();
    let segs: Vec<String> = (0..chars.len().div_ceil(SEG))
        .map(|k| {
            let end = ((k + 1) * SEG).min(chars.len());
            chars[k * SEG..end].iter().collect()
        })
        .collect();
    if segs.len() <= 1 {
        let user = format!(
            "{}\n\n【文本】\n{}",
            p["decompose_single"].as_str().unwrap_or(""),
            sample.chars().take(SEG).collect::<String>()
        );
        let params = ChatParams {
            base_url: base_url.clone(),
            api_key: api_key.clone(),
            model: model.clone(),
            messages: vec![sys, json!({"role": "user", "content": user})],
            max_tokens: 8192,
            ..Default::default()
        };
        drain_chat_completion(params, &mut cx).await?;
    } else {
        let map_tpl = p["decompose_map"].as_str().unwrap_or("").to_string();
        let mut notes = Vec::new();
        let mut cancelled = false;
        for (i, seg) in segs.iter().enumerate() {
            // 分段之间检查取消（abort_chat 置位后不再发起下一段请求）
            if take_abort(&req_id) {
                cancelled = true;
                break;
            }
            cx.step(
                (i + 1) as i64,
                &format!("逐段拆解 · 第 {}/{} 段", i + 1, segs.len()),
            )
            .await;
            let user = format!("{}\n\n【第 {} 段】\n{}", map_tpl, i + 1, seg);
            let seg_params = ChatParams {
                base_url: base_url.clone(),
                api_key: api_key.clone(),
                model: model.clone(),
                messages: vec![sys.clone(), json!({"role": "user", "content": user})],
                max_tokens: 8192,
                ..Default::default()
            };
            let out = match chat_once_retry_n(seg_params, 2).await {
                Ok(out) => out,
                Err(e) => {
                    tracing::warn!("拆解第 {} 段失败（已重试）：{}", i + 1, e);
                    format!("（第 {} 段拆解失败：{}）", i + 1, e)
                }
            };
            notes.push(format!("## 第 {} 段拆解笔记\n{}", i + 1, out.trim()));
        }
        if cancelled {
            take_abort(&req_id);
            cx.error("拆解已暂停——已完成的分段会保留，可从断点继续")
                .await;
            return Ok(Some(json!({"ok": false, "aborted": true})));
        }
        cx.step((segs.len() + 1) as i64, "汇总全书拆解报告").await;
        let user = format!(
            "{}\n\n【分段笔记】\n{}",
            p["decompose_reduce"].as_str().unwrap_or(""),
            notes.join("\n\n")
        );
        if let Ok(out) = chat_once_retry_n(
            ChatParams {
                base_url: base_url.clone(),
                api_key: api_key.clone(),
                model: model.clone(),
                messages: vec![sys.clone(), json!({"role": "user", "content": user})],
                max_tokens: 16384,
                // 汇总=纯合并任务：关思考防预算被思考吃空
                no_thinking: true,
                reasoning_effort: String::new(),
                ..Default::default()
            },
            2,
        )
        .await
        {
            cx.full = out;
        }
    }
    take_abort(&req_id);
    let report = cx.full.clone();
    // 空报告防御：上游偶发失败时明确报错
    if report.trim().is_empty() {
        cx.error("拆解失败（模型返回了空内容，多为上游渠道瞬时故障）——请重试一次")
            .await;
        return Ok(Some(json!({"ok": false, "err": "empty-output"})));
    }
    // 拆解完成自动入库技能广场：下次对话可直接点名调用（与风格蒸馏同等待遇）
    let dname = s("name");
    if !dname.is_empty() && report.trim().chars().count() > 200 {
        let card = format!(
            "# 《{}》拆解

{}",
            dname,
            report.trim()
        );
        save_style_card_labeled(db, &dname, &dname, &card, "拆解");
        cx.ev(json!({"type": "saved", "files": [format!("技能广场 / {}", dname)]}))
            .await;
    }
    cx.done(json!({})).await;
    Ok(Some(json!({"ok": true, "report": report})))
}

/// gen_scene_body：场景正文生成（原 dispatch 分支体原样搬入）。
pub(crate) async fn gen_scene_body(
    st: &Arc<AppState>,
    _cmd: &str,
    args: &Value,
    tx: &tokio::sync::mpsc::Sender<String>,
) -> anyhow::Result<Option<Value>> {
    let db = &st.db;
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();
    let channel = channel_id(&a("onEvent"));
    if a("onEvent").is_null() {
        return Err(anyhow!("缺少事件通道"));
    }
    let mut cx = Ctx::new(tx, &channel);
    let chn = molan_llm::resolve_agent_channel(db, "chapter")
        .ok_or_else(|| anyhow!("未配置可用的模型渠道。请在 设置→渠道管理 里添加"))?;
    let book_id = s("bookId");
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
                .and_then(|r| r["genre"].as_str().map(|x| x.to_string()))
        })
    };
    let style = resolve_book_style(db, &st.root, &book_id, book_genre.as_deref());
    let p = prompts(&st.root);
    let mut sys = String::new();
    if let Some(x) = p["system"].as_str() {
        sys.push_str(x.trim());
        sys.push_str("\n\n");
    }
    if let Some(g) = &book_genre {
        if let Some(card) = p[format!("genre__{}", genre_key(g))].as_str() {
            sys.push_str(card);
            sys.push_str("\n\n");
        }
    }
    if let Some(st2) = &style {
        sys.push_str(st2);
        sys.push_str("\n\n");
    }
    if let Some(t) = p["anti_ai_tone"].as_str() {
        sys.push_str(t);
        sys.push_str("\n\n");
    }
    if let Some(t) = p["scene_body"].as_str() {
        sys.push_str(t);
        sys.push_str("\n\n");
    }
    // 本书写作偏好 + 结构级反AI腔（同 build_system）
    let prefs = book_prefs_block(db, &book_id);
    if !prefs.is_empty() {
        sys.push_str(&prefs);
        sys.push_str("\n\n");
    }
    sys.push_str(ANTI_AI_STRUCTURE);
    // 资料库上下文：自动带设定/细纲/参考（保证人名、设定与资料一致）
    let mut ctx = String::new();
    let tree = files::scan_tree(db, &book_id);
    if let Some(arr) = tree.as_array() {
        // 细纲最优先（本章怎么写全看它），其次设定、参考
        let prio = |d: &str| match d {
            "细纲" => 0,
            "设定" => 1,
            _ => 2,
        };
        let mut gs: Vec<&Value> = arr
            .iter()
            .filter(|g| {
                let d = g["dir"].as_str().unwrap_or("");
                d == "设定" || d == "细纲" || d == "参考"
            })
            .collect();
        gs.sort_by_key(|g| prio(g["dir"].as_str().unwrap_or("")));
        for g in gs {
            let dir = g["dir"].as_str().unwrap_or("");
            if let Some(fs) = g["files"].as_array() {
                for f in fs {
                    let name = f["name"].as_str().unwrap_or("");
                    if name.is_empty() {
                        continue;
                    }
                    if let Some(c) = files::read_file(db, &book_id, dir, name) {
                        ctx.push_str(&format!(
                            "# 资料：{}\n{}\n",
                            name,
                            c.chars().take(3000).collect::<String>()
                        ));
                    }
                }
            }
        }
    }
    if !ctx.is_empty() {
        sys.push_str("\n\n【资料库上下文（人名/设定/前文必须以资料为准）】\n");
        sys.push_str(&ctx.chars().take(12000).collect::<String>());
    }
    let user = format!(
        "【章节】{}\n【章节摘要】{}\n【场景 {}/{}】\n{}",
        s("chapterTitle"),
        s("chapterBrief"),
        a("sceneIndex").as_i64().unwrap_or(1),
        a("sceneCount").as_i64().unwrap_or(1),
        s("sceneJson")
    );
    cx.progress().await;
    let params = ChatParams {
        base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: chn["key"].as_str().unwrap_or("").to_string(),
        model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
        messages: vec![
            json!({"role": "system", "content": sys}),
            json!({"role": "user", "content": user}),
        ],
        max_tokens: 16384,
        ..Default::default()
    };
    let run = async {
        drain_chat_completion(params, &mut cx).await?;
        Ok::<_, anyhow::Error>(())
    };
    run.await?;
    let mut full = cx.full.clone();
    // 空输出防御：先关思考自动补跑一次（防思考吃满预算），仍空才报错
    if full.trim().is_empty() {
        let p2 = ChatParams {
            base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
            api_key: chn["key"].as_str().unwrap_or("").to_string(),
            model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
            messages: vec![
                json!({"role": "system", "content": sys}),
                json!({"role": "user", "content": user}),
            ],
            temperature: 0.7,
            max_tokens: 16384,
            stream: false,
            no_thinking: true,
            reasoning_effort: String::new(),
            ..Default::default()
        };
        if let Ok(out) = molan_llm::chat_once(p2).await {
            if !out.trim().is_empty() {
                cx.delta(&out).await;
                full = out.trim().to_string();
                cx.full = full.clone();
            }
        }
    }
    if full.trim().is_empty() {
        cx.error("本章生成失败（模型返回了空内容，多为上游渠道瞬时故障）——请点重试再生成一次")
            .await;
        return Ok(Some(json!({"ok": false, "err": "empty-output"})));
    }
    // 生成正文后的自动审核去AI味：读本书「自动去味」设置（默认官方标准）
    if !book_id.is_empty() && full.chars().count() >= 300 {
        let ov = a("humanizeOverride").as_str().unwrap_or("").to_string();
        let ov = if ov.is_empty() {
            None
        } else {
            Some(ov.as_str())
        };
        let (hb, rep) = auto_humanize(db, &book_id, &full, ov, "chapter").await;
        if hb != full {
            full = hb;
            cx.full = full.clone();
        }
        cx.ev(
            json!({"type": "deai", "score": rep["score"], "passed": rep["passed"],
                "method": rep["method"], "rounds": rep["rounds"],
                "violations": rep["violations"].as_array().map(|x| x.len()).unwrap_or(0)}),
        )
        .await;
    }
    // 自动落盘：正文按章节标题写入「正文」目录（多场景追加），并记录 docName 到消息
    if !book_id.is_empty() && !full.trim().is_empty() {
        let mut fname = s("chapterTitle").trim().to_string();
        if fname.is_empty() {
            let tree = files::scan_tree(db, &book_id);
            let next = crate::handlers::max_chapter_num(&tree, "正文") + 1;
            fname = format!("第{}章.md", next);
        }
        fname = crate::handlers::safe(&fname);
        if !fname.ends_with(".md") && !fname.ends_with(".txt") {
            fname.push_str(".md");
        }
        // 默认走待审（契约 #6 / F07）：场景正文不直接写「正文」，除非用户显式
        // 走正式提交（当前无该开关，故一律待审）；同一章多场景在待审组内 CAS 追加，
        // 绝不覆盖作者已改的正式稿或他人待审稿。
        let existing = files::read_file(db, &book_id, molan_core::db::REVIEW_GROUP, &fname)
            .unwrap_or_default();
        let merged = if existing.trim().is_empty() {
            full.trim().to_string()
        } else {
            format!("{}\n\n---\n\n{}", existing.trim(), full.trim())
        };
        if existing.trim().is_empty() {
            files::write_ai_file(db, &book_id, molan_core::db::REVIEW_GROUP, &fname, &merged)?;
        } else {
            // 追加既有待审稿：CAS 保证期间无人改动，且锁定文件会被拒绝
            files::write_file_cas(
                db,
                &book_id,
                molan_core::db::REVIEW_GROUP,
                &fname,
                &existing,
                &merged,
            )?;
        }
        if let Some(ch) = crate::handlers::chapter_num_from_name(&fname) {
            let _ = db.exec(
                    "INSERT OR REPLACE INTO pending_chapter(book_id,ch,review_file,status,created_at) VALUES(?,?,?,?,?)",
                    &[
                        &book_id as &dyn rusqlite::ToSql,
                        &ch,
                        &fname,
                        &"pending",
                        &stats::now_ms(),
                    ],
                );
        }
        if let Some(mid) = a("messageId").as_str() {
            if !mid.is_empty() {
                let rows = db
                    .q_json(
                        "SELECT context_json FROM messages WHERE id=?",
                        &[&mid as &dyn rusqlite::ToSql],
                    )
                    .unwrap_or_default();
                if let Some(row) = rows.first() {
                    let ctx =
                        serde_json::from_str::<Value>(row["context_json"].as_str().unwrap_or("{}"))
                            .unwrap_or(json!({}));
                    let mut o = if let Value::Object(m) = ctx {
                        m
                    } else {
                        serde_json::Map::new()
                    };
                    o.insert("docName".into(), json!(fname));
                    let _ = db.exec(
                        "UPDATE messages SET context_json=?, updated_at=? WHERE id=?",
                        &[
                            &Value::Object(o).to_string() as &dyn rusqlite::ToSql,
                            &stats::now_ms(),
                            &mid,
                        ],
                    );
                }
            }
        }
    }
    cx.done(json!({"messageId": a("messageId")})).await;
    Ok(Some(json!({"ok": true, "report": full})))
}

/// distill_style_stream / redistill_book_style：风格蒸馏（原 dispatch 分支体原样搬入）。
pub(crate) async fn distill_style_stream(
    st: &Arc<AppState>,
    _cmd: &str,
    args: &Value,
    tx: &tokio::sync::mpsc::Sender<String>,
) -> anyhow::Result<Option<Value>> {
    let db = &st.db;
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();
    let channel = channel_id(&a("onEvent"));
    let mut cx = Ctx::new(tx, &channel);
    let chn = molan_llm::resolve_agent_channel(db, "distill")
        .ok_or_else(|| anyhow!("未配置可用的模型渠道"))?;
    let p = prompts(&st.root);
    let mut sample = String::new();
    if let Some(texts) = a("texts").as_array() {
        sample = texts
            .iter()
            .filter_map(|t| t.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    if sample.is_empty() {
        sample = s("text");
    }
    let book_id = s("bookId");
    if sample.is_empty() && !book_id.is_empty() {
        let tree = files::scan_tree(db, &book_id);
        if let Some(arr) = tree.as_array() {
            for g in arr {
                let dir = g["dir"].as_str().unwrap_or("");
                if dir != "正文" && dir != "参考" {
                    continue;
                }
                if let Some(fs) = g["files"].as_array() {
                    for f in fs {
                        let name = f["name"].as_str().unwrap_or("");
                        let c = files::read_file(db, &book_id, dir, name).unwrap_or_default();
                        sample.push_str(&format!("[{}]\n{}\n", name, c));
                    }
                }
            }
        }
    }
    if sample.trim().is_empty() {
        return Err(anyhow!(
            "没有可蒸馏的文本：请提供 texts 或让本书正文/参考目录有内容"
        ));
    }
    let sample = distill_sample(&sample, 24000);
    let title = s("title");
    let title = if title.is_empty() {
        "参考作品".to_string()
    } else {
        title
    };
    let distill_tpl = p["distill"].as_str().unwrap_or("").to_string();
    let msgs = vec![
        json!({"role": "system", "content": "你是资深网文编辑与文体分析师。"}),
        json!({"role": "user", "content": format!("题目《{}》\n\n{}\n\n【文本】\n{}", title, distill_tpl, sample)}),
    ];
    let _step = tokio::spawn({
        let tx = tx.clone();
        let channel = channel.clone();
        async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(900)).await;
                let _ = tx
                    .send(format!(
                        "{}\n",
                        json!({"ch": channel, "e": {"type": "progress"}})
                    ))
                    .await;
            }
        }
    });
    let params = ChatParams {
        base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: chn["key"].as_str().unwrap_or("").to_string(),
        model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
        messages: msgs,
        max_tokens: 8192,
        ..Default::default()
    };
    let run = async {
        drain_chat_completion(params, &mut cx).await?;
        Ok::<_, anyhow::Error>(())
    };
    let res = run.await;
    // step task 由 drop tx 结束
    res?;
    let full = cx.full.clone().trim().to_string();
    cx.full = full.clone();
    // 入库文风卡（对齐 saveStyleCard）
    if !full.is_empty() {
        let mut name = s("name").trim().to_string();
        if name.is_empty() && !book_id.is_empty() {
            let t = db
                .q_json(
                    "SELECT title FROM books WHERE id=?1",
                    &[&book_id as &dyn rusqlite::ToSql],
                )
                .ok()
                .and_then(|v| {
                    v.first()
                        .and_then(|r| r["title"].as_str().map(|s| s.to_string()))
                })
                .unwrap_or_default();
            if !t.is_empty() {
                name = format!("《{}》原作风格", t);
            }
        }
        if !name.is_empty() {
            save_style_card(db, &name, &title, &full);
        }
        if s("name").trim().is_empty() && !book_id.is_empty() {
            molan_core::books::set_book_style_text(db, &book_id, &full);
        }
    }
    cx.done(json!({})).await;
    Ok(Some(json!({"ok": true})))
}

/// gen_style_sample：一次性风格样例（原 dispatch 分支体原样搬入）。
pub(crate) async fn gen_style_sample(
    st: &Arc<AppState>,
    _cmd: &str,
    args: &Value,
    _tx: &tokio::sync::mpsc::Sender<String>,
) -> anyhow::Result<Option<Value>> {
    let db = &st.db;
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();
    let chn = molan_llm::resolve_agent_channel(db, "distill")
        .ok_or_else(|| anyhow!("未配置可用的模型渠道"))?;
    let prompt_text = {
        let sp = s("stylePrompt");
        if !sp.is_empty() {
            sp
        } else {
            let st_ = molan_core::books::get_book_style(db, &s("bookId"));
            st_.as_str().unwrap_or("").to_string()
        }
    };
    let sys = if !prompt_text.is_empty() {
        format!(
            "参考下面风格卡，输出一段约 200 字的示例文段，仅输出文段本身：\n\n{}",
            prompt_text.chars().take(8000).collect::<String>()
        )
    } else {
        "你是网文风格师。".to_string()
    };
    let p = prompts(&st.root);
    let user = p["style_sample"]
        .as_str()
        .unwrap_or("按风格卡写一段 200 字左右的示例文段。")
        .to_string();
    let out = molan_llm::chat_once_retry(ChatParams {
        base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: chn["key"].as_str().unwrap_or("").to_string(),
        model: chn["model"].as_str().unwrap_or("").to_string(),
        messages: vec![
            json!({"role": "system", "content": sys}),
            json!({"role": "user", "content": user}),
        ],
        max_tokens: 4096,
        ..Default::default()
    })
    .await?;
    Ok(Some(json!(out.trim())))
}

fn distill_sample(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text.to_string();
    }
    let part = limit / 4;
    let mut chunks = Vec::new();
    for k in 0..4 {
        let i = ((chars.len() - part) * k / 3).min(chars.len().saturating_sub(part));
        let seg: String = chars[i..i + part].iter().collect();
        chunks.push(format!(
            "【第 {} 段（约全文 {}% 处）】\n{}",
            k + 1,
            i * 100 / chars.len().max(1),
            seg
        ));
    }
    chunks.join("\n\n")
}

fn save_style_card(
    db: &molan_core::db::Db,
    name: &str,
    title: &str,
    full_text: &str,
) -> Option<String> {
    save_style_card_labeled(db, name, title, full_text, "蒸馏")
}

fn save_style_card_labeled(
    db: &molan_core::db::Db,
    name: &str,
    title: &str,
    full_text: &str,
    label: &str,
) -> Option<String> {
    if name.is_empty() || full_text.trim().is_empty() {
        return None;
    }
    let first_line = full_text
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let desc = format!(
        "{}自《{}》：{}",
        label,
        title,
        first_line.chars().take(60).collect::<String>()
    );
    let existing = db
        .q_json(
            "SELECT id FROM skills WHERE name=?1 AND kind='style'",
            &[&name as &dyn rusqlite::ToSql],
        )
        .unwrap_or_default();
    if let Some(row) = existing.first() {
        let id = row["id"].as_str().unwrap_or("").to_string();
        let _ = db.exec(
            "UPDATE skills SET description=?1, prompt_template=?2, enabled=1, origin='user', source='' WHERE id=?3",
            &[&desc as &dyn rusqlite::ToSql, &full_text, &id],
        );
        return Some(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    let _ = db.exec(
        "INSERT INTO skills(id,name,description,prompt_template,kind,source,enabled,builtin_key) VALUES(?1,?2,?3,?4,'style','',1,NULL)",
        &[&id as &dyn rusqlite::ToSql, &name, &desc, &full_text],
    );
    Some(id)
}

fn unwrap_import_json(text: &str) -> String {
    let s = text.trim();
    if !s.starts_with('{') {
        return s.to_string();
    }
    let Ok(j) = serde_json::from_str::<Value>(s) else {
        return s.to_string();
    };
    if !j.is_object() {
        return s.to_string();
    }
    if let Some(chapters) = j["chapters"].as_array() {
        let body = chapters
            .iter()
            .map(|c| {
                format!(
                    "## {}\n{}",
                    c["title"].as_str().unwrap_or("章节"),
                    c["summary"].as_str().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        return format!("# 导入分析 · 章节摘要\n\n{}", body);
    }
    if !j["worldview"].is_null() || !j["synopsis"].is_null() || !j["characters"].is_null() {
        let mut parts = vec!["# 导入分析 · 设定资料".to_string()];
        if let Some(g) = j["genre"].as_str() {
            parts.push(format!("**题材**：{}\n", g));
        }
        if let Some(w) = j["worldview"].as_str() {
            parts.push(format!("{}\n", w.trim()));
        }
        if let Some(chars) = j["characters"].as_array() {
            if !chars.is_empty() {
                parts.push("## 人物".into());
                for c in chars {
                    parts.push(format!(
                        "### {}\n- 身份：{}\n- 性格：{}\n- 目标：{}\n- 关系：{}",
                        c["name"].as_str().unwrap_or("无名"),
                        c["identity"].as_str().unwrap_or(""),
                        c["personality"].as_str().unwrap_or(""),
                        c["goal"].as_str().unwrap_or(""),
                        c["relation"].as_str().unwrap_or(""),
                    ));
                }
                parts.push(String::new());
            }
        }
        if let Some(syn) = j["synopsis"].as_str() {
            parts.push(format!("{}\n", syn.trim()));
        }
        return parts.join("\n\n");
    }
    s.to_string()
}
