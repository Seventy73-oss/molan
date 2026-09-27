//! 可信共创工作区生命周期（W2）。
//!
//! `begin` 原子创建「新书 + 共创会话 + 工作区标记」；只有带该服务端标记的来源作品，
//! 才允许 `save_book_setup_selection` 的 source 模式完成改名。`context` 只对
//! 服务端创建的工作区会话中的 assistant 建书卡返回 eligible=true，绝不凭书名或空目录猜测。
//!
//! 锁序约定：本模块只在锁内读取 settings，绝不在持有 db.conn 时调用会重新取锁的
//! files 函数（ensure_book_dir 在事务提交、锁释放之后调用）。
use crate::db::Db;
use crate::stats::now_ms;
use anyhow::{anyhow, bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

/// 工作区标记的 settings 键前缀：`book_setup_workspace__<bookId>`。
pub const KEY_PREFIX: &str = "book_setup_workspace__";
/// 用户未给标题时的占位书名。
pub const UNNAMED_TITLE: &str = "未命名新书";
const MAX_TEXT: usize = 200;

pub fn workspace_key(book_id: &str) -> String {
    format!("{}{}", KEY_PREFIX, book_id)
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

/// 读取可选文本参数：缺省/null 视为未给；首尾空白去除；空串按未给处理；长度有界。
fn bounded(args: &Value, key: &str) -> Result<Option<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| anyhow!("建书资料参数 {} 必须是文本", key))?;
            let t = s.trim();
            if t.chars().count() > MAX_TEXT {
                bail!("建书资料参数 {} 过长", key);
            }
            Ok(if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            })
        }
    }
}

/// 锁内读取可信工作区记录（不重新获取 db.conn，供事务内复用，避免重入死锁）。
pub(crate) fn workspace_record_conn(conn: &Connection, book_id: &str) -> Result<Option<Value>> {
    let key = workspace_key(book_id);
    let raw: Option<String> = conn
        .query_row("SELECT value FROM settings WHERE key=?1", [&key], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(raw) = raw else { return Ok(None) };
    let record: Value = serde_json::from_str(&raw).map_err(|_| anyhow!("共创工作区记录损坏"))?;
    if text(&record, "bookId") != book_id || text(&record, "sessionId").is_empty() {
        bail!("共创工作区记录与作品不一致");
    }
    Ok(Some(record))
}

/// 读取可信工作区记录（自行取锁的只读入口）。
pub fn workspace_record(db: &Db, book_id: &str) -> Result<Option<Value>> {
    let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
    workspace_record_conn(&conn, book_id)
}

/// 开始新书共创：原子建书 + 共创会话 + 工作区标记，然后初始化新书目录。
/// 返回 `{ok:true, book:<list_books 形状>, session:<list_sessions 形状>}`。
pub fn begin(db: &Db, args: &Value) -> Result<Value> {
    let title = bounded(args, "title")?.unwrap_or_else(|| UNNAMED_TITLE.to_string());
    let genre = bounded(args, "genre")?.unwrap_or_default();
    let pov = bounded(args, "pov")?.unwrap_or_else(|| "第三人称".to_string());
    let form = bounded(args, "form")?;
    let platform = bounded(args, "platform")?;
    let audience = bounded(args, "audience")?;

    let book_id = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();
    let now = now_ms();
    let cover = title
        .chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "书".into());

    let mut record = json!({
        "bookId": book_id,
        "sessionId": session_id,
        "state": "draft",
        "title": title,
        "genre": genre,
        "pov": pov,
    });
    if let Some(v) = form.as_deref() {
        record["form"] = json!(v);
    }
    if let Some(v) = platform.as_deref() {
        record["platform"] = json!(v);
    }
    if let Some(v) = audience.as_deref() {
        record["audience"] = json!(v);
    }

    {
        // 建书 + 会话 + 工作区标记同事务：任一步失败都不留下半态工作区。
        let mut guard = db.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = guard.transaction()?;
        tx.execute(
            "INSERT INTO books(id,title,genre,pov,status,cover_char,word_count,chapter_count,created_at,updated_at) VALUES(?1,?2,?3,?4,'构思中',?5,0,0,?6,?6)",
            params![book_id, title, genre, pov, cover, now],
        )?;
        tx.execute(
            "INSERT INTO sessions(id,book_id,title,preview,msg_count,created_at,updated_at) VALUES(?1,?2,'新书共创',NULL,0,?3,?3)",
            params![session_id, book_id, now],
        )?;
        tx.execute(
            "INSERT INTO settings(key,value) VALUES(?1,?2)",
            params![workspace_key(&book_id), record.to_string()],
        )?;
        tx.commit()?;
    }

    // 必须在释放 conn 锁之后再初始化目录：ensure_book_dir 会重新取锁做 valid_book_id 校验。
    if let Err(e) = crate::files::ensure_book_dir(db, &book_id) {
        // 工作区与书籍已持久化；文件写入会自动补齐父目录，不因目录初始化失败回滚。
        eprintln!(
            "[molan-core] 新书目录初始化失败（工作区已持久化，可继续）：{}",
            e
        );
    }

    let book = crate::books::list_books(db)
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|b| b["id"].as_str().unwrap_or("") == book_id.as_str())
                .cloned()
        })
        .ok_or_else(|| anyhow!("新书创建后无法读取"))?;
    let session = crate::books::list_sessions(db, &book_id)
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|s| s["id"].as_str().unwrap_or("") == session_id.as_str())
                .cloned()
        })
        .ok_or_else(|| anyhow!("共创会话创建后无法读取"))?;
    Ok(json!({"ok": true, "book": book, "session": session}))
}

/// 判断某条 assistant 建书卡是否属于服务端创建的可信工作区。
/// 返回固定形状 `{eligible, sourceBookId, sourceSessionId, title}`；不可信一律 eligible=false，
/// 不凭书名/目录空猜测，也不因目标已删而自动解绑。
pub fn context(db: &Db, args: &Value) -> Result<Value> {
    let source_book_id = text(args, "sourceBookId").trim().to_string();
    let message_id = text(args, "messageId").trim().to_string();
    let mut out = json!({
        "eligible": false,
        "sourceBookId": source_book_id,
        "sourceSessionId": "",
        "title": "",
    });
    if source_book_id.is_empty() || message_id.is_empty() {
        return Ok(out);
    }
    let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
    let title: Option<String> = conn
        .query_row(
            "SELECT title FROM books WHERE id=?1 AND deleted_at IS NULL",
            [&source_book_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(title) = title else { return Ok(out) };
    out["title"] = json!(title);

    let workspace = match workspace_record_conn(&conn, &source_book_id) {
        Ok(Some(w)) => w,
        Ok(None) => return Ok(out),
        Err(_) => return Ok(out),
    };
    let ws_session = text(&workspace, "sessionId").to_string();

    let message_session: Option<String> = conn
        .query_row(
            "SELECT m.session_id FROM messages m JOIN sessions s ON s.id=m.session_id WHERE m.id=?1 AND s.book_id=?2 AND m.role='assistant'",
            params![message_id, source_book_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(message_session) = message_session else {
        return Ok(out);
    };
    if message_session != ws_session {
        return Ok(out);
    }

    let card: Option<String> = conn
        .query_row(
            "SELECT result_json FROM messages WHERE id=?1",
            [&message_id],
            |r| r.get(0),
        )
        .optional()?;
    let has_card = card
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .map(|v| v.get("bookSetup").and_then(Value::as_object).is_some())
        .unwrap_or(false);
    if !has_card {
        return Ok(out);
    }

    out["eligible"] = json!(true);
    out["sourceSessionId"] = json!(ws_session);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::books::{create_book, create_session};

    fn open() -> (tempfile::TempDir, Db) {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(d.path(), None).unwrap();
        (d, db)
    }

    fn card() -> Value {
        json!({"bookSetup":{"titles":["新书"],"saved":false,"files":[
            {"name":"设定.md","group":"设定","content":"设定内容"}]}})
    }

    fn insert_msg(db: &Db, id: &str, session_id: &str, role: &str, result: &Value) {
        db.exec(
            "INSERT INTO messages(id,session_id,role,content,result_json) VALUES(?1,?2,?3,'预览',?4)",
            &[&id, &session_id, &role, &result.to_string()],
        )
        .unwrap();
    }

    fn setting(db: &Db, key: &str) -> Option<String> {
        db.q_json("SELECT value FROM settings WHERE key=?1", &[&key])
            .ok()
            .and_then(|rows| {
                rows.first()
                    .and_then(|r| r["value"].as_str().map(str::to_string))
            })
    }

    /// begin 原子建书 + 共创会话 + draft 标记，并初始化新书目录。
    #[test]
    fn begin_creates_book_session_and_marker() {
        let (_d, db) = open();
        let args = json!({
            "title":"星空之海","genre":"科幻","pov":"第一人称",
            "form":"长篇","platform":"起点","audience":"青年"});
        let out = begin(&db, &args).unwrap();
        assert_eq!(out["ok"], true);
        let book_id = out["book"]["id"].as_str().unwrap().to_string();
        let session_id = out["session"]["id"].as_str().unwrap().to_string();
        // book 形状与 list_books 一致。
        for key in [
            "id",
            "title",
            "genre",
            "pov",
            "status",
            "coverChar",
            "wordCount",
        ] {
            assert!(out["book"].get(key).is_some(), "book 缺少字段 {}", key);
        }
        assert_eq!(out["book"]["title"], json!("星空之海"));
        assert_eq!(out["book"]["genre"], json!("科幻"));
        assert_eq!(out["book"]["pov"], json!("第一人称"));
        assert_eq!(out["book"]["coverChar"], json!("星"));
        // session 形状与 list_sessions 一致。
        assert_eq!(out["session"]["bookId"], json!(book_id));
        assert_eq!(out["session"]["title"], json!("新书共创"));
        assert_eq!(out["session"]["msgCount"], json!(0));
        // 只有一部作品、一个会话，且列表可见。
        assert_eq!(crate::books::list_books(&db).as_array().unwrap().len(), 1);
        assert_eq!(
            crate::books::list_sessions(&db, &book_id)
                .as_array()
                .unwrap()
                .len(),
            1
        );
        // 标记记录 draft 及配置，且会话归属正确。
        let raw = setting(&db, &workspace_key(&book_id)).expect("工作区标记必须写入");
        let ws: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(ws["bookId"], json!(book_id));
        assert_eq!(ws["sessionId"], json!(session_id));
        assert_eq!(ws["state"], json!("draft"));
        assert_eq!(ws["form"], json!("长篇"));
        assert_eq!(ws["platform"], json!("起点"));
        assert_eq!(ws["audience"], json!("青年"));
        assert_eq!(ws["title"], json!("星空之海"));
        // 目录已初始化（非空树），且 begin 未因锁重入死锁。
        assert!(!crate::files::scan_tree(&db, &book_id)
            .as_array()
            .unwrap()
            .is_empty());
    }

    /// 未给标题时使用「未命名新书」占位，且不因缺省参数失败。
    #[test]
    fn begin_without_title_uses_placeholder() {
        let (_d, db) = open();
        let out = begin(&db, &json!({})).unwrap();
        assert_eq!(out["book"]["title"], json!(UNNAMED_TITLE));
        assert_eq!(out["book"]["pov"], json!("第三人称"));
        let book_id = out["book"]["id"].as_str().unwrap().to_string();
        let raw = setting(&db, &workspace_key(&book_id)).unwrap();
        let ws: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(ws["state"], json!("draft"));
        // 未提供的可选配置不得被塞入 null。
        assert!(ws.get("form").is_none());
        assert!(ws.get("platform").is_none());
        assert!(ws.get("audience").is_none());
    }

    /// 非法参数在创建任何作品之前拒绝。
    #[test]
    fn begin_rejects_invalid_args_before_creating() {
        let (_d, db) = open();
        assert!(begin(&db, &json!({"title": 123})).is_err());
        assert!(begin(&db, &json!({"title": "长".repeat(201)})).is_err());
        assert_eq!(crate::books::list_books(&db).as_array().unwrap().len(), 0);
        assert!(db
            .q_json(
                "SELECT key FROM settings WHERE key LIKE 'book_setup_workspace__%'",
                &[]
            )
            .unwrap()
            .is_empty());
    }

    /// 只有服务端工作区会话内的 assistant 建书卡 eligible。
    #[test]
    fn context_eligible_only_for_workspace_session_card() {
        let (_d, db) = open();
        let out = begin(&db, &json!({"title":"新书"})).unwrap();
        let book_id = out["book"]["id"].as_str().unwrap().to_string();
        let ws_session = out["session"]["id"].as_str().unwrap().to_string();
        insert_msg(&db, "ws-card", &ws_session, "assistant", &card());
        let ctx = context(&db, &json!({"sourceBookId":book_id,"messageId":"ws-card"})).unwrap();
        assert_eq!(ctx["eligible"], json!(true));
        assert_eq!(ctx["sourceBookId"], json!(book_id));
        assert_eq!(ctx["sourceSessionId"], json!(ws_session));
        assert_eq!(ctx["title"], json!("新书"));

        // 同一部书但非工作区会话：不可信。
        let other = create_session(&db, &book_id, "别的会话").unwrap();
        let other_id = other["id"].as_str().unwrap().to_string();
        insert_msg(&db, "other-card", &other_id, "assistant", &card());
        let ctx = context(
            &db,
            &json!({"sourceBookId":book_id,"messageId":"other-card"}),
        )
        .unwrap();
        assert_eq!(ctx["eligible"], json!(false));
        assert_eq!(ctx["title"], json!("新书"));

        // 工作区会话里的 user 消息不算建书卡。
        insert_msg(&db, "user-msg", &ws_session, "user", &card());
        assert_eq!(
            context(&db, &json!({"sourceBookId":book_id,"messageId":"user-msg"})).unwrap()
                ["eligible"],
            json!(false)
        );

        // 工作区会话里没有 bookSetup 的 assistant 消息不算。
        insert_msg(&db, "plain", &ws_session, "assistant", &json!({"other":1}));
        assert_eq!(
            context(&db, &json!({"sourceBookId":book_id,"messageId":"plain"})).unwrap()["eligible"],
            json!(false)
        );
    }

    /// 普通旧书（无工作区标记）永远不可信；消息归属另一部书也不可信。
    #[test]
    fn context_rejects_plain_books_and_cross_book_messages() {
        let (_d, db) = open();
        let plain = create_book(&db, "旧作品", "怪谈", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        let sid = create_session(&db, &plain, "共创").unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        insert_msg(&db, "plain-card", &sid, "assistant", &card());
        let ctx = context(&db, &json!({"sourceBookId":plain,"messageId":"plain-card"})).unwrap();
        assert_eq!(ctx["eligible"], json!(false));
        assert_eq!(ctx["title"], json!("旧作品"));
        assert_eq!(ctx["sourceSessionId"], json!(""));

        // 工作区卡被跨书查询时不可信。
        let ws = begin(&db, &json!({"title":"新书"})).unwrap();
        let ws_session = ws["session"]["id"].as_str().unwrap().to_string();
        insert_msg(&db, "ws-card", &ws_session, "assistant", &card());
        let ctx = context(&db, &json!({"sourceBookId":plain,"messageId":"ws-card"})).unwrap();
        assert_eq!(ctx["eligible"], json!(false));
    }

    /// 缺参/已删目标只返回明确 ineligible，不自动解绑、不报错。
    #[test]
    fn context_is_ineligible_without_mutating_state() {
        let (_d, db) = open();
        let ws = begin(&db, &json!({"title":"新书"})).unwrap();
        let book_id = ws["book"]["id"].as_str().unwrap().to_string();
        let ws_session = ws["session"]["id"].as_str().unwrap().to_string();
        insert_msg(&db, "ws-card", &ws_session, "assistant", &card());

        // 缺参：eligible=false，字段仍为固定形状。
        for args in [
            json!({}),
            json!({"sourceBookId":book_id}),
            json!({"messageId":"ws-card"}),
            json!({"sourceBookId":"","messageId":""}),
        ] {
            let ctx = context(&db, &args).unwrap();
            assert_eq!(ctx["eligible"], json!(false));
            assert!(ctx.get("sourceBookId").is_some());
            assert!(ctx.get("sourceSessionId").is_some());
            assert!(ctx.get("title").is_some());
        }

        // 目标已删：明确 ineligible，且工作区标记仍在（不自动解绑/迁移）。
        db.exec("UPDATE books SET deleted_at=1 WHERE id=?1", &[&book_id])
            .unwrap();
        let ctx = context(&db, &json!({"sourceBookId":book_id,"messageId":"ws-card"})).unwrap();
        assert_eq!(ctx["eligible"], json!(false));
        assert!(setting(&db, &workspace_key(&book_id)).is_some());
    }
}
