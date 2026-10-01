//! ArtifactService：生成产物、修订与交付回执的唯一存储。
//!
//! 事实来源分层：
//! - 文件 / 待审队列 / 批准回执 / 细纲确认回执 = 领域事实；
//! - `artifact_delivery` = 本产物对这些事实做过什么（写入回执 id、写后 hash）；
//! - 卡片状态由 `artifact_view` 用「交付记录 + 当前磁盘/队列」派生——不信任消息文本、
//!   前端标记或 run.status，文件被改过的旧卡会显示「已失效」而不是永远绿色。
//!
//! 交付动作：save（DocumentWriteService，五种写入操作）/ submit_pending（章节提交服务）/
//! confirm_outline / approve / reject / discard。内容永远取自服务端持久化的修订，
//! 客户端只能通过 `revise` 产生新的修订（保留原生成内容与来源）。

use crate::chapter_commit;
use crate::continuity::content_hash;
use crate::db::{Db, REVIEW_GROUP};
use crate::doc_write::{self, Actor, WriteOp, WritePlan};
use crate::files;
use crate::stats::now_ms;
use crate::task_kind::TaskKind;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

pub fn ensure_schema(db: &Db) -> Result<()> {
    let conn = db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS artifact (
            id TEXT PRIMARY KEY,
            book_id TEXT NOT NULL,
            session_id TEXT NOT NULL DEFAULT '',
            message_id TEXT NOT NULL DEFAULT '',
            run_id TEXT NOT NULL DEFAULT '',
            kind TEXT NOT NULL,
            task TEXT NOT NULL DEFAULT 'chat',
            title TEXT NOT NULL DEFAULT '',
            scope TEXT NOT NULL DEFAULT 'document',
            format TEXT NOT NULL DEFAULT 'markdown',
            target_json TEXT NOT NULL DEFAULT '{}',
            provenance_json TEXT NOT NULL DEFAULT '{}',
            lifecycle TEXT NOT NULL DEFAULT 'generated',
            head_rev INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_artifact_session ON artifact(session_id, created_at);
        CREATE INDEX IF NOT EXISTS idx_artifact_book ON artifact(book_id, created_at);
        CREATE INDEX IF NOT EXISTS idx_artifact_message ON artifact(message_id);
        CREATE TABLE IF NOT EXISTS artifact_rev (
            artifact_id TEXT NOT NULL,
            rev INTEGER NOT NULL,
            content TEXT NOT NULL DEFAULT '',
            content_hash TEXT NOT NULL DEFAULT '',
            items_json TEXT NOT NULL DEFAULT '',
            origin TEXT NOT NULL DEFAULT 'model',
            note TEXT NOT NULL DEFAULT '',
            created_at INTEGER NOT NULL,
            PRIMARY KEY(artifact_id, rev)
        );
        CREATE TABLE IF NOT EXISTS artifact_delivery (
            id TEXT PRIMARY KEY,
            artifact_id TEXT NOT NULL,
            rev INTEGER NOT NULL,
            item_index INTEGER NOT NULL DEFAULT 0,
            action TEXT NOT NULL,
            idem_key TEXT NOT NULL DEFAULT '',
            write_id TEXT NOT NULL DEFAULT '',
            book_id TEXT NOT NULL,
            group_name TEXT NOT NULL DEFAULT '',
            file_name TEXT NOT NULL DEFAULT '',
            ch INTEGER,
            op TEXT NOT NULL DEFAULT '',
            after_hash TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL,
            detail_json TEXT NOT NULL DEFAULT '{}',
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_artifact_delivery ON artifact_delivery(artifact_id, created_at);
        CREATE UNIQUE INDEX IF NOT EXISTS uq_artifact_delivery_idem ON artifact_delivery(artifact_id, idem_key) WHERE idem_key<>'';",
    )?;
    Ok(())
}

/// 新产物参数。`items` 非空 = 多文件交付（每项 {title, group, name, content}）。
#[derive(Debug, Clone, Default)]
pub struct NewArtifact {
    pub book_id: String,
    pub session_id: String,
    pub message_id: String,
    pub run_id: String,
    pub kind: String,
    pub task: String,
    pub title: String,
    /// document（完整文档）| fragment（片段/选区改写，绝不可整篇替换）
    pub scope: String,
    pub target: Value,
    pub provenance: Value,
    pub content: String,
    pub items: Vec<Value>,
    /// model | tool | user_edit
    pub origin: String,
    /// generating | generated | interrupted | failed
    pub lifecycle: String,
}

pub fn create(db: &Db, a: &NewArtifact) -> Result<String> {
    // 技能草稿属于全局技能库，不归属作品；其余产物必须绑定作品
    if a.book_id.trim().is_empty() && a.kind != "skill_draft" {
        bail!("产物缺少 bookId");
    }
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_ms();
    let scope = if a.scope == "fragment" {
        "fragment"
    } else {
        "document"
    };
    let lifecycle = if a.lifecycle.is_empty() {
        "generated"
    } else {
        a.lifecycle.as_str()
    };
    let mut conn = db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))?;
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO artifact(id,book_id,session_id,message_id,run_id,kind,task,title,scope,format,target_json,provenance_json,lifecycle,head_rev,created_at,updated_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'markdown',?10,?11,?12,1,?13,?13)",
        rusqlite::params![
            id, a.book_id, a.session_id, a.message_id, a.run_id, a.kind,
            if a.task.is_empty() { "chat" } else { a.task.as_str() }, a.title, scope,
            a.target.to_string(), a.provenance.to_string(), lifecycle, now
        ],
    )?;
    tx.execute(
        "INSERT INTO artifact_rev(artifact_id,rev,content,content_hash,items_json,origin,created_at) VALUES(?1,1,?2,?3,?4,?5,?6)",
        rusqlite::params![
            id, a.content, content_hash(&a.content),
            if a.items.is_empty() { String::new() } else { Value::Array(a.items.clone()).to_string() },
            if a.origin.is_empty() { "model" } else { a.origin.as_str() }, now
        ],
    )?;
    tx.commit()?;
    Ok(id)
}

/// 生成过程结束时更新内容与生命周期（同一修订 1；尚未交付时才允许）。
pub fn finish(db: &Db, id: &str, content: &str, lifecycle: &str, message_id: &str) -> Result<()> {
    let now = now_ms();
    db.exec(
        "UPDATE artifact_rev SET content=?2, content_hash=?3 WHERE artifact_id=?1 AND rev=1 AND NOT EXISTS (SELECT 1 FROM artifact_delivery WHERE artifact_id=?1)",
        &[&id as &dyn rusqlite::ToSql, &content, &content_hash(content)],
    )?;
    db.exec(
        "UPDATE artifact SET lifecycle=?2, message_id=CASE WHEN ?3<>'' THEN ?3 ELSE message_id END, updated_at=?4 WHERE id=?1",
        &[&id as &dyn rusqlite::ToSql, &lifecycle, &message_id, &now],
    )?;
    Ok(())
}

pub fn get(db: &Db, id: &str) -> Result<Value> {
    db.q_json(
        "SELECT * FROM artifact WHERE id=?1",
        &[&id as &dyn rusqlite::ToSql],
    )?
    .into_iter()
    .next()
    .ok_or_else(|| anyhow!("产物不存在：{}", id))
}

pub fn get_rev(db: &Db, id: &str, rev: i64) -> Result<Value> {
    db.q_json(
        "SELECT * FROM artifact_rev WHERE artifact_id=?1 AND rev=?2",
        &[&id as &dyn rusqlite::ToSql, &rev],
    )?
    .into_iter()
    .next()
    .ok_or_else(|| anyhow!("产物修订不存在：{}@{}", id, rev))
}

pub fn items_of(rev_row: &Value) -> Vec<Value> {
    serde_json::from_str::<Value>(rev_row["itemsJson"].as_str().unwrap_or(""))
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

/// 作者在预览中编辑 → 新修订（保留原生成内容）。base_rev 不是当前头修订 → 冲突（另一窗口已改）。
pub fn revise(
    db: &Db,
    id: &str,
    book_id: &str,
    base_rev: i64,
    content: &str,
    items: Option<Vec<Value>>,
    note: &str,
) -> Result<i64> {
    let a = get(db, id)?;
    if a["bookId"].as_str() != Some(book_id) {
        bail!("产物不属于当前作品");
    }
    if a["lifecycle"].as_str() == Some("generating") {
        bail!("产物仍在生成中，请等待完成或停止后再编辑");
    }
    let head = a["headRev"].as_i64().unwrap_or(1);
    if base_rev != head {
        bail!(
            "REV_CONFLICT：产物已在其他窗口修改（当前修订 {}，你基于 {}），请刷新后再编辑",
            head,
            base_rev
        );
    }
    if content.chars().count() > doc_write::MAX_CONTENT_CHARS {
        bail!("内容超过上限");
    }
    let next = head + 1;
    let now = now_ms();
    let mut conn = db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))?;
    let tx = conn.transaction()?;
    let n = tx.execute(
        "UPDATE artifact SET head_rev=?2, updated_at=?3 WHERE id=?1 AND head_rev=?4",
        rusqlite::params![id, next, now, head],
    )?;
    if n != 1 {
        bail!("REV_CONFLICT：产物修订已变化，请刷新");
    }
    tx.execute(
        "INSERT INTO artifact_rev(artifact_id,rev,content,content_hash,items_json,origin,note,created_at) VALUES(?1,?2,?3,?4,?5,'user_edit',?6,?7)",
        rusqlite::params![
            id, next, content, content_hash(content),
            items.map(|i| Value::Array(i).to_string()).unwrap_or_default(), note, now
        ],
    )?;
    tx.commit()?;
    Ok(next)
}

pub fn deliveries(db: &Db, id: &str) -> Vec<Value> {
    db.q_json(
        "SELECT * FROM artifact_delivery WHERE artifact_id=?1 ORDER BY created_at, rowid",
        &[&id as &dyn rusqlite::ToSql],
    )
    .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn record(
    db: &Db,
    artifact_id: &str,
    rev: i64,
    item: i64,
    action: &str,
    idem: &str,
    book: &str,
    group: &str,
    name: &str,
    ch: Option<i64>,
    op: &str,
    write_id: &str,
    after_hash: &str,
    status: &str,
    detail: &Value,
) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    db.exec(
        "INSERT INTO artifact_delivery(id,artifact_id,rev,item_index,action,idem_key,write_id,book_id,group_name,file_name,ch,op,after_hash,status,detail_json,created_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
        &[
            &id as &dyn rusqlite::ToSql, &artifact_id, &rev, &item, &action, &idem, &write_id, &book,
            &group, &name, &ch, &op, &after_hash, &status, &detail.to_string(), &now_ms(),
        ],
    )?;
    db.exec(
        "UPDATE artifact SET updated_at=?2 WHERE id=?1",
        &[&artifact_id as &dyn rusqlite::ToSql, &now_ms()],
    )?;
    Ok(id)
}

/// 交付请求（来自 IPC `artifact_deliver`）。
#[derive(Debug, Clone, Default)]
pub struct Deliver {
    pub artifact_id: String,
    pub book_id: String,
    pub rev: i64,
    pub item: i64,
    pub action: String,
    pub idempotency_key: String,
    pub group: String,
    pub name: String,
    pub op: String,
    pub base_hash: Option<String>,
    pub start: Option<usize>,
    pub end: Option<usize>,
    pub expected: Option<String>,
    pub ch: Option<i64>,
}

impl Deliver {
    pub fn from_args(args: &Value) -> Deliver {
        let s = |k: &str| {
            args.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        Deliver {
            artifact_id: s("artifactId"),
            book_id: s("bookId"),
            rev: args["rev"].as_i64().unwrap_or(0),
            item: args["item"].as_i64().unwrap_or(0),
            action: s("action"),
            idempotency_key: s("idempotencyKey"),
            group: s("group"),
            name: s("name"),
            op: s("op"),
            base_hash: Some(s("baseHash")).filter(|h| !h.is_empty()),
            start: args["start"].as_u64().map(|n| n as usize),
            end: args["end"].as_u64().map(|n| n as usize),
            expected: args
                .get("expected")
                .and_then(Value::as_str)
                .map(str::to_string),
            ch: args["ch"].as_i64().filter(|c| *c > 0),
        }
    }
}

/// 服务端交付（如「保存为技能」）的幂等查询：同 (artifactId, idempotencyKey) 的既有回执。
pub fn prior_delivery(db: &Db, artifact_id: &str, idem: &str) -> Option<Value> {
    prior(db, artifact_id, idem)
}

/// 记录「保存为技能」交付：afterHash 为保存时的模板 hash，detail 含 skillId。
#[allow(clippy::too_many_arguments)]
pub fn record_skill_delivery(
    db: &Db,
    artifact_id: &str,
    rev: i64,
    idem: &str,
    skill_name: &str,
    after_hash: &str,
    status: &str,
    detail: &Value,
) -> Result<String> {
    record(
        db,
        artifact_id,
        rev,
        0,
        "save_skill",
        idem,
        "",
        "技能",
        skill_name,
        None,
        "create",
        "",
        after_hash,
        status,
        detail,
    )
}

fn prior(db: &Db, artifact_id: &str, idem: &str) -> Option<Value> {
    if idem.is_empty() {
        return None;
    }
    db.q_json(
        "SELECT * FROM artifact_delivery WHERE artifact_id=?1 AND idem_key=?2",
        &[&artifact_id as &dyn rusqlite::ToSql, &idem],
    )
    .ok()?
    .into_iter()
    .next()
}

fn last_delivery(db: &Db, artifact_id: &str, item: i64, action: &str) -> Option<Value> {
    db.q_json(
        "SELECT * FROM artifact_delivery WHERE artifact_id=?1 AND item_index=?2 AND action=?3 AND status IN ('committed','noop') ORDER BY created_at DESC, rowid DESC LIMIT 1",
        &[&artifact_id as &dyn rusqlite::ToSql, &item, &action],
    )
    .ok()?
    .into_iter()
    .next()
}

/// 执行交付。返回 `{ok, action, status, receipt|result, deliveryId}`；冲突/拒绝也以 ok=false 返回，
/// 只有数据库意外返回 Err。approve 成功后由服务端负责异步记忆同步。
pub fn deliver(db: &Db, d: &Deliver) -> Result<Value> {
    let a = get(db, &d.artifact_id)?;
    let book = a["bookId"].as_str().unwrap_or("").to_string();
    if book != d.book_id {
        bail!("产物不属于当前作品，拒绝交付");
    }
    if let Some(p) = prior(db, &d.artifact_id, &d.idempotency_key) {
        let detail: Value =
            serde_json::from_str(p["detailJson"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
        return Ok(
            json!({"ok": matches!(p["status"].as_str(), Some("committed"|"noop")), "action": p["action"], "status": p["status"], "result": detail, "deliveryId": p["id"], "replayed": true}),
        );
    }
    let head = a["headRev"].as_i64().unwrap_or(1);
    let rev = if d.rev > 0 { d.rev } else { head };
    if rev != head && d.action != "approve" && d.action != "reject" {
        bail!("REV_STALE：只能交付当前修订（当前 {}，请求 {}）", head, rev);
    }
    if a["lifecycle"].as_str() == Some("generating") {
        bail!("产物仍在生成中，不能交付");
    }
    let rev_row = get_rev(db, &d.artifact_id, rev)?;
    let items = items_of(&rev_row);
    let content = if items.is_empty() {
        rev_row["content"].as_str().unwrap_or("").to_string()
    } else {
        items
            .get(d.item as usize)
            .and_then(|i| i["content"].as_str())
            .ok_or_else(|| anyhow!("条目不存在：{}", d.item))?
            .to_string()
    };
    let task = TaskKind::parse(a["task"].as_str().unwrap_or("chat")).unwrap_or(TaskKind::Chat);
    let fragment = a["scope"].as_str() == Some("fragment");
    let rec = |action: &str,
               group: &str,
               name: &str,
               ch: Option<i64>,
               op: &str,
               write_id: &str,
               after: &str,
               status: &str,
               detail: &Value| {
        record(
            db,
            &d.artifact_id,
            rev,
            d.item,
            action,
            &d.idempotency_key,
            &book,
            group,
            name,
            ch,
            op,
            write_id,
            after,
            status,
            detail,
        )
    };
    match d.action.as_str() {
        "save" => {
            let op = WriteOp::parse(&d.op)
                .ok_or_else(|| anyhow!("op 必须是 create/replace/append/insert/replace_range"))?;
            if fragment && matches!(op, WriteOp::Replace) {
                bail!("片段产物不能整篇替换目标文档（只能替换选区、插入、追加或另存）");
            }
            if !task.rewrites_target() && !matches!(op, WriteOp::Create | WriteOp::Append) {
                bail!("「{}」产物只能另存或追加，不能改写原文", task.label());
            }
            if d.idempotency_key.is_empty() {
                bail!("缺少 idempotencyKey");
            }
            let plan = WritePlan {
                book_id: book.clone(),
                group: d.group.clone(),
                name: d.name.clone(),
                op,
                base_hash: d.base_hash.clone(),
                content,
                start: d.start,
                end: d.end,
                expected: d.expected.clone(),
                actor: Actor::Ai,
                idempotency_key: format!("artifact:{}:{}", d.artifact_id, d.idempotency_key),
                source: json!({"artifactId": d.artifact_id, "rev": rev, "item": d.item}),
            };
            let r = doc_write::execute(db, &plan)?;
            let status = r.commit.clone();
            let ch = chapter_commit::chapter_num_from_name(&r.name);
            let id = rec(
                "save",
                &r.group,
                &r.name,
                ch,
                op.id(),
                &r.write_id,
                r.after_hash.as_deref().unwrap_or(""),
                &status,
                &r.to_json(),
            )?;
            Ok(
                json!({"ok": r.ok(), "action": "save", "status": status, "receipt": r.to_json(), "deliveryId": id}),
            )
        }
        "submit_pending" => {
            if fragment {
                bail!("片段不能作为整章提交待审");
            }
            let ch =
                d.ch.or_else(|| a_target(&a)["ch"].as_i64())
                    .ok_or_else(|| anyhow!("提交待审需要章号"))?;
            match chapter_commit::submit_pending(
                db,
                &book,
                ch,
                &content,
                &format!("artifact:{}@{}", d.artifact_id, rev),
                || Ok(()),
            ) {
                Ok(r) => {
                    let id = rec(
                        "submit_pending",
                        REVIEW_GROUP,
                        r["name"].as_str().unwrap_or(""),
                        Some(ch),
                        "create",
                        "",
                        r["hash"].as_str().unwrap_or(""),
                        "committed",
                        &r,
                    )?;
                    Ok(
                        json!({"ok": true, "action": "submit_pending", "status": "committed", "result": r, "deliveryId": id}),
                    )
                }
                Err(e) => {
                    let detail = json!({"error": e.to_string()});
                    let status = if e.to_string().contains("已存在") {
                        "conflict"
                    } else {
                        "failed"
                    };
                    let id = rec(
                        "submit_pending",
                        REVIEW_GROUP,
                        &format!("第{}章.md", ch),
                        Some(ch),
                        "create",
                        "",
                        "",
                        status,
                        &detail,
                    )?;
                    Ok(
                        json!({"ok": false, "action": "submit_pending", "status": status, "result": detail, "deliveryId": id}),
                    )
                }
            }
        }
        "confirm_outline" => {
            let saved = last_delivery(db, &d.artifact_id, d.item, "save")
                .ok_or_else(|| anyhow!("细纲尚未保存，不能确认"))?;
            let (group, name) = (
                saved["groupName"].as_str().unwrap_or(""),
                saved["fileName"].as_str().unwrap_or(""),
            );
            if group != "细纲" {
                bail!("只有保存在「细纲」中的产物可以确认（当前：{}）", group);
            }
            let ch =
                d.ch.or(saved["ch"].as_i64())
                    .or_else(|| a_target(&a)["ch"].as_i64())
                    .ok_or_else(|| anyhow!("确认细纲需要章号"))?;
            let expected = saved["afterHash"].as_str().unwrap_or("");
            match crate::outline_confirm::confirm(db, &book, ch, name, Some(expected)) {
                Ok(r) => {
                    let id = rec(
                        "confirm_outline",
                        group,
                        name,
                        Some(ch),
                        "",
                        "",
                        expected,
                        "committed",
                        &r,
                    )?;
                    Ok(
                        json!({"ok": true, "action": "confirm_outline", "status": "committed", "result": r, "deliveryId": id}),
                    )
                }
                Err(e) => {
                    let detail = json!({"error": e.to_string()});
                    let id = rec(
                        "confirm_outline",
                        group,
                        name,
                        Some(ch),
                        "",
                        "",
                        expected,
                        "conflict",
                        &detail,
                    )?;
                    Ok(
                        json!({"ok": false, "action": "confirm_outline", "status": "conflict", "result": detail, "deliveryId": id}),
                    )
                }
            }
        }
        "approve" | "reject" => {
            let sub = last_delivery(db, &d.artifact_id, d.item, "submit_pending")
                .ok_or_else(|| anyhow!("该产物没有提交过待审"))?;
            let (name, ch, hash) = (
                sub["fileName"].as_str().unwrap_or("").to_string(),
                sub["ch"].as_i64().unwrap_or(0),
                sub["afterHash"].as_str().unwrap_or("").to_string(),
            );
            let out = if d.action == "approve" {
                chapter_commit::approve(db, &book, &name, ch, Some(&hash), "artifact")
            } else {
                chapter_commit::reject(db, &book, ch, &name)
            };
            match out {
                Ok(r) => {
                    let (group, file) = if d.action == "approve" {
                        ("正文", r["finalName"].as_str().unwrap_or(&name).to_string())
                    } else {
                        (REVIEW_GROUP, name.clone())
                    };
                    let id = rec(
                        &d.action,
                        group,
                        &file,
                        Some(ch),
                        "",
                        "",
                        &hash,
                        "committed",
                        &r,
                    )?;
                    Ok(
                        json!({"ok": true, "action": d.action, "status": "committed", "result": r, "deliveryId": id}),
                    )
                }
                Err(e) => {
                    let detail = json!({"error": e.to_string()});
                    let id = rec(
                        &d.action,
                        REVIEW_GROUP,
                        &name,
                        Some(ch),
                        "",
                        "",
                        &hash,
                        "failed",
                        &detail,
                    )?;
                    Ok(
                        json!({"ok": false, "action": d.action, "status": "failed", "result": detail, "deliveryId": id}),
                    )
                }
            }
        }
        "discard" => {
            db.exec(
                "UPDATE artifact SET lifecycle='discarded', updated_at=?2 WHERE id=?1",
                &[&d.artifact_id as &dyn rusqlite::ToSql, &now_ms()],
            )?;
            Ok(json!({"ok": true, "action": "discard", "status": "committed"}))
        }
        other => bail!("未知交付动作：{}", other),
    }
}

/// 采纳已由章节服务提交待审的草稿为产物（Agent 工具 / 单章按钮产出的正文草稿）：
/// 建产物并记一条已提交的 submit_pending 交付，后续定稿/驳回与卡片状态走同一条链。
pub fn adopt_pending(
    db: &Db,
    a: &NewArtifact,
    ch: i64,
    name: &str,
    hash: &str,
    receipt: &Value,
) -> Result<String> {
    let id = create(db, a)?;
    record(
        db,
        &id,
        1,
        0,
        "submit_pending",
        "",
        &a.book_id,
        REVIEW_GROUP,
        name,
        Some(ch),
        "create",
        "",
        hash,
        "committed",
        receipt,
    )?;
    Ok(id)
}

pub(crate) fn a_target(a: &Value) -> Value {
    serde_json::from_str(a["targetJson"].as_str().unwrap_or("{}")).unwrap_or(json!({}))
}

/// 当前磁盘某文档的 hash（不存在 = None）。
pub(crate) fn disk_hash(db: &Db, book: &str, group: &str, name: &str) -> Option<String> {
    files::read_checked(db, book, group, name)
        .ok()
        .flatten()
        .map(|c| content_hash(&c))
}

#[cfg(test)]
#[path = "artifact_tests.rs"]
mod tests;
