//! 手动线：章细纲确认回执（HANDOFF §5.3「确认入库绑定具体内容版本；修改后不能继承旧批准」）。
//!
//! 设计原则：
//! - 回执表 outline_confirm 只存「作者确认过的那一版」的 (name, hash, confirmed_at)；
//! - 状态一律**推导**：confirmed = 回执 hash == 当前文件 hash；文件再被编辑即 stale，
//!   不靠失效事件传播（事件会丢，hash 对比不会）；
//! - annotate() 给 pipeline 状态补 chapters[].outlineStatus、把未确认细纲的 chapter_body
//!   下一步收敛为 outline_confirm，并对「确认后又被改且已有正文稿」的章追加 outline blocker。
//! - 确认是作者动作：本模块只提供绑定 hash 的机制，谁有权调用由上层（IPC/Agent 工具）约束。
use crate::continuity::{content_hash, ensure_book};
use crate::db::Db;
use crate::files;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub fn ensure_schema(db: &Db) -> Result<()> {
    db.exec(
        "CREATE TABLE IF NOT EXISTS outline_confirm(
            book_id TEXT NOT NULL, ch INTEGER NOT NULL, name TEXT NOT NULL,
            hash TEXT NOT NULL, confirmed_at INTEGER NOT NULL,
            PRIMARY KEY(book_id, ch))",
        &[],
    )?;
    Ok(())
}

/// 全书回执：ch -> (name, hash, confirmed_at)。表缺失时静默视为空（旧库首查即建）。
fn confirmed_map(db: &Db, book: &str) -> BTreeMap<i64, (String, String, i64)> {
    let _ = ensure_schema(db);
    db.q_json(
        "SELECT ch,name,hash,confirmed_at FROM outline_confirm WHERE book_id=?1",
        &[&book as &dyn rusqlite::ToSql],
    )
    .unwrap_or_default()
    .into_iter()
    .filter_map(|r| {
        let ch = r["ch"].as_i64()?;
        Some((
            ch,
            (
                r["name"].as_str().unwrap_or("").to_string(),
                r["hash"].as_str().unwrap_or("").to_string(),
                r["confirmedAt"].as_i64().unwrap_or(0),
            ),
        ))
    })
    .collect()
}

fn hash_of(db: &Db, book: &str, name: &str) -> Option<String> {
    files::read_file(db, book, "细纲", name)
        .filter(|t| !t.trim().is_empty())
        .map(|t| content_hash(&t))
}

fn short(h: &str) -> String {
    h.chars().take(8).collect()
}

/// 某章细纲状态：none（无文件）/ saved（有文件未确认）/ confirmed（回执与当前内容一致）
/// / stale（确认后被修改——原确认失效，正文若已生成需复核）。
pub fn status_for(db: &Db, book: &str, ch: i64, name: &str) -> Value {
    let cur = hash_of(db, book, name);
    let rec = confirmed_map(db, book).remove(&ch);
    let (status, confirmed_hash, confirmed_at) = match (&cur, rec) {
        (None, _) => ("none", None, None),
        (Some(_), None) => ("saved", None, None),
        (Some(h), Some((_, rh, ts))) if *h == rh => ("confirmed", Some(rh), Some(ts)),
        (Some(_), Some((_, rh, ts))) => ("stale", Some(rh), Some(ts)),
    };
    json!({
        "ch": ch, "name": name, "status": status,
        "hash": cur, "confirmedHash": confirmed_hash, "confirmedAt": confirmed_at,
    })
}

/// 确认入库：把 `细纲/name` 当前内容的 hash 绑定为已确认版本。
/// expected 非空时先校验（作者确认的必须是他看到的那一版；期间被改则拒绝，绝不静默绑新版）。
pub fn confirm(db: &Db, book: &str, ch: i64, name: &str, expected: Option<&str>) -> Result<Value> {
    ensure_schema(db)?;
    ensure_book(db, book)?;
    if !(1..=1_000_000).contains(&ch) {
        bail!("章号超出范围：{}", ch);
    }
    let cur = hash_of(db, book, name)
        .ok_or_else(|| anyhow::anyhow!("细纲文件不存在或为空：细纲/{}", name))?;
    if let Some(exp) = expected.map(str::trim).filter(|s| !s.is_empty()) {
        if exp != cur {
            bail!(
                "细纲在你查看后被修改（期望 {}…，当前 {}…），本次确认已失效，请重新查看后再确认",
                short(exp),
                short(&cur)
            );
        }
    }
    let now = crate::stats::now_ms();
    db.exec(
        "INSERT INTO outline_confirm(book_id,ch,name,hash,confirmed_at) VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(book_id,ch) DO UPDATE SET name=excluded.name,hash=excluded.hash,confirmed_at=excluded.confirmed_at",
        &[
            &book as &dyn rusqlite::ToSql,
            &ch,
            &name,
            &cur,
            &now,
        ],
    )?;
    Ok(json!({"ok": true, "ch": ch, "name": name, "hash": cur, "confirmedAt": now}))
}

/// 已确认且仍有效（当前文件 hash 与回执一致）才放行正文起草（§5.4 前置检查）。
pub fn is_confirmed(db: &Db, book: &str, ch: i64, name: &str) -> bool {
    status_for(db, book, ch, name)["status"]
        .as_str()
        .unwrap_or("")
        == "confirmed"
}

/// pipeline 状态标注（在 derive_state_with_memory 之后调用；state 原地修改）。
/// 只读「有回执」章的细纲文件（推导 confirmed/stale），无回执章一律 saved，避免全量读盘。
pub fn annotate(db: &Db, book: &str, state: &mut Value) {
    let receipts = confirmed_map(db, book);
    let mut stale_with_body: Vec<i64> = Vec::new();
    if let Some(chs) = state.get_mut("chapters").and_then(Value::as_array_mut) {
        for c in chs.iter_mut() {
            let n = c["n"].as_i64().unwrap_or(0);
            let Some(name) = c["outline"].as_str().map(str::to_string) else {
                c["outlineStatus"] = Value::Null;
                continue;
            };
            let st = match receipts.get(&n) {
                None => "saved",
                Some((_, rh, _)) => match hash_of(db, book, &name) {
                    Some(h) if h == *rh => "confirmed",
                    _ => "stale",
                },
            };
            c["outlineStatus"] = json!(st);
            if st == "stale" && matches!(c["body"].as_str(), Some("pending") | Some("approved")) {
                stale_with_body.push(n);
            }
        }
    }
    if !stale_with_body.is_empty() {
        let mut blockers = state["blockers"].as_array().cloned().unwrap_or_default();
        for n in stale_with_body {
            blockers.push(json!({"type": "outline", "chapter": n, "status": "stale"}));
        }
        state["blockers"] = json!(blockers);
    }
    // 下一步收敛：要写正文但细纲未确认/已失效 → 先确认（手动线的核心门）。
    if state["next"]["stage"].as_str() == Some("chapter_body") {
        let nch = state["next"]["chapter"].as_i64().unwrap_or(0);
        let confirmed = state["chapters"]
            .as_array()
            .and_then(|a| a.iter().find(|c| c["n"].as_i64() == Some(nch)))
            .and_then(|c| c["outlineStatus"].as_str())
            .unwrap_or("")
            == "confirmed";
        if !confirmed {
            state["next"] = json!({"stage": "outline_confirm", "chapter": nch});
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline;

    fn fixture() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let book = crate::books::create_book(&db, "manual-test", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        files::write_file(&db, &book, "设定", "建书档案.md", "定位：测试").unwrap();
        files::write_file(&db, &book, "设定", "大纲.md", "全书大纲：测试").unwrap();
        (dir, db, book)
    }

    fn state_of(db: &Db, book: &str) -> Value {
        let tree = files::scan_tree(db, book);
        let queue = Value::Array(
            db.q_json(
                "SELECT ch, review_file, status FROM pending_chapter WHERE book_id=?1",
                &[&book as &dyn rusqlite::ToSql],
            )
            .unwrap_or_default(),
        );
        let mut s = pipeline::derive_state_with_memory(db, book, &tree, &queue);
        annotate(db, book, &mut s);
        s
    }

    #[test]
    fn confirm_binds_hash_and_edit_makes_stale() {
        let (_d, db, book) = fixture();
        files::write_file(&db, &book, "细纲", "细纲_第1章.md", "第一版细纲").unwrap();
        let st = status_for(&db, &book, 1, "细纲_第1章.md");
        assert_eq!(st["status"], "saved");
        let r = confirm(&db, &book, 1, "细纲_第1章.md", None).unwrap();
        let h = r["hash"].as_str().unwrap().to_string();
        assert_eq!(
            status_for(&db, &book, 1, "细纲_第1章.md")["status"],
            "confirmed"
        );
        assert!(is_confirmed(&db, &book, 1, "细纲_第1章.md"));
        // 作者再编辑 → 确认失效（stale），is_confirmed 必须翻 false
        files::write_file(&db, &book, "细纲", "细纲_第1章.md", "第二版细纲").unwrap();
        assert_eq!(
            status_for(&db, &book, 1, "细纲_第1章.md")["status"],
            "stale"
        );
        assert!(!is_confirmed(&db, &book, 1, "细纲_第1章.md"));
        // 带旧 hash 的确认必须被拒（不能绑到没看过的版本）
        assert!(confirm(&db, &book, 1, "细纲_第1章.md", Some(&h)).is_err());
        // 重新确认当前版 → confirmed
        confirm(&db, &book, 1, "细纲_第1章.md", None).unwrap();
        assert!(is_confirmed(&db, &book, 1, "细纲_第1章.md"));
    }

    #[test]
    fn confirm_requires_existing_nonempty_file() {
        let (_d, db, book) = fixture();
        assert!(confirm(&db, &book, 1, "细纲_第1章.md", None).is_err());
        files::write_file(&db, &book, "细纲", "细纲_第1章.md", "   ").unwrap();
        assert!(confirm(&db, &book, 1, "细纲_第1章.md", None).is_err());
        assert!(confirm(&db, &book, 0, "细纲_第1章.md", None).is_err());
    }

    #[test]
    fn annotate_gates_chapter_body_until_confirmed() {
        let (_d, db, book) = fixture();
        files::write_file(&db, &book, "细纲", "细纲_第1章.md", "第一版细纲").unwrap();
        let s = state_of(&db, &book);
        // 有细纲但未确认 → 下一步必须先确认，不许直接写正文
        assert_eq!(s["next"]["stage"], "outline_confirm");
        assert_eq!(s["next"]["chapter"], 1);
        assert_eq!(s["chapters"][0]["outlineStatus"], "saved");
        confirm(&db, &book, 1, "细纲_第1章.md", None).unwrap();
        let s2 = state_of(&db, &book);
        assert_eq!(s2["next"]["stage"], "chapter_body");
        assert_eq!(s2["chapters"][0]["outlineStatus"], "confirmed");
        // 确认后再改细纲 → stale + outline blocker（该章已有待审稿时）
        files::write_file(&db, &book, "细纲", "细纲_第1章.md", "改版细纲").unwrap();
        files::write_file(&db, &book, "正文待审", "第1章.md", "草稿").unwrap();
        db.exec(
            "INSERT INTO pending_chapter(book_id,ch,review_file,status,created_at,updated_at) VALUES(?1,1,'第1章.md','pending',0,0)",
            &[&book as &dyn rusqlite::ToSql],
        )
        .unwrap();
        let s3 = state_of(&db, &book);
        assert_eq!(s3["chapters"][0]["outlineStatus"], "stale");
        let has_outline_blocker =
            s3["blockers"].as_array().unwrap().iter().any(|b| {
                b["type"] == "outline" && b["chapter"] == json!(1) && b["status"] == "stale"
            });
        assert!(
            has_outline_blocker,
            "stale 细纲 + 已有稿件必须进 blockers：{}",
            s3
        );
        // 无细纲文件的章 outlineStatus 恒为 null（字段必须在，前端契约）
        assert!(s3["chapters"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c.get("outlineStatus").is_some()));
    }

    #[test]
    fn annotate_without_receipts_keeps_existing_next() {
        let (_d, db, book) = fixture();
        // setup+大纲已建、无任何章 → next=chapter_outline（写第1章细纲）；annotate 不得干扰既有阶段，
        // 也不得凭空造 outline blocker（无回执、无 stale）。
        let s = state_of(&db, &book);
        assert_eq!(s["next"]["stage"], "chapter_outline");
        assert!(s["blockers"].as_array().unwrap().is_empty());
    }
}
