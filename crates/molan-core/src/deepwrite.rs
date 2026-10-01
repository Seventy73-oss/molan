//! DeepWrite-inspired project workflow primitives for Molan.
use crate::{db::Db, files, stats::now_ms};
use anyhow::{anyhow, bail, Result};
use rusqlite::ToSql;
use serde_json::{json, Value};

const ROLES: [(&str, &str, &str); 4] = [
    (
        "planner",
        "剧情策划",
        "负责主题、结构、大纲与章节目标，只提出可执行的剧情方案。",
    ),
    (
        "character",
        "人物导演",
        "负责人设、人物弧光、关系与角色行为一致性。",
    ),
    (
        "continuity",
        "连续性审校",
        "负责时间线、设定、伏笔、事实证据与跨章一致性。",
    ),
    (
        "editor",
        "文字编辑",
        "负责正文表达、节奏、对白、删改与最终质量，不擅自改变核心剧情。",
    ),
];

pub fn ensure_schema(db: &Db) -> Result<()> {
    let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS dw_agent_profile (
            book_id TEXT NOT NULL, role TEXT NOT NULL, name TEXT NOT NULL,
            system_prompt TEXT NOT NULL, model TEXT NOT NULL DEFAULT '',
            enabled INTEGER NOT NULL DEFAULT 1, updated_at INTEGER NOT NULL,
            PRIMARY KEY(book_id, role));
         CREATE TABLE IF NOT EXISTS dw_book_skill (
            book_id TEXT NOT NULL, skill_id TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1, updated_at INTEGER NOT NULL,
            PRIMARY KEY(book_id, skill_id));
         CREATE TABLE IF NOT EXISTS dw_change_proposal (
            id TEXT PRIMARY KEY, book_id TEXT NOT NULL, role TEXT NOT NULL,
            group_name TEXT NOT NULL, file_name TEXT NOT NULL,
            summary TEXT NOT NULL DEFAULT '', base_hash TEXT NOT NULL,
            base_content TEXT NOT NULL, proposed_content TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending', created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL, applied_at INTEGER, error TEXT);
         CREATE INDEX IF NOT EXISTS idx_dw_agent_book ON dw_agent_profile(book_id, role);
         CREATE INDEX IF NOT EXISTS idx_dw_skill_book ON dw_book_skill(book_id, enabled);
         CREATE INDEX IF NOT EXISTS idx_dw_proposal_book ON dw_change_proposal(book_id, status, created_at);"
    )?;
    Ok(())
}

fn require_book(db: &Db, book_id: &str) -> Result<()> {
    if !files::valid_book_id(db, book_id) {
        bail!("书籍不存在或未初始化：{}", book_id);
    }
    Ok(())
}
fn valid_role(role: &str) -> bool {
    ROLES.iter().any(|(id, _, _)| *id == role)
}

pub fn ensure_book_agents(db: &Db, book_id: &str) -> Result<()> {
    require_book(db, book_id)?;
    let now = now_ms();
    for (role, name, prompt) in ROLES {
        db.exec(
            "INSERT OR IGNORE INTO dw_agent_profile(book_id,role,name,system_prompt,model,enabled,updated_at) VALUES(?1,?2,?3,?4,'',1,?5)",
            &[&book_id as &dyn ToSql, &role, &name, &prompt, &now],
        )?;
    }
    Ok(())
}

pub fn list_agents(db: &Db, book_id: &str) -> Result<Value> {
    ensure_book_agents(db, book_id)?;
    Ok(json!(db.q_json(
        "SELECT role,name,system_prompt,model,enabled,updated_at FROM dw_agent_profile WHERE book_id=?1 ORDER BY CASE role WHEN 'planner' THEN 1 WHEN 'character' THEN 2 WHEN 'continuity' THEN 3 ELSE 4 END",
        &[&book_id as &dyn ToSql],
    )?))
}

pub fn save_agent(
    db: &Db,
    book_id: &str,
    role: &str,
    name: &str,
    system_prompt: &str,
    model: &str,
    enabled: bool,
) -> Result<Value> {
    require_book(db, book_id)?;
    if !valid_role(role) {
        bail!("未知智能体角色：{}", role);
    }
    if name.trim().is_empty() || system_prompt.trim().is_empty() {
        bail!("智能体名称和系统提示词不能为空");
    }
    let now = now_ms();
    db.exec(
        "INSERT INTO dw_agent_profile(book_id,role,name,system_prompt,model,enabled,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(book_id,role) DO UPDATE SET name=excluded.name,system_prompt=excluded.system_prompt,model=excluded.model,enabled=excluded.enabled,updated_at=excluded.updated_at",
        &[&book_id as &dyn ToSql, &role, &name.trim(), &system_prompt.trim(), &model.trim(), &(enabled as i64), &now],
    )?;
    Ok(db.q_json("SELECT role,name,system_prompt,model,enabled,updated_at FROM dw_agent_profile WHERE book_id=?1 AND role=?2", &[&book_id as &dyn ToSql, &role])?.into_iter().next().unwrap_or(Value::Null))
}

pub fn list_skill_bindings(db: &Db, book_id: &str) -> Result<Value> {
    require_book(db, book_id)?;
    Ok(json!(db.q_json(
        "SELECT s.id,s.name,s.description,s.prompt_template,s.kind,s.usage_mode,b.enabled,b.updated_at FROM dw_book_skill b JOIN skills s ON s.id=b.skill_id WHERE b.book_id=?1 ORDER BY s.name",
        &[&book_id as &dyn ToSql],
    )?))
}

pub fn bind_skill(db: &Db, book_id: &str, skill_id: &str, enabled: bool) -> Result<Value> {
    require_book(db, book_id)?;
    if db
        .q_json(
            "SELECT id FROM skills WHERE id=?1",
            &[&skill_id as &dyn ToSql],
        )?
        .is_empty()
    {
        bail!("技能不存在：{}", skill_id);
    }
    let now = now_ms();
    db.exec(
        "INSERT INTO dw_book_skill(book_id,skill_id,enabled,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(book_id,skill_id) DO UPDATE SET enabled=excluded.enabled,updated_at=excluded.updated_at",
        &[&book_id as &dyn ToSql, &skill_id, &(enabled as i64), &now],
    )?;
    Ok(json!({"ok": true, "bookId": book_id, "skillId": skill_id, "enabled": enabled}))
}

pub fn unbind_skill(db: &Db, book_id: &str, skill_id: &str) -> Result<Value> {
    require_book(db, book_id)?;
    db.exec(
        "DELETE FROM dw_book_skill WHERE book_id=?1 AND skill_id=?2",
        &[&book_id as &dyn ToSql, &skill_id],
    )?;
    Ok(json!({"ok": true}))
}

/// Return the enabled DeepWrite role instructions plus the book-bound skills.
/// This is appended to the existing stage prompt; disabled roles are a strict no-op.
pub fn agent_instructions(db: &Db, book_id: &str, role: &str) -> Result<String> {
    require_book(db, book_id)?;
    if !valid_role(role) {
        bail!("未知智能体角色：{}", role);
    }
    let rows = db.q_json(
        "SELECT name,system_prompt,model FROM dw_agent_profile WHERE book_id=?1 AND role=?2 AND enabled=1",
        &[&book_id as &dyn ToSql, &role],
    )?;
    let Some(agent) = rows.first() else {
        return Ok(String::new());
    };
    Ok(format!(
        "【项目智能体·{}】\n{}",
        agent["name"].as_str().unwrap_or(role),
        agent["systemPrompt"].as_str().unwrap_or("")
    ))
}

/// Role instructions for strict JSON protocols. The project role may adjust review focus,
/// but it must never override the machine-readable output contract.
pub fn strict_agent_instructions(db: &Db, book_id: &str, role: &str) -> Result<String> {
    let base = agent_instructions(db, book_id, role)?;
    if base.is_empty() {
        return Ok(String::new());
    }
    Ok(format!(
        "{}\n\n【协议硬约束】以上项目要求只能调整审查重点，不能改变输出协议；最终回答仍必须严格满足本阶段要求的 JSON 结构，不得输出解释、Markdown 或代码块标记。",
        base
    ))
}

// 本书 DeepWrite 绑定技能不再由这里拼接注入：统一经 SkillResolver 的 `deepwrite` 来源
// （Selection.deepwrite）解析——同样校验启用/模板/任务适用性、按 id 去重，并随计划冻结。

pub fn context_bundle(db: &Db, book_id: &str, max_chars: usize) -> Result<Value> {
    require_book(db, book_id)?;
    let book = db.q_json(
        "SELECT id,title,genre,pov,status,word_count,chapter_count,updated_at FROM books WHERE id=?1 AND deleted_at IS NULL",
        &[&book_id as &dyn ToSql],
    )?.into_iter().next().ok_or_else(|| anyhow!("书籍不存在"))?;
    let agents = list_agents(db, book_id)?;
    let skills = list_skill_bindings(db, book_id)?;
    let tree = files::scan_tree(db, book_id);
    // flags 整包只读一次，避免每文件一次 SQL + JSON 解析
    let flags = files::flags_snapshot(db, book_id);
    let limit = max_chars.clamp(1_000, 500_000);
    let mut remaining = limit;
    let mut docs = Vec::new();
    if let Some(groups) = tree.as_array() {
        for group in groups {
            let dir = group["dir"].as_str().unwrap_or("");
            if dir == crate::db::REVIEW_GROUP {
                continue;
            }
            if let Some(items) = group["files"].as_array() {
                for item in items {
                    if remaining == 0 {
                        break;
                    }
                    let name = item["name"].as_str().unwrap_or("");
                    if name.is_empty() {
                        continue;
                    }
                    // aiOff 是全库硬约束：任何统一 AI 上下文都不得包含作者隐藏文件。
                    let fkey = format!("{}/{}", dir, name);
                    if flags
                        .get(&fkey)
                        .and_then(|v| v.get("aiOff"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    if let Some(text) = files::read_file(db, book_id, dir, name) {
                        let total = text.chars().count();
                        let take = remaining.min(total);
                        let clipped: String = text.chars().take(take).collect();
                        remaining -= take;
                        docs.push(json!({"group": dir, "name": name, "content": clipped, "truncated": take < total}));
                    }
                }
            }
        }
    }
    Ok(
        json!({"schema":"molan-deepwrite-context/v1","book":book,"agents":agents,"skills":skills,"documents":docs,"limits":{"maxChars":limit,"usedChars":limit-remaining}}),
    )
}

pub fn create_proposal(
    db: &Db,
    book_id: &str,
    role: &str,
    group: &str,
    name: &str,
    summary: &str,
    proposed_content: &str,
) -> Result<Value> {
    require_book(db, book_id)?;
    if !valid_role(role) {
        bail!("未知智能体角色：{}", role);
    }
    let canon = files::normalize_group(group);
    let safe = files::safe_name(name);
    if canon == "_invalid_" || safe == "_invalid_" {
        bail!("非法目标文件");
    }
    // aiOff/locked 是作者侧硬边界：隐藏文件全文不得进提案库，锁定文件不接受 AI 改稿
    if files::file_flag(db, book_id, &canon, &safe, "aiOff") {
        bail!("文件已设为 AI 不可见（aiOff），禁止对其创建变更提案");
    }
    if files::file_flag(db, book_id, &canon, &safe, "locked") {
        bail!("文件已被作者锁定，禁止创建变更提案");
    }
    let base = files::read_file(db, book_id, group, name).unwrap_or_default();
    if base == proposed_content {
        bail!("提案内容与当前文件相同");
    }
    let id = uuid::Uuid::new_v4().to_string();
    let hash = crate::continuity::content_hash(&base);
    let now = now_ms();
    db.exec(
        "INSERT INTO dw_change_proposal(id,book_id,role,group_name,file_name,summary,base_hash,base_content,proposed_content,status,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'pending',?10,?10)",
        &[&id as &dyn ToSql, &book_id, &role, &files::normalize_group(group), &files::safe_name(name), &summary.trim(), &hash, &base, &proposed_content, &now],
    )?;
    get_proposal(db, &id)
}

pub fn get_proposal(db: &Db, id: &str) -> Result<Value> {
    db.q_json("SELECT id,book_id,role,group_name,file_name,summary,base_hash,base_content,proposed_content,status,created_at,updated_at,applied_at,error FROM dw_change_proposal WHERE id=?1", &[&id as &dyn ToSql])?
        .into_iter().next().ok_or_else(|| anyhow!("变更提案不存在：{}", id))
}

pub fn get_proposal_for_book(db: &Db, book_id: &str, id: &str) -> Result<Value> {
    require_book(db, book_id)?;
    let proposal = get_proposal(db, id)?;
    if proposal["bookId"].as_str() != Some(book_id) {
        bail!("变更提案不属于当前书籍");
    }
    Ok(proposal)
}

pub fn list_proposals(db: &Db, book_id: &str, status: Option<&str>) -> Result<Value> {
    require_book(db, book_id)?;
    let rows = match status.filter(|s| !s.is_empty()) {
        Some(status) => db.q_json("SELECT id,book_id,role,group_name,file_name,summary,base_hash,status,created_at,updated_at,applied_at,error FROM dw_change_proposal WHERE book_id=?1 AND status=?2 ORDER BY created_at DESC", &[&book_id as &dyn ToSql, &status])?,
        None => db.q_json("SELECT id,book_id,role,group_name,file_name,summary,base_hash,status,created_at,updated_at,applied_at,error FROM dw_change_proposal WHERE book_id=?1 ORDER BY created_at DESC", &[&book_id as &dyn ToSql])?,
    };
    Ok(json!(rows))
}

pub fn accept_proposal(db: &Db, book_id: &str, id: &str) -> Result<Value> {
    // 接受与拒绝共用 fs_lock，保证状态检查、文件 CAS 和状态翻转不可交错。
    let _guard = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    let p = get_proposal_for_book(db, book_id, id)?;
    if p["status"].as_str() != Some("pending") {
        bail!("提案不是待审状态");
    }
    let group = p["groupName"].as_str().unwrap_or("");
    let name = p["fileName"].as_str().unwrap_or("");
    let base = p["baseContent"].as_str().unwrap_or("");
    let proposed = p["proposedContent"].as_str().unwrap_or("");
    if crate::continuity::content_hash(base) != p["baseHash"].as_str().unwrap_or("") {
        bail!("提案基线损坏，拒绝应用");
    }
    // 与待审队列交叉保护：提案写「正文」时，同章存在待审稿会卡死审批队列；
    // 提案写「正文待审」时必须存在队列记录，否则留下永远无法审批的孤儿稿。
    if group == "正文" {
        let pending_rows = db.q_json(
            "SELECT id FROM pending_chapter WHERE book_id=?1 AND review_file=?2 AND status='pending'",
            &[&book_id as &dyn ToSql, &name],
        )?;
        if !pending_rows.is_empty()
            || files::read_file(db, book_id, crate::db::REVIEW_GROUP, name).is_some()
        {
            bail!("该章存在待审稿，请先在待审队列处理后，再接受此提案");
        }
    }
    if group == crate::db::REVIEW_GROUP {
        let pending_rows = db.q_json(
            "SELECT id FROM pending_chapter WHERE book_id=?1 AND review_file=?2 AND status='pending'",
            &[&book_id as &dyn ToSql, &name],
        )?;
        if pending_rows.is_empty() {
            bail!("待审队列无此文件记录，拒绝写入孤儿待审稿");
        }
    }
    // 抢占式状态机：先 claim（pending→applying），写盘成功置 accepted，失败置回 pending。
    // 绝不依赖「反向 CAS 回滚写」——回滚写自身可能失败，会造成文件与状态永久不一致。
    let now = now_ms();
    let claimed = db.exec(
        "UPDATE dw_change_proposal SET status='applying',updated_at=?2 WHERE id=?1 AND book_id=?3 AND status='pending'",
        &[&id as &dyn ToSql, &now, &book_id],
    )?;
    if claimed != 1 {
        bail!("提案状态已变化，接受操作未执行");
    }
    if let Err(e) = files::write_file_cas_locked(db, book_id, group, name, base, proposed) {
        let msg = e.to_string();
        let now = now_ms();
        // 文件已落盘、只是派生索引失败：状态必须是 accepted（附错误），绝不回滚为 pending——
        // 否则重试时 CAS 基线已不成立，提案永远无法再接受（旧缺陷）。
        if files::read_file(db, book_id, group, name).as_deref() == Some(proposed) {
            ledger(
                db,
                book_id,
                group,
                name,
                base,
                proposed,
                id,
                Some(msg.clone()),
            );
            db.exec(
                "UPDATE dw_change_proposal SET status='accepted',error=?2,applied_at=?3,updated_at=?3 WHERE id=?1 AND book_id=?4 AND status='applying'",
                &[&id as &dyn ToSql, &msg, &now, &book_id],
            )?;
            return Ok(json!({
                "ok": true, "id": id, "status": "accepted", "bookId": book_id, "group": group, "name": name,
                "appliedHash": crate::continuity::content_hash(proposed), "indexError": msg,
            }));
        }
        let _ = db.exec(
            "UPDATE dw_change_proposal SET status='pending',error=?2,updated_at=?3 WHERE id=?1 AND book_id=?4 AND status='applying'",
            &[&id as &dyn ToSql, &msg, &now, &book_id],
        );
        return Err(anyhow!("提案应用冲突：{}", msg));
    }
    let now = now_ms();
    let changed = db.exec(
        "UPDATE dw_change_proposal SET status='accepted',error=NULL,applied_at=?2,updated_at=?2 WHERE id=?1 AND book_id=?3 AND status='applying'",
        &[&id as &dyn ToSql, &now, &book_id],
    )?;
    if changed != 1 {
        bail!("提案状态提交失败：文件已写入但状态未翻转，请从版本历史核查");
    }
    ledger(db, book_id, group, name, base, proposed, id, None);
    // appliedHash：实际落盘内容（=proposed）的指纹。手动线前端用它作 confirm_outline 的
    // expectedHash（接受→确认一步链，绑定作者刚接受的那一版），不在客户端算 hash。
    Ok(json!({
        "ok":true,"id":id,"status":"accepted","bookId":book_id,"group":group,"name":name,
        "appliedHash": crate::continuity::content_hash(proposed),
    }))
}

/// 提案接受的统一写入回执（DocumentWriteService 账本）：基线为空即新建，否则为整篇替换。
#[allow(clippy::too_many_arguments)]
fn ledger(
    db: &Db,
    book: &str,
    group: &str,
    name: &str,
    base: &str,
    new: &str,
    id: &str,
    idx: Option<String>,
) {
    let (op, before) = if base.is_empty() {
        ("create", None)
    } else {
        ("replace", Some(crate::continuity::content_hash(base)))
    };
    let src = json!({"service": "deepwrite.accept_proposal", "proposalId": id});
    let after = crate::continuity::content_hash(new);
    let n = new.chars().count() as i64;
    let actor = crate::doc_write::Actor::Ai;
    if let Err(e) = crate::doc_write::record_external(
        db, book, group, name, op, actor, before, &after, n, idx, src,
    ) {
        eprintln!("[molan-core] 提案写入回执记账失败（文件已保存）：{}", e);
    }
}

pub fn reject_proposal(db: &Db, book_id: &str, id: &str, reason: &str) -> Result<Value> {
    let _guard = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    let p = get_proposal_for_book(db, book_id, id)?;
    if p["status"].as_str() != Some("pending") {
        bail!("提案不是待审状态");
    }
    let now = now_ms();
    let changed = db.exec(
        "UPDATE dw_change_proposal SET status='rejected',error=?2,updated_at=?3 WHERE id=?1 AND book_id=?4 AND status='pending'",
        &[&id as &dyn ToSql, &reason.trim(), &now, &book_id],
    )?;
    if changed != 1 {
        bail!("提案状态已变化，拒绝操作未执行");
    }
    Ok(json!({"ok":true,"id":id,"status":"rejected"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::books;
    fn fixture() -> (tempfile::TempDir, Db, String) {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(d.path(), None).unwrap();
        let id = books::create_book(&db, "迁移测试", "悬疑", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        (d, db, id)
    }
    #[test]
    fn agents_are_seeded_and_customizable() {
        let (_d, db, book) = fixture();
        assert_eq!(
            list_agents(&db, &book).unwrap().as_array().unwrap().len(),
            4
        );
        save_agent(
            &db,
            &book,
            "editor",
            "终审",
            "只做最小必要修改",
            "model-x",
            false,
        )
        .unwrap();
        let a = list_agents(&db, &book).unwrap();
        let e = a
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["role"] == "editor")
            .unwrap();
        assert_eq!(e["name"], "终审");
        assert_eq!(e["enabled"], 0);
        assert_eq!(agent_instructions(&db, &book, "editor").unwrap(), "");
        save_agent(
            &db,
            &book,
            "editor",
            "终审",
            "只做最小必要修改",
            "model-x",
            true,
        )
        .unwrap();
        let ins = agent_instructions(&db, &book, "editor").unwrap();
        assert!(ins.contains("项目智能体·终审"));
        assert!(ins.contains("只做最小必要修改"));
        let strict = strict_agent_instructions(&db, &book, "editor").unwrap();
        assert!(strict.contains("协议硬约束"));
        assert!(strict.contains("严格满足本阶段要求的 JSON 结构"));
    }
    #[test]
    fn skill_bindings_are_book_scoped_and_context_is_bounded() {
        let (_d, db, a) = fixture();
        let b = books::create_book(&db, "另一本", "都市", "第一人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        db.exec("INSERT INTO skills(id,name,description,prompt_template,enabled) VALUES('s1','节奏','控制节奏','模板',1)",&[]).unwrap();
        bind_skill(&db, &a, "s1", true).unwrap();
        let ins = agent_instructions(&db, &a, "planner").unwrap();
        assert!(!ins.contains("本书绑定技能"));
        // 绑定技能经 SkillResolver 的 deepwrite 来源进入计划，且只作用于本书
        let sel = crate::skill_resolver::Selection {
            deepwrite: true,
            ..Default::default()
        };
        let ids = |book: &str| {
            crate::skill_resolver::resolve(&db, book, crate::task_kind::TaskKind::Revise, &sel)
                ["skills"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| (s["id"].as_str().unwrap().to_string(), s["source"].clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&a), vec![("s1".to_string(), json!("deepwrite"))]);
        assert!(ids(&b).is_empty());
        assert_eq!(
            list_skill_bindings(&db, &a)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list_skill_bindings(&db, &b)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            0
        );
        files::write_file(&db, &a, "设定", "世界.md", &"甲".repeat(2000)).unwrap();
        files::write_file(&db, &a, "设定", "作者私稿.md", "绝密内容").unwrap();
        files::set_file_flag(&db, &a, "设定", "作者私稿.md", "aiOff", true).unwrap();
        let ctx = context_bundle(&db, &a, 1000).unwrap();
        assert_eq!(ctx["schema"], "molan-deepwrite-context/v1");
        assert_eq!(ctx["limits"]["usedChars"], 1000);
        assert_eq!(ctx["skills"].as_array().unwrap().len(), 1);
        assert!(!ctx["documents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|doc| doc["name"] == "作者私稿.md"
                || doc["content"].as_str().unwrap_or("").contains("绝密内容")));
    }
    #[test]
    fn proposal_accepts_once_and_rejects_concurrent_edit() {
        let (_d, db, book) = fixture();
        files::write_file(&db, &book, "正文", "第1章.md", "原稿").unwrap();
        let p = create_proposal(&db, &book, "editor", "正文", "第1章.md", "润色", "新稿").unwrap();
        accept_proposal(&db, &book, p["id"].as_str().unwrap()).unwrap();
        assert_eq!(
            files::read_file(&db, &book, "正文", "第1章.md").as_deref(),
            Some("新稿")
        );
        // 提案接受与其他写入共用同一份写入回执账本
        let h = crate::doc_write::history(&db, &book, "正文", "第1章.md", 5).unwrap();
        assert_eq!(h[0]["source"]["service"], "deepwrite.accept_proposal");
        assert_eq!(h[0]["op"], "replace");
        assert_eq!(
            h[0]["beforeHash"],
            json!(crate::continuity::content_hash("原稿"))
        );
        assert_eq!(
            h[0]["afterHash"],
            json!(crate::continuity::content_hash("新稿"))
        );
        let p2 =
            create_proposal(&db, &book, "editor", "正文", "第1章.md", "再润色", "提案稿").unwrap();
        files::write_file(&db, &book, "正文", "第1章.md", "作者并发修改").unwrap();
        assert!(accept_proposal(&db, &book, p2["id"].as_str().unwrap()).is_err());
        assert_eq!(
            files::read_file(&db, &book, "正文", "第1章.md").as_deref(),
            Some("作者并发修改")
        );
        assert_eq!(
            get_proposal(&db, p2["id"].as_str().unwrap()).unwrap()["status"],
            "pending"
        );

        let other = books::create_book(&db, "其他书", "都市", "第一人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(get_proposal_for_book(&db, &other, p2["id"].as_str().unwrap()).is_err());
        assert!(accept_proposal(&db, &other, p2["id"].as_str().unwrap()).is_err());
        assert!(reject_proposal(&db, &other, p2["id"].as_str().unwrap(), "越权").is_err());
    }

    #[test]
    fn proposal_respects_aioff_and_locked_flags() {
        let (_d, db, book) = fixture();
        files::write_file(&db, &book, "设定", "secret.md", "绝密内容").unwrap();
        files::set_file_flag(&db, &book, "设定", "secret.md", "aiOff", true).unwrap();
        let e =
            create_proposal(&db, &book, "editor", "设定", "secret.md", "x", "改稿").unwrap_err();
        assert!(
            e.to_string().contains("aiOff"),
            "aiOff 文件禁止建提案：{}",
            e
        );

        files::write_file(&db, &book, "设定", "lock.md", "锁定内容").unwrap();
        files::set_file_flag(&db, &book, "设定", "lock.md", "locked", true).unwrap();
        let e = create_proposal(&db, &book, "editor", "设定", "lock.md", "x", "改稿").unwrap_err();
        assert!(
            e.to_string().contains("锁定"),
            "locked 文件禁止建提案：{}",
            e
        );
    }

    #[test]
    fn accept_rejects_when_pending_queue_conflicts() {
        let (_d, db, book) = fixture();
        files::write_file(&db, &book, "正文", "第1章.md", "原稿").unwrap();
        let p = create_proposal(&db, &book, "editor", "正文", "第1章.md", "改", "提案稿").unwrap();
        // 同章存在待审稿 → 接受正文提案会卡死审批队列，必须拒绝
        db.exec(
            "INSERT INTO pending_chapter(book_id,ch,review_file,status,created_at) VALUES(?1,1,'第1章.md','pending',?2)",
            &[&book as &dyn ToSql, &now_ms()],
        )
        .unwrap();
        let e = accept_proposal(&db, &book, p["id"].as_str().unwrap()).unwrap_err();
        assert!(e.to_string().contains("待审"), "待审冲突必须拒绝：{}", e);
        // 文件未被修改，提案仍 pending
        assert_eq!(
            files::read_file(&db, &book, "正文", "第1章.md").as_deref(),
            Some("原稿")
        );
        assert_eq!(
            get_proposal(&db, p["id"].as_str().unwrap()).unwrap()["status"],
            "pending"
        );

        // 待审组提案：队列无记录 → 拒绝孤儿稿
        files::write_file(&db, &book, crate::db::REVIEW_GROUP, "第2章.md", "待审原稿").unwrap();
        let p2 = create_proposal(
            &db,
            &book,
            "editor",
            crate::db::REVIEW_GROUP,
            "第2章.md",
            "改",
            "新待审稿",
        )
        .unwrap();
        let e = accept_proposal(&db, &book, p2["id"].as_str().unwrap()).unwrap_err();
        assert!(e.to_string().contains("孤儿"), "无队列记录必须拒绝：{}", e);
    }
}
