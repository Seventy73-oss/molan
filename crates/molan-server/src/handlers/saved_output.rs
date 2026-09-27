//! 保存产出：从工具 result 中解析待保存文档（纯解析与校验，不写磁盘、不碰 IPC）。
//! IPC、落盘、审批与结果返回由 Lead 处理；不接受客户端传入的任意文本。

use anyhow::{bail, Result};
use serde_json::{json, Value};

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 从 `result.docs`（数组）或 `result.doc`（单对象）取出 group/name/content。
/// content 优先取条目自身的 content，缺失或为空时回退到本轮消息原文；
/// 所有文档校验全部通过才返回，任一条目不合法则整体拒绝。
pub(super) fn documents(result: &Value, message_content: &str) -> Result<Vec<Value>> {
    if result["doc"]["section"].is_number() || !result["doc"]["op"].is_null() {
        bail!("这是分段修改而非完整文稿，尚未保存。请先在编辑器中核对并合并原文；系统不会把片段覆盖为全文");
    }
    let msg = message_content;
    let items: Vec<&Value> = match result.get("docs") {
        Some(Value::Array(list)) => list.iter().collect(),
        Some(Value::Null) | None => match result.get("doc") {
            Some(v @ Value::Object(_)) => vec![v],
            _ => bail!("result 中没有 docs 数组或 doc 对象，无法确定要保存的文档"),
        },
        Some(_) => bail!("result.docs 必须是数组"),
    };
    if items.is_empty() {
        bail!("result.docs 为空，没有可保存的文档");
    }
    let allowed = ["正文待审", "设定", "细纲", "参考"];
    let mut out = Vec::with_capacity(items.len());
    for item in &items {
        let name = text(item, "name");
        if name.is_empty() {
            bail!("文档缺少 name，拒绝保存");
        }
        // 名称是短标签：含换行说明客户端把整段正文当成了文件名。
        if name.contains('\n') {
            bail!("文档 name 疑似整段正文，拒绝作为文件名：{}", name);
        }
        let content = item["content"].as_str().unwrap_or("").to_string();
        let content = if !content.trim().is_empty() {
            content
        } else if !msg.trim().is_empty() && items.len() == 1 {
            msg.to_string()
        } else {
            bail!("文档「{}」content 为空且本轮消息无正文，拒绝保存", name);
        };
        let raw = text(item, "group");
        let group = if raw.is_empty() || raw == "正文" {
            "正文待审".to_string()
        } else {
            raw
        };
        if !allowed.contains(&group.as_str()) {
            bail!(
                "文档「{}」分组「{}」不受支持，只允许：{}",
                name,
                group,
                allowed.join("/")
            );
        }
        out.push(json!({ "group": group, "name": name, "content": content }));
    }
    Ok(out)
}

/// The UI sends flags (draft/replace), never prose in those fields. Preserve the stored message.
pub(super) fn save(db: &molan_core::db::Db, args: &Value) -> Result<Value> {
    use molan_core::files;
    static SAVE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SAVE.lock().unwrap_or_else(|e| e.into_inner());
    let book = text(args, "bookId");
    let id = text(args, "messageId");
    let rows = db.q_json("SELECT m.content,m.result_json FROM messages m JOIN sessions s ON s.id=m.session_id JOIN books b ON b.id=s.book_id WHERE m.id=?1 AND s.book_id=?2 AND m.role='assistant' AND b.deleted_at IS NULL", &[&id as &dyn rusqlite::ToSql,&book])?;
    let msg = rows
        .first()
        .ok_or_else(|| anyhow::anyhow!("消息不存在、不是AI产出或不属于本书"))?;
    let mut result: Value = serde_json::from_str(msg["resultJson"].as_str().unwrap_or("{}"))?;
    let docs = documents(&result, msg["content"].as_str().unwrap_or(""))?;
    let mut seen = std::collections::HashSet::new();
    for doc in &docs {
        let name = text(doc, "name");
        if files::safe_name(&name) != name
            || !name.ends_with(".md")
            || !seen.insert((text(doc, "group"), name))
        {
            bail!("文档路径无效或重复，未保存");
        }
    }
    let mut receipts = result["savedDocs"].as_array().cloned().unwrap_or_default();
    for doc in &docs {
        let group = text(doc, "group");
        let name = text(doc, "name");
        let content = doc["content"].as_str().unwrap();
        let receipt = json!({"group":group,"name":name});
        // Idempotence requires exact content; do not recreate deleted files after a durable receipt.
        match files::read_file(db, &book, &group, &name) {
            Some(old) if old == content => (),
            Some(_) => bail!("目标文件已存在且内容不同，未覆盖：{}/{}", group, name),
            None if receipts.contains(&receipt) => {
                bail!("已保存文件被移除，请检查目录；不会自动重建")
            }
            None => files::write_ai_file(db, &book, &group, &name, content)?,
        }
        if group == molan_core::db::REVIEW_GROUP {
            super::register_review_queue(db, &book, &name, content)?;
        }
        if !receipts.contains(&receipt) {
            receipts.push(receipt);
        }
        result["savedDocs"] = json!(receipts);
        result["saved"] = json!(false);
        db.exec(
            "UPDATE messages SET result_json=?1 WHERE id=?2",
            &[&result.to_string() as &dyn rusqlite::ToSql, &id],
        )?;
    }
    result["saved"] = json!(true);
    result["savedBookId"] = json!(book);
    db.exec(
        "UPDATE messages SET result_json=?1 WHERE id=?2",
        &[&result.to_string() as &dyn rusqlite::ToSql, &id],
    )?;
    // Existing aN/lN adapters JSON.parse this string.
    Ok(json!(result.to_string()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn section_operations_are_not_saved_as_complete_manuscripts() {
        for doc in [
            serde_json::json!({"name":"全文.md","section":2}),
            serde_json::json!({"name":"全文.md","op":{"type":"append"}}),
        ] {
            assert!(super::documents(&serde_json::json!({"doc":doc}), "只有第二段").is_err());
        }
    }
    #[test]
    fn unnumbered_prose_can_be_reviewed_without_renaming() {
        let dir = tempfile::tempdir().unwrap();
        let db = molan_core::db::Db::open(dir.path(), None).unwrap();
        let book = molan_core::books::create_book(&db, "短篇", "悬疑", "第三人称");
        let bid = book["id"].as_str().unwrap();
        for name in ["全文.md", "番外.md"] {
            molan_core::files::write_file(&db, bid, "正文待审", name, "待审文本").unwrap();
            let first = super::super::register_review_queue(&db, bid, name, "待审文本").unwrap();
            let retry = super::super::register_review_queue(&db, bid, name, "待审文本").unwrap();
            assert_eq!(first["ch"], retry["ch"]);
            assert!(first["ch"].as_i64().unwrap() < 0);
            assert_eq!(
                molan_core::files::approve_pending_chapter(&db, bid, name).unwrap(),
                name
            );
            assert_eq!(
                molan_core::files::read_file(&db, bid, "正文", name).unwrap(),
                "待审文本"
            );
        }
        assert_eq!(
            db.q_json(
                "SELECT DISTINCT ch FROM pending_chapter WHERE book_id=?1",
                &[&bid]
            )
            .unwrap()
            .len(),
            2
        );
    }

    use super::documents;
    use serde_json::json;

    #[test]
    fn body_group_becomes_pending_review_and_keeps_content_length() {
        let body = "字".repeat(200);
        let r = documents(
            &json!({"docs": [{"group": "正文", "name": "第一章", "content": body}]}),
            "消息原文",
        )
        .unwrap();
        assert_eq!(r[0]["group"], "正文待审");
        assert_eq!(r[0]["content"].as_str().unwrap().chars().count(), 200);
    }

    #[test]
    fn single_doc_without_content_falls_back_to_message() {
        let r = documents(
            &json!({"doc": {"name": "设定", "group": "设定"}}),
            "  世界观正文  ",
        )
        .unwrap();
        assert_eq!(r[0]["content"], "  世界观正文  ");
    }

    #[test]
    fn missing_doc_and_empty_content_are_rejected() {
        assert!(documents(&json!({}), "消息").is_err());
        assert!(documents(&json!({"docs": []}), "消息").is_err());
        assert!(documents(&json!({"doc": {"name": "x", "content": "  "}}), "").is_err());
    }

    #[test]
    fn second_doc_without_name_rejects_whole_batch() {
        let r = json!({"docs": [
            {"group": "细纲", "name": "卷一", "content": "A"},
            {"group": "参考", "content": "B"},
        ]});
        let err = documents(&r, "消息").unwrap_err();
        assert!(err.to_string().contains("name"));
    }
}
