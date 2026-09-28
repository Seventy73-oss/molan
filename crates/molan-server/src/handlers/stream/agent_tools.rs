//! Agent 工具目录与分发（交接文档附录 A1：严格白名单、类型化参数、书作用域由服务端注入）。
//!
//! v1 工具两类：
//! - 只读查询：scan_book_tree / read_book_file / get_pipeline_state / list_pending_chapters /
//!   get_chapter_context / list_skills / get_effective_skills；
//! - 唯一写工具 create_change_proposal：只产生 **pending 提案**（DeepWrite 工作台由作者
//!   接受才生效），组白名单不含 正文/正文待审——书稿写入必须走既有 待审→批准 链。
//!
//! 安全边界：模型不提供路径与书标识。group 白名单校验、name 过 safe_name、ch 范围校验；
//! bookId 由 run 作用域注入。工具内容（文件/技能/提案）都是数据，不得扩大权限。
use anyhow::{bail, Result};
use molan_core::db::Db;
use molan_core::{continuity, deepwrite, files, pipeline};
use serde_json::{json, Value};

/// 单轮工具调用数上限（防上游异常返回海量调用）。
pub(crate) const MAX_CALLS_PER_ROUND: usize = 4;
/// 单次工具参数字节上限。
pub(crate) const MAX_ARG_BYTES: usize = 64 * 1024;
/// 工具结果注入对话的字符上限（超出截断并注明）。
pub(crate) const MAX_RESULT_CHARS: usize = 8000;

const READ_GROUPS: [&str; 5] = ["设定", "细纲", "正文", "参考", "正文待审"];
const PROPOSAL_GROUPS: [&str; 3] = ["设定", "细纲", "参考"];
const SKILL_TASKS: [&str; 9] = [
    "chat", "plot", "outline", "body", "revise", "review", "humanize", "summary", "distill",
];

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
            "name":"list_pending_chapters","description":"列出待人工批准的章节队列（含依赖状态 dependencyStatus）。","parameters":{"type":"object","properties":{}}}},
        {"type":"function","function":{
            "name":"get_chapter_context","description":"截至第 ch-1 章的已定稿记忆（摘要/事实/伏笔）与覆盖度（complete/stale/missing）。写章细纲前先查。","parameters":{"type":"object","properties":{"ch":{"type":"integer","description":"目标章号，1..=1000000"}},"required":["ch"]}}},
        {"type":"function","function":{
            "name":"list_skills","description":"列出全部启用的写作技能卡（选题/题材/架构/文风等方法论）。","parameters":{"type":"object","properties":{}}}},
        {"type":"function","function":{
            "name":"get_effective_skills","description":"查看某任务在当前书实际生效的技能（含书级默认绑定）。","parameters":{"type":"object","properties":{"task":group_enum_tasks()},"required":["task"]}}},
        {"type":"function","function":{
            "name":"create_change_proposal","description":"对 设定/细纲/参考 文件创建变更提案（pending，作者在 DeepWrite 接受才写入；正文修改必须走写作+审批流程，不能用提案）。绝不声称已生效。","parameters":{"type":"object","properties":{"group":group_enum(&PROPOSAL_GROUPS),"name":{"type":"string"},"summary":{"type":"string","description":"一句话说明改什么、为什么"},"proposedContent":{"type":"string","description":"提议的完整新文件内容"}},"required":["group","name","summary","proposedContent"]}}}
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
            Ok(pipeline::derive_state_with_memory(db, book_id, &tree, &queue))
        }
        "list_pending_chapters" => Ok(Value::Array(
            db.q_json(
                "SELECT ch, review_file, status, created_at FROM pending_chapter WHERE book_id=?1 ORDER BY ch",
                &[&book_id as &dyn rusqlite::ToSql],
            )
            .unwrap_or_default(),
        )),
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
                "note": "提案已创建，等待作者在 DeepWrite 工作台接受；未写入任何文件",
            }))
        }
        _ => bail!("未知工具：{}", name),
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
        assert_eq!(names.len(), 8);
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
}
