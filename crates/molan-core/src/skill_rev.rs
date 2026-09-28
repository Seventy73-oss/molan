//! 技能版本化（附录 A4.1）：技能模板/目标/使用方式的可审计版本历史。
//!
//! 语义约定（本模块是唯一写方，handlers/decompose 只调用）：
//! - 主行 `skills.rev` 表示「当前版本号」，`skills.content_hash` 表示当前模板内容 hash；
//! - `skill_revision(skill_id,rev)` 存「第 rev 版当时的完整快照」，一经写入不可变；
//! - 新建技能先落库（默认 rev=1），随后调用 [`init_revision`] 记 hash + 快照 v1；
//! - 修改模板后调用 [`bump_revision`]，它归档当前版（幂等，保留旧快照）与新版本，
//!   再把主行 rev+1 并刷新 content_hash。因此任何历史版本的模板都可从
//!   `skill_revision` 取回，且 hash 与主行模板始终同口径（continuity::content_hash）。
//! - 拆书/蒸馏产生的 `kind='style'` 文风卡同样走本模块，但其 targets 不进任务路由
//!   （文风通道 book_style__<book>=style:<id> 行为不变）。
use crate::continuity::content_hash;
use crate::db::Db;
use crate::stats::now_ms;
use anyhow::{anyhow, Result};
use rusqlite::params;
use serde_json::Value;

const SNAPSHOT_COLS: &str = "rev, prompt_template, targets_json, usage_mode, kind";

fn lock(db: &Db) -> Result<std::sync::MutexGuard<'_, rusqlite::Connection>> {
    db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))
}

/// 新建技能后初始化版本 1：刷新 content_hash 并写入 v1 快照（幂等）。
pub fn init_revision(db: &Db, skill_id: &str, source_event: &str) -> Result<()> {
    let mut guard = lock(db)?;
    let tx = guard.transaction()?;
    let (rev, tpl, targets, usage, kind): (i64, String, String, String, String) = tx
        .query_row(
            &format!("SELECT {SNAPSHOT_COLS} FROM skills WHERE id=?1"),
            params![skill_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                ))
            },
        )
        .map_err(|_| anyhow!("技能不存在：{}", skill_id))?;
    let h = content_hash(&tpl);
    tx.execute(
        "INSERT OR IGNORE INTO skill_revision(skill_id,rev,prompt_template,targets_json,usage_mode,kind,content_hash,ts,source_event)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![skill_id, rev, tpl, targets, usage, kind, h, now_ms(), source_event],
    )?;
    tx.execute(
        "UPDATE skills SET content_hash=?2 WHERE id=?1",
        params![skill_id, h],
    )?;
    tx.commit()?;
    Ok(())
}

/// 模板/目标/使用方式变更后自增版本：归档当前版与新版本，主行 rev+1 并刷新 hash。
/// 同一事务内完成，崩溃不留半态。
pub fn bump_revision(db: &Db, skill_id: &str, source_event: &str) -> Result<()> {
    let mut guard = lock(db)?;
    let tx = guard.transaction()?;
    let (rev, tpl, targets, usage, kind): (i64, String, String, String, String) = tx
        .query_row(
            &format!("SELECT {SNAPSHOT_COLS} FROM skills WHERE id=?1"),
            params![skill_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                ))
            },
        )
        .map_err(|_| anyhow!("技能不存在：{}", skill_id))?;
    let h = content_hash(&tpl);
    let ts = now_ms();
    // 当前版快照：已存在则保留（这正是「旧模板」的可取回来源）
    tx.execute(
        "INSERT OR IGNORE INTO skill_revision(skill_id,rev,prompt_template,targets_json,usage_mode,kind,content_hash,ts,source_event)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![skill_id, rev, tpl, targets, usage, kind, h, ts, source_event],
    )?;
    // 新版本快照立即归档，保证下一轮修改不会覆盖它
    tx.execute(
        "INSERT OR IGNORE INTO skill_revision(skill_id,rev,prompt_template,targets_json,usage_mode,kind,content_hash,ts,source_event)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![skill_id, rev + 1, tpl, targets, usage, kind, h, ts, source_event],
    )?;
    tx.execute(
        "UPDATE skills SET rev=?2, content_hash=?3 WHERE id=?1",
        params![skill_id, rev + 1, h],
    )?;
    tx.commit()?;
    Ok(())
}

/// 迁移回填（Db::open 调用，幂等）：只处理从未初始化过的既有行——
/// content_hash 为空时按当前模板计算 hash，并补一条 v1 快照；rev 不变。
/// 已有 hash 的行原样保留，绝不静默改写用户数据。
pub fn ensure_backfill(db: &Db) -> Result<()> {
    let rows = db.q_json(
        "SELECT id, rev, prompt_template, targets_json, usage_mode, kind FROM skills WHERE content_hash IS NULL OR content_hash=''",
        &[],
    )?;
    if rows.is_empty() {
        return Ok(());
    }
    let ts = now_ms();
    for r in rows {
        let id = r["id"].as_str().unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let rev = r["rev"].as_i64().unwrap_or(1);
        let tpl = r["promptTemplate"].as_str().unwrap_or("");
        let h = content_hash(tpl);
        db.exec(
            "INSERT OR IGNORE INTO skill_revision(skill_id,rev,prompt_template,targets_json,usage_mode,kind,content_hash,ts,source_event)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'backfill')",
            &[
                &id as &dyn rusqlite::ToSql,
                &rev,
                &tpl,
                &r["targetsJson"].as_str().unwrap_or("[]"),
                &r["usageMode"].as_str().unwrap_or(""),
                &r["kind"].as_str().unwrap_or(""),
                &h,
                &ts,
            ],
        )?;
        db.exec(
            "UPDATE skills SET content_hash=?2 WHERE id=?1",
            &[&id as &dyn rusqlite::ToSql, &h],
        )?;
    }
    Ok(())
}

/// 读取某技能的全部历史版本（按 rev 升序），供审计/回滚与测试使用。
pub fn revisions(db: &Db, skill_id: &str) -> Result<Vec<Value>> {
    db.q_json(
        "SELECT * FROM skill_revision WHERE skill_id=?1 ORDER BY rev",
        &[&skill_id as &dyn rusqlite::ToSql],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(db: &Db, id: &str, tpl: &str, hash: &str) -> Result<()> {
        db.exec(
            "INSERT INTO skills(id,name,description,prompt_template,kind,source,enabled,builtin_key,usage_mode,origin,targets_json,rev,content_hash)
             VALUES(?1,'t','',?2,'craft','',1,NULL,'support','user','[\"body\"]',1,?3)",
            &[&id as &dyn rusqlite::ToSql, &tpl, &hash],
        )?;
        Ok(())
    }

    #[test]
    fn bump_archives_old_template_and_advances_rev() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        seed(&db, "s1", "OLD_TPL", "").unwrap();
        init_revision(&db, "s1", "create").unwrap();
        // 修改模板后 bump：rev+1，hash 与新模板一致
        db.exec(
            "UPDATE skills SET prompt_template='NEW_TPL' WHERE id='s1'",
            &[],
        )
        .unwrap();
        bump_revision(&db, "s1", "update").unwrap();
        let row = &db
            .q_json("SELECT rev, content_hash FROM skills WHERE id='s1'", &[])
            .unwrap()[0];
        assert_eq!(row["rev"], 2, "rev 必须自增到 2");
        assert_eq!(row["contentHash"], content_hash("NEW_TPL"));
        // 旧模板仍可从 skill_revision 取回
        let hist = revisions(&db, "s1").unwrap();
        assert_eq!(hist.len(), 2, "应有 v1/v2 两条快照：{:?}", hist);
        assert_eq!(hist[0]["rev"], 1);
        assert_eq!(hist[0]["promptTemplate"], "OLD_TPL");
        assert_eq!(hist[0]["contentHash"], content_hash("OLD_TPL"));
        assert_eq!(hist[1]["rev"], 2);
        assert_eq!(hist[1]["promptTemplate"], "NEW_TPL");
    }

    #[test]
    fn repeated_bumps_keep_every_version() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        seed(&db, "s2", "V1", "").unwrap();
        init_revision(&db, "s2", "create").unwrap();
        for tpl in ["V2", "V3"] {
            db.exec(
                "UPDATE skills SET prompt_template=?1 WHERE id='s2'",
                &[&tpl as &dyn rusqlite::ToSql],
            )
            .unwrap();
            bump_revision(&db, "s2", "update").unwrap();
        }
        let hist = revisions(&db, "s2").unwrap();
        let tpls: Vec<&str> = hist
            .iter()
            .filter_map(|r| r["promptTemplate"].as_str())
            .collect();
        assert_eq!(tpls, vec!["V1", "V2", "V3"], "每个历史版本都必须可取回");
        let rev = db
            .q_json("SELECT rev FROM skills WHERE id='s2'", &[])
            .unwrap()[0]["rev"]
            .as_i64()
            .unwrap();
        assert_eq!(rev, 3);
    }

    #[test]
    fn ensure_backfill_is_idempotent_and_keeps_rev() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        seed(&db, "legacy", "LEGACY_TPL", "").unwrap();
        ensure_backfill(&db).unwrap();
        ensure_backfill(&db).unwrap();
        let hist = revisions(&db, "legacy").unwrap();
        assert_eq!(hist.len(), 1, "回填跑两次不得重复插行：{:?}", hist);
        assert_eq!(hist[0]["promptTemplate"], "LEGACY_TPL");
        assert_eq!(hist[0]["sourceEvent"], "backfill");
        let row = &db
            .q_json(
                "SELECT rev, content_hash FROM skills WHERE id='legacy'",
                &[],
            )
            .unwrap()[0];
        assert_eq!(row["rev"], 1, "回填不得改变既有 rev");
        assert_eq!(row["contentHash"], content_hash("LEGACY_TPL"));
        // 已有 hash 的行不再被改写
        db.exec(
            "UPDATE skills SET prompt_template='CHANGED' WHERE id='legacy'",
            &[],
        )
        .unwrap();
        ensure_backfill(&db).unwrap();
        let row2 = &db
            .q_json("SELECT content_hash FROM skills WHERE id='legacy'", &[])
            .unwrap()[0];
        assert_eq!(
            row2["contentHash"],
            content_hash("LEGACY_TPL"),
            "已初始化行不被静默改写"
        );
    }

    #[test]
    fn bump_unknown_skill_errors() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        assert!(bump_revision(&db, "nope", "update").is_err());
    }
}
