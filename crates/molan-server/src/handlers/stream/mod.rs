// 流式 handler 根模块：对外入口再导出 + dispatch_stream 命令分发 + 跨块共享的状态与上下文组装。
// 事件格式 {"ch":id,"e":{...}} 与 Node 版 Ctx 类完全一致。
use super::{chapter_num_from_name, skill_row, AppState};
use anyhow::{anyhow, Result};
use molan_core::files;
use molan_core::stats;
use serde_json::{json, Value};
use std::sync::Arc;

// 书源下载运行态（DL_STATUS/DL_STOP 归 sources 模块持有）

/// LLM 用量落账：usage 为 None（mock:// 渠道发 Meta(None)）时不落账；
/// 任何失败都只写过日志，绝不影响生成主流程。
pub(crate) fn log_llm_usage(
    db: &molan_core::db::Db,
    book_id: &str,
    tag: &str,
    model: &str,
    usage: &Option<Value>,
) {
    let Some(u) = usage.as_ref() else { return };
    let n = |k: &str| u[k].as_i64().unwrap_or(0);
    let _ = db.exec(
        "INSERT INTO llm_call_log(ts,book_id,tag,model,prompt_tokens,completion_tokens,total_tokens) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        &[
            &stats::now_ms() as &dyn rusqlite::ToSql,
            &book_id,
            &tag,
            &model,
            &n("prompt_tokens"),
            &n("completion_tokens"),
            &n("total_tokens"),
        ],
    );
}

/// 严格章节标题判定（S04）：必须是「第<数字/中文数字>章 …」开头的独立标题行，
/// 不接受「他翻到第三章」这类正文句子，也不接受超长行。
fn is_chapter_head(line: &str) -> bool {
    let t = line.trim().trim_start_matches('#').trim();
    // 必须按字符剥离「第」：它是 3 字节，字节索引 [1..] 会 panic（线上实例：第1章 捡回来的魔尊）
    let Some(after) = t.strip_prefix('第') else {
        return false;
    };
    let Some(pos) = after.find('章') else {
        return false;
    };
    // 第 与 章 之间只能是数字或中文数字（允许空白）；pos 是「章」在 after 中的字节偏移，必落在字符边界
    let num_part: String = after[..pos]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if num_part.is_empty() || num_part.chars().count() > 12 {
        return false;
    }
    if !num_part
        .chars()
        .all(|c| c.is_ascii_digit() || "零一二三四五六七八九十百千两".contains(c))
    {
        return false;
    }
    if t.chars().count() > 24 {
        return false;
    }
    chapter_num_from_name(t).is_some()
}

/// 标题解析（S04 严格版）：只接受标准章节标题，不再用「含第/章」的宽泛规则。
pub(crate) fn parse_chapter_title(line: &str) -> Option<(i64, bool)> {
    if !is_chapter_head(line) {
        return None;
    }
    let title = line.trim().trim_start_matches('#').trim();
    let num = chapter_num_from_name(title)?;
    let is_outline = title.contains("细纲") || title.contains("大纲");
    Some((num, is_outline))
}

/// 聊天产出落盘（F07 / 契约 C）：正文一律进入「正文待审」审批队列，
/// 绝不覆盖已有正式稿或已有待审稿；细纲只新建不覆盖。
/// 只有真正写盘成功才计入 saved；任一写入失败返回 Err，绝不假成功。
/// 带「锁内前置校验」的落盘：`check` 在每次 AI 写盘进入 fs_lock 之后、写盘之前执行
/// （取消/依赖等），因此不存在 check/write 竞态，也不会与外部加锁形成死锁。
/// check 每个文件都会被调用一次，返回 Err 即拒绝写入。
pub(crate) fn auto_save_chat_output_checked<F>(
    db: &molan_core::db::Db,
    book_id: &str,
    full: &str,
    _explicit_ch: Option<i64>,
    suppress_body: bool,
    check: F,
) -> Result<(Vec<String>, Vec<String>)>
where
    F: Fn() -> Result<()>,
{
    let mut saved = Vec::new();
    let mut skipped = Vec::new();
    if book_id.is_empty() || full.chars().count() < 120 {
        return Ok((saved, skipped));
    }
    let lines: Vec<&str> = full.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let head = lines[i].trim_start();
        if !head.starts_with('#') {
            i += 1;
            continue;
        }
        let Some((num, is_outline)) = parse_chapter_title(head) else {
            i += 1;
            continue;
        };
        let mut j = i + 1;
        let mut body = String::new();
        while j < lines.len() && !is_chapter_head(lines[j]) {
            body.push_str(lines[j]);
            body.push('\n');
            j += 1;
        }
        let body = body.trim().to_string();
        i = j;
        if body.chars().count() < 150 {
            continue;
        }
        if is_outline {
            let fname = format!("细纲_第{}章.md", num);
            if files::read_file(db, book_id, "细纲", &fname).is_some() {
                skipped.push(format!("细纲 / {} 已存在，未覆盖", fname));
                continue;
            }
            // 细纲也是 AI 产稿：走 checked（只新建、锁内校验、locked 拒绝）
            if let Err(e) = files::write_ai_file_checked(db, book_id, "细纲", &fname, &body, &check)
            {
                skipped.push(format!("细纲 / {} 未保存：{}", fname, e));
                continue;
            }
            // 章节状态机（P0-2）：只观测不阻断，记账失败不影响已落盘文件
            let _ = molan_core::chapter_state::record_outline(
                db,
                book_id,
                num,
                &molan_core::continuity::content_hash(&body),
            );
            saved.push(format!("细纲 / {}", fname));
        } else if suppress_body {
            // 细纲/大纲指令的产出绝不进正文组：正文段整段跳过（全链路 S2 回归锁）
            tracing::info!("细纲指令产出按 suppress_body 跳过正文落盘（第{}章）", num);
            skipped.push(format!(
                "第{}章正文段按细纲/大纲指令跳过（不进正文组）；如需保留请点击「保存到书籍目录」",
                num
            ));
            continue;
        } else {
            // 正文只能生成待审稿：新章也需作者明确接受后才进入正式正文。
            // 待审已存在则拒绝，绝不覆盖正式稿或已有待审稿。
            let fname = format!("第{}章.md", num);
            if files::read_file(db, book_id, molan_core::db::REVIEW_GROUP, &fname).is_some() {
                skipped.push(format!("第{}章已有待审稿，未覆盖；请先处理审批队列", num));
                continue;
            }
            // 锁内 check：取消/依赖不满足时不落稿
            if let Err(e) = files::write_ai_file_checked(
                db,
                book_id,
                molan_core::db::REVIEW_GROUP,
                &fname,
                &body,
                &check,
            ) {
                skipped.push(format!("第{}章待审落盘失败：{}", num, e));
                continue;
            }
            // 队列登记失败必须报错：吞掉会让「文件已写但审批队列没有」，
            // 作者在审批界面看不到这篇稿。
            if let Err(e) = super::register_review_queue(db, book_id, &fname, &body) {
                skipped.push(format!("第{}章文件已写入，但审批队列登记失败：{}", num, e));
            }
            let _ = molan_core::chapter_state::record_save(
                db,
                book_id,
                num,
                molan_core::db::REVIEW_GROUP,
                &molan_core::continuity::content_hash(&body),
            );
            saved.push(format!("{} / {}", molan_core::db::REVIEW_GROUP, fname));
        }
    }
    Ok((saved, skipped))
}

/// 按预算截取，但**不静默**：返回 (截取后文本, 被省略的字符数)。
/// 调用方必须把省略量显式告知模型，避免"关键内容被悄悄截掉仍宣称已核对"。
pub(crate) fn clip_complete(text: &str, budget: usize) -> (String, usize) {
    let total = text.chars().count();
    if total <= budget {
        return (text.to_string(), 0);
    }
    (text.chars().take(budget).collect(), total - budget)
}

/// 按引用（id / builtin_key / name）解析单个技能，只接受启用且模板非空的技能。
pub(crate) fn resolve_skill_ref(db: &molan_core::db::Db, key: &str) -> Option<Value> {
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    let rows = db
        .q_json(
            "SELECT * FROM skills WHERE (id=?1 OR builtin_key=?1 OR name=?1) AND enabled=1",
            &[&key as &dyn rusqlite::ToSql],
        )
        .ok()?;
    let r = rows.first()?;
    let row = skill_row(r);
    let tpl = row["promptTemplate"].as_str().unwrap_or("").trim();
    if tpl.is_empty() {
        tracing::warn!(
            "技能「{}」模板为空，已跳过注入",
            row["name"].as_str().unwrap_or(key)
        );
        return None;
    }
    Some(row)
}

/// 读取本书某任务的设置技能 id 列表（主技能 + 辅助技能）。
fn book_task_skill_ids(db: &molan_core::db::Db, book: &str, task: &str) -> (String, Vec<String>) {
    if book.is_empty() || task.is_empty() {
        return (String::new(), Vec::new());
    }
    let primary = molan_llm::get_setting(db, &format!("book_primary_skill__{}__{}", book, task))
        .trim()
        .to_string();
    let supports: Vec<String> =
        molan_llm::get_setting(db, &format!("book_support_skills__{}__{}", book, task))
            .trim()
            .to_string()
            .parse::<Value>()
            .ok()
            .and_then(|v| v.as_array().cloned())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
    (primary, supports)
}

/// 技能对任务的适用性（对齐前端 Qt/Tb 语义，N01/N04）：
/// 1) 技能自带 targets 优先；
/// 2) 否则按 builtin_key 前缀 / 官方名做任务映射；
/// 3) 无 targets 的自定义技能按 usage_mode 与任务类别兜底。
pub(crate) fn skill_targets(row: &Value) -> Vec<String> {
    if let Some(a) = row["targets"].as_array() {
        let list: Vec<String> = a
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .collect();
        if !list.is_empty() {
            return list;
        }
    }
    let name = row["name"].as_str().unwrap_or("");
    let builtin = row["builtinKey"].as_str().unwrap_or("");
    let kind = row["kind"].as_str().unwrap_or("");
    let usage = row["usageMode"].as_str().unwrap_or("");
    // 内置官方写法 → 任务
    let by_builtin = |b: &str| -> Option<&'static str> {
        if b == "method.plot" {
            Some("plot")
        } else if b == "method.outline" {
            Some("outline")
        } else if b == "method.body" {
            Some("body")
        } else if b.starts_with("method.humanize")
            || b == "method.humanize.standard"
            || b == "method.humanize.deep"
        {
            Some("humanize")
        } else if b.ends_with("_review")
            || b == "method.consistency_review"
            || b == "method.library_audit"
            || b == "method.opening_review"
        {
            Some("review")
        } else {
            None
        }
    };
    if let Some(t) = by_builtin(builtin) {
        return vec![t.to_string()];
    }
    // 官方名兜底（与前端 Vu/Gu/Mb 对齐）
    let by_name = match name {
        "剧情推演" => Some("plot"),
        "小说细纲生成" => Some("outline"),
        "展开正文写作" | "短篇正文" | "短篇节正文生成" => Some("body"),
        "设定一致性检查"
        | "爽点节奏分析"
        | "资料库体检"
        | "开局诊断"
        | "试读反馈"
        | "短篇爆款体检" => Some("review"),
        "场景描写增强" | "对白润色" | "设定提取" | "拆解手法" => Some("body"),
        _ => None,
    };
    if let Some(t) = by_name {
        return vec![t.to_string()];
    }
    if name.contains("去AI味") || name.contains("去味") || builtin.contains("humanize") {
        return vec!["humanize".to_string()];
    }
    match usage {
        "primary" => vec!["body".to_string()],
        "support" => vec!["body".to_string(), "revise".to_string()],
        _ => {
            if kind == "craft" {
                vec!["body".to_string(), "revise".to_string()]
            } else {
                vec!["chat".to_string()]
            }
        }
    }
}

/// 该技能是否适用于指定任务（不适用就不注入，避免 body 主卡污染 review 协议）。
pub(crate) fn skill_applies_to(row: &Value, task: &str) -> bool {
    if task.is_empty() {
        return true;
    }
    skill_targets(row).iter().any(|t| t == task)
}

/// 技能任务路由（N01 / 契约 C）：
/// 1) 解析并过滤显式技能（启用 + 模板非空 + 本任务适用）；
/// 2) 只有显式列表里存在「usage=primary 且适用本任务」的写法时，才不再叠加书级主技能；
/// 3) 叠加书级主技能与辅助技能，按 id 稳定去重。
///    审核类任务（review/revise）同样遵守，避免自由写作卡污染校验协议。
pub(crate) fn effective_skills(
    db: &molan_core::db::Db,
    book: &str,
    task: &str,
    explicit: &[Value],
) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    fn push(row: Value, out: &mut Vec<Value>, seen: &mut Vec<String>) {
        let id = row["id"].as_str().unwrap_or("").to_string();
        if id.is_empty() || seen.contains(&id) {
            return;
        }
        seen.push(id);
        out.push(row);
    }
    let mut explicit_primary_for_task = false;
    for v in explicit {
        let key = v
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| v["id"].as_str().map(|s| s.to_string()))
            .or_else(|| v["name"].as_str().map(|s| s.to_string()))
            .unwrap_or_default();
        if let Some(row) = resolve_skill_ref(db, &key) {
            // 显式勾选也必须适用于本任务（N01：不能把 body 主卡注入 review）
            if !skill_applies_to(&row, task) {
                tracing::info!(
                    "技能「{}」不适用于任务 {}，本次未注入",
                    row["name"].as_str().unwrap_or(&key),
                    task
                );
                continue;
            }
            if row["usageMode"].as_str() == Some("primary") {
                explicit_primary_for_task = true;
            }
            push(row, &mut out, &mut seen);
        }
    }
    let (primary, supports) = book_task_skill_ids(db, book, task);
    if !primary.is_empty() && !explicit_primary_for_task {
        if let Some(row) = resolve_skill_ref(db, &primary) {
            if skill_applies_to(&row, task) {
                push(row, &mut out, &mut seen);
            }
        }
    }
    for s in supports {
        if let Some(row) = resolve_skill_ref(db, &s) {
            if skill_applies_to(&row, task) {
                push(row, &mut out, &mut seen);
            }
        }
    }
    out
}

/// DeepWrite 书级绑定技能：与 effective_skills 共用同一个 skill_applies_to 任务路由（N01），
/// 绝不把 review/humanize 类卡错发到其它阶段；读取失败降级为空串并告警，不打爆整轮写作。
pub(crate) fn deepwrite_bound_skills(
    db: &molan_core::db::Db,
    book: &str,
    task: &str,
    existing_prompt: &str,
) -> String {
    let rows = match db.q_json(
        "SELECT s.* FROM dw_book_skill b JOIN skills s ON s.id=b.skill_id WHERE b.book_id=?1 AND b.enabled=1 AND s.enabled=1 AND trim(s.prompt_template)<>'' ORDER BY s.name",
        &[&book as &dyn rusqlite::ToSql],
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("DeepWrite 绑定技能读取失败（{}），本阶段不注入", e);
            return String::new();
        }
    };
    let mut out = String::new();
    // 绑定技能总量预算：避免 N 张卡把阶段提示撑爆（token 成本失控）
    let mut budget = 12_000usize;
    for r in rows {
        let row = skill_row(&r);
        if !skill_applies_to(&row, task) {
            continue;
        }
        let tpl = row["promptTemplate"].as_str().unwrap_or("");
        let clipped: String = tpl.chars().take(4_000).collect();
        if clipped.trim().is_empty() || existing_prompt.contains(&clipped) {
            continue;
        }
        let cost = clipped.chars().count() + 16;
        if cost > budget {
            out.push_str("\n- （绑定技能超出总预算，其余未注入）");
            break;
        }
        budget -= cost;
        if out.is_empty() {
            out.push_str("【本书绑定技能】");
        }
        out.push_str(&format!(
            "\n- {}：{}",
            row["name"].as_str().unwrap_or("技能"),
            clipped
        ));
    }
    out
}

/// 资料语义别名（N05）：返回某个语义键对应的**全部**候选文件名（按优先级）。
/// 世界观/人物库与建书档案/人物表语义并不完全等价，因此按多份 canonical 读取并标注出处，
/// 不择一掩盖作者信息，也不移动/删除作者文件。
pub(crate) fn canonical_asset_names(key: &str) -> Vec<&'static str> {
    match key {
        "worldview" => vec!["世界观.md", "世界设定.md", "建书档案.md"],
        "characters" => vec!["人物库.md", "角色库.md", "人物表.md"],
        "synopsis" => vec!["剧情大纲.md", "大纲.md", "建书档案.md"],
        "summary" => vec!["章节摘要.md", "前情摘要.md"],
        "foreshadow" => vec!["伏笔.md", "伏笔记录.md", "伏笔台账.md"],
        _ => Vec::new(),
    }
}

pub(crate) fn prompts(root: &std::path::Path) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(root.join("data").join("prompts.defaults.json"))
            .unwrap_or_default(),
    )
    .unwrap_or(json!({}))
}

/// 技能目录（N04）：只列启用且模板非空的技能；空模板/未完成模板不再出现在目录里。
pub(crate) fn skill_catalog(db: &molan_core::db::Db) -> String {
    let rows = db
        .q_json(
            "SELECT name, description, kind, prompt_template FROM skills WHERE enabled=1 ORDER BY kind, name",
            &[],
        )
        .unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    for r in &rows {
        {
            let name = r["name"].as_str().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            // 空白模板不能执行，也不能进目录（N04）
            if r["promptTemplate"].as_str().unwrap_or("").trim().is_empty() {
                continue;
            }
            let kind = r["kind"].as_str().unwrap_or("");
            let desc = r["description"].as_str().unwrap_or("");
            let tag = match kind {
                "style" => "文风卡",
                "builtin" => "官方写法",
                _ => "技能",
            };
            lines.push(format!(
                "- {}（{}）{}",
                name,
                tag,
                desc.chars().take(30).collect::<String>()
            ));
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "【可用技能与文风目录】
技能广场当前启用的写法与文风卡如下。用户的要求与某项明显对应、或用户点名了某项（如「用XX文风写一段」「按XX推演剧情」）时，直接按该项的方法与腔调执行，无需用户手动勾选：
{}",
        lines.join("
")
    )
}

/// 对话自动路由（N04）：只匹配启用且模板非空的技能；文风卡不当作"技能"注入。
pub(crate) fn auto_match_skills(db: &molan_core::db::Db, message: &str) -> Vec<String> {
    if message.trim().is_empty() {
        return Vec::new();
    }
    let rows = db
        .q_json(
            "SELECT name, kind, prompt_template FROM skills WHERE enabled=1",
            &[],
        )
        .unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for r in &rows {
        {
            let name = r["name"].as_str().unwrap_or("").to_string();
            if name.chars().count() < 2 || out.contains(&name) {
                continue;
            }
            if r["kind"].as_str() == Some("style") {
                continue; // 文风卡走 resolve_book_style，不进技能列表
            }
            if r["promptTemplate"].as_str().unwrap_or("").trim().is_empty() {
                continue; // 空模板技能不得执行（N04）
            }
            // 命中规则：完整卡名出现在消息里；或长卡名（≥4字）取前 4 字匹配
            //（如「全球高武_第1-10章拆解」——用户说「用全球高武文风」也能命中）
            let name4: String = name.chars().take(4).collect();
            let hit =
                message.contains(&name) || (name.chars().count() >= 4 && message.contains(&name4));
            if hit {
                out.push(name);
                if out.len() >= 3 {
                    break;
                }
            }
        }
    }
    out
}

/// 统一书级配置（N14 / 契约 C）：篇制、平台、视角、字数、偏好统一从此读取，
/// 返回 (system 片段, 实际生效值) —— 便于日志/界面显示来源。
/// 全书篇制与单章字数是两个概念，绝不合成一个键。
pub(crate) fn book_config_block(
    db: &molan_core::db::Db,
    root: &std::path::Path,
    book_id: &str,
) -> (String, Value) {
    if book_id.is_empty() {
        return (String::new(), json!({}));
    }
    let p = prompts(root);
    let get = |k: &str| molan_llm::get_setting(db, &format!("{}__{}", k, book_id));
    let mut parts: Vec<String> = Vec::new();
    let form = get("book_form").trim().to_string();
    if !form.is_empty() {
        if let Some(t) = p[format!("form_{}", form)].as_str() {
            if !t.trim().is_empty() {
                parts.push(t.to_string());
            }
        }
    }
    let platform = get("book_platform").trim().to_string();
    if !platform.is_empty() {
        if let Some(t) = p[format!("platform_{}", platform)].as_str() {
            if !t.trim().is_empty() {
                parts.push(t.to_string());
            }
        }
    }
    // 视角：books.pov 是结构化字段（N14）
    let pov = db
        .q_json(
            "SELECT pov FROM books WHERE id=?1",
            &[&book_id as &dyn rusqlite::ToSql],
        )
        .ok()
        .and_then(|v| {
            v.first()
                .and_then(|r| r["pov"].as_str().map(|s| s.to_string()))
        })
        .unwrap_or_default();
    let pov = pov.trim().to_string();
    if !pov.is_empty() && pov != "未设" {
        parts.push(format!(
            "【叙事视角】全书以「{}」叙述，视角纪律贯穿各章。",
            pov
        ));
    }
    let text = if parts.is_empty() {
        String::new()
    } else {
        parts.join("\n")
    };
    let effective = json!({
        "form": if form.is_empty() { Value::Null } else { json!(form) },
        "platform": if platform.is_empty() { Value::Null } else { json!(platform) },
        "pov": if pov.is_empty() { Value::Null } else { json!(pov) },
        "bodyLength": get("book_body_length"),
    });
    (text, effective)
}

// The prompt builder receives independently sourced context dimensions; keeping them
// explicit makes each injection site auditable and avoids an opaque context struct.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_system(
    db: &molan_core::db::Db,
    root: &std::path::Path,
    book_id: &str,
    book_genre: Option<&str>,
    style: Option<&str>,
    skills: &[Value],
    user_msg: &str,
    context: &str,
) -> String {
    let p = prompts(root);
    let mut parts: Vec<String> = Vec::new();
    if let Some(sys) = p["system"].as_str() {
        parts.push(sys.trim().to_string());
    }
    // 题材卡
    if let Some(g) = book_genre {
        let key = genre_key(g);
        if let Some(card) = p[format!("genre__{}", key)].as_str() {
            parts.push(card.to_string());
        }
    }
    // 书级配置：篇制 / 平台 / 视角（N14：统一 helper 读取，显示实际生效值）
    let (cfg, _effective) = book_config_block(db, root, book_id);
    if !cfg.is_empty() {
        parts.push(cfg);
    }
    // 风格卡：带引导语注入；与题材卡同文时跳过（book_style 落到题材 key 时会解析成同一张卡，避免重复注入两遍）
    if let Some(st) = style {
        let st = st.trim();
        if !st.is_empty() {
            let dup = book_genre
                .map(|g| {
                    p[format!("genre__{}", genre_key(g))]
                        .as_str()
                        .map(|c| c.trim() == st)
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if !dup {
                let header = p["book_style_header"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(DEFAULT_BOOK_STYLE_HEADER);
                parts.push(format!("{header}\n{st}"));
            }
        }
    }
    // 技能（N01：只注入启用且模板非空者；顺序为任务方法，不是最高规则）
    for sk in skills {
        let Some(tpl) = sk["promptTemplate"].as_str() else {
            continue;
        };
        if tpl.trim().is_empty() {
            continue;
        }
        let name = sk["name"].as_str().unwrap_or("");
        parts.push(format!("【技能：{}】\n{}", name, tpl));
    }
    // 本书写作偏好（软约束，作者设定优先）
    let prefs = book_prefs_block(db, book_id);
    if !prefs.is_empty() {
        parts.push(prefs);
    }
    // 去 AI 味：作者显式选「无」时不得注入（N02 / 契约 C）
    let humanize = resolve_humanize_method(db, book_id);
    if humanize != "none" {
        if let Some(tone) = p["anti_ai_tone"].as_str() {
            parts.push(tone.to_string());
        }
        // 结构级反AI腔降级为软诊断建议，不再自称"最高优先级硬约束"（N02）
        parts.push(ANTI_AI_STRUCTURE.to_string());
    }
    if !context.is_empty() {
        parts.push(format!("【资料库上下文】\n{}", context));
    }
    if !user_msg.is_empty() {
        parts.push(format!("【用户本轮要求】{}", user_msg));
    }
    parts.join("\n\n")
}

/// 本书去味方法解析（不含提示词）：显式 override > book_humanize__<bookId> > 默认 official:standard。
/// build_system 只关心是否为 none（关闭）。
pub(crate) fn resolve_humanize_method(db: &molan_core::db::Db, book_id: &str) -> String {
    let mut method = String::new();
    if !book_id.is_empty() {
        method = molan_llm::get_setting(db, &format!("book_humanize__{}", book_id));
        if method == "null" {
            method.clear();
        }
    }
    if method.trim().is_empty() {
        "official:standard".to_string()
    } else {
        method.trim().to_string()
    }
}

pub(crate) fn book_prefs_block(db: &molan_core::db::Db, book_id: &str) -> String {
    if book_id.is_empty() {
        return String::new();
    }
    let get = |k: &str| molan_llm::get_setting(db, &format!("{}__{}", k, book_id));
    let mut lines: Vec<String> = Vec::new();
    match get("book_pref_dialogue").as_str() {
        "dense" => lines.push("- 对白：对白流——对白占全章四成以上，信息尽量借人物之口带出，叙述只做必要的场面与动作交代。".into()),
        "narrative" => lines.push("- 对白：叙述流——以叙述、氛围与心理描写为主，对白只在关键处点睛。".into()),
        _ => {}
    }
    match get("book_pref_emotion").as_str() {
        "restrained" => lines.push("- 情绪：克制——情绪藏在动作、细节与留白里，不点破、不直抒胸臆。".into()),
        "intense" => lines.push("- 情绪：浓烈——该燃就燃、该虐下狠手，用具体动作与对白把情绪推到顶点；但不许用形容词直陈情绪。".into()),
        _ => {}
    }
    match get("book_pref_pacing").as_str() {
        "fast" => lines.push("- 节奏：快爆——铺垫压到最短、爽点前置，冲突直给不绕弯。注意：节奏快靠事件密度与信息量，不靠把句子砍短。".into()),
        "slow" => lines.push("- 节奏：慢热——允许整章蓄力铺垫，重悬念与钩子而非即时引爆。".into()),
        _ => {}
    }
    match get("book_pref_style").as_str() {
        "oral" => lines.push("- 描写：直白——场景描写从简、点到为止，节奏优先。".into()),
        "literary" => lines.push("- 描写：文学——进场景、环境与过渡处文学化精描，代入感强；对话打斗仍口语自然、不掉书袋。".into()),
        _ => {}
    }
    match get("book_body_length").as_str() {
        "short" => lines.push("- 单章篇幅：短章，约 2000 字。".into()),
        "long" => lines.push("- 单章篇幅：长章，约 3000 字。".into()),
        "standard" => lines.push("- 单章篇幅：标准章，约 2400 字。".into()),
        _ => {}
    }
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "【本书写作偏好（作者在书卡里设定，必须执行）】\n{}",
        lines.join("\n")
    )
}

pub(crate) const DEFAULT_BOOK_STYLE_HEADER: &str = "【本书文风·目标风格（全程贴住）】下面是你为本书选定的文风与手法档案（作者从自己认可的作品拆解/蒸馏而来）。写作时：句式、用词、对白节奏、叙事手法全程贴住它，重点模仿「仿写范例」的语感、按频率使用白名单词；把它当写作指南——提取可执行的写法落到正文里，不要复述、议论档案内容；档案里出现的其他作品元素（人名、地名、专有设定）不得进入正文。";

/// 结构级反 AI 腔建议（N02）：这是**诊断性软建议**，不是最高优先级硬约束。
/// 当作者选定文风、短句风格或特定体裁与指标冲突时，以作者表达目标为准；
/// 固定的句长/段长配额不得覆盖作者风格选择。
pub(crate) const ANTI_AI_STRUCTURE: &str = "【反AI腔·结构建议（软约束，与作者选定文风冲突时以作者文风为准）】\n\
以下为通用可读性建议，按本章体裁与作者风格酌情采纳；若与【本书文风】或作者明确要求冲突，遵从作者选择，不要机械套用配额。\n\
1. 句长参差：避免通篇句长雷同；紧张处可以短句为主，舒缓处自然拉长。\n\
2. 段落参差：避免每段都被切成两三行；该长则长、该短则短。\n\
3. 禁止金句收尾：段落结尾不要刻意升华、总结或抛漂亮话；该停就停。\n\
4. 叙述要在场：少写复盘式总结，把判断拆进动作、台词与细节。\n\
5. 意象不重复：同一个比喻或意象不要在一章内反复使用。\n\
6. 对白带毛边：允许停顿、抢话、答非所问、半截话与口头语。\n\
7. 允许不完美：真人稿有冗余和口语颗粒，不必句句精炼、段段对称。";

pub(crate) fn has_body_heading(text: &str) -> bool {
    text.lines().any(|l| {
        let t = l.trim();
        if !is_chapter_head(t) {
            return false;
        }
        let title = t.trim_start_matches('#').trim();
        !(title.contains("细纲") || title.contains("大纲"))
    })
}

pub(crate) fn context_text(
    db: &molan_core::db::Db,
    book_id: &str,
    context_files: &Value,
) -> String {
    let Some(list) = context_files.as_array() else {
        return String::new();
    };
    let mut chunks = Vec::new();
    for f in list {
        // 兼容纯字符串条目（runNextChapter 等既有调用方传文件名）：按目录树解析所属组，绝不静默丢弃
        let name = f["name"].as_str().or_else(|| f.as_str()).unwrap_or("");
        let group = f["group"]
            .as_str()
            .or_else(|| f["groupDir"].as_str())
            .map(String::from)
            .or_else(|| stage_context::resolve_file_group(db, book_id, name))
            .unwrap_or_else(|| "设定".to_string());
        if let Some(c) = files::read_file(db, book_id, &group, name) {
            chunks.push(format!(
                "# 资料：{}\n{}",
                name,
                c.chars().take(4000).collect::<String>()
            ));
        }
    }
    chunks.join("\n\n").chars().take(8000).collect()
}

pub(crate) const ARCHIVE_BUDGET: usize = 3500;

pub(crate) fn smart_archive(content: &str, budget: usize) -> String {
    // 分节：以 # 开头的行起新节；首节 = 第一个标题之前的内容
    let mut sections: Vec<(String, String)> = Vec::new(); // (标题行, 正文)
    let mut cur_head = String::from("(开篇)");
    let mut cur_body = String::new();
    for line in content.lines() {
        if line.trim_start().starts_with('#') {
            sections.push((cur_head.clone(), cur_body.clone()));
            cur_head = line.trim().to_string();
            cur_body = String::new();
        } else {
            cur_body.push_str(line);
            cur_body.push('\n');
        }
    }
    sections.push((cur_head, cur_body));

    let key_hit = |head: &str, body: &str| {
        let t = format!("{}{}", head, body);
        ["卷", "框架", "伏笔", "结局", "规划", "后期", "远期", "锚点"]
            .iter()
            .any(|k| t.contains(k))
    };

    let mut used = 0usize;
    let mut picked: Vec<usize> = Vec::new();
    // 必保：开篇节 + 关键节
    for (i, (head, body)) in sections.iter().enumerate() {
        let need = head.chars().count() + body.chars().count();
        if i == 0 || key_hit(head, body) {
            picked.push(i);
            used += need;
        }
        if used > budget {
            break;
        }
    }
    // 剩余预算按顺序补其他节
    if used < budget {
        for (i, (head, body)) in sections.iter().enumerate() {
            if picked.contains(&i) {
                continue;
            }
            let need = head.chars().count() + body.chars().count();
            if used + need > budget {
                continue;
            }
            picked.push(i);
            used += need;
        }
    }
    picked.sort();
    let mut out = String::new();
    for i in picked {
        let (head, body) = &sections[i];
        out.push_str(head);
        out.push('\n');
        let rest = budget.saturating_sub(out.chars().count());
        if rest == 0 {
            break;
        }
        let clipped: String = body.chars().take(rest).collect();
        out.push_str(&clipped);
    }
    if out.chars().count() > budget {
        out.chars().take(budget).collect()
    } else {
        out
    }
}

pub(crate) fn clip_ledger(text: &str, cap: usize) -> String {
    let mut head = String::new();
    let mut pending = String::new();
    let mut others: Vec<&str> = Vec::new();
    for (i, l) in text.lines().enumerate() {
        if i < 2 {
            head.push_str(l);
            head.push('\n');
        } else if l.contains("[待回收") && !l.contains("已回收") {
            pending.push_str(l);
            pending.push('\n');
        } else {
            others.push(l);
        }
    }
    let mut out = head;
    for l in pending.lines() {
        if out.chars().count() + l.chars().count() + 1 > cap {
            break;
        }
        out.push_str(l);
        out.push('\n');
    }
    let mut picked: Vec<&str> = Vec::new();
    for l in others.iter().rev() {
        if out.chars().count() + l.chars().count() + 1 > cap {
            break;
        }
        picked.push(l);
    }
    for l in picked.iter().rev() {
        out.push_str(l);
        out.push('\n');
    }
    out
}

pub(crate) fn clip_people(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let head_budget = cap * 3 / 5;
    let tail_budget = cap.saturating_sub(head_budget).saturating_sub(20);
    let head: String = text.chars().take(head_budget).collect();
    let total = text.chars().count();
    let tail: String = text
        .chars()
        .skip(total.saturating_sub(tail_budget))
        .collect();
    format!("{}\n…（中段略）…\n{}", head, tail)
}

pub(crate) fn requested_chapter(message: &str) -> Option<i64> {
    let chars: Vec<char> = message.chars().collect();
    let mut hits: Vec<(usize, usize, i64)> = Vec::new(); // (第 的位置, 章 的位置, 章号)
    for i in 0..chars.len() {
        if chars[i] == '第' {
            let slice: String = chars[i..].iter().collect();
            if let Some(n) = chapter_num_from_name(&slice) {
                let zhang = chars[i..]
                    .iter()
                    .position(|c| *c == '章')
                    .map(|p| i + p)
                    .unwrap_or(i);
                hits.push((i, zhang, n));
            }
        }
    }
    if hits.is_empty() {
        return None;
    }
    // 目标章判定（回归教训："请接着已定稿第6章写第7章正文" 不能取到 6）：
    // 引用（"接着第6章"）与对象（"写第7章正文"）都会出现，必须按信号强度分层，
    // 强信号先扫全部命中，弱信号后补；全部无信号才取最后一个。
    const VERBS: &[char] = &['写', '续', '补', '改', '生'];
    let near = |c: &char| !c.is_whitespace() && !matches!(c, '，' | '、' | '。' | '：');
    // 1（最强）：「第N章」后紧跟「正文」
    for &(_, zhang, n) in &hits {
        let rest: Vec<char> = chars[zhang + 1..]
            .iter()
            .copied()
            .filter(near)
            .take(2)
            .collect();
        if rest.starts_with(&['正', '文']) {
            return Some(n);
        }
    }
    // 2：动词紧贴「第N章」前（写第7章 / 续写第7章）
    for &(i, _, n) in &hits {
        let before: Vec<char> = chars[..i]
            .iter()
            .copied()
            .rev()
            .filter(near)
            .take(2)
            .collect();
        if before.iter().any(|c| VERBS.contains(c)) {
            return Some(n);
        }
    }
    // 3：动词紧贴「第N章」后（把第3章重写一下）——注意"第6章写第7章"里
    //    第6 后也有写，但 1、2 已先扫过，走不到这；真到这说明前面没有更强目标。
    for &(_, zhang, n) in &hits {
        let after: Vec<char> = chars[zhang + 1..]
            .iter()
            .copied()
            .filter(near)
            .take(2)
            .collect();
        if after.iter().any(|c| VERBS.contains(c)) {
            return Some(n);
        }
    }
    // 兜底：取最后一个（"第2章和第4章对不上"这类讨论不是写作指令，取哪个都安全）
    hits.last().map(|&(_, _, n)| n)
}

/// 兼容入口：从自然语言消息里取目标章（不可靠，仅用于旧调用点）。
pub(crate) fn auto_book_context(db: &molan_core::db::Db, book_id: &str, message: &str) -> String {
    let target = requested_chapter(message);
    auto_book_context_for_chapter(db, book_id, target.unwrap_or(0), false)
}

/// 目标章上下文（契约 C / N09 / N12）：
/// - target_ch > 0：只用**目标章之前**的正式稿与记忆，绝不引用未来章（补章不读未来）；
/// - 全局设定（建书档案/世界观）按多份 canonical 并读、标出处，超总预算计 omitted；
/// - 作者人物资产标注"全书范围，可能含未来"；派生摘要（前情摘要/章节摘要）不自动注入；
/// - 叠加父代理 continuity::chapter_context 的已定稿记忆（唯一带 sourceHash 的时点事实），资料不足明确标注；
/// - include_pending=true **不自行读取待审稿**：缺可验证同任务标识时只给提示，由任务侧校验 taskId 后显式追加。
pub(crate) fn auto_book_context_for_chapter(
    db: &molan_core::db::Db,
    book_id: &str,
    target_ch: i64,
    include_pending: bool,
) -> String {
    if book_id.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();

    // 作者静态资产总预算：整块条目只进不出，超预算的条目明确计数 omitted，不静默丢。
    const ASSET_BUDGET: usize = 12000;
    let mut asset_used = 0usize;
    let mut asset_omitted = 0usize;
    let push_asset =
        |parts: &mut Vec<String>, body: String, used: &mut usize, omitted: &mut usize| {
            let n = body.chars().count();
            if *used + n > ASSET_BUDGET {
                *omitted += 1;
                return;
            }
            *used += n;
            parts.push(body);
        };
    // L0 作者总纲：高于模型自拟剧情、自动生成细纲；仅取明确命名的作者资料，尊重 aiOff。
    // 常见上传位置既有「设定」也有「细纲」，不能只认细纲_第N章.md。
    let tree = files::scan_tree(db, book_id);
    let mut plan_seen = std::collections::HashSet::new();
    let mut plan_used = 0usize;
    let mut plan_omitted = 0usize;
    for group in ["设定", "细纲"] {
        let Some(g) = tree
            .as_array()
            .and_then(|arr| arr.iter().find(|g| g["dir"] == group))
        else {
            continue;
        };
        let Some(entries) = g["files"].as_array() else {
            continue;
        };
        let mut names: Vec<&str> = entries
            .iter()
            .filter_map(|f| f["name"].as_str())
            .filter(|name| {
                (name.ends_with(".md") || name.ends_with(".txt"))
                    && [
                        "总纲",
                        "剧情大纲",
                        "故事大纲",
                        "全书大纲",
                        "章节规划",
                        "章节大纲",
                        "大纲",
                    ]
                    .iter()
                    .any(|key| name.contains(key))
                    && chapter_num_from_name(name).is_none()
            })
            .collect();
        names.sort_unstable();
        for name in names {
            if !plan_seen.insert((group, name))
                || files::file_flag(db, book_id, group, name, "aiOff")
            {
                continue;
            }
            let Some(content) = files::read_file(db, book_id, group, name) else {
                continue;
            };
            if content.trim().is_empty() {
                continue;
            }
            let (body, cut) = clip_complete(&content, 4200);
            let block = format!(
                "【作者总纲·{}/{}（优先于自动规划；不得擅改主线，未写明情节先询问作者）】\n{}{}",
                group,
                name,
                body,
                if cut > 0 {
                    format!(
                        "\n（总纲尚有 {} 字未注入；请提示作者引用完整文件，不能凭猜测续写）",
                        cut
                    )
                } else {
                    String::new()
                }
            );
            if plan_used + block.chars().count() > 9000 {
                plan_omitted += 1;
                continue;
            }
            plan_used += block.chars().count();
            parts.push(block);
        }
    }
    if plan_omitted > 0 {
        parts.push(format!(
            "【作者总纲未注入】另有 {} 份总纲超过预算；请先请作者选定或引用，不得自行补写情节。",
            plan_omitted
        ));
    }
    // L0 作者静态全局设定（建书档案/世界观）：**不随时间失效**，可全局注入。
    // 多份 canonical 名称全部并读并标出处，不再只取首份（N05）。
    for name in canonical_asset_names("worldview") {
        if files::file_flag(db, book_id, "设定", name, "aiOff") {
            continue;
        }
        let text = files::read_file(db, book_id, "设定", name).unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }
        let label = if name.contains("建书") {
            "建书档案"
        } else {
            "世界观"
        };
        push_asset(
            &mut parts,
            format!(
                "【全局设定·{}（{}）】\n{}",
                label,
                name,
                smart_archive(&text, ARCHIVE_BUDGET)
            ),
            &mut asset_used,
            &mut asset_omitted,
        );
    }
    // L1 作者维护人物资产（人物表/人物库）：作者拥有修改权，但**可能含全书范围信息**，
    // 不保证"截至本章"，因此明确标注范围；本章时点事实以已定稿章节记忆为准。
    for name in canonical_asset_names("characters") {
        if files::file_flag(db, book_id, "设定", name, "aiOff") {
            continue;
        }
        let text = files::read_file(db, book_id, "设定", name).unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }
        push_asset(
            &mut parts,
            format!(
                "【作者人物资产·{}（全书范围，可能含未来状态；本章时点事实以下方已定稿记忆为准）】\n{}",
                name,
                clip_people(&text, 3200)
            ),
            &mut asset_used,
            &mut asset_omitted,
        );
    }
    // 派生摘要（前情摘要/章节摘要）**不自动注入**：它按最大章号滚动，可能覆盖目标章之后的内容，
    // 且没有 sourceHash，补早期章时不能当作"截至本章的事实"。需要时由作者显式勾选引用。
    for name in canonical_asset_names("summary") {
        if files::file_flag(db, book_id, "设定", name, "aiOff") {
            continue;
        }
        let exists = !files::read_file(db, book_id, "设定", name)
            .unwrap_or_default()
            .trim()
            .is_empty();
        if exists {
            parts.push(format!(
                "【未自动注入·{}】该滚动摘要是派生信息、可能包含第{}章之后的内容，且无来源指纹，故不作为本章事实。如需引用请在资料库里手动勾选。",
                name, target_ch
            ));
        }
    }
    // L1 伏笔台账（作者维护，含待回收标记）：未回收的不许提前揭底，已回收的不许重复使用
    for name in canonical_asset_names("foreshadow") {
        if files::file_flag(db, book_id, "设定", name, "aiOff") {
            continue;
        }
        let text = files::read_file(db, book_id, "设定", name).unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }
        push_asset(
            &mut parts,
            format!(
                "【伏笔台账（未回收的不要提前揭底）·{}】\n{}",
                name,
                clip_ledger(&text, 2600)
            ),
            &mut asset_used,
            &mut asset_omitted,
        );
    }
    // 伏笔滞留提醒（A1）：埋了很久没推进的伏笔，提醒本章安排侧写强化或回收（只提醒不强制）
    if let Ok(alerts) = molan_core::facts::thread_alerts(db, book_id, 0) {
        let overdue: Vec<&Value> = alerts["alerts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| a["overdue"].as_bool() == Some(true))
            .take(6)
            .collect();
        if !overdue.is_empty() {
            let lines: Vec<String> = overdue
                .iter()
                .map(|a| {
                    format!(
                        "- {}（埋于第{}章，已{}章未推进）",
                        a["threadId"].as_str().unwrap_or(""),
                        a["lastChapter"].as_i64().unwrap_or(0),
                        a["age"].as_i64().unwrap_or(0)
                    )
                })
                .collect();
            parts.push(format!(
                "【伏笔提醒】以下伏笔长期未推进，如合适请在本章安排一次侧写强化或回收（绑定在器物/动作上，不要生硬口播）：\n{}",
                lines.join("\n")
            ));
        }
    }
    // 信息差账本（A2）：谁知道什么秘密。角色严禁表现出自己不在知情列表里的信息。
    if let Ok(facts) = molan_core::facts::list_facts(db, book_id, "", "current", 300) {
        let mut lines: Vec<String> = Vec::new();
        for f in facts["facts"].as_array().into_iter().flatten() {
            if f["subjectType"].as_str() != Some("secret") {
                continue;
            }
            let val: Value =
                serde_json::from_str(f["value"].as_str().unwrap_or("")).unwrap_or(Value::Null);
            let join = |k: &str| {
                val[k]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str())
                            .collect::<Vec<_>>()
                            .join("、")
                    })
                    .unwrap_or_default()
            };
            let known = join("known_by");
            let unknown = join("unknown_to");
            lines.push(format!(
                "- {}｜知情：{}｜不知情：{}",
                val["fact"].as_str().unwrap_or(""),
                if known.is_empty() {
                    "（未标注）"
                } else {
                    &known
                },
                if unknown.is_empty() {
                    "（其余人）"
                } else {
                    &unknown
                }
            ));
            if lines.len() >= 8 {
                break;
            }
        }
        if !lines.is_empty() {
            parts.push(format!(
                "【信息差账本】角色不得泄露或预判自己不知情的秘密：\n{}",
                lines.join("\n")
            ));
        }
    }
    // 已定稿记忆（父代理 continuity）：这是**唯一**带 sourceHash 的时点事实来源，
    // 覆盖目标章之前的有效 memory；资料不足必须明确标注，不得假装完整。
    // 未点名目标章（"接着写下一章"）时按最新正式章+1 定位，否则定稿记忆整段丢失（P1-3）。
    let eff_target = if target_ch > 0 {
        target_ch
    } else {
        let mut m = 0i64;
        if let Some(arr) = tree.as_array() {
            for g in arr {
                if g["dir"].as_str() != Some("正文") {
                    continue;
                }
                if let Some(fs) = g["files"].as_array() {
                    for f in fs {
                        if let Some(n) = chapter_num_from_name(f["name"].as_str().unwrap_or("")) {
                            m = m.max(n);
                        }
                    }
                }
            }
        }
        m + 1
    };
    if eff_target > 1 {
        match molan_core::continuity::chapter_context(db, book_id, eff_target) {
            Ok(cx) => {
                let text = cx["text"].as_str().unwrap_or("");
                if !text.trim().is_empty() {
                    parts.push(format!(
                        "【已定稿章节记忆（截至第{}章）】\n{}",
                        eff_target - 1,
                        text
                    ));
                }
                if cx["complete"].as_bool() != Some(true) {
                    let missing = cx["missingCount"].as_i64().unwrap_or(0);
                    let stale = cx["stale"].as_array().map(|a| a.len()).unwrap_or(0);
                    let hidden = cx["hidden"].as_array().map(|a| a.len()).unwrap_or(0);
                    let omitted = cx["omittedEntries"].as_i64().unwrap_or(0);
                    parts.push(format!(
                        "【记忆覆盖提示】本章之前的记忆不完整（缺失{}章、过期{}章、隐藏{}章、超预算{}条）。缺失部分不得凭猜测补写；如需依赖，请先按章重建记忆。",
                        missing, stale, hidden, omitted
                    ));
                }
            }
            Err(e) => {
                parts.push(format!(
                    "【记忆覆盖提示】已定稿记忆读取失败（{}）。不得凭猜测补写缺失事实。",
                    e
                ));
            }
        }
    }
    if asset_omitted > 0 {
        parts.push(format!(
            "【资产覆盖提示】作者资料超出本次预算，有 {} 份未注入；不得据此宣称已核对全部设定。",
            asset_omitted
        ));
    }

    // 最近正文：只在**目标章之前**的正式稿里取（N09：补章不读未来）
    let tree = files::scan_tree(db, book_id);
    let upper = if target_ch > 0 {
        target_ch - 1
    } else {
        i64::MAX
    };
    let mut latest: Option<(String, i64)> = None;
    if let Some(arr) = tree.as_array() {
        for g in arr {
            if g["dir"].as_str() != Some("正文") {
                continue;
            }
            if let Some(fs) = g["files"].as_array() {
                for f in fs {
                    let name = f["name"].as_str().unwrap_or("");
                    if let Some(n) = chapter_num_from_name(name) {
                        if n > upper {
                            continue; // 未来章不得作为续写锚点
                        }
                        if files::file_flag(db, book_id, "正文", name, "aiOff") {
                            continue;
                        }
                        if latest.as_ref().map(|(_, m)| n > *m).unwrap_or(true) {
                            latest = Some((name.to_string(), n));
                        }
                    }
                }
            }
        }
    }
    if let Some((name, num)) = latest.clone() {
        if let Some(c) = files::read_file(db, book_id, "正文", &name) {
            let tail: String = {
                let total = c.chars().count();
                c.chars().skip(total.saturating_sub(1200)).collect()
            };
            parts.push(format!(
                "【最近正文·第{}章结尾（续写需自然衔接）】\n{}",
                num, tail
            ));
        }
        // 前一章结尾（400 字）：覆盖滚动摘要未及的中段
        if num >= 2 {
            let prev_name = format!("第{}章.md", num - 1);
            if let Some(pc) = files::read_file(db, book_id, "正文", &prev_name) {
                let ptail: String = {
                    let t = pc.chars().count();
                    pc.chars().skip(t.saturating_sub(400)).collect()
                };
                if !ptail.trim().is_empty() {
                    parts.push(format!(
                        "【第{}章结尾（衔接参考）】
{}",
                        num - 1,
                        ptail
                    ));
                }
            }
        }
    }
    // 同任务待审草稿承接（F10 / N09）：**保守策略**——本函数不自行读取任何待审稿。
    // 原因：pending_chapter 没有 task 标识，continuity::check_draft_dependency 只校验父章 hash/失效，
    // 不能作为"同一任务"的证据；缺明确 task 身份就读待审稿会把别的任务草稿当成本任务前情。
    // 因此 include_pending=true 时只给提示，由调用方（B）用自己持有的 taskId 校验后显式追加；
    // 本函数绝不"假装校验过"。
    if include_pending && target_ch >= 2 {
        let prev = format!("第{}章.md", target_ch - 1);
        let has_pending = !files::read_file(db, book_id, molan_core::db::REVIEW_GROUP, &prev)
            .unwrap_or_default()
            .trim()
            .is_empty();
        if has_pending {
            parts.push(format!(
                "【待审承接提示】第{}章存在待审草稿；本上下文未自动读取它（缺少可验证的同任务标识）。若确属本次连写任务，请由任务侧校验 taskId 后显式追加该草稿，并标注为未审批临时依赖。",
                target_ch - 1
            ));
        }
    }

    // 细纲类文件统一遵守 aiOff（自动引用必须过滤作者隐藏项；显式引用走 context_text）。
    let read_outline = |name: &str| -> Option<String> {
        if files::file_flag(db, book_id, "细纲", name, "aiOff") {
            return None;
        }
        let c = files::read_file(db, book_id, "细纲", name)?;
        if c.trim().is_empty() {
            None
        } else {
            Some(c)
        }
    };

    // L0.5 分卷细纲/卷纲（长篇方向锚点，防止写到后面跑偏）
    if let Some(arr) = tree.as_array() {
        for g in arr {
            if g["dir"].as_str() != Some("细纲") {
                continue;
            }
            if let Some(fs) = g["files"].as_array() {
                for f in fs {
                    let name = f["name"].as_str().unwrap_or("");
                    if name.contains('卷') {
                        if let Some(c) = read_outline(name) {
                            let (body, cut) = clip_complete(&c, 3000);
                            let mut block = format!("【分卷细纲·{}】\n{}", name, body);
                            if cut > 0 {
                                block.push_str(&format!(
                                    "\n（该卷纲超出预算，尚有 {} 字未注入；如需完整方向请查阅原文）",
                                    cut
                                ));
                            }
                            parts.push(block);
                        }
                    }
                }
            }
        }
    }

    // L2 本章细纲自动注入（N08/N09）：目标章由结构化参数给出，不再从自然语言猜。
    // 规则：目标章有细纲就只用目标章细纲（并标注"必须逐条落实"）；
    //       目标章没有时才退到下一章细纲，且必须用"参考"措辞，绝不要求逐条落实（避免误章）。
    let next_ch = latest.as_ref().map(|(_, n)| n + 1).unwrap_or(1);
    let want = if target_ch > 0 { Some(target_ch) } else { None };
    let mut injected_any = false;
    // 先试目标章（唯一权威）
    if let Some(ch) = want {
        let fname = format!("细纲_第{}章.md", ch);
        if let Some(c) = read_outline(&fname) {
            let (body, cut) = clip_complete(&c, 1800);
            let mut block = format!(
                "【本章细纲·第{}章（作者总纲 > 本章细纲 > 模型自拟；以下节拍必须落实，若与总纲冲突则暂停并询问作者；正文第一行必须是「第{}章 章名」）】\n{}",
                ch, ch, body
            );
            if cut > 0 {
                block.push_str(&format!(
                    "\n（本章细纲超出预算，尚有 {} 字未注入；动笔前请先查阅完整细纲，勿凭猜测补写）",
                    cut
                ));
            }
            parts.push(block);
            injected_any = true;
        }
    }
    // 仅在**未指定目标章**时才退到 next_ch 参考细纲（仅方向参考，不强制逐条落实）。
    // 已指定目标章时只认目标章：目标章无细纲就不注入任何其它章细纲，改为提醒，
    // 绝不把无关章的细纲塞进本章（会误章）。
    if !injected_any && want.is_none() {
        let ref_ch = next_ch;
        let fname = format!("细纲_第{}章.md", ref_ch);
        if let Some(c) = read_outline(&fname) {
            let (body, cut) = clip_complete(&c, 1800);
            let mut block = format!(
                "【参考细纲·第{}章（仅供把握走向，不是本章规定内容；若与本章目标冲突，以本章目标为准，不得照搬）】\n{}",
                ref_ch, body
            );
            if cut > 0 {
                block.push_str(&format!("\n（该参考细纲超出预算，尚有 {} 字未注入）", cut));
            }
            parts.push(block);
            injected_any = true;
        }
    }
    if !injected_any {
        parts.push(format!(
            "【待作者确认】第{}章尚无经确认的本章细纲；即使有作者总纲也不能自行编排该章情节。先根据总纲提出本章走向选项（目标/冲突/钩子），等待作者选择后再动笔。",
            want.unwrap_or(next_ch)
        ));
    }

    // 长篇一致性红线（防止自动连写跑偏、胡说）
    parts.push(
        "【长篇一致性红线（违反即算失败）】\n\
- 只写本章细纲规定的节拍，不提前推进后续章节的剧情，不提前揭底未回收的伏笔。\n\
- 不新增档案/人物表里没有的重要人物、门派、地名、道具、能力；细纲明确要求的新元素除外。\n\
- 人物姓名、身份、称呼、能力、伤势、所在位置必须与人物表、伏笔台账、上一章结尾一致。\n\
- 时间线从上一章结尾的时点接着走；不复述前文，不重写已经发生的事。\n\
- 上一章已解决的冲突不重复解决；已回收的伏笔不再使用。\n\
- 拿不准的设定，宁可留白也不要自己编。"
            .to_string(),
    );

    if parts.is_empty() {
        return String::new();
    }
    format!(
        "【本书设定 · 系统自动注入，写作必须与以下设定保持一致】\n{}",
        parts.join("\n\n")
    )
}

pub(crate) fn extract_json_block(text: &str) -> Option<Value> {
    let i = text.find("```json")?;
    let rest = &text[i + 7..];
    let end = rest.find("```")?;
    serde_json::from_str::<Value>(rest[..end].trim()).ok()
}

/// AI 产稿写盘（契约 C）：走 files::write_ai_file——锁定文件拒绝、目标已存在拒绝、
/// 正式/待审交叉保护，绝不给普通聊天留隐式绕过审批的通道。
pub(crate) async fn write_ai_file_async(
    st: &Arc<AppState>,
    book_id: &str,
    group: &str,
    name: &str,
    content: &str,
) -> Result<()> {
    let st2 = Arc::clone(st);
    let (b, g, n, c) = (
        book_id.to_string(),
        group.to_string(),
        name.to_string(),
        content.to_string(),
    );
    tokio::task::spawn_blocking(move || files::write_ai_file(&st2.db, &b, &g, &n, &c))
        .await
        .map_err(|e| anyhow!("AI 写盘任务异常（{}/{}）：{}", group, name, e))?
}

pub async fn dispatch_stream(
    st: &Arc<AppState>,
    cmd: &str,
    args: &Value,
    tx: &tokio::sync::mpsc::Sender<String>,
) -> Result<Option<Value>> {
    let db = &st.db;
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();

    match cmd {
        // ============ 自动写作引擎（start/续跑断点/最近任务） ============
        "auto_write_start" => return auto_write::auto_write_start(st, cmd, args, tx).await,
        "auto_write_resume" => return auto_write::auto_write_resume(st, cmd, args, tx).await,
        "auto_write_last_task" => return auto_write::auto_write_last_task(st, cmd, args, tx).await,
        // Agent 对话（工具循环，独立于 chat_stream）
        "agent_turn" => return agent_loop::agent_turn(st, cmd, args, tx).await,
        // ============ 多智能体分工 ============
        "get_agent_profiles" => {
            let (channels, active_id) = molan_llm::all_settings(db);
            let chs: Vec<Value> = channels
                .iter()
                .filter(|c| c["builtin"] != json!(true))
                .map(|c| {
                    json!({
                        "id": c["id"].as_str().unwrap_or(""),
                        "label": c["label"].as_str().unwrap_or(""),
                        "model": c["model"].as_str().unwrap_or(""),
                    })
                })
                .collect();
            Ok(Some(json!({
                "profiles": molan_llm::all_agent_profiles(db),
                "channels": chs,
                "activeChannelId": active_id,
            })))
        }
        "set_agent_profile" => {
            let task = s("task");
            if !["distill", "outline", "chapter", "review", "summary"].contains(&task.as_str()) {
                return Err(anyhow!("未知任务角色：{}", task));
            }
            let profile = json!({
                "channelId": s("channelId"),
                "model": s("model"),
            });
            db.exec(
                "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                &[
                    &format!("agent_profile__{}", task) as &dyn rusqlite::ToSql,
                    &profile.to_string(),
                ],
            )?;
            Ok(Some(json!({"ok": true})))
        }
        "book_dl_start" => return sources::book_dl_start(st, cmd, args, tx).await,
        "book_dl_status" => return sources::book_dl_status(st, cmd, args, tx).await,
        "book_dl_stop" => return sources::book_dl_stop(st, cmd, args, tx).await,
        "book_dl_get" => return sources::book_dl_get(st, cmd, args, tx).await,
        "auto_write_status" => return auto_write::auto_write_status(st, cmd, args, tx).await,
        "auto_write_stop" => return auto_write::auto_write_stop(st, cmd, args, tx).await,
        "chat_stream" => return chat::chat_stream(st, cmd, args, tx).await, // 流式对话
        "distill_style_stream" | "redistill_book_style" => {
            return decompose::distill_style_stream(st, cmd, args, tx).await; // 风格蒸馏（流式）
        }
        "gen_style_sample" => return decompose::gen_style_sample(st, cmd, args, tx).await, // 风格样例（一次性）
        "import_analyze" => return decompose::import_analyze(st, cmd, args, tx).await, // 导入通读分析（事件流4步）
        // ============ 会话标题生成 ============
        "gen_session_title" => return decompose::gen_session_title(st, cmd, args, tx).await,
        // ============ 小说拆解（流式：单段直出 / 分段 map 后 reduce 汇总） ============
        "decompose_novel" => return decompose::decompose_novel(st, cmd, args, tx).await,
        // ============ 场景正文生成（「写第1章/展开正文」流式步骤） ============
        "gen_scene_body" => return decompose::gen_scene_body(st, cmd, args, tx).await,
        // ============ 编辑器内联助手（改写/润色 与 与作者对话） ============
        "inline_chat" | "inline_assist" => return chat::inline_chat(st, cmd, args, tx).await,
        _ => Err(anyhow!("未知命令 {}", cmd)),
    }
}

// ---- 对外入口再导出（handlers/mod.rs 以 stream::xxx 路径访问） ----
#[allow(unused_imports)]
pub(crate) use auto_write::{
    auto_humanize, auto_log, auto_log_clear, auto_logs_for, genre_key, latest_chapter_num,
    resolve_book_style, save_auto_message, set_auto_progress,
};
#[allow(unused_imports)]
pub use auto_write::{latest_auto_task, persist_auto_task, recover_interrupted_auto_tasks};
pub use chat::request_abort;

mod auto_write;
// B: 按「实际批准章 + hash」事件化写入正式记忆（F 审批成功 / rebuild_memory 调用；幂等）
pub(crate) use auto_write::post_approved_chapter;
pub(crate) mod agent_loop;
mod agent_tools;
pub(crate) mod chat;
mod decompose;
pub(crate) mod fallback;
mod sources;
pub(crate) mod stage_context;

#[cfg(test)]
mod helper_tests {
    use super::*;
    use serde_json::json;

    // 线上回归：生产 panic「start byte index 1 is not a char boundary」——
    // 「第」是 3 字节，任何以 第N章 开头的行都会打崩旧实现的字节切片。
    #[test]
    fn chapter_head_parses_multibyte_titles_without_panic() {
        // 线上炸掉的原标题，逐字节复刻
        assert!(is_chapter_head("第1章 捡回来的魔尊"));
        assert_eq!(parse_chapter_title("第1章 捡回来的魔尊"), Some((1, false)));
        assert!(is_chapter_head("# 第一章 雪夜来访"));
        assert_eq!(parse_chapter_title("# 第一章 雪夜来访"), Some((1, false)));
        assert!(is_chapter_head("第12章 试炼"));
        assert_eq!(parse_chapter_title("第12章 试炼"), Some((12, false)));
        assert!(is_chapter_head("# 第3章 细纲"));
        assert_eq!(parse_chapter_title("# 第3章 细纲"), Some((3, true)));
    }

    #[test]
    fn chapter_head_rejects_non_titles() {
        assert!(!is_chapter_head("他翻到第三章"));
        assert!(!is_chapter_head(
            "第1章这行标题故意加长超过二十四个字符上限所以必须被拒绝掉"
        ));
        assert!(!is_chapter_head(""));
        assert!(!is_chapter_head("第章"));
    }

    // 线上回归：「请接着已定稿第6章写第7章正文」曾被取目标章为 6，
    // 导致注入的是第 6 章上下文、模型以「没有第6章内容」为由拒写。
    #[test]
    fn requested_chapter_prefers_write_target_over_reference() {
        assert_eq!(
            requested_chapter("请接着已定稿第6章写第7章正文，写完即止"),
            Some(7)
        );
        assert_eq!(requested_chapter("请写第7章"), Some(7));
        assert_eq!(
            requested_chapter("把第3章重写一下，参照第5章的风格"),
            Some(3)
        );
        assert_eq!(requested_chapter("续写第八章，别理第2章的旧稿"), Some(8));
        // 无意图线索的讨论句：取最后一个即可（调用方另有 is_body_cmd 门槛，不会误落盘）
        assert_eq!(requested_chapter("第2章和第4章的节奏对不上"), Some(4));
        assert_eq!(requested_chapter("今天天气不错"), None);
    }

    /// S2 回归锁：细纲指令不写正文；明确正文任务只入待审，作者接受后才转正。
    #[test]
    fn suppress_body_never_writes_body_group() {
        let dir = tempfile::tempdir().unwrap();
        let db = molan_core::db::Db::open(dir.path(), None).unwrap();
        let book = molan_core::books::create_book(&db, "t", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        let full = format!("# 第2章 正文产出段\n\n{}\n", "正".repeat(400));
        let saved =
            auto_save_chat_output_checked(&db, &book, &full, Some(2), true, || Ok(())).unwrap();
        assert!(saved.0.is_empty(), "suppress_body 不得落盘：{:?}", saved);
        assert!(
            molan_core::files::read_file(&db, &book, "正文", "第2章.md").is_none(),
            "正文目录不得出现该章"
        );
        let saved2 =
            auto_save_chat_output_checked(&db, &book, &full, Some(2), false, || Ok(())).unwrap();
        assert!(saved2
            .0
            .iter()
            .any(|s| s.starts_with("正文待审 / 第2章.md")));
        assert!(molan_core::files::read_file(&db, &book, "正文", "第2章.md").is_none());
        assert!(molan_core::files::read_file(&db, &book, "正文待审", "第2章.md").is_some());
        assert!(db
            .q_json(
                "SELECT ch FROM pending_chapter WHERE book_id=?",
                &[&book as &dyn rusqlite::ToSql]
            )
            .unwrap()
            .iter()
            .any(|row| row["ch"] == 2));
    }

    fn fixture() -> (tempfile::TempDir, molan_core::db::Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = molan_core::db::Db::open(dir.path(), None).unwrap();
        let book = molan_core::books::create_book(&db, "helper-test", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        (dir, db, book)
    }

    fn add_skill(
        db: &molan_core::db::Db,
        name: &str,
        builtin: &str,
        usage: &str,
        targets: &str,
        tpl: &str,
        enabled: i64,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        db.exec(
            "INSERT INTO skills(id,name,description,prompt_template,kind,source,enabled,builtin_key,usage_mode,targets_json,origin)
             VALUES(?1,?2,'',?3,'method','',?4,?5,?6,?7,'user')",
            &[
                &id as &dyn rusqlite::ToSql,
                &name,
                &tpl,
                &enabled,
                &builtin,
                &usage,
                &targets,
            ],
        )
        .unwrap();
        id
    }

    /// 显式 primary 优先：显式列表里有本任务 primary 时，书级主技能不覆盖它。
    #[test]
    fn explicit_primary_wins_over_book_primary() {
        let (_d, db, book) = fixture();
        let book_primary = add_skill(
            &db,
            "书级正文写法",
            "method.body",
            "primary",
            "[]",
            "BOOK_PRIMARY",
            1,
        );
        let explicit = add_skill(
            &db,
            "显式正文写法",
            "",
            "primary",
            "[]",
            "EXPLICIT_PRIMARY",
            1,
        );
        db.exec(
            "INSERT INTO settings(key,value) VALUES(?1,?2)",
            &[
                &format!("book_primary_skill__{}__body", book) as &dyn rusqlite::ToSql,
                &book_primary,
            ],
        )
        .unwrap();
        let got = effective_skills(&db, &book, "body", &[json!(explicit)]);
        let names: Vec<&str> = got.iter().filter_map(|r| r["name"].as_str()).collect();
        assert!(
            names.contains(&"显式正文写法"),
            "显式主技能必须保留：{:?}",
            names
        );
        assert!(
            !names.contains(&"书级正文写法"),
            "显式 primary 存在时不得再叠加书级主技能：{:?}",
            names
        );
    }

    /// 书级主技能 + 辅助技能在无显式时被加载，且稳定去重。
    #[test]
    fn book_primary_and_support_are_loaded_and_deduped() {
        let (_d, db, book) = fixture();
        let primary = add_skill(&db, "书级主卡", "method.body", "primary", "[]", "P", 1);
        let support = add_skill(
            &db,
            "叠加技巧",
            "",
            "support",
            "[\"body\",\"revise\"]",
            "S",
            1,
        );
        db.exec(
            "INSERT INTO settings(key,value) VALUES(?1,?2),(?3,?4)",
            &[
                &format!("book_primary_skill__{}__body", book) as &dyn rusqlite::ToSql,
                &primary,
                &format!("book_support_skills__{}__body", book) as &dyn rusqlite::ToSql,
                &format!("[\"{}\",\"{}\"]", support, support),
            ],
        )
        .unwrap();
        let got = effective_skills(&db, &book, "body", &[]);
        assert_eq!(got.len(), 2, "主卡+辅助各一次，重复 id 去重：{:?}", got);
        let ids: Vec<&str> = got.iter().filter_map(|r| r["id"].as_str()).collect();
        assert_eq!(ids.iter().filter(|x| **x == support).count(), 1);
    }

    /// 空模板 / 已停用技能不得注入。
    #[test]
    fn empty_and_disabled_skills_are_skipped() {
        let (_d, db, book) = fixture();
        let empty = add_skill(&db, "空技能", "", "primary", "[]", "   ", 1);
        let disabled = add_skill(&db, "停用技能", "", "primary", "[]", "DISABLED_TPL", 0);
        db.exec(
            "INSERT INTO settings(key,value) VALUES(?1,?2)",
            &[
                &format!("book_primary_skill__{}__body", book) as &dyn rusqlite::ToSql,
                &empty,
            ],
        )
        .unwrap();
        let got = effective_skills(&db, &book, "body", &[json!(disabled), json!(empty)]);
        assert!(got.is_empty(), "空模板与停用技能都不得注入：{:?}", got);
    }

    /// 任务路由：body 主卡不得注入 review 任务。
    #[test]
    fn body_primary_is_not_injected_into_review() {
        let (_d, db, book) = fixture();
        let body_primary = add_skill(
            &db,
            "正文写法",
            "method.body",
            "primary",
            "[]",
            "BODY_TPL",
            1,
        );
        db.exec(
            "INSERT INTO settings(key,value) VALUES(?1,?2)",
            &[
                &format!("book_primary_skill__{}__body", book) as &dyn rusqlite::ToSql,
                &body_primary,
            ],
        )
        .unwrap();
        // review 任务：显式传入也不得注入（不适用本任务）
        let got = effective_skills(&db, &book, "review", &[json!(body_primary)]);
        assert!(got.is_empty(), "body 主卡不得进 review 协议：{:?}", got);
        // 同一张卡在 body 任务下应当可用
        let body = effective_skills(&db, &book, "body", &[]);
        assert_eq!(body.len(), 1, "body 任务应加载书级正文写法：{:?}", body);
    }

    /// 目标章上下文：不引用未来章正文。
    #[test]
    fn target_context_never_reads_future_chapters() {
        let (_d, db, book) = fixture();
        molan_core::files::write_file(&db, &book, "正文", "第1章.md", "FIRST_CH_PAST_MARK")
            .unwrap();
        molan_core::files::write_file(&db, &book, "正文", "第10章.md", "FUTURE_CH_MARK").unwrap();
        let ctx = auto_book_context_for_chapter(&db, &book, 2, false);
        assert!(ctx.contains("FIRST_CH_PAST_MARK"), "应含第1章：{}", ctx);
        assert!(
            !ctx.contains("FUTURE_CH_MARK"),
            "补第2章不得引用第10章：{}",
            ctx
        );
    }

    /// aiOff：自动上下文不得注入被隐藏的设定内容。
    #[test]
    fn ai_off_assets_are_hidden_from_auto_context() {
        let (_d, db, book) = fixture();
        molan_core::files::write_file(&db, &book, "设定", "人物表.md", "SECRET_AI_OFF_MARK")
            .unwrap();
        molan_core::files::set_file_flag(&db, &book, "设定", "人物表.md", "aiOff", true).unwrap();
        let ctx = auto_book_context_for_chapter(&db, &book, 2, false);
        assert!(
            !ctx.contains("SECRET_AI_OFF_MARK"),
            "aiOff 内容不得自动注入：{}",
            ctx
        );
    }

    /// 派生摘要不自动注入（可能覆盖目标章之后）。
    #[test]
    fn derived_summary_is_not_auto_injected() {
        let (_d, db, book) = fixture();
        molan_core::files::write_file(
            &db,
            &book,
            "设定",
            "前情摘要.md",
            "（覆盖至第99章）\nSUMMARY_FUTURE_MARK",
        )
        .unwrap();
        let ctx = auto_book_context_for_chapter(&db, &book, 2, false);
        assert!(
            !ctx.contains("SUMMARY_FUTURE_MARK"),
            "滚动摘要不得当作截至本章事实：{}",
            ctx
        );
    }

    /// include_pending 保守：缺同任务标识时不读取待审稿。
    #[test]
    fn include_pending_does_not_read_unverified_drafts() {
        let (_d, db, book) = fixture();
        molan_core::files::write_file(&db, &book, "正文待审", "第1章.md", "PENDING_DRAFT_MARK")
            .unwrap();
        let ctx = auto_book_context_for_chapter(&db, &book, 2, true);
        assert!(
            !ctx.contains("PENDING_DRAFT_MARK"),
            "缺 task 标识时不得读待审稿：{}",
            ctx
        );
        assert!(ctx.contains("待审承接提示"), "应给出提示：{}", ctx);
    }

    /// 细纲注入遵守 aiOff：隐藏的细纲不得自动进入上下文。
    #[test]
    fn outline_injection_respects_ai_off() {
        let (_d, db, book) = fixture();
        molan_core::files::write_file(&db, &book, "细纲", "细纲_第2章.md", "OUTLINE_HIDDEN_MARK")
            .unwrap();
        molan_core::files::set_file_flag(&db, &book, "细纲", "细纲_第2章.md", "aiOff", true)
            .unwrap();
        let ctx = auto_book_context_for_chapter(&db, &book, 2, false);
        assert!(
            !ctx.contains("OUTLINE_HIDDEN_MARK"),
            "aiOff 细纲不得注入：{}",
            ctx
        );
    }

    /// 目标章有细纲时，只注入目标章；不得再用下一章细纲，且措辞为"必须逐条落实"。
    #[test]
    fn target_outline_used_when_present_and_next_not_forced() {
        let (_d, db, book) = fixture();
        molan_core::files::write_file(&db, &book, "正文", "第1章.md", "PAST_MARK").unwrap();
        molan_core::files::write_file(&db, &book, "细纲", "细纲_第2章.md", "TARGET_OUTLINE_MARK")
            .unwrap();
        molan_core::files::write_file(&db, &book, "细纲", "细纲_第3章.md", "NEXT_OUTLINE_MARK")
            .unwrap();
        let ctx = auto_book_context_for_chapter(&db, &book, 2, false);
        assert!(
            ctx.contains("TARGET_OUTLINE_MARK"),
            "应含目标章细纲：{}",
            ctx
        );
        assert!(ctx.contains("本章细纲"), "目标章细纲应为权威措辞：{}", ctx);
        assert!(
            !ctx.contains("NEXT_OUTLINE_MARK"),
            "目标章有细纲时不得注入下一章细纲：{}",
            ctx
        );
    }

    /// 已指定目标章时，目标章无细纲**不得**注入其它章细纲（避免误章），只给提醒。
    #[test]
    fn missing_target_outline_injects_no_other_outline() {
        let (_d, db, book) = fixture();
        molan_core::files::write_file(&db, &book, "正文", "第1章.md", "PAST_MARK").unwrap();
        molan_core::files::write_file(&db, &book, "细纲", "细纲_第3章.md", "NEXT_ONLY_MARK")
            .unwrap();
        let ctx = auto_book_context_for_chapter(&db, &book, 2, false);
        assert!(
            !ctx.contains("NEXT_ONLY_MARK"),
            "已指定目标章时不得注入其它章细纲：{}",
            ctx
        );
        assert!(
            !ctx.contains("参考细纲"),
            "已指定目标章时不得出现参考细纲：{}",
            ctx
        );
        assert!(
            ctx.contains("尚无经确认的本章细纲"),
            "应给出目标章缺细纲的提醒：{}",
            ctx
        );
    }

    /// 未指定目标章（target_ch=0）时，才允许把 next_ch 细纲作为"参考"注入。
    #[test]
    fn unspecified_target_may_use_reference_outline() {
        let (_d, db, book) = fixture();
        molan_core::files::write_file(&db, &book, "正文", "第1章.md", "PAST_MARK").unwrap();
        molan_core::files::write_file(&db, &book, "细纲", "细纲_第2章.md", "REFERENCE_MARK")
            .unwrap();
        let ctx = auto_book_context_for_chapter(&db, &book, 0, false);
        assert!(
            ctx.contains("REFERENCE_MARK"),
            "未指定目标章时可参考下一章：{}",
            ctx
        );
        assert!(ctx.contains("参考细纲"), "回退细纲必须标为参考：{}", ctx);
        assert!(
            !ctx.contains("必须逐条落实"),
            "参考细纲不得要求逐条落实：{}",
            ctx
        );
    }

    /// 细纲超预算必须显式报告省略量，不静默截断。
    #[test]
    fn oversized_outline_reports_omission() {
        let (_d, db, book) = fixture();
        molan_core::files::write_file(&db, &book, "正文", "第1章.md", "PAST_MARK").unwrap();
        let long = "细".repeat(2500);
        molan_core::files::write_file(&db, &book, "细纲", "细纲_第2章.md", &long).unwrap();
        let ctx = auto_book_context_for_chapter(&db, &book, 2, false);
        assert!(
            ctx.contains("超出预算"),
            "超预算必须显式提示：{}",
            &ctx[..ctx.len().min(400)]
        );
        assert!(
            ctx.contains("700"),
            "应报告未注入字数 2500-1800=700：{}",
            &ctx[..ctx.len().min(400)]
        );
    }
}
