//! 阶段技能计划（T4a）：作品×阶段×本次选择 → StagePlan + plan_hash。
//!
//! 只读：不改任何表、不写文件。技能快照冻结当前 `rev` 与 `content_hash`，
//! 因此运行中改动技能不会让已计算的 plan 悄悄变形；同一输入 planHash 稳定，
//! 换卡版本（bump_revision）后 planHash 随之改变。
//!
//! targets 语义：bundle 导入 / 草稿转技能必须显式给出已知任务名数组，
//! 非法即整条拒绝（不静默丢弃）。`kind=style` 的拆书/蒸馏卡仍走文风通道
//! `book_style__<book>=style:<id>`，其 targets 不参与任务路由，行为不变。
use super::{book_task_skill_ids, effective_skills, resolve_skill_ref, skill_targets};
use anyhow::{anyhow, Result};
use molan_core::continuity::content_hash;
use molan_core::db::Db;
use serde_json::{json, Value};

/// 已知任务名（与 agent_tools::SKILL_TASKS / effective_skills 路由一致）。
pub(crate) const SKILL_TASKS: [&str; 9] = molan_core::task_kind::IDS;

pub(crate) fn valid_task(task: &str) -> bool {
    SKILL_TASKS.contains(&task)
}

fn unknown_task(task: &str) -> anyhow::Error {
    anyhow!("未知任务 {:?}（允许：{}）", task, SKILL_TASKS.join("/"))
}

/// 校验 targets：必须是已知任务名数组。返回 (任务名列表, 是否应用了缺省)。
/// 缺省（未提供 / null / 空数组）→ default_targets 且 defaultApplied=true；
/// 任一元素非字符串或不是已知任务名 → 整条拒绝并报错，绝不静默丢弃。
pub(crate) fn parse_targets(v: &Value, default_targets: &[&str]) -> Result<(Vec<String>, bool)> {
    let defaulted = || {
        (
            default_targets
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            true,
        )
    };
    if v.is_null() {
        return Ok(defaulted());
    }
    let arr = v.as_array().ok_or_else(|| anyhow!("targets 必须是数组"))?;
    if arr.is_empty() {
        return Ok(defaulted());
    }
    let mut out = Vec::with_capacity(arr.len());
    for x in arr {
        let t = x.as_str().ok_or_else(|| anyhow!("targets 含非字符串项"))?;
        if !valid_task(t) {
            return Err(unknown_task(t));
        }
        out.push(t.to_string());
    }
    Ok((out, false))
}

/// 读技能当前版本号与内容 hash（缺列/缺行时安全降级）。
fn skill_version(db: &Db, id: &str) -> (i64, String) {
    db.q_json(
        "SELECT rev, content_hash FROM skills WHERE id=?1",
        &[&id as &dyn rusqlite::ToSql],
    )
    .ok()
    .and_then(|v| v.into_iter().next())
    .map(|r| {
        (
            r["rev"].as_i64().unwrap_or(1),
            r["contentHash"].as_str().unwrap_or("").to_string(),
        )
    })
    .unwrap_or((0, String::new()))
}

/// 文风快照：key 为 book_style__<book> 原值；style:<id> 时带来源技能版本，
/// 其余（distill/auto/题材/off）回落到实际生效文本的 hash，便于 planHash 感知变化。
fn style_snapshot(db: &Db, root: &std::path::Path, book: &str) -> Value {
    let key = molan_llm::get_setting(db, &format!("book_style__{}", book))
        .trim()
        .to_string();
    let source = key.strip_prefix("style:").unwrap_or("").to_string();
    let fallback_hash = || {
        let text = super::auto_write::resolve_book_style(db, root, book, None).unwrap_or_default();
        if text.trim().is_empty() {
            String::new()
        } else {
            content_hash(&text)
        }
    };
    let (rev, ch) = if source.is_empty() {
        (0, fallback_hash())
    } else {
        let (r, h) = skill_version(db, &source);
        if h.is_empty() {
            (r, fallback_hash())
        } else {
            (r, h)
        }
    };
    json!({"key": key, "sourceSkillId": source, "rev": rev, "contentHash": ch})
}

/// 解析阶段计划（只读）。reason 标注每项入选原因：
/// explicit（本轮显式勾选）/ book-primary（书级主技能）/ book-support（书级辅助技能）。
pub(crate) fn plan_stage(
    db: &Db,
    root: &std::path::Path,
    book: &str,
    task: &str,
    explicit: &[Value],
) -> Result<Value> {
    if book.is_empty() {
        return Err(anyhow!("缺少 bookId"));
    }
    if !valid_task(task) {
        return Err(unknown_task(task));
    }
    // 既有解析器：显式 + 书级绑定合并、按任务过滤（不改它们）
    let list = effective_skills(db, book, task, explicit);
    // 显式解析出的 id 集合（用于 reason 标注；名称/内置键也能解析成 id）
    let explicit_ids: Vec<String> = explicit
        .iter()
        .filter_map(|v| {
            let key = v
                .as_str()
                .map(|s| s.to_string())
                .or_else(|| v["id"].as_str().map(|s| s.to_string()))
                .or_else(|| v["name"].as_str().map(|s| s.to_string()))
                .unwrap_or_default();
            resolve_skill_ref(db, &key).and_then(|r| r["id"].as_str().map(|s| s.to_string()))
        })
        .collect();
    let (primary, supports) = book_task_skill_ids(db, book, task);
    let mut skills: Vec<Value> = Vec::with_capacity(list.len());
    for row in &list {
        let id = row["id"].as_str().unwrap_or("");
        let reason = if explicit_ids.iter().any(|x| x == id) {
            "explicit"
        } else if !primary.is_empty() && primary == id {
            "book-primary"
        } else if supports.iter().any(|x| x == id) {
            "book-support"
        } else {
            // 既非显式也非书级绑定：仍属本轮实际生效，按显式来源标注
            "explicit"
        };
        let (rev, ch) = skill_version(db, id);
        skills.push(json!({
            "id": id,
            "name": row["name"].as_str().unwrap_or(""),
            "kind": row["kind"].as_str().unwrap_or(""),
            "origin": row["origin"].as_str().unwrap_or(""),
            "usageMode": row["usageMode"].as_str().unwrap_or(""),
            "targets": skill_targets(row),
            "rev": rev,
            "contentHash": ch,
            "reason": reason,
        }));
    }
    let style = style_snapshot(db, root, book);
    let (humanize, _) = super::auto_write::resolve_humanize(db, book, None);
    // 规范化 planHash 输入：只取影响注入结果的字段，键序由 serde_json 排序保证稳定
    let canonical = json!({
        "bookId": book,
        "task": task,
        "skills": skills.iter().map(|s| json!({
            "id": s["id"], "rev": s["rev"], "contentHash": s["contentHash"],
            "targets": s["targets"], "usageMode": s["usageMode"], "reason": s["reason"],
        })).collect::<Vec<_>>(),
        "style": {"key": style["key"], "sourceSkillId": style["sourceSkillId"],
                  "rev": style["rev"], "contentHash": style["contentHash"]},
        "humanize": {"method": humanize},
    });
    let plan_hash = content_hash(&canonical.to_string());
    Ok(json!({
        "bookId": book, "task": task, "skills": skills,
        "style": style, "humanize": {"method": humanize}, "planHash": plan_hash,
    }))
}

/// IPC 入参包装：bookId/task 校验 + 可选本轮 skills。
pub(crate) fn plan_stage_args(db: &Db, root: &std::path::Path, args: &Value) -> Result<Value> {
    let book = args.get("bookId").and_then(|v| v.as_str()).unwrap_or("");
    if book.is_empty() {
        return Err(anyhow!("缺少 bookId"));
    }
    let task = args.get("task").and_then(|v| v.as_str()).unwrap_or("");
    let explicit: Vec<Value> = args
        .get("skills")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    plan_stage(db, root, book, task, &explicit)
}

/// 草稿转技能：接受 args.targets（校验为已知任务名），缺省 ["chat"] 并回报 defaultApplied。
/// 返回 (技能 id, 是否应用缺省 targets)。
pub(crate) fn create_from_draft(db: &Db, args: &Value) -> Result<(String, bool)> {
    let s = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let id = uuid::Uuid::new_v4().to_string();
    let name = {
        let n = s("name");
        if n.is_empty() {
            "新技能".to_string()
        } else {
            n
        }
    };
    let (targets, defaulted) = parse_targets(&args["targets"], &["chat"])?;
    let targets_s = serde_json::to_string(&targets).unwrap_or_default();
    let desc: String = s("description").chars().take(200).collect();
    let draft = s("draft");
    db.exec(
        "INSERT INTO skills(id,name,description,prompt_template,kind,source,enabled,builtin_key,usage_mode,origin,targets_json) VALUES(?1,?2,?3,?4,'user','',1,NULL,'standalone','user',?5)",
        &[
            &id as &dyn rusqlite::ToSql,
            &name,
            &desc,
            &draft,
            &targets_s,
        ],
    )?;
    molan_core::skill_rev::init_revision(db, &id, "create_skill_from_draft")?;
    Ok((id, defaulted))
}

/// 安装技能包：整批先校验 targets，任一非法即整条拒绝（不做部分导入）。
/// 非 style 卡透传 bundle 的 targets（缺省 ["chat"]）；style 卡走文风通道，targets 留空。
pub(crate) fn install_bundle(db: &Db, j: &Value) -> Result<Value> {
    let skills = j["skills"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| vec![j.clone()]);
    struct Pending {
        name: String,
        desc: String,
        tpl: String,
        kind: String,
        targets_s: Option<String>,
    }
    let mut pending: Vec<Pending> = Vec::new();
    for sk in &skills {
        let name = sk["name"].as_str().unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let kind = sk["kind"].as_str().unwrap_or("imported").to_string();
        // style 卡不进任务路由（book_style__<book>=style:<id>），保持原行为：targets 留空
        let targets_s = if kind == "style" {
            None
        } else {
            let (targets, _) = parse_targets(&sk["targets"], &["chat"])?;
            Some(serde_json::to_string(&targets).unwrap_or_default())
        };
        pending.push(Pending {
            name: name.to_string(),
            desc: sk["description"].as_str().unwrap_or("").to_string(),
            tpl: sk["promptTemplate"]
                .as_str()
                .or_else(|| sk["prompt_template"].as_str())
                .unwrap_or("")
                .to_string(),
            kind,
            targets_s,
        });
    }
    let mut added = 0;
    for p in pending {
        let id = uuid::Uuid::new_v4().to_string();
        db.exec(
            "INSERT INTO skills(id,name,description,prompt_template,kind,source,enabled,builtin_key,usage_mode,origin,targets_json) VALUES(?1,?2,?3,?4,?5,'',1,NULL,'standalone','imported',?6)",
            &[
                &id as &dyn rusqlite::ToSql,
                &p.name,
                &p.desc,
                &p.tpl,
                &p.kind,
                &p.targets_s,
            ],
        )?;
        molan_core::skill_rev::init_revision(db, &id, "install_skill_bundle")?;
        added += 1;
    }
    Ok(json!({"ok": true, "added": added}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        (dir, db)
    }

    fn add_skill(db: &Db, name: &str, targets: &str, usage: &str, tpl: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        db.exec(
            "INSERT INTO skills(id,name,description,prompt_template,kind,source,enabled,builtin_key,usage_mode,targets_json,origin)
             VALUES(?1,?2,'',?3,'craft','',1,NULL,?4,?5,'user')",
            &[
                &id as &dyn rusqlite::ToSql,
                &name,
                &tpl,
                &usage,
                &targets,
            ],
        )
        .unwrap();
        id
    }

    fn set(db: &Db, key: &str, val: &str) {
        db.exec(
            "INSERT INTO settings(key,value) VALUES(?1,?2)",
            &[&key as &dyn rusqlite::ToSql, &val],
        )
        .unwrap();
    }

    #[test]
    fn targets_three_branches_validate_or_default() {
        // 透传：合法数组原样保留
        let (got, def) = parse_targets(&json!(["body", "outline"]), &["chat"]).unwrap();
        assert_eq!(got, vec!["body", "outline"]);
        assert!(!def, "显式 targets 不得标 defaultApplied");
        // 缺省：未提供 / null / 空数组 → 缺省值 + defaultApplied
        for v in [Value::Null, json!([])] {
            let (got, def) = parse_targets(&v, &["chat"]).unwrap();
            assert_eq!(got, vec!["chat"]);
            assert!(def, "缺省必须标 defaultApplied：{:?}", v);
        }
        // 拒绝：未知任务名 / 非数组 / 非字符串项
        assert!(parse_targets(&json!(["body", "hack"]), &["chat"]).is_err());
        assert!(parse_targets(&json!("body"), &["chat"]).is_err());
        assert!(parse_targets(&json!(["body", 3]), &["chat"]).is_err());
    }

    #[test]
    fn create_from_draft_stores_targets_and_reports_default() {
        let (_d, db) = db();
        // 显式 targets 透传
        let (id, def) = create_from_draft(
            &db,
            &json!({"name": "细纲卡", "draft": "TPL", "targets": ["outline"]}),
        )
        .unwrap();
        assert!(!def);
        let row = &db
            .q_json(
                "SELECT targets_json, rev, content_hash FROM skills WHERE id=?1",
                &[&id as &dyn rusqlite::ToSql],
            )
            .unwrap()[0];
        assert_eq!(row["targetsJson"], "[\"outline\"]");
        assert_eq!(row["rev"], 1);
        assert_eq!(row["contentHash"], content_hash("TPL"));
        // 缺省 → ["chat"] + defaultApplied
        let (id2, def2) = create_from_draft(&db, &json!({"name": "聊天卡", "draft": "C"})).unwrap();
        assert!(def2, "缺省 targets 必须回报 defaultApplied");
        let row2 = &db
            .q_json(
                "SELECT targets_json FROM skills WHERE id=?1",
                &[&id2 as &dyn rusqlite::ToSql],
            )
            .unwrap()[0];
        assert_eq!(row2["targetsJson"], "[\"chat\"]");
        // 非法 → 整条拒绝且不落库
        assert!(create_from_draft(&db, &json!({"draft": "X", "targets": ["nope"]})).is_err());
    }

    #[test]
    fn install_bundle_passes_targets_and_rejects_illegal_atomically() {
        let (_d, db) = db();
        let bundle = json!({
            "kind": "writerx-skill-bundle",
            "skills": [
                {"name": "正文卡", "promptTemplate": "B", "targets": ["body"]},
                {"name": "缺省卡", "promptTemplate": "C"},
                {"name": "文风卡", "promptTemplate": "S", "kind": "style"},
            ],
        });
        let out = install_bundle(&db, &bundle).unwrap();
        assert_eq!(out["added"], 3);
        let body = &db
            .q_json("SELECT targets_json FROM skills WHERE name='正文卡'", &[])
            .unwrap()[0];
        assert_eq!(body["targetsJson"], "[\"body\"]", "bundle targets 必须透传");
        let dflt = &db
            .q_json("SELECT targets_json FROM skills WHERE name='缺省卡'", &[])
            .unwrap()[0];
        assert_eq!(dflt["targetsJson"], "[\"chat\"]");
        let style = &db
            .q_json("SELECT targets_json FROM skills WHERE name='文风卡'", &[])
            .unwrap()[0];
        assert!(
            style["targetsJson"].is_null(),
            "style 卡 targets 留空，行为不变"
        );
        // 非法 targets：整条拒绝，且不部分导入
        let bad = json!({"skills": [
            {"name": "合法", "promptTemplate": "A", "targets": ["body"]},
            {"name": "非法", "promptTemplate": "B", "targets": ["bogus"]},
        ]});
        assert!(install_bundle(&db, &bad).is_err());
        let cnt = db
            .q_json("SELECT COUNT(*) AS n FROM skills WHERE name='合法'", &[])
            .unwrap()[0]["n"]
            .as_i64()
            .unwrap();
        assert_eq!(cnt, 0, "整条拒绝时不得留下部分导入");
    }

    /// 蒸馏落库（decompose 既有路径）必须写 skill_revision：首次建 v1，
    /// 同名 style 卡再次蒸馏 → 归档旧模板并自增 rev。
    #[test]
    fn distill_style_card_writes_revision_history() {
        let (_d, db) = db();
        let save = |tpl: &str| {
            super::super::decompose::save_style_card_labeled(&db, "拆解卡", "某书", tpl, "蒸馏")
                .unwrap()
        };
        let id = save("第一版模板");
        let v1 = molan_core::skill_rev::revisions(&db, &id).unwrap();
        assert_eq!(v1.len(), 1, "首次蒸馏应落 v1：{:?}", v1);
        assert_eq!(v1[0]["rev"], 1);
        assert_eq!(v1[0]["promptTemplate"], "第一版模板");
        // 再次蒸馏同一张 style 卡 → 复用 id，归档旧模板并 rev+1
        let again = save("第二版模板");
        assert_eq!(id, again, "同名 style 卡必须复用同一 id");
        let v2 = molan_core::skill_rev::revisions(&db, &id).unwrap();
        assert_eq!(v2.len(), 2, "旧模板必须可取回：{:?}", v2);
        assert_eq!(v2[0]["promptTemplate"], "第一版模板");
        assert_eq!(v2[1]["promptTemplate"], "第二版模板");
        let row = &db
            .q_json(
                "SELECT rev, content_hash FROM skills WHERE id=?1",
                &[&id as &dyn rusqlite::ToSql],
            )
            .unwrap()[0];
        assert_eq!(row["rev"], 2);
        assert_eq!(row["contentHash"], content_hash("第二版模板"));
    }

    #[test]
    fn plan_stage_labels_reason_and_is_stable() {
        let (_d, db) = db();
        let root = std::path::Path::new(".");
        let book = molan_core::books::create_book(&db, "t", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        // 显式卡用 support：显式 primary 会按 N01 抑制书级主技能，那就测不到 book-primary
        let explicit = add_skill(&db, "本轮卡", "[\"body\"]", "support", "E");
        let primary = add_skill(&db, "书级主卡", "[\"body\"]", "primary", "P");
        let support = add_skill(&db, "书级辅卡", "[\"body\"]", "support", "S");
        set(
            &db,
            &format!("book_primary_skill__{}__body", book),
            &primary,
        );
        set(
            &db,
            &format!("book_support_skills__{}__body", book),
            &format!("[\"{}\"]", support),
        );
        let plan = plan_stage(&db, root, &book, "body", &[json!(explicit)]).unwrap();
        assert_eq!(plan["task"], "body");
        let by_id = |id: &str| -> Value {
            plan["skills"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["id"] == id)
                .cloned()
                .unwrap_or(Value::Null)
        };
        assert_eq!(by_id(&explicit)["reason"], "explicit", "显式卡标 explicit");
        assert_eq!(by_id(&primary)["reason"], "book-primary");
        assert_eq!(by_id(&support)["reason"], "book-support");
        // 同输入 planHash 稳定
        let again = plan_stage(&db, root, &book, "body", &[json!(explicit)]).unwrap();
        assert_eq!(plan["planHash"], again["planHash"]);
        // 换卡版本 → planHash 变化
        db.exec(
            "UPDATE skills SET prompt_template='E2' WHERE id=?1",
            &[&explicit as &dyn rusqlite::ToSql],
        )
        .unwrap();
        molan_core::skill_rev::bump_revision(&db, &explicit, "update").unwrap();
        let changed = plan_stage(&db, root, &book, "body", &[json!(explicit)]).unwrap();
        assert_ne!(
            plan["planHash"], changed["planHash"],
            "换卡版本必须改变 planHash"
        );
        // 未知 task 拒绝
        assert!(plan_stage(&db, root, &book, "hack", &[]).is_err());
        assert!(plan_stage(&db, root, &book, "", &[]).is_err());
    }
}
