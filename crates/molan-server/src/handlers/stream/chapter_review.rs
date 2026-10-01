//! 章节审核后关（T4b）：从 auto_write.rs 抽取的可复用审核链。
//!
//! 写作顺序（本模块只负责「审核」一环）：
//!   写前 = 文风卡约束（auto_write::resolve_book_style 注入，见 build_system）
//!   → 写后 = 审核（本模块 review_body：LLM 调用 + 严格 JSON 解析 + fail-closed）
//!   → 去味（auto_write::auto_humanize 门）。
//!
//! 两条调用链的实际执行序：
//! - 批量链 auto_write::write_one_chapter：正文 → 审核 → 去味 →（去味改文则重新复核）。
//! - 单章链 chat_stream：正文 → 去味（既有门，直接改写待审正文）→ 待审落盘
//!   → 审核已落盘内容（review_pending_saved）。审核对象始终是「最终写入待审的文件正文」，
//!   bodyHash 因此与落盘内容严格绑定；任何再次改动正文都会使该 hash 作废。
//!
//! 硬规则：审核链任何一环不可信（超预算 / 无渠道 / 调用失败 / JSON 两次解析失败 /
//! 结构不完整 / 判不通过却无问题）都必须转人工待审，绝不 Fail-Open；
//! 只有 ok=true 且无 hard 问题才算通过（软建议只提示、不阻断）。

use super::auto_write::extract_json_loose;
use super::{auto_book_context_for_chapter, auto_log, log_llm_usage};
use molan_core::files;
use molan_llm::ChatParams;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// 审核单次能覆盖的正文上限（字符）。超过即视为「未覆盖全文」，不得自动放行。
pub(crate) const REVIEW_BODY_BUDGET: usize = 9000;

/// 读取设定文件，但遵守 aiOff（作者禁止自动引用则返回空，绝不外发）。
fn read_asset_respecting_aioff(
    db: &molan_core::db::Db,
    book_id: &str,
    group: &str,
    name: &str,
) -> String {
    if files::file_flag(db, book_id, group, name, "aiOff") {
        return String::new();
    }
    files::read_file(db, book_id, group, name).unwrap_or_default()
}

/// 组装审核用 user prompt（初次审核与最终稿复审共用同一套规则，保证「审核对象=最终文本」）。
/// 返回 (prompt, body_truncated)：body_truncated=true 表示正文超出单次预算、只覆盖了前段，
/// 调用方必须据此转人工待审，绝不允许「只审了前 9000 字」却自动定稿。
fn build_review_user(
    db: &molan_core::db::Db,
    book_id: &str,
    ch: i64,
    pending_block: &str,
    body: &str,
) -> (String, bool) {
    // 作者资产：统一遵守 aiOff
    let people = read_asset_respecting_aioff(db, book_id, "设定", "人物表.md");
    let ledger = read_asset_respecting_aioff(db, book_id, "设定", "伏笔台账.md");
    let prev = read_asset_respecting_aioff(db, book_id, "正文", &format!("第{}章.md", ch - 1));
    let prev_tail: String = {
        let t = prev.chars().count();
        prev.chars().skip(t.saturating_sub(700)).collect()
    };
    // 时间上下文完全用 C 的 temporal helper（只含 < ch 的已定稿记忆与已批准正文），
    // 不再直接读「前情摘要.md」——旧摘要可能覆盖到 ch 之后，属于未来信息，必须由 C 过滤。
    let temporal = auto_book_context_for_chapter(db, book_id, ch, false);
    // 待审连写：用调用方在生成前冻结的同任务上一章待审稿（不得在此处重新读取，
    // 否则登记依赖时读到的可能已是被改动过的稿）。
    let pending_note = pending_block.to_string();
    // 当前正史事实（穿帮硬对照，A3）：活跃事实+未裁决冲突的精简清单，
    // 审核据此抓伤势/位置/物品/伏笔状态/信息差穿帮，而不是只靠模型自觉。
    let facts_block = molan_core::facts::list_facts(db, book_id, "", "current", 200)
        .ok()
        .map(|v| {
            let mut lines: Vec<String> = Vec::new();
            for f in v["facts"].as_array().into_iter().flatten() {
                let st = f["subjectType"].as_str().unwrap_or("");
                let sid = f["subjectId"].as_str().unwrap_or("");
                let pred = f["predicate"].as_str().unwrap_or("");
                let val = f["value"].as_str().unwrap_or("");
                let tag = if f["state"].as_str() == Some("disputed") {
                    "·冲突未裁决"
                } else {
                    ""
                };
                let line = if st == "secret" {
                    let p: Value = serde_json::from_str(val).unwrap_or(Value::Null);
                    let join = |k: &str| {
                        p[k].as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str())
                                    .collect::<Vec<_>>()
                                    .join("、")
                            })
                            .unwrap_or_default()
                    };
                    format!(
                        "- [秘密{}] {}（知情：{}；不知情：{}）",
                        tag,
                        p["fact"].as_str().unwrap_or(val),
                        join("known_by"),
                        join("unknown_to")
                    )
                } else if st == "thread" {
                    format!("- [伏笔{}] {} = {}", tag, sid, val)
                } else {
                    format!("- [事实{}] {}·{} = {}", tag, sid, pred, val)
                };
                lines.push(line);
            }
            let mut s = lines.join("\n");
            if s.chars().count() > 1600 {
                s = s.chars().take(1600).collect();
                s.push_str("\n…（更多事实略，可用 list_story_facts 全查）");
            }
            s
        })
        .unwrap_or_default();
    let facts_block = if facts_block.is_empty() {
        "（暂无已入账事实）".to_string()
    } else {
        facts_block
    };
    let body_len = body.chars().count();
    let truncated = body_len > REVIEW_BODY_BUDGET;
    let prompt = format!(
        "【本章细纲】\n{}\n\n【人物表】\n{}\n\n【伏笔台账】\n{}\n\n【当前正史事实（穿帮硬对照）】\n{}\n\n【时间线上下文（截至第{}章）】\n{}\n\n【上一章结尾】\n{}\n\n{}【待审正文】\n{}\n\n\
审读要求：\n\
0) 先逐条对照【当前正史事实】：伤势/位置/持有物/伏笔状态/秘密知情范围，正文与之冲突即为硬伤，直接列出；\n\
1) 与细纲/人物表/上一章结尾比对，找硬伤：设定冲突、人物言行不一致、时间线错乱、前后矛盾、称呼错误；\n\
2) 判断是否是可读的小说正文（不是提纲、不是提问、不是解释说明）；\n\
3) 人称与视角是否统一；\n\
4) 跑偏检查：是否写了本章细纲以外的剧情、是否提前揭底未回收的伏笔、是否新增了档案/人物表里没有的重要人物或设定、是否重复解决上一章已解决的冲突；\n\
5) 时间线是否紧接着上一章结尾，有没有复述前文凑字数。\n\
只输出 JSON：{{\"ok\":true 或 false,\"issues\":[\"问题1\",\"问题2\"],\"fix\":\"给写手的具体修改指令，不超过150字\"}}\n\
没有硬伤时 ok=true、issues 为空数组。",
        outline_or_empty(db, book_id, ch),
        people.chars().take(900).collect::<String>(),
        ledger.chars().take(900).collect::<String>(),
        facts_block,
        ch - 1,
        temporal.chars().take(3000).collect::<String>(),
        prev_tail,
        pending_note,
        body.chars().take(REVIEW_BODY_BUDGET).collect::<String>(),
    );
    (prompt, truncated)
}

fn outline_or_empty(db: &molan_core::db::Db, book_id: &str, ch: i64) -> String {
    // 细纲同样是作者资产：标了 aiOff 就不得进入审核 prompt
    read_asset_respecting_aioff(db, book_id, "细纲", &format!("细纲_第{}章.md", ch))
        .chars()
        .take(1200)
        .collect()
}

/// 审核结论：hard=必须处理的问题，soft=不阻断的软建议。
pub(crate) struct ReviewVerdict {
    pub ok: bool,
    pub hard: Vec<String>,
    pub soft: Vec<String>,
}

/// 严格解析审核结论：仅当 ok 是布尔且 issues 是数组（元素为字符串）时才可信。
/// 缺字段、类型不符一律 None —— Fail-Closed。warnings 可选，只作软建议不阻断。
fn parse_review_verdict(j: &Value) -> Option<ReviewVerdict> {
    let obj = j.as_object()?;
    let ok = obj.get("ok")?.as_bool()?;
    let hard: Vec<String> = obj
        .get("issues")?
        .as_array()?
        .iter()
        .map(|x| x.as_str().map(|s| s.trim().to_string()))
        .collect::<Option<Vec<String>>>()?;
    let soft: Vec<String> = obj
        .get("warnings")
        .and_then(|w| w.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.trim().to_string())
                .collect()
        })
        .unwrap_or_default();
    Some(ReviewVerdict { ok, hard, soft })
}

/// 审核判定（纯函数，便于四分支单测）：把已解析的 verdict JSON 归入
/// 通过 / 需重写 / 不可信（fail-closed）三类。
pub(crate) enum ReviewDecision {
    /// ok=true 且无 hard 问题：通过（note 已含软建议摘要）
    Pass { note: String },
    /// 有 hard 问题：需要一轮重写（ok 原样保留，fix 为重写总意见）
    Fix {
        ok: bool,
        hard: Vec<String>,
        fix: String,
        soft_note: String,
    },
    /// 审核链路不可信：必须转人工待审
    Untrusted(String),
}

/// 软建议摘要（通过/重写结论共用，避免两处措辞漂移）
fn soft_note_of(soft: &[String]) -> String {
    if soft.is_empty() {
        String::new()
    } else {
        format!(
            "（另有{}条软建议：{}）",
            soft.len(),
            soft.iter().take(2).cloned().collect::<Vec<_>>().join("；")
        )
    }
}

pub(crate) fn classify_verdict(j: &Value) -> ReviewDecision {
    let Some(v) = parse_review_verdict(j) else {
        return ReviewDecision::Untrusted(
            "审核返回结构不完整（缺少布尔 ok 或 issues 数组）→ 转人工待审".to_string(),
        );
    };
    let soft_note = soft_note_of(&v.soft);
    // 明确 ok=false 却没有任何 hard 问题：判定不可信，转人工
    if !v.ok && v.hard.is_empty() {
        return ReviewDecision::Untrusted("审核判不通过但未给出问题 → 转人工待审".to_string());
    }
    // schema 有效 + 无 hard 问题 → 通过（软建议只提示，不阻断）
    if v.ok && v.hard.is_empty() {
        return ReviewDecision::Pass {
            note: format!("通过{}", soft_note),
        };
    }
    let fix = j["fix"].as_str().unwrap_or("").trim().to_string();
    ReviewDecision::Fix {
        ok: v.ok,
        hard: v.hard,
        fix,
        soft_note,
    }
}

/// review_body 的结果：verdict JSON {ok,issues,fix} + 人类可读报告 + fail-closed 标记。
pub(crate) struct ReviewOutcome {
    /// 原始 verdict JSON（结构不完整时为 Null）
    pub verdict: Value,
    pub ok: bool,
    /// hard 问题（=issues）
    pub issues: Vec<String>,
    pub fix: String,
    /// 人类可读报告（flagged/通过时非空）
    pub note: String,
    /// true = 审核链路不可信 → 调用方必须转人工待审，绝不冒充通过
    pub flagged: bool,
    /// 软建议摘要（重写流程拼结论用）
    pub soft_note: String,
}

impl ReviewOutcome {
    fn untrusted(note: String) -> Self {
        ReviewOutcome {
            verdict: Value::Null,
            ok: false,
            issues: Vec::new(),
            fix: String::new(),
            note,
            flagged: true,
            soft_note: String::new(),
        }
    }
}

/// 审稿系统提示：固定 JSON 协议 + 项目审稿角色 + 「审稿」子阶段技能（run_plan::stage_plan 冻结）。
/// 技能只能调整审查重点，不能改变输出协议（正文写作卡因任务不适用不会出现在这里）。
pub(crate) fn review_system(db: &molan_core::db::Db, book_id: &str, plan: &Value) -> String {
    let mut sys =
        "你是网文责编，审读一章正文。只输出一个 JSON 对象，不要解释、不要代码块标记。".to_string();
    if let Ok(extra) = molan_core::deepwrite::strict_agent_instructions(db, book_id, "continuity") {
        if !extra.is_empty() {
            sys.push_str("\n\n");
            sys.push_str(&extra);
        }
    }
    let skills = super::run_plan::stage_skills_text(plan, &sys);
    if !skills.is_empty() {
        sys.push_str("\n\n");
        sys.push_str(&skills);
        sys.push_str("\n【协议硬约束】以上技能只能调整审查重点，不能改变输出协议；最终回答仍必须是本阶段要求的 JSON 对象。");
    }
    sys
}

/// 可复用审核调用主体：LLM 调用 + 严格 JSON 解析 + fail-closed 语义。
/// 返回 verdict JSON {ok,issues,fix} + 报告；flagged=true 表示必须转人工待审。
/// cancel 由调用方传入（批量走 auto_cancel_token，单章走本轮 turn 的取消令牌）。
async fn review_body_once(
    db: &molan_core::db::Db,
    book_id: &str,
    ch: i64,
    pending_block: &str,
    body: &str,
    cancel: Option<CancellationToken>,
    plan: &Value,
) -> ReviewOutcome {
    let sys = review_system(db, book_id, plan);
    let (user, body_truncated) = build_review_user(db, book_id, ch, pending_block, body);
    if body_truncated {
        // 超单次预算：未覆盖全文，绝不自动放行（可人工审或后续分块实现）
        return ReviewOutcome::untrusted(format!(
            "本章正文 {} 字超过单次审核预算 {} 字，未覆盖全文 → 转人工待审",
            body.chars().count(),
            REVIEW_BODY_BUDGET
        ));
    }
    let Some(chn) = molan_llm::resolve_agent_channel(db, "review") else {
        // 无审核渠道 = 审核链路不可信，必须待审，绝不 Fail-Open
        return ReviewOutcome::untrusted("无可用审核渠道 → 转人工待审".to_string());
    };
    let params = ChatParams {
        base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: chn["key"].as_str().unwrap_or("").to_string(),
        model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
        messages: vec![
            json!({"role": "system", "content": sys}),
            json!({"role": "user", "content": user}),
        ],
        temperature: 0.2,
        max_tokens: 1500,
        stream: false,
        no_thinking: true,
        reasoning_effort: String::new(),
        log_tag: "auto_review".to_string(),
        log_book: book_id.to_string(),
        cancel: cancel.clone(),
    };
    let raw = match molan_llm::chat_once_retry_logged_n(params.clone(), 2).await {
        Ok((x, usage)) => {
            log_llm_usage(
                db,
                book_id,
                "auto_review",
                chn["model"].as_str().unwrap_or(""),
                &usage,
            );
            x
        }
        // 审核调用失败（含 HTTP 400/网络错/取消）：审核未完成，必须待审，绝不 Fail-Open
        Err(e) => return ReviewOutcome::untrusted(format!("审核调用失败（{}）→ 转人工待审", e)),
    };
    // 审核必须 Fail-Closed：输出无法解析成 JSON 时带反馈重问一次，仍失败则强制转人工
    let j = match extract_json_loose(&raw) {
        Some(j) => j,
        None => {
            let mut params2 = params.clone();
            params2.messages.push(
                json!({"role": "assistant", "content": raw.chars().take(500).collect::<String>()}),
            );
            params2.messages.push(json!({"role": "user", "content": "你上一条输出无法解析为 JSON，请只输出一个 JSON 对象"}));
            match molan_llm::chat_once_retry_logged_n(params2, 2).await {
                Ok((raw2, usage)) => {
                    log_llm_usage(
                        db,
                        book_id,
                        "auto_review",
                        chn["model"].as_str().unwrap_or(""),
                        &usage,
                    );
                    match extract_json_loose(&raw2) {
                        Some(j) => j,
                        None => {
                            return ReviewOutcome::untrusted(
                                "审核 JSON 两次解析失败 → 转人工待审".to_string(),
                            )
                        }
                    }
                }
                Err(e) => {
                    return ReviewOutcome::untrusted(format!(
                        "审核 JSON 解析失败且重问失败（{}）→ 转人工待审",
                        e
                    ))
                }
            }
        }
    };
    match classify_verdict(&j) {
        ReviewDecision::Untrusted(note) => ReviewOutcome::untrusted(note),
        ReviewDecision::Pass { note } => ReviewOutcome {
            verdict: j,
            ok: true,
            issues: Vec::new(),
            fix: String::new(),
            note,
            flagged: false,
            soft_note: String::new(),
        },
        ReviewDecision::Fix {
            ok,
            hard,
            fix,
            soft_note,
        } => ReviewOutcome {
            verdict: j,
            ok,
            issues: hard,
            fix,
            note: String::new(),
            flagged: false,
            soft_note,
        },
    }
}

/// 单章落盘后关（T4b）：对**本轮已落盘的「正文待审」正文**执行一次审核，
/// 返回 (前端 review 事件, result_json 摘要)。没有待审正文落盘（或不足 300 字）时返回 None。
///
/// fail-closed：审核失败 / 无渠道 / 结构不可信 → 事件 ok=false + reason，待审稿保留；
/// 绝不因审核失败阻断已完成的落盘，也绝不把「没审成」当成「审过了」。
/// cancel 由调用方传入本轮 turn 的取消令牌（与生成/去味同一枚）。
pub(crate) async fn review_pending_saved(
    db: &molan_core::db::Db,
    book_id: &str,
    want_ch: Option<i64>,
    saved_files: &[String],
    cancel: Option<CancellationToken>,
    plan: &Value,
) -> Option<(Value, Value)> {
    let prefix = format!("{} / ", molan_core::db::REVIEW_GROUP);
    let fname = saved_files.iter().find_map(|s| s.strip_prefix(&prefix))?;
    let body = files::read_file(db, book_id, molan_core::db::REVIEW_GROUP, fname)?;
    if body.trim().chars().count() < 300 {
        return None;
    }
    let ch = want_ch
        .or_else(|| crate::handlers::chapter_num_from_name(fname))
        .unwrap_or(0);
    let body_hash = molan_core::continuity::content_hash(&body);
    let outcome = review_body(db, book_id, ch, "", &body, cancel, plan).await;
    let ok = !outcome.flagged && outcome.ok && outcome.issues.is_empty();
    let issues: Vec<Value> = outcome.issues.iter().cloned().map(Value::String).collect();
    let summary = json!({
        "ok": ok,
        "issues": issues,
        "bodyHash": body_hash,
        "note": outcome.note,
        "flagged": outcome.flagged,
        // 原始 verdict 的 fix 指令（ok=false 时为重写总意见），供前端展示修改建议
        "fix": outcome.verdict["fix"],
        "planHash": plan["planHash"],
    });
    let mut ev = json!({"type": "review", "ok": ok, "issues": issues, "bodyHash": body_hash});
    if !ok {
        ev["reason"] = json!(outcome.note);
    }
    Some((ev, summary))
}

/// 对「已被重写改变过的最终文本」重新执行同一套审核；返回 Err(原因) 表示不得定稿。
/// 与初次审核共用 build_review_user / parse_review_verdict，但只调用一次（不重问），
/// log_tag 记为 auto_review_final 以便用量区分。
async fn recheck_once(
    db: &molan_core::db::Db,
    book_id: &str,
    ch: i64,
    pending_block: &str,
    text: &str,
    cancel: Option<CancellationToken>,
    plan: &Value,
) -> Result<(), String> {
    // 复核同样受单次预算约束：重写后正文若超预算，绝不能只审前段就当通过
    let (review_user, truncated) = build_review_user(db, book_id, ch, pending_block, text);
    if truncated {
        return Err(format!(
            "重写稿 {} 字超过单次审核预算 {} 字，未覆盖全文",
            text.chars().count(),
            REVIEW_BODY_BUDGET
        ));
    }
    let Some(chn) = molan_llm::resolve_agent_channel(db, "review") else {
        return Err("复核阶段无可用审核渠道".to_string());
    };
    let review_sys = review_system(db, book_id, plan);
    let params = ChatParams {
        base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: chn["key"].as_str().unwrap_or("").to_string(),
        model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
        messages: vec![
            json!({"role": "system", "content": review_sys}),
            json!({"role": "user", "content": review_user}),
        ],
        temperature: 0.2,
        max_tokens: 1500,
        stream: false,
        no_thinking: true,
        reasoning_effort: String::new(),
        log_tag: "auto_review_final".to_string(),
        log_book: book_id.to_string(),
        cancel,
    };
    let raw = molan_llm::chat_once_retry_logged_n(params, 2)
        .await
        .map(|(x, usage)| {
            log_llm_usage(
                db,
                book_id,
                "auto_review_final",
                chn["model"].as_str().unwrap_or(""),
                &usage,
            );
            x
        })
        .map_err(|e| format!("复核调用失败（{}）", e))?;
    let j = extract_json_loose(&raw).ok_or_else(|| "复核返回无法解析为 JSON".to_string())?;
    let v = parse_review_verdict(&j).ok_or_else(|| "复核返回结构不完整".to_string())?;
    if v.ok && v.hard.is_empty() {
        auto_log(ch, "review", format!("第{}章 重写稿复核通过", ch));
        Ok(())
    } else {
        Err(format!("ok={} hard={}", v.ok, v.hard.len()))
    }
}

/// 审稿（见 review_body_once），并把结论按被审文本的 hash 记入审稿账本：
/// 之后文本若被修改 / 去味 / 重写，该结论在待审区显示为「已失效」。
pub(crate) async fn review_body(
    db: &molan_core::db::Db,
    book_id: &str,
    ch: i64,
    pending_block: &str,
    body: &str,
    cancel: Option<CancellationToken>,
    plan: &Value,
) -> ReviewOutcome {
    let out = review_body_once(db, book_id, ch, pending_block, body, cancel, plan).await;
    let passed = !out.flagged && out.ok && out.issues.is_empty();
    let verdict =
        json!({"ok": passed, "flagged": out.flagged, "issues": out.issues, "note": out.note});
    log_review(db, book_id, ch, body, &verdict, plan, "review");
    out
}

/// 重写 / 去味后的复核（见 recheck_once），结论同样记入审稿账本。
pub(crate) async fn recheck_reviewed_text(
    db: &molan_core::db::Db,
    book_id: &str,
    ch: i64,
    pending_block: &str,
    text: &str,
    cancel: Option<CancellationToken>,
    plan: &Value,
) -> Result<(), String> {
    let r = recheck_once(db, book_id, ch, pending_block, text, cancel, plan).await;
    let note = r.as_ref().err().cloned().unwrap_or_default();
    let verdict = json!({"ok": r.is_ok(), "flagged": r.is_err(), "issues": [], "note": note});
    log_review(db, book_id, ch, text, &verdict, plan, "recheck");
    r
}

fn log_review(
    db: &molan_core::db::Db,
    book: &str,
    ch: i64,
    text: &str,
    v: &Value,
    plan: &Value,
    src: &str,
) {
    let hash = molan_core::continuity::content_hash(text);
    let plan_hash = plan["planHash"].as_str().unwrap_or("");
    if let Err(e) = molan_core::review_log::record(db, book, ch, &hash, v, plan_hash, src) {
        tracing::warn!("审稿结论记账失败（不影响审稿结果）：{}", e);
    }
}

#[cfg(test)]
#[path = "chapter_review_tests.rs"]
mod tests;
