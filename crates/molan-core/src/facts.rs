//! 事实账本（长篇稳定性 P0-1）：把章后记忆里的 facts/threads/events 落成
//! 可查询、可追溯、可裁决的一等事实，与记忆提交同事务（P0-4）。
//!
//! 铁律：
//! - 事实带证据（沿用 chapter_memory 的原文子串校验）与来源指纹，可回溯到章节原文；
//! - 同书同主体同属性同生效章出现两个不同值 = 冲突，双方进 disputed，绝不静默覆盖；
//! - 后章新事实取代前章旧值 → 旧值 superseded（保留历史，可问"第N章时是什么状态"）；
//! - 正文被改 → 该章及之后章产出的事实 stale（记录失效，不删除，可审计）；
//! - 提取置信度一律 inferred（LLM 抽取 + 证据校验 ≠ 作者确认），人工 confirm 才升格。
use crate::{db::Db, stats::now_ms};
use anyhow::{anyhow, Result};
use rusqlite::params;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const EXTRACTOR_VERSION: &str = "mem-v1";

pub fn ensure_schema(db: &Db) -> Result<()> {
    db.conn
        .lock()
        .map_err(|_| anyhow!("数据库锁损坏"))?
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS story_fact (
                id TEXT PRIMARY KEY,
                book_id TEXT NOT NULL,
                subject_type TEXT NOT NULL,
                subject_id TEXT NOT NULL,
                predicate TEXT NOT NULL,
                value TEXT NOT NULL,
                valid_from_ch INTEGER NOT NULL,
                valid_to_ch INTEGER,
                source_group TEXT NOT NULL,
                source_name TEXT NOT NULL,
                source_hash TEXT NOT NULL,
                evidence_quote TEXT NOT NULL DEFAULT '',
                confidence TEXT NOT NULL DEFAULT 'inferred',
                state TEXT NOT NULL DEFAULT 'active',
                extractor_version TEXT NOT NULL DEFAULT 'mem-v1',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS idx_story_fact_key ON story_fact(book_id,subject_id,predicate,state);
             CREATE INDEX IF NOT EXISTS idx_story_fact_state ON story_fact(book_id,state);
             CREATE INDEX IF NOT EXISTS idx_story_fact_ch ON story_fact(book_id,valid_from_ch);",
        )?;
    Ok(())
}

/// 确定性 id：同一章同一内容重复提取得到同一 id（INSERT OR IGNORE 幂等）。
fn fact_id(book: &str, st: &str, sid: &str, pred: &str, from_ch: i64, value: &str) -> String {
    let raw = format!("{}|{}|{}|{}|{}|{}", book, st, sid, pred, from_ch, value);
    format!("{:x}", Sha256::digest(raw.as_bytes()))[..24].to_string()
}

#[derive(Default, Debug)]
pub struct ExtractOutcome {
    pub inserted: usize,
    pub superseded: usize,
    pub disputed: usize,
    pub skipped: usize,
}

/// 在记忆提交事务内调用：把本章记忆的 facts/threads/events 落成事实。
/// payload 已过 validated_payload（证据是来源正文子串），此处不再重复校验证据。
pub fn extract_from_memory_tx(
    tx: &rusqlite::Transaction,
    book: &str,
    ch: i64,
    name: &str,
    source_hash: &str,
    payload: &Value,
    now: i64,
) -> Result<ExtractOutcome> {
    let mut items: Vec<(String, String, String, String, String)> = Vec::new();
    for f in payload["facts"].as_array().into_iter().flatten() {
        items.push((
            "entity".to_string(),
            f["entity"].as_str().unwrap_or("").to_string(),
            f["field"].as_str().unwrap_or("").to_string(),
            f["value"].as_str().unwrap_or("").to_string(),
            f["evidence"].as_str().unwrap_or("").to_string(),
        ));
    }
    for t in payload["threads"].as_array().into_iter().flatten() {
        items.push((
            "thread".to_string(),
            t["id"].as_str().unwrap_or("").to_string(),
            "state".to_string(),
            t["state"].as_str().unwrap_or("").to_string(),
            t["evidence"].as_str().unwrap_or("").to_string(),
        ));
    }
    for e in payload["events"].as_array().into_iter().flatten() {
        let desc = e["description"].as_str().unwrap_or("").to_string();
        let sid = format!("{:x}", Sha256::digest(desc.as_bytes()))[..16].to_string();
        items.push((
            "event".to_string(),
            sid,
            "occurred".to_string(),
            desc,
            e["evidence"].as_str().unwrap_or("").to_string(),
        ));
    }
    // 秘密：不进 facts/threads/events 三数组，单独成 subject_type=secret（谁知道/谁不知道）
    for s in payload["secrets"].as_array().into_iter().flatten() {
        let fact = s["fact"].as_str().unwrap_or("").to_string();
        if fact.is_empty() {
            continue;
        }
        let sid = {
            let raw = s["id"].as_str().unwrap_or("").trim().to_string();
            if raw.is_empty() {
                format!("{:x}", Sha256::digest(fact.as_bytes()))[..16].to_string()
            } else {
                raw
            }
        };
        let value = json!({"fact": fact, "known_by": s["known_by"], "unknown_to": s["unknown_to"]})
            .to_string();
        items.push((
            "secret".to_string(),
            sid,
            "knowledge".to_string(),
            value,
            s["evidence"].as_str().unwrap_or("").to_string(),
        ));
    }
    let mut out = ExtractOutcome::default();
    for (st, sid, pred, value, evidence) in items {
        if sid.is_empty() || pred.is_empty() || value.is_empty() {
            out.skipped += 1;
            continue;
        }
        let id = fact_id(book, &st, &sid, &pred, ch, &value);
        // 幂等重放：完全相同的事实已存在（含已被人工处置过的），不再处理
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM story_fact WHERE id=?1)",
            params![&id],
            |r| r.get(0),
        )?;
        if exists {
            // 重建场景：同 id 事实若已失效（stale/superseded），且本次来源记忆有效，予以复活；
            // retired（人工作废）与 disputed（冲突未裁决）不复活，尊重处置。
            let st0: String = tx.query_row(
                "SELECT state FROM story_fact WHERE id=?1",
                params![&id],
                |r| r.get(0),
            )?;
            // superseded 表示已被后章事实明确取代，不能因旧章重建而复活；
            // 只有正文改写导致的 stale 才允许同 hash 回填 active。
            if st0 == "stale" {
                tx.execute(
                    "UPDATE story_fact SET state='active',valid_to_ch=NULL,updated_at=?2 WHERE id=?1",
                    params![&id, now],
                )?;
                out.inserted += 1;
            } else {
                out.skipped += 1;
            }
            continue;
        }
        // 同章同键不同值 = 真冲突（记忆提取自相矛盾）：双方进 disputed，记入事件
        // 同章已存在的同键事实（含已被判 disputed 的）：第三、四种取值同样算冲突
        let same_ch: Option<(String, String)> = tx
            .query_row(
                "SELECT id,value FROM story_fact WHERE book_id=?1 AND subject_type=?2 AND subject_id=?3
                 AND predicate=?4 AND valid_from_ch=?5 AND state IN ('active','disputed') LIMIT 1",
                params![book, &st, &sid, &pred, ch],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .ok();
        let mut new_state = "active";
        if let Some((old_id, old_value)) = same_ch {
            if old_value != value {
                tx.execute(
                    "UPDATE story_fact SET state='disputed',updated_at=?2 WHERE id=?1",
                    params![&old_id, now],
                )?;
                new_state = "disputed";
                out.disputed += 1;
                tx.execute(
                    "INSERT INTO continuity_event(book_id,kind,detail_json,created_at) VALUES(?1,'fact_conflict',?2,?3)",
                    params![
                        book,
                        json!({"chapter": ch, "subjectType": st, "subjectId": sid,
                               "predicate": pred, "valueA": old_value, "valueB": value})
                        .to_string(),
                        now
                    ],
                )?;
            } else {
                // 同章同值重提（抽取器重试）：幂等跳过
                out.skipped += 1;
                continue;
            }
        }
        // 后章新值取代前章旧值（event 是独立发生记录，secret 是累积知识，均不互相取代）
        if st != "event" && st != "secret" && new_state == "active" {
            out.superseded += tx.execute(
                "UPDATE story_fact SET state='superseded',valid_to_ch=?5,updated_at=?6
                 WHERE book_id=?1 AND subject_type=?2 AND subject_id=?3 AND predicate=?4
                 AND state='active' AND valid_from_ch<?5",
                params![book, &st, &sid, &pred, ch, now],
            )?;
        }
        tx.execute(
            "INSERT OR IGNORE INTO story_fact(id,book_id,subject_type,subject_id,predicate,value,
                valid_from_ch,valid_to_ch,source_group,source_name,source_hash,evidence_quote,
                confidence,state,extractor_version,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,NULL,'正文',?8,?9,?10,'inferred',?11,?12,?13,?13)",
            params![
                &id,
                book,
                &st,
                &sid,
                &pred,
                &value,
                ch,
                name,
                source_hash,
                &evidence,
                new_state,
                EXTRACTOR_VERSION,
                now
            ],
        )?;
        out.inserted += 1;
    }
    Ok(out)
}

/// 正文第 from_ch 章被改写：该章及之后章提取的活跃事实全部标记 stale（在 note_file_change 事务内）。
pub fn invalidate_from_tx(
    tx: &rusqlite::Transaction,
    book: &str,
    from_ch: i64,
    now: i64,
) -> Result<usize> {
    let n = tx.execute(
        "UPDATE story_fact SET state='stale',updated_at=?3
         WHERE book_id=?1 AND state IN ('active','disputed') AND valid_from_ch>=?2",
        params![book, from_ch, now],
    )?;
    Ok(n)
}

/// 事实查询。state 过滤：缺省 "current"（active+disputed，即当前认知+未裁决冲突）；
/// "all" 含历史；其余按具体状态。
pub fn list_facts(db: &Db, book: &str, subject: &str, state: &str, limit: i64) -> Result<Value> {
    ensure_schema(db)?;
    crate::continuity::ensure_book(db, book)?;
    let lim = limit.clamp(1, 2000);
    let mut sql = String::from("SELECT * FROM story_fact WHERE book_id=?1");
    let mut params_v: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(book.to_string())];
    if !subject.is_empty() {
        sql.push_str(" AND (subject_id=?2 OR subject_id LIKE ?3)");
        params_v.push(Box::new(subject.to_string()));
        params_v.push(Box::new(format!("%{}%", subject)));
    }
    match state {
        "" | "current" => sql.push_str(" AND state IN ('active','disputed')"),
        "all" => {}
        // 白名单映射，杜绝 SQL 拼接注入
        "active" => sql.push_str(" AND state='active'"),
        "disputed" => sql.push_str(" AND state='disputed'"),
        "superseded" => sql.push_str(" AND state='superseded'"),
        "stale" => sql.push_str(" AND state='stale'"),
        "retired" => sql.push_str(" AND state='retired'"),
        other => anyhow::bail!("未知事实状态：{}", other),
    }
    sql.push_str(" ORDER BY valid_from_ch, subject_id, predicate LIMIT ?");
    params_v.push(Box::new(lim));
    let refs: Vec<&dyn rusqlite::ToSql> = params_v.iter().map(|b| b.as_ref()).collect();
    let rows = db.q_json(&sql, &refs)?;
    let counts = db.q_json(
        "SELECT state, COUNT(*) AS c FROM story_fact WHERE book_id=?1 GROUP BY state",
        &[&book],
    )?;
    Ok(json!({"ok": true, "facts": rows, "counts": counts}))
}

/// 未裁决冲突清单（disputed），附带双方的来源章节与证据。
pub fn conflicts(db: &Db, book: &str) -> Result<Value> {
    ensure_schema(db)?;
    crate::continuity::ensure_book(db, book)?;
    let rows = db.q_json(
        "SELECT * FROM story_fact WHERE book_id=?1 AND state='disputed'
         ORDER BY subject_id, predicate, valid_from_ch",
        &[&book],
    )?;
    let events = db.q_json(
        "SELECT id,kind,detail_json,created_at FROM continuity_event
         WHERE book_id=?1 AND kind='fact_conflict' ORDER BY id DESC LIMIT 100",
        &[&book],
    )?;
    Ok(json!({"ok": true, "disputed": rows, "count": rows.len(), "events": events}))
}

/// 人工裁决：confirm（升格 confirmed）/ dispute（标冲突）/ retire（作废）。
pub fn set_state(db: &Db, id: &str, action: &str) -> Result<Value> {
    ensure_schema(db)?;
    let now = now_ms();
    let (state, confidence) = match action {
        "confirm" => ("active", "confirmed"),
        "dispute" => ("disputed", "inferred"),
        "retire" => ("retired", "inferred"),
        _ => anyhow::bail!("未知裁决动作：{}（支持 confirm/dispute/retire）", action),
    };
    let n = db.exec(
        "UPDATE story_fact SET state=?2, confidence=?3, updated_at=?4 WHERE id=?1",
        &[&id, &state, &confidence, &now],
    )?;
    if n == 0 {
        anyhow::bail!("事实不存在：{}", id);
    }
    let rows = db.q_json("SELECT * FROM story_fact WHERE id=?1", &[&id])?;
    let book = rows
        .first()
        .and_then(|r| r["bookId"].as_str())
        .unwrap_or("")
        .to_string();
    if !book.is_empty() {
        let _ = db.exec(
            "INSERT INTO continuity_event(book_id,kind,detail_json,created_at) VALUES(?1,'fact_triaged',?2,?3)",
            &[&book, &json!({"factId": id, "action": action}).to_string(), &now],
        );
    }
    Ok(json!({"ok": true, "fact": rows.first().cloned().unwrap_or(Value::Null)}))
}

/// 从已 valid 的章后记忆回填事实账本（不调用 LLM，payload 提交时已校验证据）。
/// 逐章独立事务；只处理「记忆指纹 == 当前正文指纹」的章，stale 记忆跳过不计。
/// 幂等：确定性 id + INSERT OR IGNORE + 失效复活，重复执行结果收敛。
pub fn rebuild_from_memories(db: &Db, book: &str) -> Result<Value> {
    ensure_schema(db)?;
    crate::continuity::ensure_schema(db)?;
    crate::continuity::ensure_book(db, book)?;
    let rows = db.q_json(
        "SELECT ch,name,source_hash,payload_json FROM chapter_memory
         WHERE book_id=?1 AND status='valid' ORDER BY ch",
        &[&book],
    )?;
    let mut done = Vec::new();
    let mut skipped_stale = Vec::new();
    let mut failed = Vec::new();
    let mut total_inserted = 0usize;
    let mut total_disputed = 0usize;
    for row in rows {
        let ch = row["ch"].as_i64().unwrap_or(0);
        let name = row["name"].as_str().unwrap_or("").to_string();
        let src_hash = row["sourceHash"].as_str().unwrap_or("").to_string();
        // 记忆必须仍与当前正文一致，否则不建仓（防止旧记忆冒充当前事实）
        let cur = crate::files::read_file(db, book, "正文", &name)
            .map(|t| crate::continuity::content_hash(&t));
        if cur.as_deref() != Some(src_hash.as_str()) {
            skipped_stale.push(ch);
            continue;
        }
        let payload: Value = match serde_json::from_str(row["payloadJson"].as_str().unwrap_or("")) {
            Ok(p) => p,
            Err(e) => {
                failed.push(json!({"ch": ch, "error": format!("记忆载荷解析失败：{}", e)}));
                continue;
            }
        };
        let r = (|| -> Result<ExtractOutcome> {
            let mut conn = db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))?;
            let tx = conn.transaction()?;
            let now = now_ms();
            let o = extract_from_memory_tx(&tx, book, ch, &name, &src_hash, &payload, now)?;
            crate::chapter_state::memory_synced_tx(
                &tx,
                book,
                ch,
                &src_hash,
                o.inserted as i64,
                now,
            )?;
            tx.commit()?;
            Ok(o)
        })();
        match r {
            Ok(o) => {
                total_inserted += o.inserted;
                total_disputed += o.disputed;
                done.push(ch);
            }
            Err(e) => failed.push(json!({"ch": ch, "error": format!("{}", e)})),
        }
    }
    Ok(json!({
        "ok": failed.is_empty(),
        "rebuilt": done, "skippedStale": skipped_stale, "failed": failed,
        "factsInserted": total_inserted, "disputed": total_disputed,
        "note": "幂等：重复执行不会重复建事实；stale 记忆（与正文不符）不建仓，请先 rebuild_memory",
    }))
}

/// 严格门禁用：是否存在未裁决冲突。
pub fn has_disputed(db: &Db, book: &str) -> bool {
    ensure_schema(db).ok();
    db.q_json(
        "SELECT 1 AS x FROM story_fact WHERE book_id=?1 AND state='disputed' LIMIT 1",
        &[&book],
    )
    .map(|r| !r.is_empty())
    .unwrap_or(false)
}

/// 伏笔（thread）滞留告警：按全书最大章号 - 最后一次出现的章号算年龄，
/// 未回收态（planted/advanced/partial）且年龄 >= stale_after 的算 overdue。
pub fn thread_alerts(db: &Db, book: &str, stale_after: i64) -> Result<Value> {
    ensure_schema(db)?;
    crate::continuity::ensure_book(db, book)?;
    let stale_after = if stale_after <= 0 { 8 } else { stale_after };
    let threads = db.q_json(
        "SELECT subject_id, value, valid_from_ch FROM story_fact
         WHERE book_id=?1 AND subject_type='thread' AND predicate='state' AND state='active'",
        &[&book],
    )?;
    // 全书最大章号：以正文目录里的 .md 文件名为准（记忆/事实表只覆盖已抽取的章）
    let mut max_ch: i64 = 0;
    let dir = db.books_dir.join(book).join("正文");
    if dir.exists() {
        for f in std::fs::read_dir(&dir)? {
            let f = f?;
            if !f.file_type()?.is_file() {
                continue;
            }
            let name = f.file_name().to_string_lossy().to_string();
            if !name.ends_with(".md") {
                continue;
            }
            if let Some(ch) = crate::continuity::chapter_number(&name) {
                max_ch = max_ch.max(ch);
            }
        }
    }
    let mut alerts: Vec<Value> = Vec::new();
    for t in &threads {
        let last_ch = t["validFromCh"].as_i64().unwrap_or(0);
        let last_state = t["value"].as_str().unwrap_or("").to_string();
        let age = max_ch - last_ch;
        // 只有"埋下但没回收"的态才告警：resolved/dropped 等已终态不算滞留
        let overdue =
            age >= stale_after && matches!(last_state.as_str(), "planted" | "advanced" | "partial");
        alerts.push(json!({
            "threadId": t["subjectId"].as_str().unwrap_or(""),
            "lastState": last_state,
            "lastChapter": last_ch,
            "age": age,
            "overdue": overdue,
        }));
    }
    // 最久未动的排前面
    alerts.sort_by_key(|a| std::cmp::Reverse(a["age"].as_i64().unwrap_or(0)));
    Ok(json!({
        "ok": true, "maxChapter": max_ch, "staleAfter": stale_after, "alerts": alerts,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::continuity::{content_hash, record_chapter_memory};
    fn fixture() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let book = crate::books::create_book(&db, "facts-test", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        (dir, db, book)
    }
    fn mem(facts: Value) -> Value {
        json!({"summary":"章摘要。","facts":facts,"threads":[],"events":[]})
    }
    fn active_values(db: &Db, book: &str, sid: &str) -> Vec<String> {
        db.q_json(
            "SELECT value FROM story_fact WHERE book_id=?1 AND subject_id=?2 AND state='active'",
            &[&book, &sid],
        )
        .unwrap()
        .iter()
        .filter_map(|r| r["value"].as_str().map(String::from))
        .collect()
    }

    #[test]
    fn extraction_is_idempotent_per_chapter() {
        let (_d, db, book) = fixture();
        let body = "阿青左臂受伤，留下伤痕。他离开了青石城。";
        crate::files::write_file(&db, &book, "正文", "第1章.md", body).unwrap();
        let p = mem(json!([
            {"entity":"阿青","field":"伤势","value":"左臂受伤","evidence":"阿青左臂受伤"},
            {"entity":"阿青","field":"位置","value":"离开青石城","evidence":"他离开了青石城"}
        ]));
        record_chapter_memory(&db, &book, 1, "第1章.md", &content_hash(body), &p).unwrap();
        record_chapter_memory(&db, &book, 1, "第1章.md", &content_hash(body), &p).unwrap();
        let v = list_facts(&db, &book, "", "all", 100).unwrap();
        // 重放不产生重复事实
        assert_eq!(v["facts"].as_array().unwrap().len(), 2);
        assert!(!has_disputed(&db, &book));
    }

    #[test]
    fn later_chapter_supersedes_earlier_value() {
        let (_d, db, book) = fixture();
        let b1 = "阿青左臂受伤，留下伤痕。";
        let b2 = "大夫治好了阿青的左臂，伤势痊愈。";
        crate::files::write_file(&db, &book, "正文", "第1章.md", b1).unwrap();
        crate::files::write_file(&db, &book, "正文", "第2章.md", b2).unwrap();
        record_chapter_memory(&db, &book, 1, "第1章.md", &content_hash(b1),
            &mem(json!([{"entity":"阿青","field":"伤势","value":"左臂受伤","evidence":"阿青左臂受伤"}]))).unwrap();
        record_chapter_memory(
            &db,
            &book,
            2,
            "第2章.md",
            &content_hash(b2),
            &mem(json!([{"entity":"阿青","field":"伤势","value":"痊愈","evidence":"左臂，伤势痊愈"}])),
        )
        .unwrap();
        // 当前有效值只有"痊愈"；旧值 superseded 且带有效期
        assert_eq!(active_values(&db, &book, "阿青"), vec!["痊愈"]);
        let old = db
            .q_json(
                "SELECT state,valid_to_ch FROM story_fact WHERE book_id=?1 AND value='左臂受伤'",
                &[&book],
            )
            .unwrap();
        assert_eq!(old[0]["state"], json!("superseded"));
        assert_eq!(old[0]["validToCh"], json!(2));
    }

    #[test]
    fn same_chapter_different_value_becomes_disputed() {
        let (_d, db, book) = fixture();
        let body = "阿青的剑是青钢剑。阿青的剑是玄铁剑。";
        crate::files::write_file(&db, &book, "正文", "第1章.md", body).unwrap();
        // validated_payload 允许同章两条不同值（去重按整条 JSON），账本必须裁决
        let p = mem(json!([
            {"entity":"阿青的剑","field":"材质","value":"青钢","evidence":"阿青的剑是青钢剑"},
            {"entity":"阿青的剑","field":"材质","value":"玄铁","evidence":"阿青的剑是玄铁剑"}
        ]));
        record_chapter_memory(&db, &book, 1, "第1章.md", &content_hash(body), &p).unwrap();
        assert!(has_disputed(&db, &book));
        let c = conflicts(&db, &book).unwrap();
        assert_eq!(c["count"], json!(2));
        // 人工裁决其中一条
        let id = c["disputed"][0]["id"].as_str().unwrap().to_string();
        set_state(&db, &id, "confirm").unwrap();
        let f = db
            .q_json(
                "SELECT state,confidence FROM story_fact WHERE id=?1",
                &[&id],
            )
            .unwrap();
        assert_eq!(f[0]["state"], json!("active"));
        assert_eq!(f[0]["confidence"], json!("confirmed"));
    }

    #[test]
    fn rebuild_from_memories_backfills_idempotently_and_revives() {
        let (_d, db, book) = fixture();
        let b1 = "阿青左臂受伤，留下伤痕。";
        crate::files::write_file(&db, &book, "正文", "第1章.md", b1).unwrap();
        record_chapter_memory(&db, &book, 1, "第1章.md", &content_hash(b1),
            &mem(json!([{"entity":"阿青","field":"伤势","value":"左臂受伤","evidence":"阿青左臂受伤"}]))).unwrap();
        // 模拟升级前旧库：清空事实表后回填
        db.exec("DELETE FROM story_fact", &[]).unwrap();
        assert_eq!(active_values(&db, &book, "阿青").len(), 0);
        let r = rebuild_from_memories(&db, &book).unwrap();
        assert_eq!(r["factsInserted"], json!(1));
        assert_eq!(active_values(&db, &book, "阿青"), vec!["左臂受伤"]);
        // 重复执行幂等
        let r2 = rebuild_from_memories(&db, &book).unwrap();
        assert_eq!(r2["factsInserted"], json!(0));
        // 失效后重建 → 复活为 active
        db.exec("UPDATE story_fact SET state='stale'", &[]).unwrap();
        let r3 = rebuild_from_memories(&db, &book).unwrap();
        assert_eq!(r3["factsInserted"], json!(1));
        assert_eq!(active_values(&db, &book, "阿青"), vec!["左臂受伤"]);
        // 已被后章事实取代的旧值不能被旧记忆回填复活
        db.exec(
            "UPDATE story_fact SET state='superseded',valid_to_ch=2",
            &[],
        )
        .unwrap();
        let r4 = rebuild_from_memories(&db, &book).unwrap();
        assert_eq!(r4["factsInserted"], json!(0));
        assert!(active_values(&db, &book, "阿青").is_empty());
    }

    #[test]
    fn chapter_rewrite_stales_derived_facts() {
        let (_d, db, book) = fixture();
        let b1 = "阿青左臂受伤，留下伤痕。";
        let b2 = "第2章内容，阿青赶路。";
        crate::files::write_file(&db, &book, "正文", "第1章.md", b1).unwrap();
        crate::files::write_file(&db, &book, "正文", "第2章.md", b2).unwrap();
        record_chapter_memory(&db, &book, 1, "第1章.md", &content_hash(b1),
            &mem(json!([{"entity":"阿青","field":"伤势","value":"左臂受伤","evidence":"阿青左臂受伤"}]))).unwrap();
        record_chapter_memory(
            &db,
            &book,
            2,
            "第2章.md",
            &content_hash(b2),
            &mem(json!([{"entity":"阿青","field":"位置","value":"在路上","evidence":"内容，阿青赶路"}])),
        )
        .unwrap();
        // 改写第1章 → 第1章及之后的事实 stale（note_file_change 事务内联动）
        let b1v2 = "阿青左臂受伤，留下伤痕。伤势加重。";
        crate::files::write_file(&db, &book, "正文", "第1章.md", b1v2).unwrap();
        let states = db
            .q_json(
                "SELECT state,COUNT(*) AS c FROM story_fact WHERE book_id=?1 GROUP BY state",
                &[&book],
            )
            .unwrap();
        let stale: i64 = states
            .iter()
            .filter(|r| r["state"] == "stale")
            .map(|r| r["c"].as_i64().unwrap_or(0))
            .sum();
        assert_eq!(stale, 2, "两章的事实都应失效");
        assert_eq!(active_values(&db, &book, "阿青").len(), 0);
    }

    #[test]
    fn thread_alerts_marks_overdue_threads() {
        let (_d, db, book) = fixture();
        // 正文写到第12章：全书最大章号 = 12。
        // 必须先写文件再插事实：write_file 会经 note_file_change 把
        // valid_from_ch >= 改写章的活跃事实标 stale，顺序反了会把测试数据全部失效。
        for n in 1..=12 {
            let body = format!("第{}章正文内容。", n);
            crate::files::write_file(&db, &book, "正文", &format!("第{}章.md", n), &body).unwrap();
        }
        // 直接落两条伏笔事实：FH-01 埋在第2章不回收，FH-02 第11章已 resolved
        for (sid, value, ch) in [("FH-01", "planted", 2i64), ("FH-02", "resolved", 11)] {
            db.exec(
                "INSERT INTO story_fact(id,book_id,subject_type,subject_id,predicate,value,
                    valid_from_ch,valid_to_ch,source_group,source_name,source_hash,evidence_quote,
                    confidence,state,extractor_version,created_at,updated_at)
                 VALUES(?1,?2,'thread',?3,'state',?4,?5,NULL,'正文','第2章.md','x','',
                    'inferred','active','mem-v1',1,1)",
                &[&format!("t{}", ch), &book, &sid, &value, &ch],
            )
            .unwrap();
        }
        let v = thread_alerts(&db, &book, 8).unwrap();
        assert_eq!(v["maxChapter"], json!(12));
        assert_eq!(v["staleAfter"], json!(8));
        let alerts = v["alerts"].as_array().unwrap();
        assert_eq!(alerts.len(), 2);
        // age 降序：FH-01(12-2=10) 在前，FH-02(12-11=1) 在后
        let fh01 = &alerts[0];
        assert_eq!(fh01["threadId"], json!("FH-01"));
        assert_eq!(fh01["lastState"], json!("planted"));
        assert_eq!(fh01["age"], json!(10));
        assert_eq!(fh01["overdue"], json!(true));
        let fh02 = &alerts[1];
        assert_eq!(fh02["threadId"], json!("FH-02"));
        // 已回收（resolved）不算滞留
        assert_eq!(fh02["age"], json!(1));
        assert_eq!(fh02["overdue"], json!(false));
    }

    #[test]
    fn secrets_extract_as_knowledge_facts() {
        let (_d, db, book) = fixture();
        let mut conn = db.conn.lock().unwrap();
        let tx = conn.transaction().unwrap();
        extract_from_memory_tx(
            &tx,
            &book,
            1,
            "第1章.md",
            "hash1",
            &json!({"secrets":[{"id":"SEC_01","fact":"玉简藏信标",
                "known_by":["算盘鬼"],"unknown_to":["陆沉"],"evidence":"染血玉简"}]}),
            1234567890,
        )
        .unwrap();
        tx.commit().unwrap();
        drop(conn);
        let rows = db
            .q_json(
                "SELECT * FROM story_fact WHERE book_id=?1 AND subject_type='secret'",
                &[&book],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["subjectId"], json!("SEC_01"));
        assert_eq!(rows[0]["predicate"], json!("knowledge"));
        assert_eq!(rows[0]["state"], json!("active"));
        let value: Value = serde_json::from_str(rows[0]["value"].as_str().unwrap()).unwrap();
        assert_eq!(value["fact"], json!("玉简藏信标"));
        let known: Vec<String> = value["known_by"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
        assert!(known.contains(&"算盘鬼".to_string()));
    }
}
