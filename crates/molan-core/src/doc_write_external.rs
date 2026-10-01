//! 其他写入服务（章节提交 / 定稿、提案接受、建书资料）的统一回执。
//!
//! 这些服务各自按原子写落盘（并保留各自的业务规则：待审登记、审批 saga、提案 CAS、建书逐项确认），
//! 这里只把结果按同一 `WriteReceipt` 形状记入 `doc_write_log`，使 `doc_history` 覆盖全部写入来源。
//! 记账失败不回滚已完成的写入（调用方忽略错误，最多缺一条历史）。

use super::{revision_of, Actor, WriteReceipt};
use crate::db::Db;
use crate::files;
use crate::stats::now_ms;
use anyhow::Result;
use serde_json::Value;

/// AI 新建文件（建书资料、章节待审稿等）的回执：记账失败只记日志，返回 writeId（失败为 None）。
pub fn record_ai_create(
    db: &Db,
    book: &str,
    group: &str,
    name: &str,
    content: &str,
    source: Value,
) -> Option<String> {
    let hash = crate::continuity::content_hash(content);
    let chars = content.chars().count() as i64;
    record_external(
        db,
        book,
        group,
        name,
        "create",
        Actor::Ai,
        None,
        &hash,
        chars,
        None,
        source,
    )
    .map_err(|e| eprintln!("[molan-core] 写入回执记账失败（文件已保存）：{}", e))
    .ok()
}

/// 记一条已提交的外部写入回执，返回 writeId。
#[allow(clippy::too_many_arguments)]
pub fn record_external(
    db: &Db,
    book: &str,
    group: &str,
    name: &str,
    op: &str,
    actor: Actor,
    before: Option<String>,
    after_hash: &str,
    chars: i64,
    index_error: Option<String>,
    source: Value,
) -> Result<String> {
    let canon = files::normalize_group(group);
    let write_id = uuid::Uuid::new_v4().to_string();
    let ts = now_ms();
    let r = WriteReceipt {
        write_id: write_id.clone(),
        idempotency_key: format!("ext:{}", write_id),
        book_id: book.to_string(),
        group: canon.clone(),
        name: name.to_string(),
        op: op.to_string(),
        actor: actor.id().to_string(),
        commit: "committed".into(),
        before_hash: before,
        after_hash: Some(after_hash.to_string()),
        revision: revision_of(db, book, &canon, name),
        index: if index_error.is_some() {
            "failed"
        } else {
            "ok"
        }
        .into(),
        index_error,
        error: None,
        replayed: false,
        recovered: false,
        chars,
        ts,
        source,
    };
    db.exec(
        "INSERT INTO doc_write_log(write_id,idem_key,book_id,group_name,name,op,actor,plan_hash,before_hash,after_hash,phase,index_status,revision,receipt_json,source_json,created_at,updated_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,'',?8,?9,'committed',?10,?11,?12,?13,?14,?14)",
        &[
            &r.write_id as &dyn rusqlite::ToSql,
            &r.idempotency_key,
            &r.book_id,
            &r.group,
            &r.name,
            &r.op,
            &r.actor,
            &r.before_hash,
            &r.after_hash,
            &r.index,
            &r.revision,
            &r.to_json().to_string(),
            &r.source.to_string(),
            &ts,
        ],
    )?;
    Ok(write_id)
}
