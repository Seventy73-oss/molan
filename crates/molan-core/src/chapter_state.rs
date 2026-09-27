//! 章节状态机 + 提交包 + 迁移日志（长篇稳定性 P0-2/P0-4）。
//!
//! 原则：只观测、不阻断。状态记录绝不改变落盘/审批/连写等现有行为；
//! 记录失败只留事件，绝不让"记账"弄坏"写作"。
//!
//! 前进态（rank 递增）：PLANNED→GENERATING→REVIEWED→HUMANIZED→DRAFT_REVIEW
//!   →SAVED→APPROVED→MEMORY_SYNCED→LOCKED
//! 回退态（rank 0，随时可记）：INTERRUPTED / REVIEW_FAILED / CONFLICTED /
//!   STALE_DEPENDENCY / ROLLED_BACK；回退态之后允许任何前进态（恢复）。
//! 指纹规则：正文指纹变化 = 旧记忆/旧审批失效，强制回到 SAVED；
//! 待审指纹独立记录（draft_hash），不覆盖正式稿指纹。
use crate::{db::Db, files, stats::now_ms};
use anyhow::{anyhow, Result};
use rusqlite::params;
use serde_json::{json, Value};

pub const PLANNED: &str = "PLANNED";
pub const GENERATING: &str = "GENERATING";
pub const REVIEWED: &str = "REVIEWED";
pub const HUMANIZED: &str = "HUMANIZED";
pub const DRAFT_REVIEW: &str = "DRAFT_REVIEW";
pub const SAVED: &str = "SAVED";
pub const APPROVED: &str = "APPROVED";
pub const MEMORY_SYNCED: &str = "MEMORY_SYNCED";
pub const LOCKED: &str = "LOCKED";
pub const INTERRUPTED: &str = "INTERRUPTED";
pub const REVIEW_FAILED: &str = "REVIEW_FAILED";
pub const CONFLICTED: &str = "CONFLICTED";
pub const STALE_DEPENDENCY: &str = "STALE_DEPENDENCY";
pub const ROLLED_BACK: &str = "ROLLED_BACK";

fn rank(state: &str) -> i64 {
    match state {
        PLANNED => 1,
        GENERATING => 2,
        REVIEWED => 3,
        HUMANIZED => 4,
        DRAFT_REVIEW => 5,
        SAVED => 6,
        APPROVED => 7,
        MEMORY_SYNCED => 8,
        LOCKED => 9,
        _ => 0,
    }
}

fn is_regression(state: &str) -> bool {
    matches!(
        state,
        INTERRUPTED | REVIEW_FAILED | CONFLICTED | STALE_DEPENDENCY | ROLLED_BACK
    )
}

pub fn ensure_schema(db: &Db) -> Result<()> {
    db.conn
        .lock()
        .map_err(|_| anyhow!("数据库锁损坏"))?
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS chapter_state (
                book_id TEXT NOT NULL, ch INTEGER NOT NULL,
                state TEXT NOT NULL,
                chapter_hash TEXT, draft_hash TEXT, outline_hash TEXT,
                humanize_hash TEXT, memory_hash TEXT,
                fact_count INTEGER NOT NULL DEFAULT 0,
                task_id TEXT NOT NULL DEFAULT '', model TEXT NOT NULL DEFAULT '',
                origin TEXT NOT NULL DEFAULT 'ai',
                updated_at INTEGER NOT NULL,
                PRIMARY KEY(book_id,ch));
             CREATE TABLE IF NOT EXISTS chapter_state_event (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                book_id TEXT NOT NULL, ch INTEGER NOT NULL,
                from_state TEXT, to_state TEXT NOT NULL,
                detail_json TEXT NOT NULL DEFAULT '{}',
                created_at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS idx_chapter_state_event ON chapter_state_event(book_id,ch,id);",
        )?;
    // origin 列（C3）：章节来源 ai/human/import。老库探测式补列，缺列绝不带病运行。
    {
        let conn = db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))?;
        let has_origin = {
            let mut stmt = conn.prepare("PRAGMA table_info(chapter_state)")?;
            let cols: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .filter_map(|r| r.ok())
                .collect();
            cols.iter().any(|c| c == "origin")
        };
        if !has_origin {
            conn.execute_batch(
                "ALTER TABLE chapter_state ADD COLUMN origin TEXT NOT NULL DEFAULT 'ai'",
            )?;
        }
    }
    Ok(())
}

/// 标注章节来源（C3）：默认 ai 可升级为 import/human；已显式标注的来源不被覆盖，
/// 导入/手写章因此不会被记忆重建误推进到 MEMORY_SYNCED（批准语义只属于系统审批链）。
pub fn record_origin(db: &Db, book: &str, ch: i64, origin: &str) -> Result<()> {
    if !matches!(origin, "ai" | "human" | "import") {
        return Err(anyhow!("非法章节来源：{}", origin));
    }
    with_tx(db, |tx, now| {
        tx.execute(
            "INSERT INTO chapter_state(book_id,ch,state,origin,updated_at) VALUES(?1,?2,'',?3,?4)
             ON CONFLICT(book_id,ch) DO UPDATE SET origin=excluded.origin,updated_at=excluded.updated_at
             WHERE chapter_state.origin='ai'",
            params![book, ch, origin, now],
        )?;
        Ok(())
    })
}

/// 锁内迁移判定：返回实际落库状态。指纹/计数等字段由调用方另行 UPDATE。
#[allow(clippy::too_many_arguments)]
fn apply_tx(
    tx: &rusqlite::Transaction,
    book: &str,
    ch: i64,
    to: &str,
    detail: &Value,
    now: i64,
    force: bool,
) -> Result<String> {
    let cur: Option<String> = tx
        .query_row(
            "SELECT state FROM chapter_state WHERE book_id=?1 AND ch=?2",
            params![book, ch],
            |r| r.get(0),
        )
        .ok();
    let apply = force
        || is_regression(to)
        || match cur.as_deref() {
            None => true,
            Some(c) if is_regression(c) => true,
            Some(c) => rank(to) >= rank(c),
        };
    let final_state = if apply {
        to
    } else {
        cur.as_deref().unwrap_or(to)
    };
    let mut d = detail.clone();
    if !apply {
        d["requestedTo"] = json!(to);
        d["kept"] = json!(final_state);
    }
    tx.execute(
        "INSERT INTO chapter_state(book_id,ch,state,updated_at) VALUES(?1,?2,?3,?4)
         ON CONFLICT(book_id,ch) DO UPDATE SET state=excluded.state,updated_at=excluded.updated_at",
        params![book, ch, final_state, now],
    )?;
    tx.execute(
        "INSERT INTO chapter_state_event(book_id,ch,from_state,to_state,detail_json,created_at)
         VALUES(?1,?2,?3,?4,?5,?6)",
        params![book, ch, cur, final_state, d.to_string(), now],
    )?;
    Ok(final_state.to_string())
}

fn with_tx<T>(db: &Db, f: impl FnOnce(&rusqlite::Transaction, i64) -> Result<T>) -> Result<T> {
    ensure_schema(db)?;
    let mut conn = db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))?;
    let tx = conn.transaction()?;
    let now = now_ms();
    let out = f(&tx, now)?;
    tx.commit()?;
    Ok(out)
}

/// 细纲落盘：记录细纲指纹；仅早期章节前进到 PLANNED，已有后续状态则只记事件。
pub fn record_outline(db: &Db, book: &str, ch: i64, outline_hash: &str) -> Result<()> {
    with_tx(db, |tx, now| {
        // state 由 apply_tx 全权决策：INSERT 只建行（state='' 视为无前置状态），不预置状态，
        // 否则历史日志会把「新建行」误记为从该状态迁移而来。
        tx.execute(
            "INSERT INTO chapter_state(book_id,ch,state,outline_hash,updated_at) VALUES(?1,?2,'',?3,?4)
             ON CONFLICT(book_id,ch) DO UPDATE SET outline_hash=excluded.outline_hash,updated_at=excluded.updated_at",
            params![book, ch, outline_hash, now],
        )?;
        apply_tx(
            tx,
            book,
            ch,
            PLANNED,
            &json!({"outlineHash": outline_hash}),
            now,
            false,
        )?;
        Ok(())
    })
}

/// 正文/待审落盘。正文指纹变化强制回到 SAVED（旧记忆/审批随之失效）；
/// 待审只更新 draft_hash，不撼动已有 APPROVED/MEMORY_SYNCED 状态。
pub fn record_save(db: &Db, book: &str, ch: i64, group: &str, hash: &str) -> Result<()> {
    with_tx(db, |tx, now| {
        if group == crate::db::REVIEW_GROUP {
            tx.execute(
                "INSERT INTO chapter_state(book_id,ch,state,draft_hash,updated_at) VALUES(?1,?2,'',?3,?4)
                 ON CONFLICT(book_id,ch) DO UPDATE SET draft_hash=excluded.draft_hash,updated_at=excluded.updated_at",
                params![book, ch, hash, now],
            )?;
            apply_tx(
                tx,
                book,
                ch,
                DRAFT_REVIEW,
                &json!({"draftHash": hash}),
                now,
                false,
            )?;
            return Ok(());
        }
        let old: Option<String> = tx
            .query_row(
                "SELECT chapter_hash FROM chapter_state WHERE book_id=?1 AND ch=?2",
                params![book, ch],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        let hash_changed = old.as_deref().is_some_and(|h| !h.is_empty() && h != hash);
        tx.execute(
            "INSERT INTO chapter_state(book_id,ch,state,chapter_hash,updated_at) VALUES(?1,?2,'',?3,?4)
             ON CONFLICT(book_id,ch) DO UPDATE SET chapter_hash=excluded.chapter_hash,updated_at=excluded.updated_at",
            params![book, ch, hash, now],
        )?;
        let detail = if hash_changed {
            json!({"chapterHash": hash, "hashChanged": true,
                   "note": "正文指纹已变化，旧记忆/旧审批基于前一版，状态如实回退"})
        } else {
            json!({"chapterHash": hash})
        };
        apply_tx(tx, book, ch, SAVED, &detail, now, hash_changed)?;
        Ok(())
    })
}

/// 去味完成：记录去味后文本指纹并前进到 HUMANIZED。
pub fn record_humanized(db: &Db, book: &str, ch: i64, hash: &str) -> Result<()> {
    with_tx(db, |tx, now| {
        tx.execute(
            "INSERT INTO chapter_state(book_id,ch,state,humanize_hash,updated_at) VALUES(?1,?2,'',?3,?4)
             ON CONFLICT(book_id,ch) DO UPDATE SET humanize_hash=excluded.humanize_hash,updated_at=excluded.updated_at",
            params![book, ch, hash, now],
        )?;
        apply_tx(
            tx,
            book,
            ch,
            HUMANIZED,
            &json!({"humanizeHash": hash}),
            now,
            false,
        )?;
        Ok(())
    })
}

/// 审批通过（人工接受待审 / 全自动定稿）。
pub fn record_approved(db: &Db, book: &str, ch: i64, detail: Value) -> Result<()> {
    with_tx(db, |tx, now| {
        apply_tx(tx, book, ch, APPROVED, &detail, now, false)?;
        Ok(())
    })
}

/// 生成/落盘被中断（停机、取消、上游断流且无法续写）。
pub fn record_interrupted(db: &Db, book: &str, ch: i64, reason: &str) -> Result<()> {
    with_tx(db, |tx, now| {
        apply_tx(
            tx,
            book,
            ch,
            INTERRUPTED,
            &json!({"reason": reason}),
            now,
            false,
        )?;
        Ok(())
    })
}

/// 依赖过期（前章/设定/技能被改，本章成果基于旧依赖）。
pub fn record_stale_dependency(db: &Db, book: &str, ch: i64, detail: Value) -> Result<()> {
    with_tx(db, |tx, now| {
        apply_tx(tx, book, ch, STALE_DEPENDENCY, &detail, now, false)?;
        Ok(())
    })
}

/// 审核未通过（全自动流水线内）。
pub fn record_review_failed(db: &Db, book: &str, ch: i64, note: &str) -> Result<()> {
    with_tx(db, |tx, now| {
        apply_tx(
            tx,
            book,
            ch,
            REVIEW_FAILED,
            &json!({"note": note}),
            now,
            false,
        )?;
        Ok(())
    })
}

/// 审核通过（全自动流水线内）。
pub fn record_reviewed(db: &Db, book: &str, ch: i64, note: &str) -> Result<()> {
    with_tx(db, |tx, now| {
        apply_tx(tx, book, ch, REVIEWED, &json!({"note": note}), now, false)?;
        Ok(())
    })
}

/// 开始生成正文（连写流水线内）。
pub fn record_generating(db: &Db, book: &str, ch: i64, task_id: &str, model: &str) -> Result<()> {
    with_tx(db, |tx, now| {
        tx.execute(
            "INSERT INTO chapter_state(book_id,ch,state,task_id,model,updated_at) VALUES(?1,?2,'',?3,?4,?5)
             ON CONFLICT(book_id,ch) DO UPDATE SET task_id=excluded.task_id,model=excluded.model,updated_at=excluded.updated_at",
            params![book, ch, task_id, model, now],
        )?;
        apply_tx(
            tx,
            book,
            ch,
            GENERATING,
            &json!({"taskId": task_id, "model": model}),
            now,
            false,
        )?;
        Ok(())
    })
}

/// 锁定/解锁：锁定置顶（rank 9）；解锁从 LOCKED 回到 SAVED（有稿）或清除回退。
pub fn record_locked(db: &Db, book: &str, ch: i64, locked: bool) -> Result<()> {
    with_tx(db, |tx, now| {
        if locked {
            apply_tx(tx, book, ch, LOCKED, &json!({}), now, true)?;
        } else {
            let cur: Option<String> = tx
                .query_row(
                    "SELECT state FROM chapter_state WHERE book_id=?1 AND ch=?2",
                    params![book, ch],
                    |r| r.get(0),
                )
                .ok();
            if cur.as_deref() == Some(LOCKED) {
                let has_chapter: bool = tx
                    .query_row(
                        "SELECT chapter_hash IS NOT NULL AND chapter_hash<>'' FROM chapter_state WHERE book_id=?1 AND ch=?2",
                        params![book, ch],
                        |r| r.get(0),
                    )
                    .unwrap_or(false);
                let back = if has_chapter { SAVED } else { PLANNED };
                apply_tx(tx, book, ch, back, &json!({"note": "解锁"}), now, true)?;
            }
        }
        Ok(())
    })
}

/// 记忆同步完成（在 record_memory_inner 的同一事务内调用 = 提交事务）：
/// 记录记忆对应的正文指纹与事实条数，前进到 MEMORY_SYNCED。
pub fn memory_synced_tx(
    tx: &rusqlite::Transaction,
    book: &str,
    ch: i64,
    memory_hash: &str,
    fact_count: i64,
    now: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO chapter_state(book_id,ch,state,memory_hash,fact_count,updated_at) VALUES(?1,?2,'',?3,?4,?5)
         ON CONFLICT(book_id,ch) DO UPDATE SET memory_hash=excluded.memory_hash,fact_count=excluded.fact_count,updated_at=excluded.updated_at",
        params![book, ch, memory_hash, fact_count, now],
    )?;
    // C3：人工/导入章记忆建成 ≠ 系统批准。origin!='ai' 封顶 SAVED，
    // 「已建仓」用 memory_hash/fact_count 表达，不推进 MEMORY_SYNCED。
    let origin: String = tx
        .query_row(
            "SELECT origin FROM chapter_state WHERE book_id=?1 AND ch=?2",
            params![book, ch],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "ai".to_string());
    let (to, note) = if origin == "ai" {
        (MEMORY_SYNCED, Value::Null)
    } else {
        (
            SAVED,
            json!(format!("{} 来源章记忆已建，不等价于系统批准", origin)),
        )
    };
    let mut detail = json!({"memoryHash": memory_hash, "factCount": fact_count});
    if !note.is_null() {
        detail["note"] = note;
        detail["origin"] = json!(origin);
    }
    apply_tx(tx, book, ch, to, &detail, now, false)?;
    Ok(())
}

/// 全书章节状态 + 实时健康度：指纹当场重算，不信缓存。
/// health：ok / untracked / no_formal / memory_missing / memory_stale /
///         modified_since_save / dependency_stale / fact_conflicts
pub fn states_view(db: &Db, book: &str) -> Result<Value> {
    ensure_schema(db)?;
    crate::continuity::ensure_schema(db)?;
    let states = db.q_json("SELECT * FROM chapter_state WHERE book_id=?1", &[&book])?;
    let memories = db.q_json(
        "SELECT ch,source_hash,status FROM chapter_memory WHERE book_id=?1",
        &[&book],
    )?;
    let deps = db.q_json(
        "SELECT ch,status FROM draft_dependency WHERE book_id=?1",
        &[&book],
    )?;
    let pendings = db.q_json(
        "SELECT ch,status FROM pending_chapter WHERE book_id=?1",
        &[&book],
    )?;
    let disputed = db.q_json(
        "SELECT valid_from_ch AS ch, COUNT(*) AS c FROM story_fact WHERE book_id=?1 AND state='disputed' GROUP BY valid_from_ch",
        &[&book],
    )?;
    let mut chapters: std::collections::BTreeMap<i64, Value> = std::collections::BTreeMap::new();
    // 原位聚合（不能用闭包：闭包长期持有 &mut map，与后续 insert 冲突）
    fn slot(map: &mut std::collections::BTreeMap<i64, Value>, ch: i64) -> &mut Value {
        map.entry(ch).or_insert_with(|| json!({"ch": ch}))
    }
    for s in &states {
        let ch = s["ch"].as_i64().unwrap_or(0);
        let e = slot(&mut chapters, ch);
        e["state"] = s["state"].clone();
        e["chapterHash"] = s["chapterHash"].clone();
        e["draftHash"] = s["draftHash"].clone();
        e["outlineHash"] = s["outlineHash"].clone();
        e["memoryHash"] = s["memoryHash"].clone();
        e["factCount"] = s["factCount"].clone();
        e["updatedAt"] = s["updatedAt"].clone();
    }
    for m in &memories {
        let ch = m["ch"].as_i64().unwrap_or(0);
        let e = slot(&mut chapters, ch);
        e["memorySourceHash"] = m["sourceHash"].clone();
        e["memoryStatus"] = m["status"].clone();
    }
    for d in &deps {
        let ch = d["ch"].as_i64().unwrap_or(0);
        slot(&mut chapters, ch)["dependencyStatus"] = d["status"].clone();
    }
    for p in &pendings {
        let ch = p["ch"].as_i64().unwrap_or(0);
        slot(&mut chapters, ch)["pendingStatus"] = p["status"].clone();
    }
    for d in &disputed {
        let ch = d["ch"].as_i64().unwrap_or(0);
        slot(&mut chapters, ch)["disputedFacts"] = d["c"].clone();
    }
    // 磁盘实况：正文/待审指纹当场重算（含没有状态行的章节 → untracked）
    for group in ["正文", crate::db::REVIEW_GROUP] {
        let dir = db.books_dir.join(book).join(group);
        if !dir.exists() {
            continue;
        }
        for f in std::fs::read_dir(&dir)? {
            let f = f?;
            if !f.file_type()?.is_file() {
                continue;
            }
            let name = f.file_name().to_string_lossy().to_string();
            let Some(ch) = crate::continuity::chapter_number(&name) else {
                continue;
            };
            let hash = crate::continuity::content_hash(&std::fs::read_to_string(f.path())?);
            let e = slot(&mut chapters, ch);
            if group == "正文" {
                e["formalExists"] = json!(true);
                e["formalHash"] = json!(hash);
            } else {
                e["draftExists"] = json!(true);
                e["draftDiskHash"] = json!(hash);
            }
        }
    }
    let mut out = Vec::new();
    for (ch, e) in &chapters {
        let mut e = e.clone();
        let formal_hash = e["formalHash"].as_str().unwrap_or("");
        let memory_src = e["memorySourceHash"].as_str().unwrap_or("");
        let memory_status = e["memoryStatus"].as_str().unwrap_or("");
        let recorded = e["chapterHash"].as_str().unwrap_or("");
        let health = if e["dependencyStatus"].as_str() == Some("stale") {
            "dependency_stale"
        } else if e["disputedFacts"].as_i64().unwrap_or(0) > 0 {
            "fact_conflicts"
        } else if e["state"].is_null() {
            "untracked"
        } else if formal_hash.is_empty() {
            "no_formal"
        } else if !recorded.is_empty() && recorded != formal_hash {
            "modified_since_save"
        } else if memory_src.is_empty() {
            "memory_missing"
        } else if memory_status != "valid" || memory_src != formal_hash {
            "memory_stale"
        } else {
            "ok"
        };
        e["health"] = json!(health);
        e["memorySynced"] = json!(health == "ok");
        out.push(e);
        let _ = ch;
    }
    Ok(json!({"ok": true, "chapters": out}))
}

/// 单章迁移日志（新→旧）。
pub fn history(db: &Db, book: &str, ch: i64, limit: i64) -> Result<Value> {
    ensure_schema(db)?;
    let lim = limit.clamp(1, 500);
    let rows = db.q_json(
        "SELECT * FROM chapter_state_event WHERE book_id=?1 AND ch=?2 ORDER BY id DESC LIMIT ?3",
        &[&book, &ch, &lim],
    )?;
    Ok(json!({"ok": true, "ch": ch, "events": rows}))
}

/// 单章提交包：绑定 正文/细纲/去味/记忆 四指纹 + 实时校验结果。
pub fn commit_info(db: &Db, book: &str, ch: i64) -> Result<Value> {
    ensure_schema(db)?;
    let rows = db.q_json(
        "SELECT * FROM chapter_state WHERE book_id=?1 AND ch=?2",
        &[&book, &ch],
    )?;
    let st = rows.first().cloned().unwrap_or(Value::Null);
    let name = format!("第{}章.md", ch);
    let formal = files::read_file(db, book, "正文", &name);
    let formal_hash = formal.as_deref().map(crate::continuity::content_hash);
    let outline = files::read_file(db, book, "细纲", &format!("细纲_第{}章.md", ch));
    let memories = db.q_json(
        "SELECT source_hash,status FROM chapter_memory WHERE book_id=?1 AND ch=?2",
        &[&book, &ch],
    )?;
    let mem = memories.first().cloned().unwrap_or(Value::Null);
    let chapter_hash = st["chapterHash"].as_str().unwrap_or("");
    let memory_src = mem["sourceHash"].as_str().unwrap_or("");
    Ok(json!({
        "ok": true, "ch": ch, "state": st,
        "live": {
            "formalExists": formal.is_some(),
            "formalHash": formal_hash,
            "outlineExists": outline.is_some(),
            "chapterHashMatch": !chapter_hash.is_empty() && Some(chapter_hash) == formal_hash.as_deref(),
            "memorySynced": mem["status"].as_str() == Some("valid")
                && !memory_src.is_empty() && Some(memory_src) == formal_hash.as_deref(),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let book = crate::books::create_book(&db, "state-test", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        (dir, db, book)
    }

    #[test]
    fn origin_import_caps_memory_synced_at_saved() {
        // C3：导入章记忆建成不得推进 MEMORY_SYNCED（rank 高于 APPROVED 的语义漂移）
        let (_d, db, book) = fixture();
        record_origin(&db, &book, 1, "import").unwrap();
        {
            let mut conn = db.conn.lock().unwrap();
            let tx = conn.transaction().unwrap();
            memory_synced_tx(&tx, &book, 1, "hash-m", 3, crate::stats::now_ms()).unwrap();
            tx.commit().unwrap();
        }
        let st = db
            .q_json(
                "SELECT state, origin, memory_hash, fact_count FROM chapter_state WHERE book_id=?1 AND ch=1",
                params![book],
            )
            .unwrap();
        assert_eq!(st[0]["state"], SAVED, "导入章封顶 SAVED");
        assert_eq!(st[0]["origin"], "import");
        assert_eq!(st[0]["memoryHash"], "hash-m", "「已建仓」用记忆指纹表达");
        assert_eq!(st[0]["factCount"].as_i64().unwrap(), 3);
    }

    #[test]
    fn origin_ai_reaches_memory_synced_and_explicit_origin_is_sticky() {
        let (_d, db, book) = fixture();
        {
            let mut conn = db.conn.lock().unwrap();
            let tx = conn.transaction().unwrap();
            memory_synced_tx(&tx, &book, 2, "h2", 1, crate::stats::now_ms()).unwrap();
            tx.commit().unwrap();
        }
        let st = db
            .q_json(
                "SELECT state FROM chapter_state WHERE book_id=?1 AND ch=2",
                params![book],
            )
            .unwrap();
        assert_eq!(st[0]["state"], MEMORY_SYNCED, "AI 章语义不变");
        record_origin(&db, &book, 2, "import").unwrap();
        record_origin(&db, &book, 2, "human").unwrap();
        let st2 = db
            .q_json(
                "SELECT origin FROM chapter_state WHERE book_id=?1 AND ch=2",
                params![book],
            )
            .unwrap();
        assert_eq!(st2[0]["origin"], "import", "显式来源不被再次覆盖");
        assert!(record_origin(&db, &book, 2, "alien").is_err());
    }

    #[test]
    fn forward_states_advance_and_never_downgrade() {
        let (_d, db, book) = fixture();
        record_generating(&db, &book, 1, "t1", "m1").unwrap();
        record_reviewed(&db, &book, 1, "ok").unwrap();
        record_humanized(&db, &book, 1, "h1").unwrap();
        record_save(&db, &book, 1, "正文", "c1").unwrap();
        record_approved(&db, &book, 1, json!({})).unwrap();
        // 低 rank 迁移不再撼动 APPROVED，但事件照记
        record_reviewed(&db, &book, 1, "late").unwrap();
        let v = states_view(&db, &book).unwrap();
        let c = &v["chapters"][0];
        assert_eq!(c["state"], json!(APPROVED));
        let h = history(&db, &book, 1, 50).unwrap();
        assert!(h["events"].as_array().unwrap().len() >= 6);
    }

    #[test]
    fn chapter_hash_change_forces_back_to_saved() {
        let (_d, db, book) = fixture();
        record_save(&db, &book, 2, "正文", "aaa").unwrap();
        record_approved(&db, &book, 2, json!({})).unwrap();
        // 同指纹重存：保持 APPROVED
        record_save(&db, &book, 2, "正文", "aaa").unwrap();
        let v = states_view(&db, &book).unwrap();
        assert_eq!(v["chapters"][0]["state"], json!(APPROVED));
        // 指纹变化：如实回退 SAVED（旧审批/旧记忆针对的是前一版）
        record_save(&db, &book, 2, "正文", "bbb").unwrap();
        let v = states_view(&db, &book).unwrap();
        let c = &v["chapters"][0];
        assert_eq!(c["state"], json!(SAVED));
        assert_eq!(c["chapterHash"], json!("bbb"));
    }

    #[test]
    fn draft_save_does_not_dethrone_approved() {
        let (_d, db, book) = fixture();
        record_save(&db, &book, 3, "正文", "aaa").unwrap();
        record_approved(&db, &book, 3, json!({})).unwrap();
        record_save(&db, &book, 3, crate::db::REVIEW_GROUP, "draft1").unwrap();
        let v = states_view(&db, &book).unwrap();
        let c = &v["chapters"][0];
        assert_eq!(c["state"], json!(APPROVED));
        assert_eq!(c["draftHash"], json!("draft1"));
    }

    #[test]
    fn regression_then_recovery_is_allowed() {
        let (_d, db, book) = fixture();
        record_save(&db, &book, 4, "正文", "aaa").unwrap();
        record_interrupted(&db, &book, 4, "停机").unwrap();
        let v = states_view(&db, &book).unwrap();
        assert_eq!(v["chapters"][0]["state"], json!(INTERRUPTED));
        // 回退态之后允许恢复前进
        record_humanized(&db, &book, 4, "h").unwrap();
        let v = states_view(&db, &book).unwrap();
        assert_eq!(v["chapters"][0]["state"], json!(HUMANIZED));
    }

    #[test]
    fn live_health_detects_memory_stale_and_untracked() {
        let (_d, db, book) = fixture();
        // 无状态行、只有磁盘文件 → untracked
        files::write_file(&db, &book, "正文", "第5章.md", "正文内容超过一百五十字……").unwrap();
        // 有状态+正文但无记忆 → memory_missing
        files::write_file(&db, &book, "正文", "第6章.md", "另一章正文内容……").unwrap();
        let h6 = crate::continuity::content_hash("另一章正文内容……");
        record_save(&db, &book, 6, "正文", &h6).unwrap();
        let v = states_view(&db, &book).unwrap();
        let arr = v["chapters"].as_array().unwrap();
        let c5 = arr.iter().find(|c| c["ch"] == 5).unwrap();
        let c6 = arr.iter().find(|c| c["ch"] == 6).unwrap();
        assert_eq!(c5["health"], json!("untracked"));
        assert_eq!(c6["health"], json!("memory_missing"));
    }

    #[test]
    fn lock_goes_top_and_unlock_returns_to_saved() {
        let (_d, db, book) = fixture();
        record_save(&db, &book, 7, "正文", "aaa").unwrap();
        record_locked(&db, &book, 7, true).unwrap();
        let v = states_view(&db, &book).unwrap();
        assert_eq!(v["chapters"][0]["state"], json!(LOCKED));
        record_locked(&db, &book, 7, false).unwrap();
        let v = states_view(&db, &book).unwrap();
        assert_eq!(v["chapters"][0]["state"], json!(SAVED));
    }
}
