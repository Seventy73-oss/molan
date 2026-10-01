// molan-core: 建书卡「保存到指定作品」——显式目标、绝不改书名、绝不覆盖。
// IPC save_book_setup_selection：args={sourceBookId,messageId,confirmed:true,
//   destination:{mode:"new"|"existing",bookId?,title?},files:[{index,group,name}]}
// 硬约束：内容只取持久化 bookSetup.files[index].content（忽略客户端 content）；
// 目录仅限设定/细纲/参考；existing 必须显式 bookId、绝不改名任何作品；
// new 仅显式 title 非空时建独立书并立即持久化绑定（重试不重复建书）；
// 冲突不覆盖、部分保存落盘可重试、全部完成才 saved=true；新书带可重开建书记录会话。
use crate::db::Db;
use crate::files;
use crate::stats::now_ms;
use anyhow::{anyhow, bail, Result};
use rusqlite::ToSql;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// 建书资料只允许落到这三个目录；正式正文/待审/自建组一律拒绝。
const ALLOWED_GROUPS: [&str; 3] = ["设定", "细纲", "参考"];

/// 本 endpoint 的进程内串行锁：防止双击/并发请求重复建书或交错写同一路径。
/// 这里不持有 db.fs_lock，写文件交给 files 模块各自取锁（锁序 fs_lock -> conn），
/// 避免「持 fs_lock 再调用内部也取 fs_lock 的函数」造成自死锁。
static OP_LOCK: Mutex<()> = Mutex::new(());

struct Item {
    index: usize,
    group: String,
    name: String,
    content: String,
}

/// 保存建书卡选中的资料到显式目标作品。返回更新后的源消息 result 与保存回执。
pub fn save_selection(db: &Db, args: &Value) -> Result<Value> {
    let _op = OP_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // ---------- 参数 ----------
    let source_book_id = required_str(args, "sourceBookId")?;
    let message_id = required_str(args, "messageId")?;
    if args.get("confirmed") != Some(&Value::Bool(true)) {
        bail!("请先在建书预览中明确确认，服务端不代作者落库");
    }
    let files_arg = args
        .get("files")
        .and_then(Value::as_array)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("请选择要保存的建书资料"))?;
    let dest = args
        .get("destination")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("请先选择目标作品"))?;
    let mode = dest.get("mode").and_then(Value::as_str).unwrap_or("");

    // ---------- 读取源 assistant 消息（JOIN sessions 校验 sourceBookId）----------
    let row = db
        .q_json(
            "SELECT m.result_json AS result_json, s.book_id AS book_id FROM messages m JOIN sessions s ON s.id=m.session_id WHERE m.id=?1 AND m.role='assistant'",
            &[&message_id as &dyn ToSql],
        )?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("建书卡消息不存在"))?;
    if row["bookId"].as_str().unwrap_or("") != source_book_id {
        bail!("建书卡消息不属于该作品，拒绝保存");
    }
    if !files::valid_book_id(db, &source_book_id) {
        bail!("源作品不存在或已删除");
    }
    let mut result: Value = serde_json::from_str(row["resultJson"].as_str().unwrap_or(""))
        .map_err(|_| anyhow!("建书卡数据无效"))?;
    let setup = result
        .get("bookSetup")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("消息不包含建书预览"))?
        .clone();
    let src_files = setup
        .get("files")
        .and_then(Value::as_array)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("建书卡没有可保存的资料"))?
        .clone();
    let mut receipts: Vec<Value> = setup
        .get("savedFiles")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let bound = setup.get("destination").and_then(Value::as_object).cloned();

    // ---------- 预先校验全部请求项（内容只认持久化的 files[index].content）----------
    let mut items: Vec<Item> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut path_content: BTreeMap<(String, String), String> = BTreeMap::new();
    for raw in files_arg {
        let o = raw.as_object().ok_or_else(|| anyhow!("资料项格式无效"))?;
        let idx = o
            .get("index")
            .and_then(Value::as_i64)
            .filter(|i| *i >= 0)
            .ok_or_else(|| anyhow!("资料序号非法"))? as usize;
        if idx >= src_files.len() {
            bail!("资料序号越界：{}（共 {} 项）", idx, src_files.len());
        }
        if !seen.insert(idx) {
            bail!("资料序号重复：{}", idx);
        }
        let group = validate_group(o.get("group").and_then(Value::as_str).unwrap_or(""))?;
        let name = validate_file_name(o.get("name").and_then(Value::as_str).unwrap_or(""))?;
        let content = src_files[idx]
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("建书卡缺少第 {} 项资料内容", idx))?
            .to_string();
        if let Some(prev) = path_content.insert((group.clone(), name.clone()), content.clone()) {
            if prev != content {
                bail!("同一目标路径被两项资料占用：{}/{}", group, name);
            }
        }
        items.push(Item {
            index: idx,
            group,
            name,
            content,
        });
    }
    // 已保存项不得改存别处；同路径同内容视为幂等重试。
    for it in &items {
        if let Some((rg, rn)) = receipt_path(&receipts, it.index) {
            if rg != it.group || rn != it.name {
                bail!(
                    "第 {} 项已保存到 {}/{}，不得改存到其他位置",
                    it.index,
                    rg,
                    rn
                );
            }
        }
    }

    // ---------- 解析/建立目标绑定（预先持久化，重试不重复建书）----------
    let now = now_ms();
    let target_book_id: String;
    let mut record_message_id: Option<String> = None;
    if let Some(b) = &bound {
        let bid = b
            .get("bookId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if bid.is_empty() {
            bail!("建书卡绑定信息损坏，请刷新后重试");
        }
        let matches_bound =
            mode == "existing" && dest.get("bookId").and_then(Value::as_str) == Some(bid.as_str());
        if !matches_bound {
            bail!("建书卡已绑定目标作品，不能更换保存位置");
        }
        if !files::valid_book_id(db, &bid) {
            bail!("已绑定的目标作品不存在或已删除");
        }
        target_book_id = bid;
        record_message_id = b
            .get("recordMessageId")
            .and_then(Value::as_str)
            .map(str::to_string);
    } else {
        match mode {
            "existing" => {
                let bid = dest
                    .get("bookId")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("请选择已有的目标作品"))?
                    .to_string();
                if !files::valid_book_id(db, &bid) {
                    bail!("目标作品不存在或已删除");
                }
                let title = book_title(db, &bid)?;
                target_book_id = bid;
                // 立即持久化绑定：即使后续某个文件冲突，重试也不会换目标。
                set_destination(
                    &mut result,
                    &json!({"bookId": target_book_id, "title": title, "created": false}),
                );
                persist_message_result(db, &message_id, &result)?;
            }
            "new" => {
                let title = dest
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("新作品书名不能为空"))?
                    .to_string();
                let genre = setup
                    .get("genre")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let (book_id, session_id, record_id) = (
                    uuid::Uuid::new_v4().to_string(),
                    uuid::Uuid::new_v4().to_string(),
                    uuid::Uuid::new_v4().to_string(),
                );
                set_destination(
                    &mut result,
                    &json!({
                        "bookId": book_id, "title": title, "created": true,
                        "sessionId": session_id, "recordMessageId": record_id,
                    }),
                );
                set_saved_files(&mut result, &receipts, false);
                create_target_with_binding(
                    db,
                    &book_id,
                    &session_id,
                    &record_id,
                    &title,
                    &genre,
                    &message_id,
                    &result,
                    now,
                )?;
                target_book_id = book_id;
                record_message_id = Some(record_id);
            }
            other => bail!("目标模式无效：{:?}", other),
        }
    }

    // ---------- 逐项写入：部分成功立即持久化回执，重试可继续 ----------
    let mut first_error: Option<anyhow::Error> = None;
    for it in &items {
        let already =
            receipt_path(&receipts, it.index).is_some_and(|(g, n)| g == it.group && n == it.name);
        if already {
            match files::read_file(db, &target_book_id, &it.group, &it.name) {
                Some(cur) if cur == it.content => continue, // 幂等重试
                None => {}                                  // 回执在但文件丢失：补写
                Some(_) => {
                    first_error = Some(anyhow!(
                        "目标文件内容与建书资料不一致，拒绝覆盖：{}/{}",
                        it.group,
                        it.name
                    ));
                    break;
                }
            }
        }
        if let Err(e) = write_one(db, &target_book_id, &it.group, &it.name, &it.content) {
            first_error = Some(e);
            break;
        }
        upsert_receipt(&mut receipts, it.index, &it.group, &it.name);
        let complete = all_saved(&receipts, src_files.len());
        set_saved_files(&mut result, &receipts, complete);
        persist_message_result(db, &message_id, &result)?;
        if let Some(rid) = &record_message_id {
            persist_message_result(db, rid, &result)?;
        }
    }

    // 收尾：把最终回执与 saved 状态写回（含全部已保存的幂等场景）。
    let complete = first_error.is_none() && all_saved(&receipts, src_files.len());
    set_saved_files(&mut result, &receipts, complete);
    persist_message_result(db, &message_id, &result)?;
    if let Some(rid) = &record_message_id {
        persist_message_result(db, rid, &result)?;
    }
    if let Some(e) = first_error {
        return Err(e);
    }
    Ok(json!({
        "ok": true,
        "result": result,
        "destination": result["bookSetup"]["destination"].clone(),
        "savedFiles": receipts,
        "complete": complete,
    }))
}

// ---------- 内部实现 ----------

fn required_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("缺少参数 {}", key))
}

fn validate_group(raw: &str) -> Result<String> {
    let canon = files::normalize_group(raw);
    if !ALLOWED_GROUPS.contains(&canon.as_str()) {
        bail!("建书资料只能保存到「设定/细纲/参考」，收到目录：{:?}", raw);
    }
    Ok(canon)
}

/// 文件名安全：拒绝空、首尾空白、路径分隔符/控制字符等（safe_name 会改写即为非法）。
fn validate_file_name(raw: &str) -> Result<String> {
    // 空串由下方 safe_name 校验拒绝；这里只拦首尾空白与非法字符。
    if raw.trim() != raw {
        bail!("文件名不能包含首尾空白：{:?}", raw);
    }
    if files::safe_name(raw) != raw {
        bail!("文件名包含非法字符（禁止路径分隔符/控制字符）：{:?}", raw);
    }
    Ok(raw.to_string())
}

fn book_title(db: &Db, book_id: &str) -> Result<String> {
    db.q_json(
        "SELECT title FROM books WHERE id=?1 AND deleted_at IS NULL",
        &[&book_id as &dyn ToSql],
    )?
    .into_iter()
    .next()
    .and_then(|r| r["title"].as_str().map(str::to_string))
    .ok_or_else(|| anyhow!("目标作品不存在或已删除"))
}

/// 单文件写入：目标已存在且内容不同 => 明确冲突；内容一致 => 幂等成功。
fn write_one(db: &Db, book_id: &str, group: &str, name: &str, content: &str) -> Result<()> {
    match files::write_ai_file_checked(db, book_id, group, name, content, || Ok(())) {
        Ok(()) => {
            let src = serde_json::json!({"service": "book_setup"});
            crate::doc_write::record_ai_create(db, book_id, group, name, content, src);
            Ok(())
        }
        Err(e) => match files::read_file(db, book_id, group, name) {
            Some(cur) if cur == content => Ok(()),
            Some(_) => Err(anyhow!(
                "目标已存在且内容不同，拒绝覆盖：{}/{}",
                group,
                name
            )),
            None => Err(e),
        },
    }
}

/// 在一个事务里：建书 + 建「建书记录」会话 + 同卡副本 + 绑定源卡。
/// 绑定与建书同事务提交，双击/并发不会建出第二本。
#[allow(clippy::too_many_arguments)]
fn create_target_with_binding(
    db: &Db,
    book_id: &str,
    session_id: &str,
    record_id: &str,
    title: &str,
    genre: &str,
    source_message_id: &str,
    card: &Value,
    now: i64,
) -> Result<()> {
    let cover = title
        .chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "书".into());
    let card_str = card.to_string();
    let content = format!("建书资料已保存至《{}》。", title);
    let preview: String = content.chars().take(80).collect();
    {
        let mut guard = db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))?;
        let tx = guard.transaction()?;
        tx.execute(
            "INSERT INTO books(id,title,genre,pov,status,cover_char,word_count,chapter_count,created_at,updated_at) VALUES(?1,?2,?3,'第三人称','构思中',?4,0,0,?5,?5)",
            rusqlite::params![book_id, title, genre, cover, now],
        )?;
        tx.execute(
            "INSERT INTO sessions(id,book_id,title,preview,msg_count,created_at,updated_at) VALUES(?1,?2,'建书记录',?3,1,?4,?4)",
            rusqlite::params![session_id, book_id, preview, now],
        )?;
        tx.execute(
            "INSERT INTO messages(id,session_id,role,content,context_json,steps_json,result_json,created_at,interrupted) VALUES(?1,?2,'assistant',?3,'{}',NULL,?4,?5,0)",
            rusqlite::params![record_id, session_id, content, card_str, now],
        )?;
        tx.execute(
            "UPDATE messages SET result_json=?1 WHERE id=?2",
            rusqlite::params![card_str, source_message_id],
        )?;
        tx.commit()?;
    }
    // 目录初始化失败不阻断：绑定已持久化，文件写入会自动补齐父目录。
    if let Err(e) = files::ensure_book_dir(db, book_id) {
        eprintln!(
            "[molan-core] 新书目录初始化失败（绑定已持久化，重试可继续）：{}",
            e
        );
    }
    Ok(())
}

fn persist_message_result(db: &Db, message_id: &str, result: &Value) -> Result<()> {
    db.exec(
        "UPDATE messages SET result_json=?1 WHERE id=?2",
        &[&result.to_string() as &dyn ToSql, &message_id as &dyn ToSql],
    )?;
    Ok(())
}

fn set_destination(result: &mut Value, dest: &Value) {
    if let Some(o) = result.get_mut("bookSetup").and_then(Value::as_object_mut) {
        o.insert("destination".to_string(), dest.clone());
    }
}

fn set_saved_files(result: &mut Value, receipts: &[Value], complete: bool) {
    if let Some(o) = result.get_mut("bookSetup").and_then(Value::as_object_mut) {
        o.insert("savedFiles".to_string(), Value::Array(receipts.to_vec()));
        o.insert("saved".to_string(), Value::Bool(complete));
    }
}

fn receipt_path(receipts: &[Value], index: usize) -> Option<(String, String)> {
    receipts
        .iter()
        .find(|r| r.get("index").and_then(Value::as_i64) == Some(index as i64))
        .and_then(|r| {
            Some((
                r.get("group")?.as_str()?.to_string(),
                r.get("name")?.as_str()?.to_string(),
            ))
        })
}

fn upsert_receipt(receipts: &mut Vec<Value>, index: usize, group: &str, name: &str) {
    let entry = json!({"index": index, "group": group, "name": name});
    if let Some(r) = receipts
        .iter_mut()
        .find(|r| r.get("index").and_then(Value::as_i64) == Some(index as i64))
    {
        *r = entry;
    } else {
        receipts.push(entry);
    }
    receipts.sort_by_key(|r| r.get("index").and_then(Value::as_i64).unwrap_or(i64::MAX));
}

fn all_saved(receipts: &[Value], total: usize) -> bool {
    (0..total).all(|i| receipt_path(receipts, i).is_some())
}

#[cfg(test)]
#[path = "book_setup_destination_tests.rs"]
mod tests;
