//! 审稿结论账本：每次审稿按「被审文本的 hash」记一行（追加，不覆盖）。
//!
//! 审稿结论只对它审过的那一版文本有效：读取时与当前待审稿 hash 比较，
//! 一致为 `current`，不一致为 `stale`（审稿后正文被修改 / 去味 / 重写），没有记录为 `none`。
//! 失效只提示、不阻断定稿——是否重审由作者决定。

use crate::db::Db;
use crate::stats::now_ms;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

pub fn ensure_schema(db: &Db) -> Result<()> {
    let conn = db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS chapter_review_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            book_id TEXT NOT NULL, ch INTEGER NOT NULL,
            body_hash TEXT NOT NULL,
            ok INTEGER NOT NULL, flagged INTEGER NOT NULL DEFAULT 0,
            issues_json TEXT NOT NULL DEFAULT '[]', note TEXT NOT NULL DEFAULT '',
            plan_hash TEXT NOT NULL DEFAULT '', source TEXT NOT NULL DEFAULT '',
            created_at INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS idx_chapter_review_log ON chapter_review_log(book_id, ch, id);",
    )?;
    Ok(())
}

/// 记录一次审稿结论。`verdict` 取 {ok, flagged, issues[], note}。记账失败不影响审稿本身（调用方忽略错误）。
pub fn record(
    db: &Db,
    book: &str,
    ch: i64,
    body_hash: &str,
    verdict: &Value,
    plan_hash: &str,
    source: &str,
) -> Result<()> {
    if book.is_empty() || body_hash.is_empty() {
        return Ok(());
    }
    let issues = verdict["issues"].as_array().cloned().unwrap_or_default();
    db.exec(
        "INSERT INTO chapter_review_log(book_id,ch,body_hash,ok,flagged,issues_json,note,plan_hash,source,created_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        &[
            &book as &dyn rusqlite::ToSql,
            &ch,
            &body_hash,
            &(verdict["ok"].as_bool().unwrap_or(false) as i64),
            &(verdict["flagged"].as_bool().unwrap_or(false) as i64),
            &json!(issues).to_string(),
            &verdict["note"].as_str().unwrap_or(""),
            &plan_hash,
            &source,
            &now_ms(),
        ],
    )?;
    Ok(())
}

/// 最近一次审稿结论 + 是否仍对应当前文本。返回
/// `{state: current|stale|none, ok, flagged, issues, note, bodyHash, planHash, source, createdAt}`。
pub fn latest(db: &Db, book: &str, ch: i64, current_hash: &str) -> Value {
    let row = db
        .q_json(
            "SELECT * FROM chapter_review_log WHERE book_id=?1 AND ch=?2 ORDER BY id DESC LIMIT 1",
            &[&book as &dyn rusqlite::ToSql, &ch],
        )
        .ok()
        .and_then(|v| v.into_iter().next());
    let Some(r) = row else {
        return json!({"state": "none"});
    };
    let hash = r["bodyHash"].as_str().unwrap_or("");
    json!({
        "state": if hash == current_hash { "current" } else { "stale" },
        "ok": r["ok"].as_i64() == Some(1),
        "flagged": r["flagged"].as_i64() == Some(1),
        "issues": serde_json::from_str::<Value>(r["issuesJson"].as_str().unwrap_or("[]")).unwrap_or(json!([])),
        "note": r["note"], "bodyHash": hash, "planHash": r["planHash"],
        "source": r["source"], "createdAt": r["createdAt"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_is_current_only_for_the_text_it_reviewed() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        assert_eq!(latest(&db, "B", 1, "h1")["state"], "none");
        record(
            &db,
            "B",
            1,
            "h1",
            &json!({"ok": false, "issues": ["时间线矛盾"], "note": "需修改"}),
            "p",
            "chapter",
        )
        .unwrap();
        let v = latest(&db, "B", 1, "h1");
        assert_eq!(
            (v["state"].as_str(), v["ok"].as_bool()),
            (Some("current"), Some(false))
        );
        assert_eq!(v["issues"][0], "时间线矛盾");
        // 正文被修改后：同一结论变为 stale（不再代表当前文本）
        assert_eq!(latest(&db, "B", 1, "h2")["state"], "stale");
        // 对新文本重审后：以最新一次为准
        record(
            &db,
            "B",
            1,
            "h2",
            &json!({"ok": true, "issues": []}),
            "p",
            "recheck",
        )
        .unwrap();
        let v = latest(&db, "B", 1, "h2");
        assert_eq!(
            (v["state"].as_str(), v["ok"].as_bool(), v["source"].as_str()),
            (Some("current"), Some(true), Some("recheck"))
        );
        // 其他书 / 其他章互不影响；空 hash 不记账
        assert_eq!(latest(&db, "C", 1, "h2")["state"], "none");
        record(&db, "B", 2, "", &json!({"ok": true}), "", "x").unwrap();
        assert_eq!(latest(&db, "B", 2, "")["state"], "none");
    }
}
