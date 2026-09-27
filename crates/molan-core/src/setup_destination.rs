//! Explicit, create-only book-setup saving. Model-supplied destinations are not trusted.
//!
//! destination 支持三种模式：
//! - `new`：新建独立作品并保存资料（重试不重复建书）；
//! - `existing`：保存到显式指定的已有作品，绝不改名；
//! - `source`：仅限 `setup_workspace` 创建的可信共创工作区。完成原书命名并保留原共创
//!   会话，不新建第二部作品、不迁移消息。改名与工作区 done 标记同事务提交，因此
//!   「同一卡原绑定重试」幂等，而「另一张卡二次改名」被拒。非法请求在改名之前预检。
use crate::{db::Db, files, stats::now_ms};
use anyhow::{anyhow, bail, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Mutex;

static SAVE_LOCK: Mutex<()> = Mutex::new(());
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}
fn active_title(conn: &rusqlite::Connection, id: &str) -> Result<String> {
    conn.query_row(
        "SELECT title FROM books WHERE id=?1 AND deleted_at IS NULL",
        [id],
        |r| r.get(0),
    )
    .map_err(|_| anyhow!("目标作品不存在或已删除"))
}
/// 锁内解析 source 模式目标：只认服务端创建的可信工作区，且必须处于 draft（未完成）。
/// 已有 new/existing 绑定卡不经此路径，因此不会被工作区规则二次改名。
fn source_title(conn: &rusqlite::Connection, book_id: &str, title: &str) -> Result<String> {
    let workspace = crate::setup_workspace::workspace_record_conn(conn, book_id)?
        .ok_or_else(|| anyhow!("该作品不是可信共创工作区，不能改名"))?;
    if text(&workspace, "state") != "draft" {
        bail!("共创工作区已完成，不能再次改名");
    }
    let session_id = text(&workspace, "sessionId");
    if session_id.is_empty() {
        bail!("共创工作区记录损坏，请重新开始共创");
    }
    let session_book: Option<String> = conn
        .query_row(
            "SELECT book_id FROM sessions WHERE id=?1",
            [session_id],
            |r| r.get(0),
        )
        .optional()?;
    if session_book.as_deref() != Some(book_id) {
        bail!("共创工作区会话与作品不一致");
    }
    Ok(title.to_owned())
}
fn store(
    conn: &rusqlite::Connection,
    message_id: &str,
    result: &Value,
    destination: &Value,
) -> Result<()> {
    let changed = conn.execute(
        "UPDATE messages SET result_json=?1 WHERE id=?2",
        params![result.to_string(), message_id],
    )?;
    if changed != 1 {
        bail!("原始建书卡已不存在，保存记录未确认");
    }
    let summary = format!(
        "建书资料保存记录：{}\n{}\n原始共创会话保留在来源作品。",
        text(destination, "title"),
        result["bookSetup"]["savedFiles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| format!("{}/{}", text(v, "group"), text(v, "name")))
            .collect::<Vec<_>>()
            .join("\n")
    );
    conn.execute(
        "UPDATE messages SET content=?1 WHERE id=?2",
        params![summary, text(destination, "receiptMessageId")],
    )?;
    Ok(())
}

pub fn save_selection(db: &Db, args: &Value) -> Result<Value> {
    if args["confirmed"].as_bool() != Some(true) {
        bail!("请先确认目标作品和每项保存目录");
    }
    let _serial = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let source = text(args, "sourceBookId");
    let message_id = text(args, "messageId");
    let key = format!("setup_destination__{}", message_id);
    let mut result: Value = {
        let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
        active_title(&conn, source)?;
        let raw: String = conn.query_row("SELECT m.result_json FROM messages m JOIN sessions s ON s.id=m.session_id WHERE m.id=?1 AND s.book_id=?2 AND m.role='assistant'", params![message_id,source],|r|r.get(0))
            .map_err(|_|anyhow!("建书消息不存在或不属于来源作品"))?;
        serde_json::from_str(&raw)?
    };
    let originals = result["bookSetup"]["files"]
        .as_array()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("消息没有可保存的建书资料"))?
        .clone();
    let requested = args["files"]
        .as_array()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("请选择至少一项资料"))?;
    let mut indices = HashSet::new();
    let mut targets = HashSet::new();
    let mut selection = Vec::new();
    for item in requested {
        let index = item["index"]
            .as_u64()
            .filter(|i| *i < originals.len() as u64)
            .ok_or_else(|| anyhow!("资料索引无效"))? as usize;
        let group = text(item, "group");
        let name = text(item, "name");
        if !["设定", "细纲", "参考"].contains(&group) {
            bail!("建书资料目录只能是设定、细纲或参考");
        }
        if name.trim() != name
            || name.is_empty()
            || name.len() > 240
            || files::safe_name(name) != name
            || !name.ends_with(".md")
        {
            bail!("文件名无效，请使用不含路径的 .md 文件名");
        }
        if !indices.insert(index) || !targets.insert((group.to_owned(), name.to_lowercase())) {
            bail!("重复的资料或目标路径");
        }
        let content = originals[index]["content"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("资料内容为空"))?;
        selection.push((index, group.to_owned(), name.to_owned(), content.to_owned()));
    }
    let request_dest = &args["destination"];
    let mode = text(request_dest, "mode");
    if mode != "new" && mode != "existing" && mode != "source" {
        bail!("必须明确选择新建作品或已有作品");
    }
    let new_title = text(request_dest, "title").trim();
    if mode == "new" && (new_title.is_empty() || new_title.chars().count() > 200) {
        bail!("请输入有效的新作品书名");
    }
    // Existing bindings remain retryable after the workspace becomes done.
    if mode == "source" && (new_title.is_empty() || new_title.chars().count() > 200) {
        bail!("请输入有效的新作品书名");
    }
    let destination;
    {
        // The binding and new work are committed together; retries cannot create a second work.
        let mut conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction()?;
        let binding: Option<String> = tx
            .query_row("SELECT value FROM settings WHERE key=?1", [&key], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(raw) = binding {
            destination = serde_json::from_str::<Value>(&raw)?;
            active_title(&tx, text(&destination, "bookId"))?;
            let matches = if mode == "existing" {
                text(request_dest, "bookId") == text(&destination, "bookId")
            } else if mode == "source" {
                // 原绑定重试：同一可信作品、同一书名才算幂等；workspace 完成后不再二次改名。
                text(&destination, "mode") == "source"
                    && text(&destination, "bookId") == source
                    && new_title == text(&destination, "title")
            } else {
                destination["created"] == true && new_title == text(&destination, "title")
            };
            if !matches {
                bail!("本卡已绑定另一目标作品，请保持原保存位置");
            }
        } else {
            let now = now_ms();
            if mode == "source" {
                // 可信工作区完成：只改原书书名并保留原共创会话，不新建第二部作品、不迁移消息。
                let title = source_title(&tx, source, new_title)?;
                let workspace = crate::setup_workspace::workspace_record_conn(&tx, source)?
                    .ok_or_else(|| anyhow!("共创工作区不存在，请重新开始共创"))?;
                let session_id = text(&workspace, "sessionId").to_owned();
                let message_session: String = tx.query_row(
                    "SELECT session_id FROM messages WHERE id=?1",
                    [message_id],
                    |r| r.get(0),
                )?;
                if message_session != session_id {
                    bail!("建书消息不属于本次共创会话，不能改名");
                }
                let changed = tx.execute(
                    "UPDATE books SET title=?1, genre=CASE WHEN ?2='' THEN genre ELSE ?2 END, updated_at=?3 WHERE id=?4 AND deleted_at IS NULL",
                    params![title, text(&result["bookSetup"], "genre"), now, source],
                )?;
                if changed != 1 {
                    bail!("目标作品不存在或已删除");
                }
                let mut updated = workspace.clone();
                updated["title"] = json!(title);
                updated["state"] = json!("done");
                tx.execute(
                    "UPDATE settings SET value=?1 WHERE key=?2",
                    params![
                        updated.to_string(),
                        crate::setup_workspace::workspace_key(source)
                    ],
                )?;
                destination = json!({
                    "bookId": source, "title": title, "created": false,
                    "mode": "source", "sourceBookId": source,
                    "sourceSessionId": session_id, "sourceMessageId": message_id,
                    "sessionId": session_id,
                });
            } else {
                let (book_id, title, created) = if mode == "new" {
                    let id = uuid::Uuid::new_v4().to_string();
                    let genre = text(&result["bookSetup"], "genre");
                    let cover = new_title.chars().next().unwrap().to_string();
                    tx.execute("INSERT INTO books(id,title,genre,pov,status,cover_char,created_at,updated_at) VALUES(?1,?2,?3,'第三人称','构思中',?4,?5,?5)",params![id,new_title,genre,cover,now])?;
                    (id, new_title.to_owned(), true)
                } else {
                    let id = text(request_dest, "bookId").to_owned();
                    let title = active_title(&tx, &id)?;
                    (id, title, false)
                };
                let session_id = uuid::Uuid::new_v4().to_string();
                let receipt_id = uuid::Uuid::new_v4().to_string();
                tx.execute("INSERT INTO sessions(id,book_id,title,preview,msg_count,created_at,updated_at) VALUES(?1,?2,'建书资料保存记录','作者确认的资料保存位置',1,?3,?3)",params![session_id,book_id,now])?;
                tx.execute("INSERT INTO messages(id,session_id,role,content,created_at) VALUES(?1,?2,'assistant','目标作品已绑定，资料等待保存',?3)",params![receipt_id,session_id,now])?;
                destination = json!({"bookId":book_id,"title":title,"created":created,"sessionId":session_id,"receiptMessageId":receipt_id});
            }
            tx.execute(
                "INSERT INTO settings(key,value) VALUES(?1,?2)",
                params![key, destination.to_string()],
            )?;
            // Discard old or model-forged success fields when no trusted binding exists.
            result["bookSetup"]["savedFiles"] = json!([]);
            result["bookSetup"]["saved"] = json!(false);
        }
        result["bookSetup"]["destination"] = destination.clone();
        if !result["bookSetup"]["savedFiles"].is_array() {
            result["bookSetup"]["savedFiles"] = json!([]);
        }
        let receipts = result["bookSetup"]["savedFiles"].as_array().unwrap();
        for (index, group, name, _) in &selection {
            for receipt in receipts {
                if receipt["index"] == json!(index)
                    && (text(receipt, "group") != group || text(receipt, "name") != name)
                {
                    bail!("已保存资料不能改换位置，请在文件管理中处理");
                }
                if receipt["index"] != json!(index)
                    && text(receipt, "group") == group
                    && text(receipt, "name").eq_ignore_ascii_case(name)
                {
                    bail!("目标路径已被本卡另一项资料使用");
                }
            }
        }
        store(&tx, message_id, &result, &destination)?;
        tx.commit()?;
    }
    let target = text(&destination, "bookId");
    for (index, group, name, content) in selection {
        let attempt = files::write_ai_file_checked(db, target, &group, &name, &content, || {
            let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
            active_title(&conn, target)?;
            Ok(())
        });
        if let Err(error) = attempt {
            // Read through the checked path API; only exact bytes qualify as an idempotent retry.
            if files::read_file(db, target, &group, &name).as_deref() != Some(content.as_str()) {
                return Err(anyhow!(
                    "保存到《{}》/{}/{}失败：{}。已完成项保留，可修正未存项后重试。",
                    text(&destination, "title"),
                    group,
                    name,
                    error
                ));
            }
        }
        let receipts = result["bookSetup"]["savedFiles"].as_array_mut().unwrap();
        if !receipts.iter().any(|r| r["index"] == json!(index)) {
            receipts.push(json!({"index":index,"group":group,"name":name}));
        }
        result["bookSetup"]["saved"] = json!(receipts.len() == originals.len());
        let mut conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction()?;
        store(&tx, message_id, &result, &destination)?;
        tx.commit()?;
    }
    Ok(
        json!({"ok":true,"result":result,"destination":destination,"savedFiles":result["bookSetup"]["savedFiles"],"complete":result["bookSetup"]["saved"]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Db, String, Value) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(tmp.path(), None).unwrap();
        let book = crate::books::create_book(&db, "旧作品", "怪谈", "第三人称");
        let id = text(&book, "id").to_owned();
        let session = crate::books::create_session(&db, &id, "共创").unwrap();
        let result = json!({"bookSetup":{"saved":true,"destination":{"bookId":"forged"},"savedFiles":[{"index":0}],"files":[
            {"name":"设定.md","group":"设定","content":"设定内容"},{"name":"第1章细纲.md","group":"设定","content":"细纲内容"}]}});
        db.exec("INSERT INTO messages(id,session_id,role,content,result_json) VALUES('msg',?1,'assistant','preview',?2)",&[&text(&session,"id"),&result.to_string()]).unwrap();
        let args = json!({"sourceBookId":id,"messageId":"msg","confirmed":true,"destination":{"mode":"new","title":"新历史作品"},"files":[{"index":0,"group":"设定","name":"设定.md"},{"index":1,"group":"细纲","name":"第1章细纲.md"}]});
        (tmp, db, id, args)
    }
    #[test]
    fn new_work_retry_and_explicit_groups() {
        let (_tmp, db, source, args) = fixture();
        let out = save_selection(&db, &args).unwrap();
        let target = text(&out["destination"], "bookId");
        assert_ne!(target, source);
        assert_eq!(out["complete"], true);
        assert_eq!(
            files::read_file(&db, target, "细纲", "第1章细纲.md").unwrap(),
            "细纲内容"
        );
        assert!(files::read_file(&db, &source, "细纲", "第1章细纲.md").is_none());
        assert_eq!(
            active_title(&db.conn.lock().unwrap(), &source).unwrap(),
            "旧作品"
        );
        assert_eq!(
            save_selection(&db, &args).unwrap()["destination"],
            out["destination"]
        );
        assert_eq!(crate::books::list_books(&db).as_array().unwrap().len(), 2);
        assert_eq!(
            crate::books::list_messages(&db, text(&out["destination"], "sessionId"))
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn rejects_invalid_inputs_before_creating_work() {
        let (_tmp, db, _source, args) = fixture();
        for patch in [
            json!({"confirmed":false}),
            json!({"sourceBookId":"wrong"}),
            json!({"messageId":"wrong"}),
            json!({"destination":{"mode":"existing","bookId":"wrong"}}),
            json!({"files":[{"index":0,"group":"正文","name":"x.md"}]}),
            json!({"files":[{"index":0,"group":"设定","name":"../x.md"}]}),
        ] {
            let mut a = args.clone();
            for (k, v) in patch.as_object().unwrap() {
                a[k] = v.clone();
            }
            assert!(save_selection(&db, &a).is_err());
            assert_eq!(crate::books::list_books(&db).as_array().unwrap().len(), 1);
        }
    }
    #[test]
    fn partial_conflict_existing_not_renamed_and_retry() {
        let (_tmp, db, source, mut args) = fixture();
        args["destination"] = json!({"mode":"existing","bookId":source});
        files::write_file_new(&db, &source, "细纲", "第1章细纲.md", "作者内容").unwrap();
        assert!(save_selection(&db, &args).is_err());
        assert_eq!(
            files::read_file(&db, &source, "细纲", "第1章细纲.md").unwrap(),
            "作者内容"
        );
        let raw: String = db
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT result_json FROM messages WHERE id='msg'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let r: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(r["bookSetup"]["saved"], false);
        assert_eq!(r["bookSetup"]["savedFiles"].as_array().unwrap().len(), 1);
        args["files"][1]["name"] = json!("第1章细纲_另存.md");
        let out = save_selection(&db, &args).unwrap();
        assert_eq!(out["complete"], true);
        assert_eq!(
            active_title(&db.conn.lock().unwrap(), &source).unwrap(),
            "旧作品"
        );
        args["destination"] = json!({"mode":"new","title":"不允许换书"});
        assert!(save_selection(&db, &args).is_err());
    }

    /// 可信共创工作区 fixture：begin 建书+会话，插入本会话内的 assistant 建书卡。
    fn workspace_fixture() -> (tempfile::TempDir, Db, String, String, Value) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(tmp.path(), None).unwrap();
        let out = crate::setup_workspace::begin(&db, &json!({"title":"未命名新书","genre":"怪谈"}))
            .unwrap();
        let book_id = out["book"]["id"].as_str().unwrap().to_string();
        let session_id = out["session"]["id"].as_str().unwrap().to_string();
        let result = json!({"bookSetup":{"genre":"怪谈","saved":false,"files":[
            {"name":"设定.md","group":"设定","content":"设定内容"},
            {"name":"第1章细纲.md","group":"细纲","content":"细纲内容"}]}});
        db.exec(
            "INSERT INTO messages(id,session_id,role,content,result_json) VALUES('ws-msg',?1,'assistant','preview',?2)",
            &[&session_id, &result.to_string()],
        )
        .unwrap();
        let args = json!({
            "sourceBookId":book_id,"messageId":"ws-msg","confirmed":true,
            "destination":{"mode":"source","title":"正式书名"},
            "files":[
                {"index":0,"group":"设定","name":"设定.md"},
                {"index":1,"group":"细纲","name":"第1章细纲.md"}]});
        (tmp, db, book_id, session_id, args)
    }

    fn setting(db: &Db, key: &str) -> Option<String> {
        db.q_json("SELECT value FROM settings WHERE key=?1", &[&key])
            .ok()
            .and_then(|rows| {
                rows.first()
                    .and_then(|r| r["value"].as_str().map(str::to_string))
            })
    }

    /// source 模式：改原书书名、保留原会话，不新建第二部作品；原卡重试幂等。
    #[test]
    fn source_mode_renames_original_book_and_keeps_session() {
        let (_tmp, db, book_id, session_id, args) = workspace_fixture();
        let out = save_selection(&db, &args).unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["destination"]["bookId"], json!(book_id));
        assert_eq!(out["destination"]["created"], false);
        assert_eq!(out["destination"]["mode"], json!("source"));
        assert_eq!(out["destination"]["sourceBookId"], json!(book_id));
        assert_eq!(out["destination"]["sourceSessionId"], json!(session_id));
        assert_eq!(out["destination"]["sessionId"], json!(session_id));
        assert_eq!(out["destination"]["sourceMessageId"], json!("ws-msg"));
        assert_eq!(out["complete"], true);
        // 原书改名，未新建第二部作品。
        assert_eq!(
            active_title(&db.conn.lock().unwrap(), &book_id).unwrap(),
            "正式书名"
        );
        assert_eq!(crate::books::list_books(&db).as_array().unwrap().len(), 1);
        // 原共创会话保留且仍可重开。
        let sessions = crate::books::list_sessions(&db, &book_id);
        let kept = sessions
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == json!(session_id))
            .cloned()
            .expect("原共创会话必须保留");
        assert_eq!(kept["title"], json!("新书共创"));
        assert!(!crate::books::list_messages(&db, &session_id)
            .as_array()
            .unwrap()
            .is_empty());
        // 资料写入原书。
        assert_eq!(
            files::read_file(&db, &book_id, "细纲", "第1章细纲.md").unwrap(),
            "细纲内容"
        );
        // 工作区标记变为 done 且记录新书名。
        let raw = setting(&db, &format!("book_setup_workspace__{}", book_id)).unwrap();
        let ws: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(ws["state"], json!("done"));
        assert_eq!(ws["title"], json!("正式书名"));
        assert_eq!(ws["sessionId"], json!(session_id));
        // 原绑定重试：同一卡、同一书名幂等，仍不建第二部作品。
        let again = save_selection(&db, &args).unwrap();
        assert_eq!(again["destination"], out["destination"]);
        assert_eq!(crate::books::list_books(&db).as_array().unwrap().len(), 1);
        assert_eq!(
            active_title(&db.conn.lock().unwrap(), &book_id).unwrap(),
            "正式书名"
        );
    }

    #[test]
    fn source_mode_rejects_other_session_in_same_workspace() {
        let (_tmp, db, book, _, mut args) = workspace_fixture();
        let other = crate::books::create_session(&db, &book, "另一个会话").unwrap();
        db.exec(
            "UPDATE messages SET session_id=?1 WHERE id='ws-msg'",
            &[&other["id"].as_str().unwrap()],
        )
        .unwrap();
        assert!(save_selection(&db, &args)
            .unwrap_err()
            .to_string()
            .contains("共创会话"));
        assert_eq!(
            active_title(&db.conn.lock().unwrap(), &book).unwrap(),
            "未命名新书"
        );
        args["destination"] = json!({"mode":"existing","bookId":book});
        assert!(save_selection(&db, &args).is_ok());
    }

    /// done 之后另一张卡不得二次改名，也不得改回旧书名。
    #[test]
    fn source_mode_second_card_cannot_rename_again() {
        let (_tmp, db, book_id, session_id, _args) = workspace_fixture();
        let args = json!({
            "sourceBookId":book_id,"messageId":"ws-msg","confirmed":true,
            "destination":{"mode":"source","title":"第一次书名"},
            "files":[{"index":0,"group":"设定","name":"设定.md"}]});
        save_selection(&db, &args).unwrap();
        let result = json!({"bookSetup":{"files":[
            {"name":"补充.md","group":"设定","content":"补充内容"}]}});
        db.exec(
            "INSERT INTO messages(id,session_id,role,content,result_json) VALUES('ws-msg-2',?1,'assistant','preview',?2)",
            &[&session_id, &result.to_string()],
        )
        .unwrap();
        let second = json!({
            "sourceBookId":book_id,"messageId":"ws-msg-2","confirmed":true,
            "destination":{"mode":"source","title":"第二次书名"},
            "files":[{"index":0,"group":"设定","name":"补充.md"}]});
        let err = save_selection(&db, &second).unwrap_err();
        assert!(err.to_string().contains("已完成"), "{}", err);
        assert_eq!(
            active_title(&db.conn.lock().unwrap(), &book_id).unwrap(),
            "第一次书名"
        );
        assert!(files::read_file(&db, &book_id, "设定", "补充.md").is_none());
        assert_eq!(crate::books::list_books(&db).as_array().unwrap().len(), 1);
    }

    /// 旧书（无工作区标记）绝不能借 source 模式改名；校验必须发生在改名之前。
    #[test]
    fn source_mode_rejects_untrusted_book_before_rename() {
        let (_tmp, db, source, mut args) = fixture();
        args["destination"] = json!({"mode":"source","title":"偷改名"});
        let err = save_selection(&db, &args).unwrap_err();
        assert!(err.to_string().contains("可信共创工作区"), "{}", err);
        assert_eq!(
            active_title(&db.conn.lock().unwrap(), &source).unwrap(),
            "旧作品"
        );
        assert_eq!(crate::books::list_books(&db).as_array().unwrap().len(), 1);
        assert!(setting(&db, "setup_destination__msg").is_none());
    }

    /// source 模式非法请求在改名/建绑定之前拒绝，工作区仍为 draft。
    #[test]
    fn source_mode_invalid_request_rejected_before_rename() {
        let (_tmp, db, book_id, _session_id, mut args) = workspace_fixture();
        args["files"] = json!([{"index":0,"group":"正文","name":"设定.md"}]);
        assert!(save_selection(&db, &args).is_err());
        assert_eq!(
            active_title(&db.conn.lock().unwrap(), &book_id).unwrap(),
            "未命名新书"
        );
        assert!(setting(&db, "setup_destination__ws-msg").is_none());
        let raw = setting(&db, &format!("book_setup_workspace__{}", book_id)).unwrap();
        let ws: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(ws["state"], json!("draft"));
    }
}
