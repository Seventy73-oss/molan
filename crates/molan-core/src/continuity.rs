//! Versioned, evidence-backed chapter memory. Author documents remain authoritative.
//! File mutation callers hold Db::fs_lock before note_file_change; this module never
//! rewrites manuscripts/author Skill cards while invalidating derived data.
use crate::{db::Db, files, stats::now_ms};
use anyhow::{anyhow, ensure, Result};
use rusqlite::params;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub fn content_hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

pub fn ensure_schema(db: &Db) -> Result<()> {
    db.conn
        .lock()
        .map_err(|_| anyhow!("数据库锁损坏"))?
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS file_revision (
            book_id TEXT NOT NULL, group_name TEXT NOT NULL, name TEXT NOT NULL,
            content_hash TEXT, revision INTEGER NOT NULL DEFAULT 1, updated_at INTEGER NOT NULL,
            PRIMARY KEY(book_id,group_name,name));
         CREATE TABLE IF NOT EXISTS chapter_memory (
            book_id TEXT NOT NULL, ch INTEGER NOT NULL, name TEXT NOT NULL,
            source_hash TEXT NOT NULL, payload_json TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'valid', updated_at INTEGER NOT NULL,
            PRIMARY KEY(book_id,ch));
         CREATE TABLE IF NOT EXISTS memory_job (
            book_id TEXT NOT NULL,ch INTEGER NOT NULL,name TEXT NOT NULL,
            source_hash TEXT NOT NULL,status TEXT NOT NULL DEFAULT 'pending',error TEXT,
            updated_at INTEGER NOT NULL,PRIMARY KEY(book_id,ch));
         CREATE TABLE IF NOT EXISTS draft_origin (
            book_id TEXT NOT NULL,ch INTEGER NOT NULL,name TEXT NOT NULL,source_hash TEXT NOT NULL,
            task_id TEXT NOT NULL,updated_at INTEGER NOT NULL,PRIMARY KEY(book_id,ch));
         CREATE TABLE IF NOT EXISTS draft_dependency (
            book_id TEXT NOT NULL,ch INTEGER NOT NULL,parent_ch INTEGER NOT NULL,
            parent_hash TEXT NOT NULL,task_id TEXT NOT NULL,status TEXT NOT NULL DEFAULT 'valid',
            updated_at INTEGER NOT NULL,PRIMARY KEY(book_id,ch));
         CREATE TABLE IF NOT EXISTS story_plan (
            book_id TEXT NOT NULL,kind TEXT NOT NULL,payload_json TEXT NOT NULL,
            revision INTEGER NOT NULL DEFAULT 1,updated_at INTEGER NOT NULL,
            PRIMARY KEY(book_id,kind));
         CREATE TABLE IF NOT EXISTS continuity_event (
            id INTEGER PRIMARY KEY AUTOINCREMENT,book_id TEXT NOT NULL,
            kind TEXT NOT NULL,detail_json TEXT NOT NULL,created_at INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS idx_continuity_event_book ON continuity_event(book_id,id);",
        )?;
    Ok(())
}

pub(crate) fn ensure_book(db: &Db, book: &str) -> Result<()> {
    ensure!(
        !book.is_empty() && files::safe_name(book) == book,
        "无效书籍标识"
    );
    ensure!(
        !db.q_json(
            "SELECT id FROM books WHERE id=?1 AND deleted_at IS NULL",
            &[&book]
        )?
        .is_empty(),
        "书籍不存在或已删除"
    );
    Ok(())
}

/// 中文数字值（章名范围：个位数字）。
fn zh_digit(c: char) -> i64 {
    match c {
        '一' => 1,
        '二' => 2,
        '三' => 3,
        '四' => 4,
        '五' => 5,
        '六' => 6,
        '七' => 7,
        '八' => 8,
        '九' => 9,
        _ => 0,
    }
}

/// 中文数字章号（十一/一百二十三/二千…）：单位累加解析，返回 (值, 消费字符数)。
fn zh_number(cs: &[char], start: usize) -> Option<(i64, usize)> {
    let (mut sec, mut cur, mut i) = (0i64, 0i64, start);
    while i < cs.len() {
        let c = cs[i];
        if c == '十' || c == '百' || c == '千' {
            let unit = if c == '十' {
                10
            } else if c == '百' {
                100
            } else {
                1000
            };
            sec += if cur == 0 { unit } else { cur * unit };
            cur = 0;
        } else if zh_digit(c) > 0 {
            cur = zh_digit(c);
        } else {
            break;
        }
        i += 1;
    }
    (sec + cur > 0 && i > start).then(|| (sec + cur, i))
}

/// 章号解析（文件名锚定）：必须以「第」开头 + 阿拉伯或中文数字 + 「章」。
/// F3：中文数字章名（第十一章.md）批准后，记忆/依赖/批准凭证必须同样可达。
pub fn chapter_number(name: &str) -> Option<i64> {
    let rest = name.strip_prefix('第')?;
    let cs: Vec<char> = rest.trim_start().chars().collect();
    let mut i = 0usize;
    let mut num: i64 = 0;
    while i < cs.len() && cs[i].is_ascii_digit() {
        num = num
            .saturating_mul(10)
            .saturating_add(cs[i].to_digit(10)? as i64);
        i += 1;
    }
    if i == 0 {
        let (v, ni) = zh_number(&cs, 0)?;
        num = v;
        i = ni;
    }
    if num == 0 || num > 1_000_000 {
        return None;
    }
    let tail: String = cs[i..].iter().skip_while(|c| c.is_whitespace()).collect();
    if !tail.starts_with('章') {
        return None;
    }
    Some(num)
}

/// Called after a durable manuscript mutation, while the file lock is still held.
/// Database updates form one transaction. No author document is deleted/rebuilt.
pub fn note_file_change(
    db: &Db,
    book: &str,
    group: &str,
    name: &str,
    old: Option<&str>,
    new: Option<&str>,
) -> Result<()> {
    if old == new {
        return Ok(());
    }
    ensure_schema(db)?;
    let group = files::normalize_group(group);
    let hash = new.map(content_hash);
    // Promotion from pending to formal is not a semantic revision when hashes match.
    let promoted_hash = if group == "正文待审" && new.is_none() {
        files::read_file(db, book, "正文", name)
            .as_deref()
            .map(content_hash)
    } else {
        None
    };
    let mut conn = db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))?;
    let tx = conn.transaction()?;
    let now = now_ms();
    if matches!(group.as_str(), "正文" | "正文待审") {
        if let Some(ch) = chapter_number(name) {
            let effective = hash
                .as_ref()
                .or(promoted_hash.as_ref())
                .map(String::as_str)
                .unwrap_or("");
            tx.execute("UPDATE draft_dependency SET status='stale',updated_at=?4 WHERE book_id=?1 AND parent_ch=?2 AND parent_hash<>?3",params![book,ch,effective,now])?;
            // Cascade only from rows staled by THIS change (updated_at=now): historical
            // stale rows must not re-amplify later edits (C5); re-registration clears.
            tx.execute("UPDATE draft_dependency SET status='stale',updated_at=?2 WHERE book_id=?1 AND ch>(SELECT MIN(ch) FROM draft_dependency WHERE book_id=?1 AND status='stale' AND updated_at=?2)",params![book,now])?;
        }
    }
    if group == "正文待审" && new.is_none() {
        if let Some(ch) = chapter_number(name) {
            // Dependencies describe an existing draft, not the chapter slot forever.
            // Its descendants were invalidated above before removing this row.
            tx.execute(
                "DELETE FROM draft_origin WHERE book_id=?1 AND ch=?2",
                params![book, ch],
            )?;
            tx.execute(
                "DELETE FROM draft_dependency WHERE book_id=?1 AND ch=?2",
                params![book, ch],
            )?;
        }
    }
    tx.execute("INSERT INTO file_revision(book_id,group_name,name,content_hash,revision,updated_at) VALUES(?1,?2,?3,?4,1,?5)
        ON CONFLICT(book_id,group_name,name) DO UPDATE SET content_hash=excluded.content_hash,revision=file_revision.revision+1,updated_at=excluded.updated_at",
        params![book,group,name,hash,now])?;
    let mut facts_staled = 0usize;
    if group == "正文" {
        if let Some(ch) = chapter_number(name) {
            tx.execute("UPDATE chapter_memory SET status='stale',updated_at=?3 WHERE book_id=?1 AND ch>=?2", params![book,ch,now])?;
            tx.execute("UPDATE memory_job SET status='pending',error='前文章节修订，需重建记忆',updated_at=?3 WHERE book_id=?1 AND ch>=?2", params![book,ch,now])?;
            // 事实账本联动：该章及之后章提取的活跃事实全部 stale（不删除，可审计）
            facts_staled = crate::facts::invalidate_from_tx(&tx, book, ch, now)?;
            if let Some(h) = &hash {
                tx.execute("INSERT INTO memory_job(book_id,ch,name,source_hash,status,updated_at) VALUES(?1,?2,?3,?4,'pending',?5)
                    ON CONFLICT(book_id,ch) DO UPDATE SET name=excluded.name,source_hash=excluded.source_hash,status='pending',error=NULL,updated_at=excluded.updated_at", params![book,ch,name,h,now])?;
            } else {
                tx.execute("UPDATE memory_job SET status='missing',error='来源章节已移除',updated_at=?3 WHERE book_id=?1 AND ch=?2", params![book,ch,now])?;
            }
        }
    } else if group == "设定" {
        // Conservative invalidation: this may require extra reviews, never rewrites author data.
        tx.execute(
            "UPDATE chapter_memory SET status='stale',updated_at=?2 WHERE book_id=?1",
            params![book, now],
        )?;
        tx.execute("UPDATE memory_job SET status='pending',error='设定修订，需重建记忆',updated_at=?2 WHERE book_id=?1", params![book,now])?;
    }
    tx.execute("INSERT INTO continuity_event(book_id,kind,detail_json,created_at) VALUES(?1,'file_changed',?2,?3)",
        params![book,json!({"group":group,"name":name,"oldHash":old.map(content_hash),"newHash":hash,"requiresReview":true,"factsStaled":facts_staled}).to_string(),now])?;
    tx.commit()?;
    Ok(())
}

fn validated_payload(payload: &Value, source: &str) -> Result<Value> {
    ensure!(payload.is_object(), "记忆必须是结构化对象");
    let summary = payload
        .get("summary")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("记忆缺少summary"))?;
    ensure!(summary.chars().count() <= 4000, "章节摘要过长");
    let mut out = json!({"summary":summary,"facts":[],"threads":[],"events":[]});
    for (key, fields) in [
        ("facts", &["entity", "field", "value"][..]),
        ("threads", &["id", "state"][..]),
        ("events", &["description"][..]),
        ("secrets", &[][..]),
    ] {
        // 秘密账本可选：缺失时不写入空数组，其余键仍必须有数组。
        if key == "secrets" && payload.get(key).is_none() {
            continue;
        }
        let items = payload
            .get(key)
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("记忆缺少数组 {}", key))?;
        ensure!(items.len() <= 200, "记忆条目过多");
        let mut unique = BTreeSet::new();
        let mut accepted = Vec::new();
        for item in items {
            for field in fields {
                ensure!(
                    item[*field]
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty() && s.chars().count() <= 1000),
                    "记忆{}缺少有效{}",
                    key,
                    field
                );
            }
            if key == "secrets" {
                // 秘密账本：谁知道什么；known_by/unknown_to 缺省视为空数组。
                ensure!(
                    item["fact"].as_str().is_some_and(|s| !s.trim().is_empty()),
                    "记忆条目缺少秘密内容"
                );
                for field in ["known_by", "unknown_to"] {
                    if let Some(names) = item.get(field) {
                        ensure!(
                            names
                                .as_array()
                                .is_some_and(|a| a.iter().all(Value::is_string)),
                            "秘密知情者必须是字符串数组"
                        );
                    }
                }
                if let Some(id) = item.get("id") {
                    ensure!(
                        id.as_str()
                            .is_some_and(|s| !s.trim().is_empty() && s.chars().count() <= 32),
                        "秘密ID无效"
                    );
                }
            }
            let evidence = item["evidence"]
                .as_str()
                .ok_or_else(|| anyhow!("记忆条目缺少原文证据"))?;
            ensure!(
                evidence.trim().chars().count() >= source.trim().chars().count().min(6)
                    && source.contains(evidence),
                "记忆证据过短或不在来源正文中"
            );
            if key == "threads" {
                ensure!(
                    matches!(
                        item["state"].as_str(),
                        Some("planted" | "advanced" | "partial" | "resolved" | "cancelled")
                    ),
                    "伏笔状态无效"
                );
            }
            let identity = item.to_string();
            if unique.insert(identity) {
                accepted.push(item.clone());
            }
        }
        out[key] = json!(accepted);
    }
    ensure!(out.to_string().len() <= 131072, "记忆大小超限");
    Ok(out)
}

/// Evidence checks prevent invented citations; semantic faithfulness still needs review.
/// Completion is idempotent per source hash; stale rows must be recomputed even if hash unchanged.
pub fn record_chapter_memory(
    db: &Db,
    book: &str,
    ch: i64,
    name: &str,
    expected_hash: &str,
    payload: &Value,
) -> Result<()> {
    record_memory_inner(db, book, ch, name, expected_hash, None, payload)
}

/// Use this after asynchronous extraction/checking. The input snapshot is tested
/// under the same file lock as the source check and memory commit.
pub fn record_chapter_memory_cas(
    db: &Db,
    book: &str,
    ch: i64,
    name: &str,
    expected_hash: &str,
    expected_inputs: &str,
    payload: &Value,
) -> Result<()> {
    record_memory_inner(
        db,
        book,
        ch,
        name,
        expected_hash,
        Some(expected_inputs),
        payload,
    )
}

fn record_memory_inner(
    db: &Db,
    book: &str,
    ch: i64,
    name: &str,
    expected_hash: &str,
    expected_inputs: Option<&str>,
    payload: &Value,
) -> Result<()> {
    ensure_schema(db)?;
    let _guard = db.fs_lock.lock().map_err(|_| anyhow!("文件锁损坏"))?;
    ensure_book(db, book)?;
    ensure!(
        chapter_number(name) == Some(ch) && files::safe_name(name) == name,
        "章节号与文件名不一致"
    );
    if let Some(expected) = expected_inputs {
        ensure!(
            input_fingerprint(db, book, ch + 1)? == expected,
            "抽取期间设定或前文发生变化，请重新重建记忆"
        );
    }
    ensure!(
        !files::file_flag(db, book, "正文", name, "aiOff"),
        "本章已禁止自动AI引用"
    );
    let source =
        files::read_file(db, book, "正文", name).ok_or_else(|| anyhow!("正式章节不存在"))?;
    ensure!(
        !source.trim().is_empty() && content_hash(&source) == expected_hash,
        "来源章节已变化，请重新提取"
    );
    let out = validated_payload(payload, &source)?;
    let mut conn = db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))?;
    let tx = conn.transaction()?;
    let already: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM chapter_memory WHERE book_id=?1 AND ch=?2 AND source_hash=?3 AND status='valid')",params![book,ch,expected_hash],|r|r.get(0))?;
    if already {
        return Ok(());
    }
    let now = now_ms();
    tx.execute("INSERT INTO chapter_memory(book_id,ch,name,source_hash,payload_json,status,updated_at) VALUES(?1,?2,?3,?4,?5,'valid',?6)
        ON CONFLICT(book_id,ch) DO UPDATE SET name=excluded.name,source_hash=excluded.source_hash,payload_json=excluded.payload_json,status='valid',updated_at=excluded.updated_at",params![book,ch,name,expected_hash,out.to_string(),now])?;
    tx.execute("INSERT INTO memory_job(book_id,ch,name,source_hash,status,updated_at) VALUES(?1,?2,?3,?4,'done',?5)
        ON CONFLICT(book_id,ch) DO UPDATE SET name=excluded.name,source_hash=excluded.source_hash,status='done',error=NULL,updated_at=excluded.updated_at",params![book,ch,name,expected_hash,now])?;
    // 事实账本 + 章节状态：与记忆提交同一事务（提交事务 P0-4）。
    // 事实提取失败必须让整笔事务回滚，不能把章节标为 MEMORY_SYNCED 后再声称可继续连写。
    let extracted =
        crate::facts::extract_from_memory_tx(&tx, book, ch, name, expected_hash, &out, now)?;
    if extracted.disputed > 0 {
        tx.execute(
            "INSERT INTO continuity_event(book_id,kind,detail_json,created_at) VALUES(?1,'fact_conflicts_detected',?2,?3)",
            params![book,json!({"chapter":ch,"disputed":extracted.disputed}).to_string(),now],
        )?;
    }
    let fact_count = extracted.inserted as i64;
    crate::chapter_state::memory_synced_tx(&tx, book, ch, expected_hash, fact_count, now)?;
    tx.execute("INSERT INTO continuity_event(book_id,kind,detail_json,created_at) VALUES(?1,'memory_committed',?2,?3)",params![book,json!({"chapter":ch,"sourceHash":expected_hash,"factCount":fact_count}).to_string(),now])?;
    tx.commit()?;
    Ok(())
}

/// Legacy books are not declared fully indexed. No future chapter is included.
/// All included memories are rechecked against the actual current manuscript hash.
pub fn chapter_context(db: &Db, book: &str, target_ch: i64) -> Result<Value> {
    ensure_schema(db)?;
    ensure_book(db, book)?;
    ensure!(target_ch > 0 && target_ch <= 1_000_000, "目标章号无效");
    let rows = db.q_json(
        "SELECT * FROM chapter_memory WHERE book_id=?1 AND ch<?2 ORDER BY ch",
        &[&book, &target_ch],
    )?;
    let mut valid = BTreeSet::new();
    let mut stale = Vec::new();
    let mut hidden = Vec::new();
    let mut facts: BTreeMap<(String, String), Value> = BTreeMap::new();
    let mut threads: BTreeMap<String, Value> = BTreeMap::new();
    let mut recent = Vec::new();
    let mut stale_from: Option<i64> = None;
    for row in rows {
        let ch = row["ch"].as_i64().unwrap_or(0);
        let name = row["name"].as_str().unwrap_or("");
        if files::file_flag(db, book, "正文", name, "aiOff") {
            hidden.push(ch);
            continue;
        }
        let source = files::read_file(db, book, "正文", name);
        if row["status"] != "valid"
            || source.as_deref().map(content_hash).as_deref() != row["sourceHash"].as_str()
        {
            stale_from = Some(stale_from.map_or(ch, |first| first.min(ch)));
        }
        if stale_from.is_some_and(|first| ch >= first) {
            stale.push(ch);
            continue;
        }
        let payload: Value = serde_json::from_str(row["payloadJson"].as_str().unwrap_or(""))?;
        valid.insert(ch);
        // 上下文只带 sourceChapter（可读时点信息）；sourceHash 对 LLM 无意义且每条白占约80字预算，
        // 哈希仍存于 chapter_memory 表供审计与 stale 判定，不进提示词。
        for f in payload["facts"].as_array().into_iter().flatten() {
            let mut f = f.clone();
            f["sourceChapter"] = json!(ch);
            facts.insert(
                (
                    f["entity"].as_str().unwrap_or("").to_string(),
                    f["field"].as_str().unwrap_or("").to_string(),
                ),
                f,
            );
        }
        for t in payload["threads"].as_array().into_iter().flatten() {
            let mut t = t.clone();
            t["sourceChapter"] = json!(ch);
            threads.insert(t["id"].as_str().unwrap_or("").to_string(), t);
        }
        recent.push(json!({"chapter":ch,"summary":payload["summary"],"events":payload["events"]}));
    }
    let missing: Vec<i64> = (1..target_ch)
        .filter(|ch| !valid.contains(ch))
        .take(1000)
        .collect();
    let missing_count = (target_ch - 1) as usize - valid.len();
    let mut selected = Vec::new();
    let mut chars = 0usize;
    let mut omitted = 0;
    // Prioritize this chapter's cast and unfinished promises, not alphabetical
    // truncation. Source strings are data only; they never change task policy.
    let mut focus = String::new();
    for name in [
        format!("细纲_第{}章.md", target_ch),
        format!("第{}章.md", target_ch),
    ] {
        if !files::file_flag(db, book, "细纲", &name, "aiOff") {
            if let Some(text) = files::read_file(db, book, "细纲", &name) {
                focus.push_str(&text);
            }
        }
    }
    if let Ok(plan) = get_plan(db, book, "arc") {
        if let Some(chapters) = plan["plan"]["chapters"].as_array() {
            for chapter in chapters {
                if chapter["n"].as_i64() == Some(target_ch) {
                    focus.push_str(&chapter.to_string());
                }
            }
        }
    }
    let mut fact_rows: Vec<Value> = facts.into_values().collect();
    fact_rows.sort_by_key(|f| {
        (
            !focus.contains(f["entity"].as_str().unwrap_or("\0")),
            -f["sourceChapter"].as_i64().unwrap_or(0),
        )
    });
    let mut thread_rows: Vec<Value> = threads.into_values().collect();
    thread_rows.sort_by_key(|t| {
        (
            matches!(t["state"].as_str(), Some("resolved" | "cancelled")),
            -t["sourceChapter"].as_i64().unwrap_or(0),
        )
    });
    // Whole entries only. A budget omission sets complete=false, never a false
    // claim that all canon was checked. Recent chapter summaries come first.
    for item in recent
        .into_iter()
        .rev()
        .take(5)
        .chain(fact_rows)
        .chain(thread_rows)
    {
        let s = item.to_string();
        if chars + s.chars().count() > 32000 {
            omitted += 1;
            continue;
        }
        chars += s.chars().count();
        selected.push(s);
    }
    let complete = missing_count == 0 && stale.is_empty() && hidden.is_empty() && omitted == 0;
    Ok(
        json!({"text":format!("【截至第{}章的已定稿记忆；摘要为派生信息，冲突以原文证据为准】\n{}",target_ch-1,selected.join("\n")),
        "coverage":valid,"stale":stale,"hidden":hidden,"missing":missing,"missingCount":missing_count,"omittedEntries":omitted,"complete":complete}),
    )
}

pub fn record_draft_origin(
    db: &Db,
    book: &str,
    ch: i64,
    name: &str,
    hash: &str,
    task_id: &str,
) -> Result<()> {
    ensure_schema(db)?;
    let _guard = db.fs_lock.lock().map_err(|_| anyhow!("文件锁损坏"))?;
    ensure_book(db, book)?;
    ensure!(
        ch > 0 && chapter_number(name) == Some(ch) && !task_id.is_empty(),
        "草稿来源无效"
    );
    let text =
        files::read_file(db, book, "正文待审", name).ok_or_else(|| anyhow!("来源草稿不存在"))?;
    ensure!(content_hash(&text) == hash, "草稿版本已变化，无法记录来源");
    db.exec("INSERT INTO draft_origin(book_id,ch,name,source_hash,task_id,updated_at) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(book_id,ch) DO UPDATE SET name=excluded.name,source_hash=excluded.source_hash,task_id=excluded.task_id,updated_at=excluded.updated_at",&[&book,&ch,&name,&hash,&task_id,&now_ms()])?;
    Ok(())
}

/// Only return an unchanged predecessor generated by this exact task. A manual
/// draft written at the same wall-clock time does not establish task provenance.
pub fn pending_for_task(
    db: &Db,
    book: &str,
    ch: i64,
    task_id: &str,
) -> Result<Option<(String, String)>> {
    ensure_schema(db)?;
    ensure_book(db, book)?;
    let rows = db.q_json(
        "SELECT name,source_hash FROM draft_origin WHERE book_id=?1 AND ch=?2 AND task_id=?3",
        &[&book, &ch, &task_id],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let name = row["name"].as_str().unwrap_or("");
    if files::file_flag(db, book, "正文待审", name, "aiOff") {
        return Ok(None);
    }
    check_draft_dependency(db, book, ch)?;
    let Some(text) = files::read_file(db, book, "正文待审", name) else {
        return Ok(None);
    };
    if content_hash(&text) != row["sourceHash"].as_str().unwrap_or("") {
        return Ok(None);
    }
    Ok(Some((name.to_string(), text)))
}

pub fn record_draft_dependency(
    db: &Db,
    book: &str,
    ch: i64,
    parent_ch: i64,
    parent_hash: &str,
    task_id: &str,
) -> Result<()> {
    ensure_schema(db)?;
    let _guard = db.fs_lock.lock().map_err(|_| anyhow!("文件锁损坏"))?;
    ensure_book(db, book)?;
    ensure!(
        ch > 1 && parent_ch == ch - 1 && !parent_hash.is_empty() && !task_id.is_empty(),
        "待审依赖无效"
    );
    let origins = db.q_json(
        "SELECT task_id FROM draft_origin WHERE book_id=?1 AND ch=?2",
        &[&book, &ch],
    )?;
    if let Some(origin) = origins.first() {
        ensure!(origin["taskId"] == task_id, "不能改写另一任务的草稿依赖");
    }
    db.exec("INSERT INTO draft_dependency(book_id,ch,parent_ch,parent_hash,task_id,status,updated_at) VALUES(?1,?2,?3,?4,?5,'valid',?6) ON CONFLICT(book_id,ch) DO UPDATE SET parent_ch=excluded.parent_ch,parent_hash=excluded.parent_hash,task_id=excluded.task_id,status='valid',updated_at=excluded.updated_at",&[&book,&ch,&parent_ch,&parent_hash,&task_id,&now_ms()])?;
    // The parent may have changed during generation, before a dependency row
    // existed for note_file_change to invalidate. Persist the stale row too.
    if let Err(e) = check_draft_dependency(db, book, parent_ch)
        .and_then(|_| check_draft_dependency(db, book, ch))
    {
        db.exec(
            "UPDATE draft_dependency SET status='stale' WHERE book_id=?1 AND ch=?2",
            &[&book, &ch],
        )?;
        return Err(e);
    }
    Ok(())
}

/// Call on draft acceptance; a stale chain needs an explicit new author review.
pub fn check_draft_dependency(db: &Db, book: &str, ch: i64) -> Result<()> {
    ensure_schema(db)?;
    let rows = db.q_json(
        "SELECT * FROM draft_dependency WHERE book_id=?1 AND ch=?2",
        &[&book, &ch],
    )?;
    if let Some(row) = rows.first() {
        ensure!(
            row["status"] == "valid",
            "上章草稿已变化，本章需重新审核后再接受"
        );
        let parent = row["parentCh"].as_i64().unwrap_or(0);
        let expected = row["parentHash"].as_str().unwrap_or("");
        let mut found = false;
        for group in ["正文", "正文待审"] {
            let path = db.books_dir.join(book).join(group);
            if !path.exists() {
                continue;
            }
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if chapter_number(&name) == Some(parent) {
                    let current = std::fs::read_to_string(entry.path())?;
                    if content_hash(&current) == expected {
                        found = true;
                    }
                }
            }
        }
        ensure!(found, "上章依赖缺失或已被外部修改，需重新审核");
    }
    Ok(())
}

/// Immutable approval receipt; file_revision instead tracks the latest author
/// edit and therefore cannot prove which text was accepted earlier.
pub fn record_approval(db: &Db, book: &str, ch: i64, name: &str, hash: &str) -> Result<()> {
    ensure_schema(db)?;
    let mut conn = db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))?;
    let tx = conn.transaction()?;
    record_approval_tx(&tx, book, ch, name, hash, now_ms())?;
    Ok(tx.commit()?)
}

/// 凭证的事务内版本：审批 saga 要求队列 UPDATE 与凭证 INSERT 同事务（approval.rs）。
pub fn record_approval_tx(
    tx: &rusqlite::Transaction,
    book: &str,
    ch: i64,
    name: &str,
    hash: &str,
    now: i64,
) -> Result<()> {
    tx.execute("INSERT INTO continuity_event(book_id,kind,detail_json,created_at) VALUES(?1,'chapter_approved',?2,?3)",params![book,json!({"chapter":ch,"name":name,"sourceHash":hash}).to_string(),now])?;
    Ok(())
}

pub fn approved_hash(db: &Db, book: &str, name: &str) -> Result<Option<String>> {
    ensure_schema(db)?;
    let rows=db.q_json("SELECT detail_json FROM continuity_event WHERE book_id=?1 AND kind='chapter_approved' ORDER BY id DESC",&[&book])?;
    for row in rows {
        let detail: Value = serde_json::from_str(row["detailJson"].as_str().unwrap_or("null"))?;
        if detail["name"] == name {
            return Ok(detail["sourceHash"].as_str().map(str::to_string));
        }
    }
    Ok(None)
}

pub fn memory_status(db: &Db, book: &str) -> Result<Value> {
    ensure_schema(db)?;
    ensure_book(db, book)?;
    Ok(
        json!({"jobs":db.q_json("SELECT ch,name,source_hash,status,error,updated_at FROM memory_job WHERE book_id=?1 ORDER BY ch",&[&book])?,
        "memories":db.q_json("SELECT ch,name,source_hash,status,updated_at FROM chapter_memory WHERE book_id=?1 ORDER BY ch",&[&book])?,
        "draftDependencies":db.q_json("SELECT * FROM draft_dependency WHERE book_id=?1 ORDER BY ch",&[&book])?}),
    )
}

pub fn mark_memory_error(db: &Db, book: &str, ch: i64, error: &str) -> Result<()> {
    ensure_schema(db)?;
    db.exec(
        "UPDATE memory_job SET status='failed',error=?3,updated_at=?4 WHERE book_id=?1 AND ch=?2",
        &[&book, &ch, &error, &now_ms()],
    )?;
    Ok(())
}

pub fn save_plan(db: &Db, book: &str, kind: &str, payload: &Value) -> Result<Value> {
    save_plan_inner(db, book, kind, None, payload)
}

pub fn save_plan_cas(
    db: &Db,
    book: &str,
    kind: &str,
    expected_revision: i64,
    payload: &Value,
) -> Result<Value> {
    save_plan_inner(db, book, kind, Some(expected_revision), payload)
}

fn save_plan_inner(
    db: &Db,
    book: &str,
    kind: &str,
    expected_revision: Option<i64>,
    payload: &Value,
) -> Result<Value> {
    ensure_schema(db)?;
    ensure_book(db, book)?;
    ensure!(matches!(kind, "arc" | "scene"), "未知计划类型");
    ensure!(
        payload.is_object() || payload.is_array(),
        "计划必须是JSON对象或数组"
    );
    ensure!(payload.to_string().len() <= 262144, "计划内容过大");
    if kind == "arc" {
        if let Some(chapters) = payload.get("chapters") {
            let chapters = chapters
                .as_array()
                .ok_or_else(|| anyhow!("chapters必须是数组"))?;
            let mut seen = BTreeSet::new();
            for chapter in chapters {
                let n = chapter["n"]
                    .as_i64()
                    .or_else(|| chapter["chapter"].as_i64())
                    .ok_or_else(|| anyhow!("计划章节缺少有效章号"))?;
                ensure!(
                    n > 0 && n <= 1_000_000 && seen.insert(n),
                    "计划章号无效或重复"
                );
            }
        }
    }
    let _guard = db.fs_lock.lock().map_err(|_| anyhow!("文件锁损坏"))?;
    ensure_book(db, book)?;
    if let Some(expected) = expected_revision {
        ensure!(
            get_plan(db, book, kind)?["revision"].as_i64() == Some(expected),
            "计划已被其他操作修改，请刷新后重试"
        );
    }
    db.exec("INSERT INTO story_plan(book_id,kind,payload_json,revision,updated_at) VALUES(?1,?2,?3,1,?4)
        ON CONFLICT(book_id,kind) DO UPDATE SET payload_json=excluded.payload_json,revision=story_plan.revision+1,updated_at=excluded.updated_at",&[&book,&kind,&payload.to_string(),&now_ms()])?;
    get_plan(db, book, kind)
}

pub fn get_plan(db: &Db, book: &str, kind: &str) -> Result<Value> {
    ensure_schema(db)?;
    ensure_book(db, book)?;
    let row = db
        .q_json(
            "SELECT payload_json,revision,updated_at FROM story_plan WHERE book_id=?1 AND kind=?2",
            &[&book, &kind],
        )?
        .into_iter()
        .next();
    match row {
        Some(r) => Ok(
            json!({"ok":true,"kind":kind,"plan":serde_json::from_str::<Value>(r["payloadJson"].as_str().unwrap_or("null"))?,"revision":r["revision"],"updatedAt":r["updatedAt"]}),
        ),
        None => Ok(json!({"ok":true,"kind":kind,"plan":null,"revision":0})),
    }
}

/// Snapshot current book inputs without acquiring fs_lock. Call while holding the
/// file lock for a commit barrier; normal pre-generation reads need no write lock.
pub fn input_fingerprint(db: &Db, book: &str, target_ch: i64) -> Result<String> {
    ensure_schema(db)?;
    ensure_book(db, book)?;
    let mut entries = BTreeMap::new();
    // Avoid scan_tree: it may ensure directories and take the file lock. This
    // function must remain callable under the commit barrier without re-locking.
    let root = db.books_dir.join(book);
    if root.exists() {
        for directory in std::fs::read_dir(&root)? {
            let directory = directory?;
            if !directory.file_type()?.is_dir() {
                continue;
            }
            let dir = directory.file_name().to_string_lossy().to_string();
            if dir == "正文待审" {
                continue;
            }
            for file in std::fs::read_dir(directory.path())? {
                let file = file?;
                if !file.file_type()?.is_file() {
                    continue;
                }
                let name = file.file_name().to_string_lossy().to_string();
                if dir == "正文" && chapter_number(&name).is_some_and(|n| n >= target_ch) {
                    continue;
                }
                let bytes = std::fs::read(file.path())?;
                entries.insert(
                    format!("{}/{}", dir, name),
                    format!("{:x}", Sha256::digest(bytes)),
                );
            }
        }
    }
    let settings = db.q_json(
        "SELECT key,value FROM settings WHERE instr(key,?1)>0 ORDER BY key",
        &[&book],
    )?;
    let plans = db.q_json(
        "SELECT kind,payload_json,revision FROM story_plan WHERE book_id=?1 ORDER BY kind",
        &[&book],
    )?;
    let skills = db.q_json(
        "SELECT id,prompt_template,enabled,usage_mode,targets_json FROM skills ORDER BY id",
        &[],
    )?;
    let metadata = db.q_json("SELECT genre,pov FROM books WHERE id=?1", &[&book])?;
    Ok(content_hash(&json!({"files":entries,"settings":settings,"plans":plans,"skills":skills,"metadata":metadata}).to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let book = crate::books::create_book(&db, "continuity-test", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        (dir, db, book)
    }
    fn payload() -> Value {
        json!({"summary":"阿青受伤。","facts":[{"entity":"阿青","field":"伤势","value":"左臂受伤","evidence":"阿青左臂受伤"}],"threads":[],"events":[]})
    }
    #[test]
    fn memory_idempotent_and_rewrite_invalidates() {
        let (_dir, db, book) = fixture();
        let name = "第1章.md";
        let body = "阿青左臂受伤，留下伤痕。";
        files::write_file(&db, &book, "正文", name, body).unwrap();
        record_chapter_memory(&db, &book, 1, name, &content_hash(body), &payload()).unwrap();
        record_chapter_memory(&db, &book, 1, name, &content_hash(body), &payload()).unwrap();
        assert_eq!(
            db.q_json(
                "SELECT COUNT(*) AS n FROM continuity_event WHERE kind='memory_committed'",
                &[]
            )
            .unwrap()[0]["n"],
            1
        );
        assert_eq!(chapter_context(&db, &book, 2).unwrap()["complete"], true);
        assert!(!chapter_context(&db, &book, 1).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("左臂"));
        files::write_file(&db, &book, "正文", name, "阿青未受伤。").unwrap();
        let cx = chapter_context(&db, &book, 2).unwrap();
        assert_eq!(cx["complete"], false);
        assert!(!cx["text"].as_str().unwrap().contains("左臂"));
        assert!(
            record_chapter_memory(&db, &book, 1, name, &content_hash(body), &payload()).is_err()
        );
    }
    #[test]
    fn fabricated_evidence_rejected() {
        let (_dir, db, book) = fixture();
        let body = "阿青左臂受伤。";
        files::write_file(&db, &book, "正文", "第1章.md", body).unwrap();
        let mut p = payload();
        p["facts"][0]["evidence"] = json!("阿青获得十枚丹药");
        assert!(record_chapter_memory(&db, &book, 1, "第1章.md", &content_hash(body), &p).is_err());
    }
    #[test]
    fn old_fact_survives_long_novel_without_future_leak() {
        let (_dir, db, book) = fixture();
        let body = "阿青左臂受伤。";
        files::write_file(&db, &book, "正文", "第40章.md", body).unwrap();
        record_chapter_memory(&db, &book, 40, "第40章.md", &content_hash(body), &payload())
            .unwrap();
        let late = "阿青在未来才痊愈。";
        files::write_file(&db, &book, "正文", "第220章.md", late).unwrap();
        let p = json!({"summary":"未来痊愈","facts":[{"entity":"阿青","field":"伤势","value":"痊愈","evidence":late}],"threads":[],"events":[]});
        record_chapter_memory(&db, &book, 220, "第220章.md", &content_hash(late), &p).unwrap();
        let cx = chapter_context(&db, &book, 200).unwrap();
        let text = cx["text"].as_str().unwrap();
        assert!(text.contains("左臂受伤"));
        assert!(!text.contains("未来才痊愈"));
        assert_eq!(cx["complete"], false); // Missing older chapters are not invented.
    }
    #[test]
    fn plans_are_real_and_book_scoped() {
        let (_dir, db, book) = fixture();
        let p = json!({"chapters":[{"n":1,"summary":"入城"}]});
        assert_eq!(save_plan(&db, &book, "arc", &p).unwrap()["revision"], 1);
        assert_eq!(get_plan(&db, &book, "arc").unwrap()["plan"], p);
        assert_eq!(save_plan(&db, &book, "arc", &p).unwrap()["revision"], 2);
        assert!(save_plan(&db, "missing", "arc", &p).is_err());
    }
    #[test]
    fn concurrent_input_change_prevents_memory_commit() {
        let (_dir, db, book) = fixture();
        let body = "阿青左臂受伤。";
        files::write_file(&db, &book, "正文", "第1章.md", body).unwrap();
        let before = input_fingerprint(&db, &book, 2).unwrap();
        files::write_file(&db, &book, "设定", "世界观.md", "伤势不会自动痊愈").unwrap();
        assert!(record_chapter_memory_cas(
            &db,
            &book,
            1,
            "第1章.md",
            &content_hash(body),
            &before,
            &payload()
        )
        .is_err());
        let now = input_fingerprint(&db, &book, 2).unwrap();
        record_chapter_memory_cas(
            &db,
            &book,
            1,
            "第1章.md",
            &content_hash(body),
            &now,
            &payload(),
        )
        .unwrap();
        let plan = json!({"chapters":[{"n":1}]});
        save_plan(&db, &book, "arc", &plan).unwrap();
        assert!(save_plan_cas(&db, &book, "arc", 0, &plan).is_err());
        assert!(save_plan(&db, &book, "arc", &json!({"chapters":[{"n":1},{"n":1}]})).is_err());
        assert_eq!(get_plan(&db, &book, "arc").unwrap()["revision"], 1);
    }
    #[test]
    fn input_fingerprint_changes_when_plan_or_canon_changes() {
        let (_dir, db, book) = fixture();
        let first = input_fingerprint(&db, &book, 2).unwrap();
        files::write_file(&db, &book, "设定", "世界观.md", "禁术需要代价").unwrap();
        let second = input_fingerprint(&db, &book, 2).unwrap();
        assert_ne!(first, second);
        save_plan(
            &db,
            &book,
            "arc",
            &json!({"chapters":[{"n":2,"summary":"退守"}]}),
        )
        .unwrap();
        assert_ne!(second, input_fingerprint(&db, &book, 2).unwrap());
        let before = input_fingerprint(&db, &book, 2).unwrap();
        files::write_file(&db, &book, "正文", "第20章.md", "未来正文").unwrap();
        assert_eq!(before, input_fingerprint(&db, &book, 2).unwrap());
    }
    #[test]
    fn pending_origin_requires_exact_task_and_unchanged_text() {
        let (_dir, db, book) = fixture();
        let body = "上一章草稿";
        files::write_file(&db, &book, "正文待审", "第1章.md", body).unwrap();
        record_draft_origin(&db, &book, 1, "第1章.md", &content_hash(body), "task-a").unwrap();
        assert!(pending_for_task(&db, &book, 1, "task-a").unwrap().is_some());
        assert!(pending_for_task(&db, &book, 1, "task-b").unwrap().is_none());
        files::write_file(&db, &book, "正文待审", "第1章.md", "作者新改稿").unwrap();
        assert!(pending_for_task(&db, &book, 1, "task-a").unwrap().is_none());
    }
    #[test]
    fn pending_chain_invalidates_without_deleting_drafts() {
        let (_dir, db, book) = fixture();
        let parent = "上一章原稿";
        files::write_file(&db, &book, "正文待审", "第1章.md", parent).unwrap();
        files::write_file(&db, &book, "正文待审", "第2章.md", "后续草稿").unwrap();
        record_draft_dependency(&db, &book, 2, 1, &content_hash(parent), "task-a").unwrap();
        assert!(check_draft_dependency(&db, &book, 2).is_ok());
        files::write_file(&db, &book, "正文待审", "第1章.md", "修改了人物结局").unwrap();
        assert!(check_draft_dependency(&db, &book, 2).is_err());
        assert_eq!(
            files::read_file(&db, &book, "正文待审", "第2章.md").unwrap(),
            "后续草稿"
        );
    }
    #[test]
    fn parent_changed_before_dependency_registration_is_stale() {
        let (_dir, db, book) = fixture();
        let old = "上一章生成时内容";
        files::write_file(&db, &book, "正文待审", "第1章.md", "生成过程中作者修改前稿").unwrap();
        files::write_file(&db, &book, "正文待审", "第2章.md", "依赖旧前稿的续章").unwrap();
        assert!(record_draft_dependency(&db, &book, 2, 1, &content_hash(old), "task-a").is_err());
        assert!(check_draft_dependency(&db, &book, 2).is_err());
        assert_eq!(
            memory_status(&db, &book).unwrap()["draftDependencies"][0]["status"],
            "stale"
        );
    }
    #[test]
    fn removing_draft_clears_own_provenance_but_stales_descendants() {
        let (_dir, db, book) = fixture();
        for ch in 1..=3 {
            files::write_file(
                &db,
                &book,
                "正文待审",
                &format!("第{}章.md", ch),
                "原始草稿",
            )
            .unwrap();
        }
        record_draft_origin(
            &db,
            &book,
            2,
            "第2章.md",
            &content_hash("原始草稿"),
            "task-a",
        )
        .unwrap();
        record_draft_dependency(&db, &book, 2, 1, &content_hash("原始草稿"), "task-a").unwrap();
        record_draft_dependency(&db, &book, 3, 2, &content_hash("原始草稿"), "task-a").unwrap();
        files::delete_file(&db, &book, "正文待审", "第2章.md").unwrap();
        assert!(pending_for_task(&db, &book, 2, "task-a").unwrap().is_none());
        assert!(check_draft_dependency(&db, &book, 2).is_ok());
        assert!(check_draft_dependency(&db, &book, 3).is_err());
    }
    #[test]
    fn chapter_numbers_are_not_guessed_from_prose() {
        assert_eq!(chapter_number("第250章 雪夜.md"), Some(250));
        assert_eq!(chapter_number("第0章.md"), None);
        // 文件名锚定：内嵌「第N章」的非章文件不当章号（设定组文件不触发记忆级联）
        assert_eq!(chapter_number("设定第2章说明.md"), None);
        // F3：中文数字章名同等可达
        assert_eq!(chapter_number("第十一章 雪夜.md"), Some(11));
        assert_eq!(chapter_number("第一百二十三章.md"), Some(123));
        assert_eq!(chapter_number("第二十章.md"), Some(20));
        assert_eq!(chapter_number("第十章.md"), Some(10));
    }
    #[test]
    fn hidden_memory_is_not_automatically_exposed() {
        let (_dir, db, book) = fixture();
        let body = "阿青左臂受伤。";
        files::write_file(&db, &book, "正文", "第1章.md", body).unwrap();
        record_chapter_memory(&db, &book, 1, "第1章.md", &content_hash(body), &payload()).unwrap();
        db.exec(
            "INSERT INTO settings(key,value) VALUES(?1,?2)",
            &[
                &format!("file_flags__{}", book),
                &json!({"正文/第1章.md":{"aiOff":true}}).to_string(),
            ],
        )
        .unwrap();
        let cx = chapter_context(&db, &book, 2).unwrap();
        assert_eq!(cx["complete"], false);
        assert_eq!(cx["hidden"], json!([1]));
        assert!(!cx["text"].as_str().unwrap().contains("左臂"));
    }
    #[test]
    fn payload_with_secrets_validated_and_bad_evidence_rejected() {
        let (_dir, db, book) = fixture();
        let body = "阿青左臂受伤，留下伤痕。染血玉简在怀中发烫。";
        files::write_file(&db, &book, "正文", "第1章.md", body).unwrap();
        // 现有证据校验要求证据长度 >= min(来源长度, 6)，故此处取完整短语而非“染血玉简”。
        let payload = json!({"summary":"测试。","facts":[],"threads":[],"events":[],"secrets":[{"id":"SEC_01","fact":"玉简内藏仙门信标","known_by":["算盘鬼"],"unknown_to":["阿青"],"evidence":"染血玉简在怀中发烫"}]});
        record_chapter_memory(&db, &book, 1, "第1章.md", &content_hash(body), &payload).unwrap();
        let rows = db
            .q_json(
                "SELECT payload_json FROM chapter_memory WHERE book_id=?1 AND ch=1",
                &[&book],
            )
            .unwrap();
        let stored: Value = serde_json::from_str(rows[0]["payloadJson"].as_str().unwrap()).unwrap();
        assert_eq!(stored["secrets"][0]["id"], "SEC_01");
        let changed = "阿青左臂受伤，留下伤痕。染血玉简在怀中发烫。玉简忽然沉默。";
        files::write_file(&db, &book, "正文", "第1章.md", changed).unwrap();
        let mut bad = payload.clone();
        bad["secrets"][0]["evidence"] = json!("不存在的证据文字");
        assert!(
            record_chapter_memory(&db, &book, 1, "第1章.md", &content_hash(changed), &bad).is_err()
        );
    }
}
