use crate::db::Db;
use crate::stats::now_ms;
use serde_json::{json, Value};

/// 作者确认建书卡：原子更新书名/题材，并把对应助手消息标记为已创建。
/// 消息必须属于该书且 result_json 内含 bookSetup，避免跨书或伪造 messageId。
pub fn confirm_book_setup(
    db: &Db,
    book_id: &str,
    message_id: &str,
    title: &str,
    genre: &str,
) -> anyhow::Result<Value> {
    let now = now_ms();
    let mut guard = db.conn.lock().unwrap_or_else(|e| e.into_inner());
    let tx = guard.transaction()?;
    let raw: String = tx
        .query_row(
            "SELECT m.result_json FROM messages m JOIN sessions s ON s.id=m.session_id WHERE m.id=?1 AND s.book_id=?2 AND m.role='assistant'",
            rusqlite::params![message_id, book_id],
            |row| row.get(0),
        )
        .map_err(|_| anyhow::anyhow!("建书预览消息不存在或不属于当前书籍"))?;
    let mut result: Value =
        serde_json::from_str(&raw).map_err(|_| anyhow::anyhow!("建书预览消息数据无效"))?;
    result
        .get_mut("bookSetup")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("消息不包含建书预览"))?
        .insert("saved".to_string(), Value::Bool(true));
    let changed = tx.execute(
        "UPDATE books SET title=?1, genre=CASE WHEN ?2='' THEN genre ELSE ?2 END, updated_at=?3 WHERE id=?4 AND deleted_at IS NULL",
        rusqlite::params![title, genre, now, book_id],
    )?;
    if changed != 1 {
        return Err(anyhow::anyhow!("书籍不存在或已删除"));
    }
    tx.execute(
        "UPDATE messages SET result_json=?1 WHERE id=?2",
        rusqlite::params![result.to_string(), message_id],
    )?;
    tx.commit()?;
    Ok(json!({"ok": true, "filesSaved": 0, "result": result}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::books::{create_book, create_session, list_books, list_messages};
    use rusqlite::ToSql;

    #[test]
    fn persists_saved_and_rejects_invalid_message_atomically() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(d.path(), None).unwrap();
        let book_id = create_book(&db, "旧书名", "旧题材", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        let sid = create_session(&db, &book_id, "新书共创").unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let result = json!({"bookSetup": {"titles": ["新书名"], "files": [], "saved": false}});
        db.exec(
            "INSERT INTO messages(id,session_id,role,content,result_json,created_at) VALUES('setup-msg',?1,'assistant','预览',?2,1)",
            &[&sid as &dyn ToSql, &result.to_string()],
        ).unwrap();
        let out = confirm_book_setup(&db, &book_id, "setup-msg", "新书名", "新题材").unwrap();
        assert_eq!(out["result"]["bookSetup"]["saved"], true);
        assert_eq!(
            list_messages(&db, &sid)[0]["result"]["bookSetup"]["saved"],
            true
        );
        let err = confirm_book_setup(&db, &book_id, "missing", "不应写入", "不应写入").unwrap_err();
        assert!(err.to_string().contains("不属于当前书籍"));
        let books = list_books(&db);
        let book = books
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == book_id)
            .unwrap();
        assert_eq!(book["title"], "新书名");
        assert_eq!(book["genre"], "新题材");
    }
}
