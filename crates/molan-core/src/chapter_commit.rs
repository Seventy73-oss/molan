//! 章节提交服务：正文待审的提交 / 登记 / 定稿 / 驳回 / 孤儿修复的唯一实现。
//!
//! 收敛前的问题（已核实）：
//! - 写「正文待审」与登记 `pending_chapter` 分属两把锁，写成功而登记失败会留下
//!   「挡住重写却无法批准」的孤儿稿；自动写作/场景还用裸 `INSERT OR REPLACE` 重置 saga 列；
//! - 「已批准重入」判定在 IPC approve_chapter、Agent finalize、approval.rs 三处各写一份且结论不同。
//!
//! 本模块：提交在**同一把 fs_lock** 内完成写盘 + 登记；登记失败时文件保留并由
//! `repair_orphans` 补登（绝不删除已生成稿件）；定稿统一走 `approve`，可选绑定审阅时的 hash。

use crate::continuity::{self, approved_hash, content_hash};
use crate::db::{Db, REVIEW_GROUP};
use crate::doc_write::Actor;
use crate::files;
use crate::stats::now_ms;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

/// 中文/阿拉伯章节数字（对齐 Node zhNum）。
fn zh_num(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        return s.parse::<i64>().ok();
    }
    let digit = |c: char| -> Option<i64> {
        Some(match c {
            '零' => 0,
            '一' => 1,
            '二' | '两' => 2,
            '三' => 3,
            '四' => 4,
            '五' => 5,
            '六' => 6,
            '七' => 7,
            '八' => 8,
            '九' => 9,
            '十' => 10,
            '百' => 100,
            '千' => 1000,
            _ => return None,
        })
    };
    let (mut r, mut cur) = (0i64, 0i64);
    for ch in s.chars() {
        let v = digit(ch)?;
        if v >= 10 {
            r += (if cur == 0 { 1 } else { cur }) * v;
            cur = 0;
        } else {
            cur = v;
        }
    }
    Some(r + cur)
}

/// 「第X章」→ X（宽松：允许前缀，如「卷一第3章.md」）。语义与旧 handlers 版逐字一致。
pub fn chapter_num_from_name(name: &str) -> Option<i64> {
    let after = name.split_once('第')?.1;
    let mut num_txt = String::new();
    for ch in after.chars() {
        if ch == '章' {
            break;
        }
        if ch.is_whitespace() {
            if num_txt.is_empty() {
                continue;
            }
            break;
        }
        num_txt.push(ch);
    }
    if num_txt.is_empty() {
        return None;
    }
    zh_num(&num_txt)
}

/// 待审登记（锁内；调用方持有 fs_lock）。保持 approved 仅当「队列已批准 且 内容 == 批准回执」，
/// 否则一律回到 pending。非章节稿件使用负编号，不与章号冲突。
pub(crate) fn register_pending_locked(
    db: &Db,
    book_id: &str,
    name: &str,
    content: &str,
) -> Result<Value> {
    let now = now_ms();
    let ch = match chapter_num_from_name(name) {
        Some(ch) if ch > 0 => ch,
        _ => {
            let existing = db.q_json(
                "SELECT ch FROM pending_chapter WHERE book_id=?1 AND review_file=?2",
                &[&book_id as &dyn rusqlite::ToSql, &name],
            )?;
            match existing.first().and_then(|r| r["ch"].as_i64()) {
                Some(ch) => ch,
                None => db
                    .q_json(
                        "SELECT MIN(ch) AS low FROM pending_chapter WHERE book_id=?1",
                        &[&book_id as &dyn rusqlite::ToSql],
                    )?
                    .first()
                    .and_then(|r| r["low"].as_i64())
                    .unwrap_or(0)
                    .min(0)
                    .checked_sub(1)
                    .ok_or_else(|| anyhow!("待审编号已耗尽"))?,
            }
        }
    };
    let prev_approved = db
        .q_json(
            "SELECT status FROM pending_chapter WHERE book_id=?1 AND ch=?2",
            &[&book_id as &dyn rusqlite::ToSql, &ch],
        )
        .unwrap_or_default()
        .first()
        .and_then(|r| r["status"].as_str())
        == Some("approved");
    let same_as_approved = approved_hash(db, book_id, name)
        .unwrap_or(None)
        .is_some_and(|rec| content_hash(content) == rec);
    let status = if prev_approved && same_as_approved {
        "approved"
    } else {
        "pending"
    };
    db.exec(
        "INSERT INTO pending_chapter(book_id,ch,review_file,status,created_at,updated_at)
         VALUES(?1,?2,?3,?4,?5,?5)
         ON CONFLICT(book_id,ch) DO UPDATE SET
             review_file=excluded.review_file, status=excluded.status, updated_at=excluded.updated_at",
        &[&book_id as &dyn rusqlite::ToSql, &ch, &name, &status, &now],
    )?;
    Ok(json!({ "ch": ch, "status": status }))
}

/// 待审登记（自取 fs_lock）。
pub fn register_pending(db: &Db, book_id: &str, name: &str, content: &str) -> Result<Value> {
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    register_pending_locked(db, book_id, name, content)
}

/// 提交章节草稿到「正文待审」。`origin` 记录来源任务（如 manual-draft / auto:<id> / artifact:<id>）。
/// `check` 在持锁后、写盘前执行（取消 / 指纹 / 细纲确认等），返回 Err 则不写任何内容。
/// 返回 `{ok, ch, name, group, hash, status, index, indexError?, chars}`。
pub fn submit_pending<F>(
    db: &Db,
    book_id: &str,
    ch: i64,
    content: &str,
    origin: &str,
    check: F,
) -> Result<Value>
where
    F: FnOnce() -> Result<()>,
{
    if !(1..=1_000_000).contains(&ch) {
        bail!("章号超出范围：{}", ch);
    }
    if content.trim().is_empty() {
        bail!("正文为空，拒绝提交待审");
    }
    if !files::valid_book_id(db, book_id) {
        bail!("书籍不存在或已删除");
    }
    let name = format!("第{}章.md", ch);
    let hash = content_hash(content);
    let (index_err, queued) = {
        let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
        check()?;
        let index_err = files::write_ai_new_locked(db, book_id, REVIEW_GROUP, &name, content)?;
        // 写盘已成功：登记失败时保留稿件，返回可恢复错误（repair_orphans 会补登）
        let queued = register_pending_locked(db, book_id, &name, content).map_err(|e| {
            anyhow!(
                "待审稿已保存为 正文待审/{}，但待审登记失败（可重试修复，不会丢稿）：{}",
                name,
                e
            )
        })?;
        (index_err, queued)
    };
    let _ = crate::chapter_state::record_save(db, book_id, ch, REVIEW_GROUP, &hash);
    if !origin.is_empty() {
        let _ = continuity::record_draft_origin(db, book_id, ch, &name, &hash, origin);
    }
    let src = json!({"service": "chapter_commit.submit_pending", "origin": origin});
    let n = content.chars().count() as i64;
    let write_id = ext(
        db,
        book_id,
        REVIEW_GROUP,
        &name,
        "create",
        Actor::Ai,
        None,
        &hash,
        n,
        index_err.clone(),
        src,
    );
    Ok(json!({
        "ok": true, "ch": ch, "name": name, "group": REVIEW_GROUP, "hash": hash, "writeId": write_id,
        "status": queued["status"], "chars": content.chars().count(),
        "index": if index_err.is_some() { "failed" } else { "ok" }, "indexError": index_err,
    }))
}

/// 旧入口（聊天保存 / 手动写待审 / 中断残稿 / 场景拼稿）共用：同一把 fs_lock 内写「正文待审」并登记。
/// `ai=true` 走 AI 新建规则（锁定、已存在、正式稿同名均拒绝）；否则为作者写入（允许覆盖，写前留快照）。
/// 返回登记结果 `{ch, status}`，派生索引失败时附 `indexError`（文件已保存）。
pub fn write_review_and_register(
    db: &Db,
    book_id: &str,
    name: &str,
    content: &str,
    ai: bool,
) -> Result<Value> {
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    let before = files::read_checked(db, book_id, REVIEW_GROUP, name)
        .ok()
        .flatten()
        .map(|c| content_hash(&c));
    let index_err = if ai {
        files::write_ai_new_locked(db, book_id, REVIEW_GROUP, name, content)?
    } else {
        files::write_file_locked_outcome(db, book_id, REVIEW_GROUP, name, content)?
    };
    let mut queued = register_pending_locked(db, book_id, name, content).map_err(|e| {
        anyhow!(
            "待审稿已保存为 正文待审/{}，但待审登记失败（可重试修复，不会丢稿）：{}",
            name,
            e
        )
    })?;
    let after = content_hash(content);
    if before.as_deref() != Some(after.as_str()) {
        let op = if before.is_some() {
            "replace"
        } else {
            "create"
        };
        let actor = if ai { Actor::Ai } else { Actor::User };
        let src = json!({"service": "chapter_commit.review_write"});
        let n = content.chars().count() as i64;
        queued["writeId"] = json!(ext(
            db,
            book_id,
            REVIEW_GROUP,
            name,
            op,
            actor,
            before,
            &after,
            n,
            index_err.clone(),
            src
        ));
    }
    if let Some(e) = index_err {
        queued["indexError"] = json!(e);
    }
    Ok(queued)
}

/// 已登记待审稿的改写（去味后整篇替换 / 场景追加）：同一把锁内 CAS 写盘（基线不符或文件锁定即拒绝）
/// + 重新登记（内容变了即回到 pending）+ 统一写入回执。返回登记结果并附 writeId。
pub fn rewrite_pending(
    db: &Db,
    book_id: &str,
    name: &str,
    base: &str,
    new: &str,
    op: &str,
    service: &str,
) -> Result<Value> {
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    files::write_file_cas_locked(db, book_id, REVIEW_GROUP, name, base, new)?;
    let mut queued = register_pending_locked(db, book_id, name, new)?;
    let n = new.chars().count() as i64;
    let before = Some(content_hash(base));
    let src = json!({"service": service});
    queued["writeId"] = json!(ext(
        db,
        book_id,
        REVIEW_GROUP,
        name,
        op,
        Actor::Ai,
        before,
        &content_hash(new),
        n,
        None,
        src
    ));
    Ok(queued)
}

/// 统一写入回执（DocumentWriteService 账本，见 doc_write::record_external）；记账失败只记日志。
#[allow(clippy::too_many_arguments)]
fn ext(
    db: &Db,
    book: &str,
    group: &str,
    name: &str,
    op: &str,
    actor: Actor,
    before: Option<String>,
    after: &str,
    chars: i64,
    idx: Option<String>,
    src: Value,
) -> Option<String> {
    crate::doc_write::record_external(
        db, book, group, name, op, actor, before, after, chars, idx, src,
    )
    .map_err(|e| eprintln!("[molan-core] 写入回执记账失败（文件已保存）：{}", e))
    .ok()
}

/// 统一定稿：`name` 为空时按 `ch` 反查 pending 记录。`expected_hash` 非空时绑定审阅版本。
/// 已批准重入：正式稿 == 批准回执 → 幂等成功；不一致 → 报错；旧库无回执 → 成功但标注 unverified。
pub fn approve(
    db: &Db,
    book_id: &str,
    name: &str,
    ch: i64,
    expected_hash: Option<&str>,
    by: &str,
) -> Result<Value> {
    if !files::valid_book_id(db, book_id) {
        bail!("书籍不存在或已删除");
    }
    let name = if !name.trim().is_empty() {
        name.trim().to_string()
    } else {
        db.q_json(
            "SELECT review_file FROM pending_chapter WHERE book_id=?1 AND ch=?2 AND status IN ('pending','approved') ORDER BY status='pending' DESC LIMIT 1",
            &[&book_id as &dyn rusqlite::ToSql, &ch],
        )?
        .first()
        .and_then(|r| r["reviewFile"].as_str().map(str::to_string))
        .ok_or_else(|| anyhow!("第{}章没有待审稿，无法定稿", ch))?
    };
    let status = db
        .q_json(
            "SELECT status FROM pending_chapter WHERE book_id=?1 AND review_file=?2",
            &[&book_id as &dyn rusqlite::ToSql, &name],
        )?
        .first()
        .and_then(|r| r["status"].as_str().map(str::to_string));
    let final_name = files::safe_name(&name);
    if status.as_deref() == Some("approved") {
        let formal = files::read_file(db, book_id, "正文", &final_name);
        let receipt = approved_hash(db, book_id, &name).unwrap_or(None);
        return match (formal.as_deref(), receipt.as_deref()) {
            (Some(f), Some(r)) if content_hash(f) == r => Ok(json!({
                "ok": true, "finalName": final_name, "alreadyApproved": true,
                "verified": "approved-receipt-hash-match", "hash": r,
                "ch": chapter_num_from_name(&final_name),
            })),
            (Some(_), Some(_)) => Err(anyhow!(
                "该章已批准，但当前正式稿与批准回执的指纹不一致（批准后被改动过）。请人工确认后再处理，系统不会静默重复接受。"
            )),
            (Some(f), None) => Ok(json!({
                "ok": true, "finalName": final_name, "alreadyApproved": true, "verified": "unverified",
                "hash": content_hash(f), "ch": chapter_num_from_name(&final_name),
                "warning": "该章已标记批准，但缺少批准回执（历史数据），无法核对批准后是否被改动，请人工确认。",
            })),
            (None, _) => Err(anyhow!("队列显示该章已批准，但正式稿文件不存在：{}。请检查磁盘或重新生成。", name)),
        };
    }
    let pending = files::read_file(db, book_id, REVIEW_GROUP, &name)
        .ok_or_else(|| anyhow!("待审文件不存在：{}", name))?;
    let cur = content_hash(&pending);
    if let Some(exp) = expected_hash.map(str::trim).filter(|e| !e.is_empty()) {
        if exp != cur {
            bail!(
                "待审稿在你审阅后被修改（期望 {}…，当前 {}…）：本次定稿失效，请重新审阅",
                exp.chars().take(8).collect::<String>(),
                &cur[..8]
            );
        }
    }
    let before =
        files::read_file(db, book_id, "正文", &files::safe_name(&name)).map(|c| content_hash(&c));
    let final_name = files::approve_pending_chapter(db, book_id, &name)?;
    crate::stats::refresh_words(db, book_id);
    let op = if before.is_some() {
        "replace"
    } else {
        "create"
    };
    let src = json!({"service": "chapter_commit.approve", "by": by, "from": name});
    let n = pending.chars().count() as i64;
    let write_id = ext(
        db,
        book_id,
        "正文",
        &final_name,
        op,
        Actor::User,
        before,
        &cur,
        n,
        None,
        src,
    );
    let ch = chapter_num_from_name(&final_name).unwrap_or(0);
    if ch > 0 {
        let _ = crate::chapter_state::record_approved(
            db,
            book_id,
            ch,
            json!({"finalName": final_name, "by": by}),
        );
    }
    Ok(
        json!({"ok": true, "finalName": final_name, "ch": ch, "hash": cur, "alreadyApproved": false, "writeId": write_id}),
    )
}

/// 全自动定稿（直接写正文、不经待审稿）的队列状态与不可变批准回执：同一事务提交。
pub fn record_auto_approved(db: &Db, book_id: &str, ch: i64, name: &str, hash: &str) -> Result<()> {
    let mut conn = db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))?;
    let tx = conn.transaction()?;
    let now = now_ms();
    tx.execute(
        "INSERT INTO pending_chapter(book_id,ch,review_file,status,created_at,updated_at,formal_hash,approve_phase)
         VALUES(?1,?2,?3,'approved',?4,?4,?5,'done')
         ON CONFLICT(book_id,ch) DO UPDATE SET review_file=excluded.review_file, status='approved',
             formal_hash=excluded.formal_hash, approve_phase='done', updated_at=excluded.updated_at",
        rusqlite::params![book_id, ch, name, now, hash],
    )?;
    continuity::record_approval_tx(&tx, book_id, ch, name, hash, now)?;
    tx.commit()?;
    drop(conn);
    let n = files::read_file(db, book_id, "正文", name).map_or(0, |c| c.chars().count() as i64);
    let src = json!({"service": "auto_write.full_auto"});
    ext(
        db,
        book_id,
        "正文",
        name,
        "create",
        Actor::Ai,
        None,
        hash,
        n,
        None,
        src,
    );
    Ok(())
}

/// 驳回：待审稿进回收站（可恢复）+ 队列标记 rejected。返回 trashId。
pub fn reject(db: &Db, book_id: &str, ch: i64, name: &str) -> Result<Value> {
    let trash_id = files::delete_file(db, book_id, REVIEW_GROUP, name)?;
    let n = db.exec(
        "UPDATE pending_chapter SET status='rejected', updated_at=?2 WHERE book_id=?1 AND (ch=?3 OR review_file=?4)",
        &[&book_id as &dyn rusqlite::ToSql, &now_ms(), &ch, &name],
    )?;
    Ok(json!({"ok": true, "trashId": trash_id, "queueUpdated": n > 0}))
}

/// 孤儿修复：「正文待审」里有文件、却没有任何队列记录的稿件补登为 pending。返回修复数。
/// 只补缺失的行，不改动已有 rejected/approved 记录，不删除任何文件。
pub fn repair_orphans(db: &Db) -> Result<usize> {
    let books = db.q_json("SELECT id FROM books WHERE deleted_at IS NULL", &[])?;
    let mut fixed = 0;
    for b in books {
        let book = b["id"].as_str().unwrap_or("");
        let Ok(dir) = files::book_dir(db, book) else {
            continue;
        };
        let Ok(rd) = std::fs::read_dir(dir.join(REVIEW_GROUP)) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || !e.path().is_file() {
                continue;
            }
            let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
            let has_row = !db
                .q_json(
                    "SELECT 1 FROM pending_chapter WHERE book_id=?1 AND (review_file=?2 OR ch=?3)",
                    &[
                        &book as &dyn rusqlite::ToSql,
                        &name,
                        &chapter_num_from_name(&name).unwrap_or(i64::MIN),
                    ],
                )?
                .is_empty();
            if has_row {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(e.path()) {
                register_pending_locked(db, book, &name, &text)?;
                fixed += 1;
            }
        }
    }
    Ok(fixed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let id = crate::books::create_book(&db, "书", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        (dir, db, id)
    }

    #[test]
    fn chapter_numbers_match_legacy_parser() {
        assert_eq!(chapter_num_from_name("第3章.md"), Some(3));
        assert_eq!(chapter_num_from_name("第二十章.md"), Some(20));
        assert_eq!(chapter_num_from_name("卷一第 12 章"), Some(12));
        assert_eq!(chapter_num_from_name("第一百零五章 标题"), Some(105));
        assert_eq!(chapter_num_from_name("设定.md"), None);
        assert_eq!(chapter_num_from_name("第x章"), None);
    }

    #[test]
    fn submit_writes_and_registers_in_one_step() {
        let (_d, db, b) = setup();
        let r = submit_pending(&db, &b, 2, "第二章正文\n\n内容", "test", || Ok(())).unwrap();
        assert_eq!(r["status"], "pending");
        assert_eq!(r["name"], "第2章.md");
        let row = db
            .q_json(
                "SELECT status, review_file FROM pending_chapter WHERE ch=2",
                &[],
            )
            .unwrap();
        assert_eq!(row[0]["reviewFile"], "第2章.md");
        assert_eq!(
            files::read_file(&db, &b, REVIEW_GROUP, "第2章.md").as_deref(),
            Some("第二章正文\n\n内容")
        );
        // 已有同章待审 → 拒绝覆盖
        assert!(submit_pending(&db, &b, 2, "另一版", "test", || Ok(())).is_err());
        // check 失败 → 不写
        assert!(submit_pending(&db, &b, 3, "x", "t", || Err(anyhow!("取消"))).is_err());
        assert!(files::read_file(&db, &b, REVIEW_GROUP, "第3章.md").is_none());
    }

    #[test]
    fn approve_binds_expected_hash_and_is_idempotent() {
        let (_d, db, b) = setup();
        submit_pending(&db, &b, 1, "第一章", "", || Ok(())).unwrap();
        let err = approve(&db, &b, "", 1, Some("deadbeef"), "test").unwrap_err();
        assert!(err.to_string().contains("审阅后被修改"));
        let h = content_hash("第一章");
        let r = approve(&db, &b, "", 1, Some(&h), "test").unwrap();
        assert_eq!(r["finalName"], "第1章.md");
        assert_eq!(
            files::read_file(&db, &b, "正文", "第1章.md").as_deref(),
            Some("第一章")
        );
        let again = approve(&db, &b, "第1章.md", 0, None, "test").unwrap();
        assert_eq!(again["alreadyApproved"], true);
        assert_eq!(again["verified"], "approved-receipt-hash-match");
    }

    /// 所有章节写入（AI 提交待审、作者写待审、定稿、全自动定稿）都在统一账本留下 WriteReceipt。
    #[test]
    fn every_chapter_write_leaves_a_unified_receipt() {
        let (_d, db, b) = setup();
        let hist = |g: &str, n: &str| {
            crate::doc_write::history(&db, &b, g, n, 20)
                .unwrap()
                .as_array()
                .unwrap()
                .clone()
        };
        let r = submit_pending(&db, &b, 1, "第一章", "t", || Ok(())).unwrap();
        let h = hist(REVIEW_GROUP, "第1章.md");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0]["writeId"], r["writeId"]);
        assert_eq!(
            (
                h[0]["op"].as_str(),
                h[0]["actor"].as_str(),
                h[0]["source"]["service"].as_str()
            ),
            (
                Some("create"),
                Some("ai"),
                Some("chapter_commit.submit_pending")
            )
        );
        let a = approve(&db, &b, "", 1, None, "test").unwrap();
        let h = hist("正文", "第1章.md");
        assert_eq!(h[0]["writeId"], a["writeId"]);
        assert_eq!(
            (h[0]["actor"].as_str(), h[0]["op"].as_str()),
            (Some("user"), Some("create"))
        );
        assert_eq!(h[0]["afterHash"], json!(content_hash("第一章")));
        write_review_and_register(&db, &b, "第2章.md", "初稿", false).unwrap();
        write_review_and_register(&db, &b, "第2章.md", "改稿", false).unwrap();
        write_review_and_register(&db, &b, "第2章.md", "改稿", false).unwrap();
        let h = hist(REVIEW_GROUP, "第2章.md");
        assert_eq!(h.len(), 2, "同内容重写不记账");
        let replace = h.iter().find(|x| x["op"] == "replace").unwrap();
        assert_eq!(replace["beforeHash"], json!(content_hash("初稿")));
        assert_eq!(replace["actor"], "user");
        // 已登记待审稿的追加：基线不符拒绝且不改盘；成功则同锁重新登记并留下 append 回执
        assert!(rewrite_pending(&db, &b, "第2章.md", "旧基线", "x", "append", "t").is_err());
        assert_eq!(
            files::read_file(&db, &b, REVIEW_GROUP, "第2章.md").as_deref(),
            Some("改稿")
        );
        let q = rewrite_pending(
            &db,
            &b,
            "第2章.md",
            "改稿",
            "改稿\n场景二",
            "append",
            "scene",
        )
        .unwrap();
        assert_eq!(q["status"], "pending");
        let h = hist(REVIEW_GROUP, "第2章.md");
        let app = h.iter().find(|x| x["op"] == "append").unwrap();
        assert_eq!(app["writeId"], q["writeId"]);
        assert_eq!(app["beforeHash"], json!(content_hash("改稿")));
        files::write_file(&db, &b, "正文", "第3章.md", "全自动正文").unwrap();
        record_auto_approved(&db, &b, 3, "第3章.md", &content_hash("全自动正文")).unwrap();
        let h = hist("正文", "第3章.md");
        assert_eq!(h[0]["source"]["service"], "auto_write.full_auto");
        assert_eq!(h[0]["chars"], 5);
    }

    #[test]
    fn legacy_review_writes_register_under_one_lock() {
        let (_d, db, b) = setup();
        // 作者写入：可覆盖，重复写同章仍是一条 pending
        let q = write_review_and_register(&db, &b, "第4章.md", "初稿", false).unwrap();
        assert_eq!(
            (q["ch"].as_i64(), q["status"].as_str()),
            (Some(4), Some("pending"))
        );
        write_review_and_register(&db, &b, "第4章.md", "改稿", false).unwrap();
        let rows = db
            .q_json("SELECT review_file FROM pending_chapter WHERE ch=4", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        // AI 写入：已存在拒绝，且不改动队列与原稿
        assert!(write_review_and_register(&db, &b, "第4章.md", "AI 稿", true).is_err());
        assert_eq!(
            files::read_file(&db, &b, REVIEW_GROUP, "第4章.md").as_deref(),
            Some("改稿")
        );
        // 无章号的场景拼稿也会登记（负编号），不会成为孤儿
        let q = write_review_and_register(&db, &b, "场景_ab.md", "场景", true).unwrap();
        assert!(q["ch"].as_i64().unwrap() < 0);
        assert_eq!(repair_orphans(&db).unwrap(), 0);
    }

    #[test]
    fn reject_moves_to_trash_and_marks_queue() {
        let (_d, db, b) = setup();
        submit_pending(&db, &b, 1, "稿", "", || Ok(())).unwrap();
        let r = reject(&db, &b, 1, "第1章.md").unwrap();
        assert!(!r["trashId"].as_str().unwrap().is_empty());
        let row = db
            .q_json("SELECT status FROM pending_chapter WHERE ch=1", &[])
            .unwrap();
        assert_eq!(row[0]["status"], "rejected");
    }

    #[test]
    fn orphan_pending_file_is_registered_not_deleted() {
        let (_d, db, b) = setup();
        files::write_ai_file(&db, &b, REVIEW_GROUP, "第4章.md", "孤儿稿").unwrap();
        assert!(db
            .q_json("SELECT 1 FROM pending_chapter", &[])
            .unwrap()
            .is_empty());
        assert_eq!(repair_orphans(&db).unwrap(), 1);
        assert_eq!(repair_orphans(&db).unwrap(), 0, "幂等");
        let row = db
            .q_json("SELECT status FROM pending_chapter WHERE ch=4", &[])
            .unwrap();
        assert_eq!(row[0]["status"], "pending");
        assert!(files::read_file(&db, &b, REVIEW_GROUP, "第4章.md").is_some());
    }
}
