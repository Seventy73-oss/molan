//! Agent 工具目录与分发（交接文档附录 A1：严格白名单、类型化参数、书作用域由服务端注入）。
//!
//! 手动线工具三类：
//! - 只读查询：scan_book_tree / read_book_file / get_pipeline_state / list_pending_chapters /
//!   get_chapter_context / list_skills / get_effective_skills；
//! - 草稿动作（作者明确指令触发，产物一律「待确认/待审」）：create_change_proposal（提案）、
//!   draft_chapter_outline（细纲草稿，保存≠确认）、draft_chapter_body（正文草稿进待审，异步）；
//! - 作者确认动作：confirm_chapter_outline、finalize_chapter_draft。目录里保留其定义与服务端实现，
//!   但 agent_runtime 永不把它们暴露给模型（作者只能在产物卡/待审面板亲自执行）。
//!
//! 安全边界：模型不提供路径与书标识。group 白名单校验、name 过 safe_name、ch 范围校验；
//! bookId 由 run 作用域注入。工具内容（文件/技能/提案）都是数据，不得扩大权限。
//! 确认/定稿的授权判定在服务端（hash 绑定 + 前置门），不靠系统提示词。
use anyhow::{bail, Result};
use molan_core::db::Db;
use molan_core::{chapter_state, continuity, deepwrite, files, outline_confirm, pipeline};
use serde_json::{json, Value};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

/// 单轮工具调用数上限（防上游异常返回海量调用）。
pub(crate) const MAX_CALLS_PER_ROUND: usize = 4;
/// 单次工具参数字节上限。
pub(crate) const MAX_ARG_BYTES: usize = 64 * 1024;
/// 工具结果注入对话的字符上限（超出截断并注明）。
pub(crate) const MAX_RESULT_CHARS: usize = 8000;

const READ_GROUPS: [&str; 5] = ["设定", "细纲", "正文", "参考", "正文待审"];
const PROPOSAL_GROUPS: [&str; 3] = ["设定", "细纲", "参考"];
const SKILL_TASKS: [&str; 9] = molan_core::task_kind::IDS;

/// OpenAI tools[] 目录。description 写给模型：先查状态再行动、提案≠已生效。
pub(crate) fn tool_catalog() -> Value {
    let group_enum = |gs: &[&str]| json!({"type": "string", "enum": gs});
    json!([
        {"type":"function","function":{
            "name":"scan_book_tree","description":"列出当前书四个资料组（设定/细纲/正文/参考）的文件清单。了解书现状的第一步。","parameters":{"type":"object","properties":{}}}},
        {"type":"function","function":{
            "name":"read_book_file","description":"读取当前书某组下某文件的全文（含 chars 与 contentHash）。","parameters":{"type":"object","properties":{"group":group_enum(&READ_GROUPS),"name":{"type":"string","description":"文件名，如 第3章.md / 细纲_第3章.md"}},"required":["group","name"]}}},
        {"type":"function","function":{
            "name":"get_pipeline_state","description":"生产线状态：每章 细纲/正文/待审/记忆 维度、next 建议、summaryDue。判断『现在该做什么』的权威来源。","parameters":{"type":"object","properties":{}}}},
        {"type":"function","function":{
            "name":"list_pending_chapters","description":"列出待人工批准的章节队列（pending 行含 contentHash/chars，供定稿绑定作者审阅过的版本）。","parameters":{"type":"object","properties":{}}}},
        {"type":"function","function":{
            "name":"get_chapter_context","description":"截至第 ch-1 章的已定稿记忆（摘要/事实/伏笔）与覆盖度（complete/stale/missing）。写章细纲前先查。","parameters":{"type":"object","properties":{"ch":{"type":"integer","description":"目标章号，1..=1000000"}},"required":["ch"]}}},
        {"type":"function","function":{
            "name":"list_skills","description":"列出全部启用的写作技能卡（选题/题材/架构/文风等方法论）。","parameters":{"type":"object","properties":{}}}},
        {"type":"function","function":{
            "name":"get_effective_skills","description":"查看某任务在当前书实际生效的技能（含书级默认绑定）。","parameters":{"type":"object","properties":{"task":group_enum_tasks()},"required":["task"]}}},
        {"type":"function","function":{
            "name":"create_change_proposal","description":"对 设定/细纲/参考 文件创建变更提案（pending，作者在提案卡/DeepWrite 接受才写入；正文修改必须走写作+审批流程，不能用提案）。绝不声称已生效。","parameters":{"type":"object","properties":{"group":group_enum(&PROPOSAL_GROUPS),"name":{"type":"string"},"summary":{"type":"string","description":"一句话说明改什么、为什么"},"proposedContent":{"type":"string","description":"提议的完整新文件内容"}},"required":["group","name","summary","proposedContent"]}}},
        {"type":"function","function":{
            "name":"draft_chapter_outline","description":"把起草好的第 ch 章细纲保存为草稿文件（仅新建；该章已有细纲时拒绝——修改既有细纲必须改用 create_change_proposal）。保存≠确认：作者确认入库前不能起草正文。","parameters":{"type":"object","properties":{"ch":{"type":"integer","description":"目标章号"},"content":{"type":"string","description":"细纲全文（Markdown；含本章目标/场景顺序/信息差/结尾状态）"}},"required":["ch","content"]}}},
        {"type":"function","function":{
            "name":"confirm_chapter_outline","description":"仅当作者明确表示「这个细纲可以，入库/确认」时调用：把该章细纲当前内容绑定为已确认版本，返回真实回执。expectedHash 传作者看过那一版的 hash（来自 read_book_file / draft_chapter_outline 回执）；期间被改则拒绝。绝不代替作者决定确认。","parameters":{"type":"object","properties":{"ch":{"type":"integer"},"expectedHash":{"type":"string","description":"作者看过版本的 contentHash（可省略=绑定当前内容）"}},"required":["ch"]}}},
        {"type":"function","function":{
            "name":"draft_chapter_body","description":"仅当作者明确要求写正文时调用：前置检查（细纲已确认且未失效、无既有稿件、前序记忆无阻塞、上一章已定稿）通过后生成第 ch 章正文草稿，自动去AI味并做剧情审核，落「正文待审」+审批队列。草稿≠定稿。耗时较长。","parameters":{"type":"object","properties":{"ch":{"type":"integer"},"instruction":{"type":"string","description":"本轮特别要求（可选）"},"skillIds":{"type":"array","items":{"type":"string"},"description":"本次临时技能覆盖（id 或名字；只影响本轮，不改默认绑定）"}},"required":["ch"]}}},
        {"type":"function","function":{
            "name":"finalize_chapter_draft","description":"仅当作者明确表示「这个版本定稿」时调用：按 expectedHash 绑定作者审阅过的待审版本执行定稿（与审批按钮同一核心动作），故事记忆自动排队同步。hash 不匹配即拒绝。绝不代替作者决定定稿。","parameters":{"type":"object","properties":{"ch":{"type":"integer"},"expectedHash":{"type":"string","description":"必填：作者审阅版本的 contentHash（来自 draft_chapter_body 回执或 list_pending_chapters）"}},"required":["ch","expectedHash"]}}}
    ])
}

fn group_enum_tasks() -> Value {
    json!({"type":"string","enum":SKILL_TASKS})
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("缺少参数 {}", key))
}

fn validate_group(g: &str, allowed: &[&str]) -> Result<String> {
    let canon = files::normalize_group(g.trim());
    if allowed.iter().any(|x| *x == canon) {
        Ok(canon)
    } else {
        bail!("非法分组 {:?}（允许：{}）", g, allowed.join("/"))
    }
}

fn validate_name(n: &str) -> Result<String> {
    let n = n.trim();
    if n.is_empty() || n.chars().count() > 120 {
        bail!("非法文件名：长度无效");
    }
    if n.contains('/') || n.contains('\\') || n.contains("..") {
        bail!("非法文件名：不允许路径分隔符或 ..");
    }
    if files::safe_name(n) != n {
        bail!("非法文件名：含不允许的字符");
    }
    Ok(n.to_string())
}

fn validate_ch(v: &Value) -> Result<i64> {
    let ch = v
        .as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
        .ok_or_else(|| anyhow::anyhow!("缺少有效章号 ch"))?;
    if !(1..=1_000_000).contains(&ch) {
        bail!("章号超出范围（1..=1000000）：{}", ch);
    }
    Ok(ch)
}

/// 截断到 max 字符并注明省略量（显式汇报省略，不静默）。
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{}…（截断，共{}字，省略{}字）", head, total, total - max)
}

/// 分发执行。同步调用（本地 SQLite/文件 IO，与既有 dispatch 各命令同模式）；
/// 调用方负责在取消边界之间调用，本函数不做网络请求。
pub(crate) fn dispatch_tool(db: &Db, book_id: &str, name: &str, args: &Value) -> Result<Value> {
    if serde_json::to_string(args).unwrap_or_default().len() > MAX_ARG_BYTES {
        bail!("工具参数超限（>{}字节）", MAX_ARG_BYTES);
    }
    if !files::valid_book_id(db, book_id) {
        bail!("书籍不存在或已删除");
    }
    match name {
        "scan_book_tree" => Ok(files::scan_tree(db, book_id)),
        "read_book_file" => {
            let group = validate_group(arg_str(args, "group")?, &READ_GROUPS)?;
            let fname = validate_name(arg_str(args, "name")?)?;
            // aiOff 是作者侧硬边界：对模型隐藏的文件连读取都要拒绝（B0 契约 §8.2）
            if files::file_flag(db, book_id, &group, &fname, "aiOff") {
                bail!("文件已设为 AI 不可见（aiOff）：{}/{}", group, fname);
            }
            let content = files::read_file(db, book_id, &group, &fname)
                .ok_or_else(|| anyhow::anyhow!("文件不存在：{}/{}", group, fname))?;
            Ok(json!({
                "group": group, "name": fname,
                "chars": content.chars().count(),
                "contentHash": continuity::content_hash(&content),
                "content": content,
            }))
        }
        "get_pipeline_state" => {
            let tree = files::scan_tree(db, book_id);
            let queue = Value::Array(
                db.q_json(
                    "SELECT ch, review_file, status FROM pending_chapter WHERE book_id=?1",
                    &[&book_id as &dyn rusqlite::ToSql],
                )
                .unwrap_or_default(),
            );
            let mut state = pipeline::derive_state_with_memory(db, book_id, &tree, &queue);
            // 手动线标注：outlineStatus / outline_confirm 收敛 / outline blockers（与 IPC 同源）
            outline_confirm::annotate(db, book_id, &mut state);
            Ok(state)
        }
        "list_pending_chapters" => {
            let rows = db
                .q_json(
                    "SELECT ch, review_file, status, created_at FROM pending_chapter WHERE book_id=?1 ORDER BY ch",
                    &[&book_id as &dyn rusqlite::ToSql],
                )
                .unwrap_or_default();
            // pending 行补 contentHash/chars：finalize_chapter_draft 必须绑定作者审阅过的版本
            let mut out = Vec::with_capacity(rows.len());
            for mut r in rows {
                let fname = r["reviewFile"].as_str().unwrap_or("");
                if r["status"].as_str() == Some("pending") && !fname.is_empty() {
                    if let Some(text) =
                        files::read_file(db, book_id, molan_core::db::REVIEW_GROUP, fname)
                    {
                        r["contentHash"] = json!(continuity::content_hash(&text));
                        r["chars"] = json!(text.chars().count());
                    }
                }
                out.push(r);
            }
            Ok(Value::Array(out))
        }
        "get_chapter_context" => {
            let ch = validate_ch(args.get("ch").unwrap_or(&Value::Null))?;
            let mut ctx = continuity::chapter_context(db, book_id, ch)?;
            if let Some(t) = ctx.get("text").and_then(Value::as_str) {
                ctx["text"] = json!(truncate_chars(t, MAX_RESULT_CHARS));
            }
            Ok(ctx)
        }
        "list_skills" => Ok(Value::Array(
            db.q_json(
                "SELECT id, name, kind, origin, usage_mode, enabled, targets_json FROM skills WHERE enabled=1 ORDER BY kind, name",
                &[],
            )
            .unwrap_or_default(),
        )),
        "get_effective_skills" => {
            let task = arg_str(args, "task")?;
            if !SKILL_TASKS.contains(&task) {
                bail!("未知任务 {:?}（允许：{}）", task, SKILL_TASKS.join("/"));
            }
            Ok(json!(super::effective_skills(db, book_id, task, &[])))
        }
        "create_change_proposal" => {
            let group = validate_group(arg_str(args, "group")?, &PROPOSAL_GROUPS)?;
            let fname = validate_name(arg_str(args, "name")?)?;
            let summary = truncate_chars(arg_str(args, "summary")?, 200);
            let content = arg_str(args, "proposedContent")?;
            if content.chars().count() > 100_000 {
                bail!("提案内容超限（>100000字）");
            }
            let p = deepwrite::create_proposal(
                db, book_id, "planner", &group, &fname, &summary, content,
            )?;
            Ok(json!({
                "proposalId": p["id"], "status": p["status"],
                "group": group, "name": fname, "summary": summary,
                "chars": content.chars().count(),
                "note": "提案已创建（pending），等待作者接受；未写入任何文件",
            }))
        }
        "draft_chapter_outline" => {
            let ch = validate_ch(args.get("ch").unwrap_or(&Value::Null))?;
            let content = arg_str(args, "content")?;
            if content.chars().count() < 30 {
                bail!("细纲内容过短（<30字），不足以作为章细纲");
            }
            if content.chars().count() > 20_000 {
                bail!("细纲内容超限（>20000字）");
            }
            // 已有细纲（任意命名）不覆盖：修改既有细纲必须走提案（作者接受才生效，CAS 保护）
            if let Some(existing) = super::stage_context::target_outline_name(db, book_id, ch) {
                bail!(
                    "第{}章已有细纲（{}）：修改既有细纲请用 create_change_proposal，不覆盖",
                    ch,
                    existing
                );
            }
            let fname = format!("细纲_第{}章.md", ch);
            files::write_ai_file(db, book_id, "细纲", &fname, content)?;
            let hash = continuity::content_hash(content);
            let _ = chapter_state::record_outline(db, book_id, ch, &hash);
            Ok(json!({
                "ok": true, "kind": "outline_saved", "ch": ch, "name": fname,
                "hash": hash, "chars": content.chars().count(), "status": "saved",
                "note": "细纲已保存为草稿（未确认）；作者确认入库后才能起草正文",
            }))
        }
        "confirm_chapter_outline" => {
            let ch = validate_ch(args.get("ch").unwrap_or(&Value::Null))?;
            let Some(oname) = super::stage_context::target_outline_name(db, book_id, ch) else {
                bail!("未找到第{}章的细纲文件，无法确认入库", ch);
            };
            let exp = args
                .get("expectedHash")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let mut r = outline_confirm::confirm(db, book_id, ch, &oname, exp)?;
            r["kind"] = json!("outline_confirmed");
            r["status"] = json!("confirmed");
            Ok(r)
        }
        "finalize_chapter_draft" => {
            let ch = validate_ch(args.get("ch").unwrap_or(&Value::Null))?;
            // expectedHash 工具路径必填：定稿必须绑定作者审阅过的那一版（§5.5）
            let exp = arg_str(args, "expectedHash")?;
            super::chapter_service::finalize_draft(db, book_id, ch, exp)
        }
        _ => bail!("未知工具：{}", name),
    }
}

/// 异步分发入口（agent_loop 唯一调用点）：draft_chapter_body 走单章起草服务（LLM，需
/// 取消令牌与进度通道），其余工具落回同步 dispatch_tool。参数上限与书校验同 sync 路径。
pub(crate) async fn dispatch_tool_io(
    db: &Db,
    book_id: &str,
    name: &str,
    args: &Value,
    cancel: &CancellationToken,
    tx: &Sender<String>,
    channel: &str,
) -> Result<Value> {
    if name == "draft_chapter_body" {
        if serde_json::to_string(args).unwrap_or_default().len() > MAX_ARG_BYTES {
            bail!("工具参数超限（>{}字节）", MAX_ARG_BYTES);
        }
        if !files::valid_book_id(db, book_id) {
            bail!("书籍不存在或已删除");
        }
        let ch = validate_ch(args.get("ch").unwrap_or(&Value::Null))?;
        let instruction = args
            .get("instruction")
            .and_then(Value::as_str)
            .unwrap_or("");
        // 本次技能覆盖（§6.3 临时作用域）：只影响本轮，不写任何默认绑定
        // skillIds 接受数组或逗号分隔串（mock 工具协议参数内不能含 ]，数组写法测不了）
        let skill_ids: Vec<String> = match args.get("skillIds") {
            Some(v) if v.is_array() => v
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            Some(v) => v
                .as_str()
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            None => Vec::new(),
        };
        // root 供 prompts/文风档案解析：data_dir = root/data（main.rs 构造恒等式）
        let root = db.data_dir.parent().unwrap_or(&db.data_dir);
        let emit = super::chapter_service::Emit::new(tx, channel);
        let opts = super::chapter_service::DraftOpts {
            instruction,
            skill_ids: &skill_ids,
            selection: None,
        };
        return super::chapter_service::draft_chapter(
            db,
            root,
            book_id,
            ch,
            &opts,
            cancel.clone(),
            &emit,
        )
        .await;
    }
    dispatch_tool(db, book_id, name, args)
}

/// 结构化产物（工件）：写类工具成功时从回执提取，随 tool 事件下发并持久化进 steps_json，
/// 前端按 kind 渲染确认卡（提案/细纲/草稿/定稿），历史恢复同一形状。读类工具与错误 → Null。
pub(crate) fn artifact_of(name: &str, payload: &Value) -> Value {
    if payload.get("error").is_some() {
        return Value::Null;
    }
    match name {
        "create_change_proposal" => json!({
            "kind": "proposal", "proposalId": payload["proposalId"],
            "group": payload["group"], "name": payload["name"], "summary": payload["summary"],
        }),
        "draft_chapter_outline"
        | "confirm_chapter_outline"
        | "draft_chapter_body"
        | "finalize_chapter_draft" => payload.clone(),
        _ => Value::Null,
    }
}

/// 脱敏：上游错误文本里若带 API key 必须打码（agent_loop 与工具错误共用）。
pub(crate) fn sanitize(api_key: &str, e: &anyhow::Error) -> String {
    let msg = e.to_string();
    if api_key.len() >= 8 && msg.contains(api_key) {
        msg.replace(api_key, "***")
    } else {
        msg
    }
}

/// 工具事件的公开摘要（脱敏：只留结构与关键字段，不回显全文）。
pub(crate) fn tool_summary(out: &Value) -> String {
    if let Some(e) = out.get("error").and_then(Value::as_str) {
        return truncate_chars(e, 160);
    }
    truncate_chars(&out.to_string(), 160)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let book = molan_core::books::create_book(&db, "tools-test", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        (dir, db, book)
    }

    #[test]
    fn catalog_names_match_dispatch_and_reject_unknown() {
        let (_d, db, book) = fixture();
        let names: Vec<String> = tool_catalog()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_string())
            .collect();
        // 手动线 12 工具：7 读 + 提案 + 细纲草稿/确认 + 正文草稿（仅异步入口）+ 定稿
        assert_eq!(names.len(), 12);
        for n in [
            "draft_chapter_outline",
            "confirm_chapter_outline",
            "draft_chapter_body",
            "finalize_chapter_draft",
        ] {
            assert!(names.contains(&n.to_string()), "目录缺少 {}", n);
        }
        let err = dispatch_tool(&db, &book, "definitely_not_a_tool", &json!({})).unwrap_err();
        assert!(err.to_string().contains("未知工具"));
        assert!(names.contains(&"scan_book_tree".to_string()));
        assert!(names.contains(&"create_change_proposal".to_string()));
    }

    #[test]
    fn rejects_traversal_groups_and_bad_names() {
        let (_d, db, book) = fixture();
        let bad = [
            json!({"name":"read_book_file","args":{"group":"设定","name":"../../etc/passwd"}}),
            json!({"name":"read_book_file","args":{"group":"设定","name":"a/b.md"}}),
            json!({"name":"read_book_file","args":{"group":"正文待审稿","name":"x.md"}}),
            json!({"name":"read_book_file","args":{"group":"设定"}}),
            json!({"name":"get_chapter_context","args":{"ch":0}}),
            json!({"name":"get_chapter_context","args":{"ch":2000000}}),
            json!({"name":"get_effective_skills","args":{"task":"hack"}}),
        ];
        for b in bad {
            let out = dispatch_tool(&db, &book, b["name"].as_str().unwrap(), &b["args"]);
            assert!(out.is_err(), "必须拒绝：{}", b);
        }
    }

    #[test]
    fn read_scan_pipeline_pending_context() {
        let (_d, db, book) = fixture();
        files::write_file(&db, &book, "设定", "世界观.md", "修真体系").unwrap();
        files::write_file(&db, &book, "正文", "第1章.md", "第一章内容").unwrap();
        db.exec(
            "INSERT INTO pending_chapter(book_id,ch,review_file,status,created_at,updated_at) VALUES(?1,2,'第2章.md','pending',0,0)",
            &[&book as &dyn rusqlite::ToSql],
        )
        .unwrap();

        let r = dispatch_tool(
            &db,
            &book,
            "read_book_file",
            &json!({"group":"设定","name":"世界观.md"}),
        )
        .unwrap();
        assert_eq!(r["content"], "修真体系");
        assert_eq!(r["chars"], 4);
        assert!(!r["contentHash"].as_str().unwrap().is_empty());

        let tree = dispatch_tool(&db, &book, "scan_book_tree", &json!({})).unwrap();
        assert!(tree.as_array().unwrap().iter().any(|g| g["dir"] == "设定"));

        let st = dispatch_tool(&db, &book, "get_pipeline_state", &json!({})).unwrap();
        assert_eq!(st["chapters"][1]["body"], "pending");
        assert!(st["next"].is_object());

        let pending = dispatch_tool(&db, &book, "list_pending_chapters", &json!({})).unwrap();
        assert_eq!(pending.as_array().unwrap().len(), 1);

        let ctx = dispatch_tool(&db, &book, "get_chapter_context", &json!({"ch":2})).unwrap();
        assert!(ctx.get("complete").is_some());

        let missing = dispatch_tool(
            &db,
            &book,
            "read_book_file",
            &json!({"group":"设定","name":"没有.md"}),
        );
        assert!(missing.unwrap_err().to_string().contains("文件不存在"));
    }

    #[test]
    fn proposal_is_pending_only_and_body_rejected() {
        let (_d, db, book) = fixture();
        files::write_file(&db, &book, "设定", "世界观.md", "旧设定").unwrap();
        let out = dispatch_tool(
            &db,
            &book,
            "create_change_proposal",
            &json!({"group":"设定","name":"世界观.md","summary":"补充力量体系","proposedContent":"旧设定\n\n力量体系：炼气→筑基"}),
        )
        .unwrap();
        assert_eq!(out["status"], "pending");
        assert!(!out["proposalId"].as_str().unwrap().is_empty());
        // 文件绝未被改动
        assert_eq!(
            files::read_file(&db, &book, "设定", "世界观.md").as_deref(),
            Some("旧设定")
        );
        // 正文组一律拒绝（必须走 待审→批准 链）
        let body = dispatch_tool(
            &db,
            &book,
            "create_change_proposal",
            &json!({"group":"正文","name":"第1章.md","summary":"x","proposedContent":"y"}),
        );
        assert!(body.is_err());
    }

    #[test]
    fn skills_listing_and_effective() {
        let (_d, db, book) = fixture();
        db.exec(
            "INSERT INTO skills(id,name,description,prompt_template,kind,source,enabled,builtin_key,usage_mode,origin,targets_json) VALUES('sk1','三幕结构','结构方法','模板','craft','',1,NULL,'support','user','[\"outline\"]')",
            &[],
        )
        .unwrap();
        let list = dispatch_tool(&db, &book, "list_skills", &json!({})).unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["name"], "三幕结构");
        let eff = dispatch_tool(
            &db,
            &book,
            "get_effective_skills",
            &json!({"task":"outline"}),
        )
        .unwrap();
        assert!(eff.is_array());
    }

    // ---------- 手动线同步工具 ----------

    #[test]
    fn draft_outline_saves_unconfirmed_and_blocks_overwrite() {
        let (_d, db, book) = fixture();
        let content = "第1章细纲：目标、冲突、场景顺序、章末钩子。".repeat(2);
        let out = dispatch_tool(
            &db,
            &book,
            "draft_chapter_outline",
            &json!({"ch":1,"content":content}),
        )
        .unwrap();
        assert_eq!(out["kind"], "outline_saved");
        assert_eq!(out["status"], "saved");
        assert_eq!(out["name"], "细纲_第1章.md");
        // 保存≠确认：outlineStatus 必须是 saved 而非 confirmed
        let st = outline_confirm::status_for(&db, &book, 1, "细纲_第1章.md");
        assert_eq!(st["status"], "saved");
        // 已有细纲不覆盖：再存同章必须被拒（引导走提案）
        let again = dispatch_tool(
            &db,
            &book,
            "draft_chapter_outline",
            &json!({"ch":1,"content":"另一版"}),
        );
        assert!(again.is_err());
        // 过短拒绝
        assert!(dispatch_tool(
            &db,
            &book,
            "draft_chapter_outline",
            &json!({"ch":2,"content":"太短"}),
        )
        .is_err());
    }

    #[test]
    fn confirm_outline_binds_hash_and_rejects_stale_expected() {
        let (_d, db, book) = fixture();
        let content = "第1章细纲：目标、冲突、场景顺序、章末钩子。".repeat(2);
        dispatch_tool(
            &db,
            &book,
            "draft_chapter_outline",
            &json!({"ch":1,"content":content}),
        )
        .unwrap();
        let st = outline_confirm::status_for(&db, &book, 1, "细纲_第1章.md");
        let h = st["hash"].as_str().unwrap().to_string();
        // 错误 expectedHash → 拒绝（作者看的不是这一版）
        assert!(dispatch_tool(
            &db,
            &book,
            "confirm_chapter_outline",
            &json!({"ch":1,"expectedHash":"deadbeef"}),
        )
        .is_err());
        // 正确 hash → confirmed 回执
        let r = dispatch_tool(
            &db,
            &book,
            "confirm_chapter_outline",
            &json!({"ch":1,"expectedHash":h}),
        )
        .unwrap();
        assert_eq!(r["kind"], "outline_confirmed");
        assert_eq!(r["status"], "confirmed");
        assert_eq!(
            outline_confirm::status_for(&db, &book, 1, "细纲_第1章.md")["status"],
            "confirmed"
        );
    }

    #[test]
    fn read_book_file_respects_ai_off() {
        let (_d, db, book) = fixture();
        files::write_file(&db, &book, "设定", "世界观.md", "修真体系").unwrap();
        // aiOff 后模型不得读取（B0 契约 §8.2）
        db.exec(
            "INSERT INTO settings(key,value) VALUES(?1,?2)",
            &[
                &format!("file_flags__{}", book),
                &json!({"设定/世界观.md":{"aiOff":true}}).to_string(),
            ],
        )
        .unwrap();
        let r = dispatch_tool(
            &db,
            &book,
            "read_book_file",
            &json!({"group":"设定","name":"世界观.md"}),
        );
        assert!(r.is_err(), "aiOff 文件必须拒绝读取");
    }

    #[test]
    fn finalize_requires_expected_hash_and_no_pending_is_error() {
        let (_d, db, book) = fixture();
        // expectedHash 必填
        assert!(dispatch_tool(&db, &book, "finalize_chapter_draft", &json!({"ch":1})).is_err());
        // 无待审稿 → 明确错误（不静默成功）
        let e = dispatch_tool(
            &db,
            &book,
            "finalize_chapter_draft",
            &json!({"ch":1,"expectedHash":"x"}),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("没有待审稿"), "{}", e);
    }

    #[test]
    fn artifact_of_shapes_match_contract() {
        let p = artifact_of(
            "create_change_proposal",
            &json!({"proposalId":"pid","group":"细纲","name":"n","summary":"s"}),
        );
        assert_eq!(p["kind"], "proposal");
        assert_eq!(p["proposalId"], "pid");
        // 错误回执不产生工件
        assert!(artifact_of("create_change_proposal", &json!({"error":"x"})).is_null());
        // 读类工具无工件
        assert!(artifact_of("scan_book_tree", &json!({"ok":true})).is_null());
        // 写类工具回执原样带 kind
        let d = artifact_of("draft_chapter_body", &json!({"kind":"body_draft","ch":3}));
        assert_eq!(d["kind"], "body_draft");
    }

    #[test]
    fn list_pending_includes_content_hash() {
        let (_d, db, book) = fixture();
        let body = "第1章正文待审内容。".repeat(20);
        files::write_file(&db, &book, molan_core::db::REVIEW_GROUP, "第1章.md", &body).unwrap();
        crate::handlers::register_review_queue(&db, &book, "第1章.md", &body).unwrap();
        let rows = dispatch_tool(&db, &book, "list_pending_chapters", &json!({})).unwrap();
        let r0 = &rows.as_array().unwrap()[0];
        assert_eq!(
            r0["contentHash"].as_str().unwrap(),
            continuity::content_hash(&body)
        );
        assert!(r0["chars"].as_i64().unwrap() > 0);
    }
}
