//! 审批 saga：待审稿 → 正式稿的原子提交与崩溃恢复（交接文档附录 A3.3）。
//!
//! 现状问题（G1/C1，已核实）：旧实现四步独立提交——写正文、队列 UPDATE、批准凭证
//! INSERT、删待审——凭证失败会留下「队列 approved + 无回执 + 待审残留」的孤儿态，
//! 且重试命中假成功；正文已写而队列未更（窗口A）时重试恒报「正式稿已存在」卡死。
//!
//! 本模块契约：
//! - 队列 approved + 不可变批准凭证在**同一事务**提交，任一步失败整体回滚保持 pending；
//! - 恢复判据唯一：磁盘正文 content_hash == 标记/凭证 hash（文件存在性只是辅助）；
//! - 相位标记 approve_phase：'' → started → formal_written → receipted → done，
//!   标记写入是 best-effort（恢复以 hash 对比兜底，标记失败不阻断审批）；
//! - 待审源稿清理走 files::delete_file_locked：进回收站 + note_file_change 记账
//!   （draft_origin/draft_dependency 清理、promoted_hash 语义），失败不回翻已批准结果，
//!   留给 recover_approvals 重试；
//! - memory_job 预置不在事务内重复做：write_file_locked → note_file_change(正文)
//!   已按既有语义登记 pending（continuity.rs），恢复路径跳过写盘时该记账已发生过。
use crate::continuity::{approved_hash, chapter_number, check_draft_dependency, content_hash};
use crate::db::{Db, REVIEW_GROUP};
use crate::files;
use crate::stats::now_ms;
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::params;

/// best-effort 相位标记：失败只影响恢复提示，不影响审批正确性（hash 判据兜底）。
fn set_phase(
    db: &Db,
    book_id: &str,
    review_file: &str,
    phase: &str,
    draft_hash: Option<&str>,
    formal_hash: Option<&str>,
) -> Result<()> {
    let now = now_ms();
    match (draft_hash, formal_hash) {
        (Some(d), None) => db.exec(
            "UPDATE pending_chapter SET approve_phase=?3, draft_hash=?4, updated_at=?5 WHERE book_id=?1 AND review_file=?2",
            params![book_id, review_file, phase, d, now],
        )?,
        (None, Some(f)) => db.exec(
            "UPDATE pending_chapter SET approve_phase=?3, formal_hash=?4, updated_at=?5 WHERE book_id=?1 AND review_file=?2",
            params![book_id, review_file, phase, f, now],
        )?,
        _ => db.exec(
            "UPDATE pending_chapter SET approve_phase=?3, updated_at=?4 WHERE book_id=?1 AND review_file=?2",
            params![book_id, review_file, phase, now],
        )?,
    };
    Ok(())
}

/// 单事务提交：队列 approved + 不可变批准凭证。0 行更新（并发/触发器故障）→ Err，
/// 事务回滚后队列保持 pending，可安全重试（G1 修复核心）。
fn commit_approval_tx(
    db: &Db,
    book_id: &str,
    review_file: &str,
    final_name: &str,
    hash: &str,
) -> Result<()> {
    let mut conn = db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))?;
    let tx = conn.transaction()?;
    let now = now_ms();
    let n = tx.execute(
        "UPDATE pending_chapter SET status='approved', formal_hash=?3, approve_phase='receipted', updated_at=?4
         WHERE book_id=?1 AND review_file=?2 AND status='pending'",
        params![book_id, review_file, hash, now],
    )?;
    if n == 0 {
        bail!("审批队列记录不一致（未找到待审记录或状态已变化），已保留待审源稿以便重试");
    }
    if let Some(ch) = chapter_number(final_name) {
        crate::continuity::record_approval_tx(&tx, book_id, ch, final_name, hash, now)?;
    }
    tx.commit()?;
    Ok(())
}

/// 审批主体。调用方必须已持有 db.fs_lock（files::approve_pending_chapter 与
/// recover_approvals 都满足）。绝不获取 fs_lock（不可重入，会死锁）。
pub(crate) fn approve_locked(db: &Db, book_id: &str, name: &str) -> Result<String> {
    if book_id.trim().is_empty() || name.trim().is_empty() {
        bail!("缺少 bookId/name");
    }
    let review_path = files::book_base_checked(db, book_id, REVIEW_GROUP, name)?;
    if !review_path.is_file() {
        bail!("待审文件不存在：{}", name);
    }
    // 依赖检查：本待审稿依赖的前序章若已变更/驳回，必须重审而不是直接定稿
    if let Some(ch) = chapter_number(name) {
        check_draft_dependency(db, book_id, ch)
            .with_context(|| format!("待审依赖检查失败：第{}章", ch))?;
    }
    // 队列预检（写盘之前）：只有 pending 记录可被接受
    let rows = db.q_json(
        "SELECT status FROM pending_chapter WHERE book_id=?1 AND review_file=?2",
        &[
            &book_id as &dyn rusqlite::ToSql,
            &name as &dyn rusqlite::ToSql,
        ],
    )?;
    let queue_status = rows
        .first()
        .and_then(|r| r["status"].as_str())
        .map(str::to_string);
    let final_name = files::safe_name(name);
    match queue_status.as_deref() {
        Some("pending") => {}
        Some(other) => bail!(
            "待审记录状态为 {}，不是 pending，拒绝接受（请先重审或重新生成）",
            other
        ),
        None => {
            // 无队列记录的幂等重入：已有批准凭证且正式稿 hash 一致 => 直接返回
            if chapter_number(name).is_some() {
                if let (Some(h), Some(cur)) = (
                    approved_hash(db, book_id, name).unwrap_or(None),
                    files::read_file(db, book_id, "正文", &final_name),
                ) {
                    if content_hash(&cur) == h {
                        return Ok(final_name);
                    }
                }
            }
            bail!("没有待审记录（队列不存在），拒绝接受：{}", name);
        }
    }
    let content = std::fs::read_to_string(&review_path).context("读取待审稿失败")?;
    let draft_hash = content_hash(&content);
    let final_path = files::book_base_checked(db, book_id, "正文", &final_name)?;
    // 正式稿已存在：同 hash = 中断 saga 残留（窗口A），续提交；异 hash = 真冲突，拒绝
    let mut formal_matches = false;
    if final_path.is_file() {
        let formal = std::fs::read_to_string(&final_path)
            .with_context(|| format!("读取正式稿失败，拒绝审批：正文/{}", final_name))?;
        if !formal.trim().is_empty() {
            if content_hash(&formal) == draft_hash {
                formal_matches = true;
            } else {
                bail!("正式稿已存在，拒绝覆盖：正文/{}", final_name);
            }
        }
    }
    // 1) 相位标记（best-effort）
    let _ = set_phase(db, book_id, name, "started", Some(&draft_hash), None);
    // 2) 正文持久化（原子；note_file_change 顺带预置 memory_job pending）
    if !formal_matches {
        files::write_file_locked(db, book_id, "正文", &final_name, &content)?;
    }
    let _ = set_phase(db, book_id, name, "formal_written", None, Some(&draft_hash));
    // 3) 单事务：队列 approved + 批准凭证。失败 → 保持 pending，待审源稿保留，可重试
    commit_approval_tx(db, book_id, name, &final_name, &draft_hash).map_err(|e| {
        anyhow!(
            "正文已保存为 {}，但审批提交失败（待审源稿已保留，可重试）：{}",
            final_name,
            e
        )
    })?;
    // 4) 事务提交后才清理唯一待审源稿（回收站可恢复 + 记账）；失败不回翻批准结果
    if let Err(e) = files::delete_file_locked(db, book_id, REVIEW_GROUP, name) {
        eprintln!(
            "[molan-core] 待审源稿清理失败（正文已批准，recover_approvals 将重试）：{}",
            e
        );
    } else {
        let _ = set_phase(db, book_id, name, "done", None, None);
    }
    Ok(final_name)
}

/// 启动自愈：收敛上次进程中断留下的审批 saga。判据唯一——磁盘正文 hash 与
/// 相位标记/凭证一致才续跑；不一致一律保留现场等待人工，绝不重放生成新内容。
/// 返回处理的行数。由 Db::open 迁移链后调用；失败不阻断启动（下次审批入口仍会自愈）。
pub fn recover_approvals(db: &Db) -> Result<usize> {
    crate::continuity::ensure_schema(db)?;
    let rows = db.q_json(
        "SELECT book_id, review_file, status, approve_phase, formal_hash
         FROM pending_chapter
         WHERE approve_phase IS NOT NULL AND approve_phase NOT IN ('', 'done')",
        &[],
    )?;
    let mut recovered = 0usize;
    for row in rows {
        let book = row["bookId"].as_str().unwrap_or("").to_string();
        let name = row["reviewFile"].as_str().unwrap_or("").to_string();
        let status = row["status"].as_str().unwrap_or("").to_string();
        let phase = row["approvePhase"].as_str().unwrap_or("").to_string();
        let marker = row["formalHash"].as_str().unwrap_or("").to_string();
        if book.is_empty() || name.is_empty() {
            continue;
        }
        let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
        let final_name = files::safe_name(&name);
        let review = files::read_file(db, &book, REVIEW_GROUP, &name);
        let formal = files::read_file(db, &book, "正文", &final_name);
        let formal_hash_now = formal.as_deref().map(content_hash);
        let marker_ok = formal_hash_now.as_deref() == Some(marker.as_str()) && !marker.is_empty();
        let outcome: Result<()> = match (phase.as_str(), status.as_str()) {
            // 事务已提交：只补清理
            ("receipted", _) => {
                if review.is_some() {
                    files::delete_file_locked(db, &book, REVIEW_GROUP, &name)?;
                }
                set_phase(db, &book, &name, "done", None, None)
            }
            // 队列已 approved 但相位停在事务前（异常组合）：凭证与磁盘一致才收敛
            (_, "approved") => {
                if marker_ok
                    || formal_hash_now.is_some()
                        && approved_hash(db, &book, &name).unwrap_or(None).as_deref()
                            == formal_hash_now.as_deref()
                {
                    if review.is_some() {
                        files::delete_file_locked(db, &book, REVIEW_GROUP, &name)?;
                    }
                    set_phase(db, &book, &name, "done", None, None)
                } else {
                    eprintln!(
                        "[molan-core] 审批恢复跳过（队列 approved 但凭证/正文 hash 不一致，需人工确认）：{}/{}",
                        book, name
                    );
                    Ok(())
                }
            }
            // 窗口A：正文已写、队列未更。hash 一致 → 续提交；否则保留现场
            ("formal_written", "pending") | ("started", "pending") => {
                if review.is_some() {
                    if formal.is_none() {
                        // 写盘未存活：清标记，回到普通 pending（下次审批全新走）
                        set_phase(db, &book, &name, "", None, None)
                    } else if marker.is_empty() || marker_ok {
                        approve_locked(db, &book, &name).map(|_| ())
                    } else {
                        eprintln!(
                            "[molan-core] 审批恢复跳过（formal_written 但正文 hash 与标记不符，需人工确认）：{}/{}",
                            book, name
                        );
                        Ok(())
                    }
                } else if formal.is_some() && marker_ok {
                    // 待审源稿已丢失但标记可证内容：直接补事务
                    commit_approval_tx(db, &book, &name, &final_name, &marker)
                        .and_then(|_| set_phase(db, &book, &name, "done", None, None))
                } else {
                    set_phase(db, &book, &name, "", None, None)
                }
            }
            _ => Ok(()),
        };
        match outcome {
            Ok(()) => recovered += 1,
            Err(e) => eprintln!(
                "[molan-core] 审批恢复失败（保留现场，可重试）：{}/{} phase={} err={}",
                book, name, phase, e
            ),
        }
    }
    Ok(recovered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let book = crate::books::create_book(&db, "saga-test", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        (dir, db, book)
    }

    fn enqueue(db: &Db, book: &str, ch: i64, name: &str) {
        db.exec(
            "INSERT INTO pending_chapter(book_id,ch,review_file,status,created_at,updated_at) VALUES(?1,?2,?3,'pending',0,0)
             ON CONFLICT(book_id,ch) DO UPDATE SET review_file=excluded.review_file,status='pending'",
            params![book, ch, name],
        )
        .unwrap();
    }

    fn queue_row(db: &Db, book: &str, name: &str) -> serde_json::Value {
        db.q_json(
            "SELECT status, approve_phase, draft_hash, formal_hash FROM pending_chapter WHERE book_id=?1 AND review_file=?2",
            params![book, name],
        )
        .unwrap()
        .remove(0)
    }

    #[test]
    fn happy_path_commits_queue_receipt_and_bookkeeping() {
        let (_d, db, book) = fixture();
        files::write_file(&db, &book, REVIEW_GROUP, "第1章.md", "AI 定稿一").unwrap();
        enqueue(&db, &book, 1, "第1章.md");
        // 预置草稿来源账本：批准后必须被 note_file_change 清理（G2）
        let hash = content_hash("AI 定稿一");
        crate::continuity::record_draft_origin(&db, &book, 1, "第1章.md", &hash, "task-x").unwrap();
        let name = files::approve_pending_chapter(&db, &book, "第1章.md").unwrap();
        assert_eq!(name, "第1章.md");
        assert_eq!(
            files::read_file(&db, &book, "正文", "第1章.md").as_deref(),
            Some("AI 定稿一")
        );
        assert!(files::read_file(&db, &book, REVIEW_GROUP, "第1章.md").is_none());
        let row = queue_row(&db, &book, "第1章.md");
        assert_eq!(row["status"], "approved");
        assert_eq!(row["approvePhase"], "done");
        assert_eq!(row["formalHash"].as_str().unwrap(), hash);
        // 不可变凭证可查且 hash 一致
        assert_eq!(
            approved_hash(&db, &book, "第1章.md").unwrap().as_deref(),
            Some(hash.as_str())
        );
        // note_file_change 记账：draft_origin 已清理；memory_job 已预置 pending
        assert!(db
            .q_json("SELECT * FROM draft_origin WHERE book_id=?1", params![book])
            .unwrap()
            .is_empty());
        let jobs = db
            .q_json(
                "SELECT status FROM memory_job WHERE book_id=?1 AND ch=1",
                params![book],
            )
            .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["status"], "pending");
        // 清理走回收站：待审源稿可恢复
        assert!(!files::list_file_trash(&db, &book)
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn window_a_same_content_formal_resumes_instead_of_deadlock() {
        // 崩溃窗口A：正文已写、队列未更。旧实现重试恒报「正式稿已存在」卡死；
        // 新实现同 hash 续提交，异 hash 仍拒绝。
        let (_d, db, book) = fixture();
        files::write_file(&db, &book, REVIEW_GROUP, "第2章.md", "定稿二").unwrap();
        enqueue(&db, &book, 2, "第2章.md");
        // 模拟上次中断：作者侧写入同内容正式稿（绕过审批）
        files::write_file(&db, &book, "正文", "第2章.md", "定稿二").unwrap();
        let name = files::approve_pending_chapter(&db, &book, "第2章.md").unwrap();
        assert_eq!(name, "第2章.md");
        let row = queue_row(&db, &book, "第2章.md");
        assert_eq!(row["status"], "approved");
        assert_eq!(
            approved_hash(&db, &book, "第2章.md").unwrap().as_deref(),
            Some(content_hash("定稿二").as_str())
        );
        assert!(files::read_file(&db, &book, REVIEW_GROUP, "第2章.md").is_none());
    }

    #[test]
    fn conflicting_formal_is_rejected_and_preserved() {
        let (_d, db, book) = fixture();
        files::write_file(&db, &book, "正文", "第3章.md", "作者手改稿").unwrap();
        files::write_file(&db, &book, REVIEW_GROUP, "第3章.md", "AI 待审稿").unwrap();
        enqueue(&db, &book, 3, "第3章.md");
        assert!(files::approve_pending_chapter(&db, &book, "第3章.md").is_err());
        assert_eq!(
            files::read_file(&db, &book, "正文", "第3章.md").as_deref(),
            Some("作者手改稿")
        );
        assert_eq!(
            files::read_file(&db, &book, REVIEW_GROUP, "第3章.md").as_deref(),
            Some("AI 待审稿")
        );
        assert_eq!(queue_row(&db, &book, "第3章.md")["status"], "pending");
    }

    #[test]
    fn receipt_failure_keeps_pending_and_retry_converges() {
        // G1 回归：凭证/队列事务故障 → 队列仍 pending、双稿保留、错误可见；
        // 故障排除后重试幂等收敛（不重复计数、不卡窗口A）。
        let (_d, db, book) = fixture();
        files::write_file(&db, &book, REVIEW_GROUP, "第6章.md", "待审稿六").unwrap();
        enqueue(&db, &book, 6, "第6章.md");
        db.exec(
            "CREATE TRIGGER fail_approve BEFORE UPDATE ON pending_chapter BEGIN SELECT RAISE(ABORT,'injected'); END",
            &[],
        )
        .unwrap();
        let out = files::approve_pending_chapter(&db, &book, "第6章.md");
        assert!(out.is_err(), "DB 故障必须报错");
        assert_eq!(
            files::read_file(&db, &book, "正文", "第6章.md").as_deref(),
            Some("待审稿六"),
            "正式稿已保存，不得回滚"
        );
        assert_eq!(
            files::read_file(&db, &book, REVIEW_GROUP, "第6章.md").as_deref(),
            Some("待审稿六"),
            "DB 故障时必须保留待审源稿"
        );
        // 触发器同样挡住队列 UPDATE → 状态必须仍是 pending（不是假 approved）
        db.exec("DROP TRIGGER fail_approve", &[]).unwrap();
        let still = db
            .q_json(
                "SELECT status FROM pending_chapter WHERE book_id=?1 AND ch=6",
                params![book],
            )
            .unwrap();
        assert_eq!(still[0]["status"], "pending");
        // 重试：窗口A 续提交，一次成功
        let name = files::approve_pending_chapter(&db, &book, "第6章.md").unwrap();
        assert_eq!(name, "第6章.md");
        let row = queue_row(&db, &book, "第6章.md");
        assert_eq!(row["status"], "approved");
        assert_eq!(row["approvePhase"], "done");
        // 凭证只有一份（重试不重复计数）
        let receipts = db
            .q_json(
                "SELECT COUNT(*) AS c FROM continuity_event WHERE book_id=?1 AND kind='chapter_approved'",
                params![book],
            )
            .unwrap();
        assert_eq!(receipts[0]["c"].as_i64().unwrap(), 1);
        assert!(files::read_file(&db, &book, REVIEW_GROUP, "第6章.md").is_none());
    }

    #[test]
    fn reentry_without_queue_row_uses_receipt_hash() {
        // 历史幂等语义保留：无队列行但凭证+正式稿 hash 一致 → Ok，不重建待审
        let (_d, db, book) = fixture();
        files::write_file(&db, &book, "正文", "第7章.md", "已批准稿").unwrap();
        let hash = content_hash("已批准稿");
        crate::continuity::record_approval(&db, &book, 7, "第7章.md", &hash).unwrap();
        files::write_file(&db, &book, REVIEW_GROUP, "第7章.md", "已批准稿").unwrap();
        let name = files::approve_pending_chapter(&db, &book, "第7章.md").unwrap();
        assert_eq!(name, "第7章.md");
    }

    #[test]
    fn recover_receipted_completes_cleanup() {
        // 事务已提交但清理未跑（进程崩溃）：recover 补删待审并置 done
        let (_d, db, book) = fixture();
        files::write_file(&db, &book, "正文", "第8章.md", "定稿八").unwrap();
        files::write_file(&db, &book, REVIEW_GROUP, "第8章.md", "定稿八").unwrap();
        let hash = content_hash("定稿八");
        db.exec(
            "INSERT INTO pending_chapter(book_id,ch,review_file,status,created_at,updated_at,approve_phase,formal_hash) VALUES(?1,8,?2,'approved',0,0,'receipted',?3)",
            params![book, "第8章.md", hash],
        )
        .unwrap();
        crate::continuity::record_approval(&db, &book, 8, "第8章.md", &hash).unwrap();
        let n = recover_approvals(&db).unwrap();
        assert!(n >= 1);
        assert!(files::read_file(&db, &book, REVIEW_GROUP, "第8章.md").is_none());
        assert_eq!(queue_row(&db, &book, "第8章.md")["approvePhase"], "done");
    }

    #[test]
    fn recover_started_without_formal_resets_phase() {
        // 相位标记后写盘未存活：清标记回普通 pending，稿子不动，之后可正常审批
        let (_d, db, book) = fixture();
        files::write_file(&db, &book, REVIEW_GROUP, "第9章.md", "定稿九").unwrap();
        enqueue(&db, &book, 9, "第9章.md");
        db.exec(
            "UPDATE pending_chapter SET approve_phase='started', draft_hash=?2 WHERE book_id=?1 AND ch=9",
            params![book, content_hash("定稿九")],
        )
        .unwrap();
        recover_approvals(&db).unwrap();
        assert_eq!(queue_row(&db, &book, "第9章.md")["approvePhase"], "");
        assert_eq!(
            files::read_file(&db, &book, REVIEW_GROUP, "第9章.md").as_deref(),
            Some("定稿九")
        );
        let name = files::approve_pending_chapter(&db, &book, "第9章.md").unwrap();
        assert_eq!(name, "第9章.md");
        assert_eq!(queue_row(&db, &book, "第9章.md")["status"], "approved");
    }

    #[test]
    fn recover_formal_written_hash_mismatch_keeps_scene() {
        // 标记 hash 与磁盘正文不符：绝不猜测续跑，保留现场等人工
        let (_d, db, book) = fixture();
        files::write_file(&db, &book, REVIEW_GROUP, "第10章.md", "定稿十").unwrap();
        files::write_file(&db, &book, "正文", "第10章.md", "被人改过的正文").unwrap();
        enqueue(&db, &book, 10, "第10章.md");
        db.exec(
            "UPDATE pending_chapter SET approve_phase='formal_written', formal_hash=?2 WHERE book_id=?1 AND ch=10",
            params![book, content_hash("定稿十")],
        )
        .unwrap();
        recover_approvals(&db).unwrap();
        let row = queue_row(&db, &book, "第10章.md");
        assert_eq!(row["status"], "pending", "不得自动批准 hash 不符的现场");
        assert_eq!(row["approvePhase"], "formal_written", "保留现场标记");
        assert_eq!(
            files::read_file(&db, &book, "正文", "第10章.md").as_deref(),
            Some("被人改过的正文")
        );
    }

    /// F3：中文数字章名（第十一章.md）批准后，凭证/记忆/溯源同等可达——
    /// 旧 chapter_number 只认阿拉伯数字，中文章名会让依赖检查与 origin 登记静默失效。
    #[test]
    fn chinese_numeral_chapter_approve_and_provenance_reachable() {
        let (_d, db, book) = fixture();
        let body = "第十一章 雪夜。内容足够长以通过各类长度校验。".repeat(6);
        files::write_file(&db, &book, REVIEW_GROUP, "第十一章.md", &body).unwrap();
        enqueue(&db, &book, 11, "第十一章.md");
        // 溯源登记在批准前（草稿仍在待审组）；旧解析器在此即 Err「草稿来源无效」
        crate::continuity::record_draft_origin(
            &db,
            &book,
            11,
            "第十一章.md",
            &content_hash(&body),
            "task-zh",
        )
        .unwrap();
        let final_name = files::approve_pending_chapter(&db, &book, "第十一章.md").unwrap();
        assert_eq!(final_name, "第十一章.md");
        // 批准凭证可达（记忆同步/幂等重入依赖它）
        let receipt = approved_hash(&db, &book, "第十一章.md").unwrap();
        assert_eq!(receipt.as_deref(), Some(content_hash(&body).as_str()));
        // 依赖可达：第12章依赖第11章正式稿 hash
        files::write_file(&db, &book, REVIEW_GROUP, "第十二章.md", "第十二章 草稿").unwrap();
        crate::continuity::record_draft_origin(
            &db,
            &book,
            12,
            "第十二章.md",
            &content_hash("第十二章 草稿"),
            "task-zh",
        )
        .unwrap();
        crate::continuity::record_draft_dependency(
            &db,
            &book,
            12,
            11,
            &content_hash(&body),
            "task-zh",
        )
        .unwrap();
        assert!(check_draft_dependency(&db, &book, 12).is_ok());
        // 父稿被改 → 依赖失效（中文章名同样触发级联）
        files::write_file(&db, &book, "正文", "第十一章.md", "第十一章 被改").unwrap();
        assert!(check_draft_dependency(&db, &book, 12).is_err());
    }
}
