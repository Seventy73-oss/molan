//! SkillResolver：本次选择 × 作品默认 × 任务 → 可解释的 SkillPlan（含冻结模板）。
//!
//! 优先级（兼容旧 `effective_skills`，并把隐含规则显式化）：
//! 1. 本次显式主技能（`primarySkillId`）：只要适用本任务，就替代作品主技能——不再依赖技能自身
//!    usage_mode 是否为 primary（旧实现里 standalone 卡被当主技能时两者会同时注入）。
//! 2. 旧式混合列表（`skills`）与本次辅助（`supportSkillIds`）中 usage_mode=primary 的卡视为主技能候选；
//!    同一任务只保留第一个主技能，其余排除并说明（不再随意拼接多个主技能）。
//! 3. 名称自动匹配（仅旧聊天入口）只能叠加辅助，**不能**替代作品主技能。
//! 4. 作品主技能：无本次主技能时生效；被替代时列入排除并注明「本次有效，不改作品默认」。
//! 5. 作品辅助技能：始终叠加。
//! 6. 文风卡（kind=style）属于文风通道：被当作技能选中时改作本次文风覆盖，不重复注入。
//! 7. 每个候选都校验：存在 / 启用 / 模板非空 / targets 适用；不通过即排除并给出原因，绝不静默丢弃。
//!
//! 冻结：`freeze` 把模板全文、rev、content_hash 写入 `skill_plan_snapshot`（按 planHash 去重），
//! 运行期间只读快照；技能被编辑不会改变已开始的运行。

use crate::continuity::content_hash;
use crate::db::Db;
use crate::stats::now_ms;
use crate::task_kind::TaskKind;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

#[derive(Debug, Clone, Default)]
pub struct Selection {
    pub primary: Option<String>,
    pub supports: Vec<String>,
    /// 旧 `args.skills` 混合列表（按 usage_mode 判定角色）。
    pub legacy: Vec<String>,
    /// 旧聊天入口的名称自动匹配结果。
    pub auto_matched: Vec<String>,
    /// 本次文风覆盖（style:<id> / off / distill / auto / 题材 key）。
    pub style: Option<String>,
    /// 本次去AI味覆盖（official:standard / official:deep / skill:<id> / none）。
    pub humanize: Option<String>,
    /// DeepWrite 子阶段（重写 / 去AI味）：叠加本书 DeepWrite 绑定技能，同样校验适用性并去重。
    pub deepwrite: bool,
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| {
                x.as_str()
                    .map(str::to_string)
                    .or_else(|| x["id"].as_str().map(str::to_string))
                    .or_else(|| x["name"].as_str().map(str::to_string))
            })
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) if !s.trim().is_empty() => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

impl Selection {
    /// 从请求参数读取：`skillSelection{primarySkillId,supportSkillIds,styleKey,humanize}` + 旧 `skills` + `humanizeOverride`。
    pub fn from_args(args: &Value) -> Selection {
        let sel = args.get("skillSelection");
        let opt = |v: Option<&Value>| {
            v.and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        Selection {
            primary: opt(sel.and_then(|s| s.get("primarySkillId"))),
            supports: str_list(sel.and_then(|s| s.get("supportSkillIds"))),
            legacy: str_list(args.get("skills")),
            auto_matched: Vec::new(),
            style: opt(sel.and_then(|s| s.get("styleKey"))),
            humanize: opt(sel.and_then(|s| s.get("humanize")))
                .or_else(|| opt(args.get("humanizeOverride"))),
            deepwrite: false,
        }
    }
}

/// 规范化技能行（camelCase；含 rev/contentHash）。
pub fn row(r: &Value) -> Value {
    json!({
        "id": r["id"], "name": r["name"].as_str().unwrap_or(""),
        "description": r["description"].as_str().unwrap_or(""),
        "kind": r["kind"].as_str().unwrap_or("user"),
        "enabled": r["enabled"].as_i64().unwrap_or(1) != 0,
        "builtinKey": r["builtinKey"].as_str().unwrap_or(""),
        "usageMode": r["usageMode"].as_str().unwrap_or("support"),
        "origin": r["origin"].as_str().unwrap_or("user"),
        "promptTemplate": r["promptTemplate"].as_str().unwrap_or(""),
        "targets": serde_json::from_str::<Value>(r["targetsJson"].as_str().unwrap_or("[]")).unwrap_or(json!([])),
        "rev": r["rev"].as_i64().unwrap_or(1),
        "contentHash": r["contentHash"].as_str().unwrap_or(""),
    })
}

/// 技能适用任务（与旧 `skill_targets` 逐条一致）：targets → 内置键 → 官方名 → usage_mode 兜底。
pub fn targets(row: &Value) -> Vec<String> {
    if let Some(a) = row["targets"].as_array() {
        let list: Vec<String> = a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
        if !list.is_empty() {
            return list;
        }
    }
    let name = row["name"].as_str().unwrap_or("");
    let b = row["builtinKey"].as_str().unwrap_or("");
    let one = |t: &str| vec![t.to_string()];
    match b {
        "method.plot" => return one("plot"),
        "method.outline" => return one("outline"),
        "method.body" => return one("body"),
        _ => {}
    }
    if b.starts_with("method.humanize") {
        return one("humanize");
    }
    if b.ends_with("_review") || b == "method.library_audit" {
        return one("review");
    }
    match name {
        "剧情推演" => return one("plot"),
        "小说细纲生成" => return one("outline"),
        "展开正文写作" | "短篇正文" | "短篇节正文生成" => return one("body"),
        "设定一致性检查"
        | "爽点节奏分析"
        | "资料库体检"
        | "开局诊断"
        | "试读反馈"
        | "短篇爆款体检" => return one("review"),
        "场景描写增强" | "对白润色" | "设定提取" | "拆解手法" => {
            return one("body")
        }
        _ => {}
    }
    if name.contains("去AI味") || name.contains("去味") || b.contains("humanize") {
        return one("humanize");
    }
    match row["usageMode"].as_str().unwrap_or("") {
        "primary" => one("body"),
        "support" => vec!["body".into(), "revise".into()],
        _ if row["kind"].as_str() == Some("craft") => vec!["body".into(), "revise".into()],
        _ => one("chat"),
    }
}

pub fn applies(row: &Value, task: TaskKind) -> bool {
    targets(row).iter().any(|t| t == task.id())
}

enum Lookup {
    Ok(Value),
    NotFound,
    Disabled(Value),
    Empty(Value),
}

/// 确定性解析：id 精确命中优先，其次 builtin_key，再次名称；同名按官方优先、rev 新优先、id 排序。
fn lookup(db: &Db, key: &str) -> Lookup {
    let key = key.trim();
    if key.is_empty() {
        return Lookup::NotFound;
    }
    for col in ["id", "builtin_key", "name"] {
        let sql = format!(
            "SELECT * FROM skills WHERE {}=?1 ORDER BY enabled DESC, origin='official' DESC, rev DESC, id LIMIT 1",
            col
        );
        if let Some(r) = db
            .q_json(&sql, &[&key as &dyn rusqlite::ToSql])
            .ok()
            .and_then(|v| v.into_iter().next())
        {
            let r = row(&r);
            if !r["enabled"].as_bool().unwrap_or(true) {
                return Lookup::Disabled(r);
            }
            if r["promptTemplate"].as_str().unwrap_or("").trim().is_empty() {
                return Lookup::Empty(r);
            }
            return Lookup::Ok(r);
        }
    }
    Lookup::NotFound
}

/// 作品对某任务的默认绑定（主技能, 辅助技能）。
pub fn book_bindings(db: &Db, book: &str, task: TaskKind) -> (String, Vec<String>) {
    if book.is_empty() {
        return (String::new(), Vec::new());
    }
    let get = |k: String| {
        db.q_json(
            "SELECT value FROM settings WHERE key=?1",
            &[&k as &dyn rusqlite::ToSql],
        )
        .ok()
        .and_then(|v| {
            v.first()
                .and_then(|r| r["value"].as_str().map(str::to_string))
        })
        .unwrap_or_default()
    };
    let primary = get(format!("book_primary_skill__{}__{}", book, task.id()))
        .trim()
        .to_string();
    let supports = serde_json::from_str::<Value>(&get(format!(
        "book_support_skills__{}__{}",
        book,
        task.id()
    )))
    .ok();
    (primary, str_list(supports.as_ref()))
}

/// 本书 DeepWrite 绑定（启用的绑定，按技能名、id 排序以保证确定性）。表不存在时为空。
fn deepwrite_bindings(db: &Db, book: &str) -> Vec<String> {
    db.q_json(
        "SELECT b.skill_id FROM dw_book_skill b JOIN skills s ON s.id=b.skill_id WHERE b.book_id=?1 AND b.enabled=1 ORDER BY s.name, s.id",
        &[&book as &dyn rusqlite::ToSql],
    )
    .unwrap_or_default()
    .iter()
    .filter_map(|r| r["skillId"].as_str().map(str::to_string))
    .collect()
}

struct Builder<'a> {
    db: &'a Db,
    task: TaskKind,
    skills: Vec<Value>,
    excluded: Vec<Value>,
    notes: Vec<String>,
    style_override: Option<String>,
    has_primary: bool,
}

impl Builder<'_> {
    fn seen(&self, id: &str) -> bool {
        self.skills.iter().any(|s| s["id"].as_str() == Some(id))
    }
    fn exclude(&mut self, id: &str, name: &str, source: &str, code: &str, reason: String) {
        if self
            .excluded
            .iter()
            .any(|e| e["id"].as_str() == Some(id) && e["code"].as_str() == Some(code))
        {
            return;
        }
        self.excluded.push(
            json!({"id": id, "name": name, "source": source, "code": code, "reason": reason}),
        );
    }
    /// 校验候选：返回可用行或记录排除原因。
    fn check(&mut self, key: &str, source: &str) -> Option<Value> {
        match lookup(self.db, key) {
            Lookup::NotFound => {
                self.exclude(
                    key,
                    key,
                    source,
                    "NOT_FOUND",
                    format!("技能「{}」不存在或已删除", key),
                );
                None
            }
            Lookup::Disabled(r) => {
                let n = r["name"].as_str().unwrap_or(key).to_string();
                self.exclude(
                    r["id"].as_str().unwrap_or(key),
                    &n,
                    source,
                    "DISABLED",
                    format!("「{}」已停用", n),
                );
                None
            }
            Lookup::Empty(r) => {
                let n = r["name"].as_str().unwrap_or(key).to_string();
                self.exclude(
                    r["id"].as_str().unwrap_or(key),
                    &n,
                    source,
                    "EMPTY_TEMPLATE",
                    format!("「{}」模板为空", n),
                );
                None
            }
            Lookup::Ok(r) => {
                let (id, n) = (
                    r["id"].as_str().unwrap_or("").to_string(),
                    r["name"].as_str().unwrap_or("").to_string(),
                );
                if r["kind"].as_str() == Some("style") {
                    if self.style_override.is_none() {
                        self.style_override = Some(format!("style:{}", id));
                        self.notes.push(format!(
                            "「{}」是文风卡，已作为本次文风使用（不作为技能重复注入）",
                            n
                        ));
                    } else {
                        self.exclude(
                            &id,
                            &n,
                            source,
                            "STYLE_CHANNEL",
                            format!("「{}」是文风卡；本次已有文风，未重复注入", n),
                        );
                    }
                    return None;
                }
                if !applies(&r, self.task) {
                    let t = targets(&r).join("/");
                    self.exclude(
                        &id,
                        &n,
                        source,
                        "NOT_APPLICABLE",
                        format!(
                            "「{}」适用于 {}，不适用于「{}」任务",
                            n,
                            t,
                            self.task.label()
                        ),
                    );
                    return None;
                }
                if self.seen(&id) {
                    return None;
                }
                Some(r)
            }
        }
    }
    fn push(&mut self, mut r: Value, source: &str, role: &str) {
        r["source"] = json!(source);
        r["role"] = json!(role);
        if role == "primary" {
            self.has_primary = true;
            self.skills.insert(0, r);
        } else {
            self.skills.push(r);
        }
    }
    /// 主技能候选：已有主技能则排除并说明。
    fn primary_candidate(&mut self, r: Value, source: &str) {
        if self.has_primary {
            let first = self.skills[0]["name"].as_str().unwrap_or("").to_string();
            let n = r["name"].as_str().unwrap_or("").to_string();
            self.exclude(
                r["id"].as_str().unwrap_or(""),
                &n,
                source,
                "MULTI_PRIMARY",
                format!(
                    "同一任务只用一个主技能：已采用「{}」，「{}」未注入",
                    first, n
                ),
            );
        } else {
            self.push(r, source, "primary");
        }
    }
}

/// 解析技能计划（只读）。返回 JSON：
/// `{task, taskLabel, skills:[{id,name,kind,usageMode,origin,targets,rev,contentHash,source,role,promptTemplate}],
///   excluded:[{id,name,source,code,reason}], notes:[..], styleOverride, humanizeOverride}`。
pub fn resolve(db: &Db, book: &str, task: TaskKind, sel: &Selection) -> Value {
    let mut b = Builder {
        db,
        task,
        skills: Vec::new(),
        excluded: Vec::new(),
        notes: Vec::new(),
        style_override: sel.style.clone(),
        has_primary: false,
    };
    if let Some(p) = sel.primary.as_deref() {
        if let Some(r) = b.check(p, "explicit") {
            b.push(r, "explicit", "primary");
        }
    }
    for (list, source) in [(&sel.legacy, "explicit"), (&sel.supports, "explicit")] {
        for k in list.iter() {
            if let Some(r) = b.check(k, source) {
                if r["usageMode"].as_str() == Some("primary") {
                    b.primary_candidate(r, source);
                } else {
                    b.push(r, source, "support");
                }
            }
        }
    }
    let explicit_primary = b.has_primary;
    let (book_primary, book_supports) = book_bindings(db, book, task);
    for k in sel.auto_matched.iter() {
        if let Some(r) = b.check(k, "auto_match") {
            let is_primary = r["usageMode"].as_str() == Some("primary");
            if is_primary && (b.has_primary || !book_primary.is_empty()) {
                let n = r["name"].as_str().unwrap_or("").to_string();
                b.exclude(
                    r["id"].as_str().unwrap_or(""),
                    &n,
                    "auto_match",
                    "AUTO_MATCH_NO_REPLACE",
                    format!("按名称识别到「{}」，自动匹配不替代主技能", n),
                );
            } else if is_primary {
                b.push(r, "auto_match", "primary");
            } else {
                b.push(r, "auto_match", "support");
            }
        }
    }
    if !book_primary.is_empty() {
        if explicit_primary {
            let n = match lookup(db, &book_primary) {
                Lookup::Ok(r) | Lookup::Disabled(r) | Lookup::Empty(r) => {
                    r["name"].as_str().unwrap_or("").to_string()
                }
                Lookup::NotFound => book_primary.clone(),
            };
            let cur = b.skills[0]["name"].as_str().unwrap_or("").to_string();
            b.exclude(
                &book_primary,
                &n,
                "book_primary",
                "REPLACED",
                format!(
                    "作品主技能「{}」本次由「{}」替代（只影响本次，不改作品默认）",
                    n, cur
                ),
            );
        } else if let Some(r) = b.check(&book_primary, "book_primary") {
            if b.has_primary {
                b.primary_candidate(r, "book_primary");
            } else {
                b.push(r, "book_primary", "primary");
            }
        }
    }
    for k in book_supports.iter() {
        if let Some(r) = b.check(k, "book_support") {
            b.push(r, "book_support", "support");
        }
    }
    if sel.deepwrite {
        for k in deepwrite_bindings(db, book) {
            if let Some(r) = b.check(&k, "deepwrite") {
                b.push(r, "deepwrite", "support");
            }
        }
    }
    json!({
        "task": task.id(), "taskLabel": task.label(), "skills": b.skills,
        "excluded": b.excluded, "notes": b.notes,
        "styleOverride": b.style_override, "humanizeOverride": sel.humanize,
    })
}

/// 计划哈希：只取影响注入结果的字段（模板以 content_hash 代表）。
pub fn plan_hash(book: &str, plan: &Value, style: &Value, humanize: &Value) -> String {
    let skills: Vec<Value> = plan["skills"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|s| json!({"id": s["id"], "rev": s["rev"], "contentHash": content_hash(s["promptTemplate"].as_str().unwrap_or("")), "role": s["role"]}))
                .collect()
        })
        .unwrap_or_default();
    content_hash(
        &json!({"bookId": book, "task": plan["task"], "skills": skills,
            "style": {"key": style["key"], "contentHash": style["contentHash"]},
            "humanize": {"method": humanize["method"], "contentHash": humanize["contentHash"]}})
        .to_string(),
    )
}

pub fn ensure_schema(db: &Db) -> Result<()> {
    let conn = db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS skill_plan_snapshot (
            plan_hash TEXT PRIMARY KEY,
            book_id TEXT NOT NULL,
            task TEXT NOT NULL,
            plan_json TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );",
    )?;
    Ok(())
}

/// 冻结完整计划（含模板全文、文风文本、去味提示词）。同 planHash 幂等。返回 planHash。
pub fn freeze(db: &Db, book: &str, full_plan: &Value) -> Result<String> {
    let hash = full_plan["planHash"]
        .as_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| anyhow!("计划缺少 planHash"))?
        .to_string();
    db.exec(
        "INSERT OR IGNORE INTO skill_plan_snapshot(plan_hash,book_id,task,plan_json,created_at) VALUES(?1,?2,?3,?4,?5)",
        &[
            &hash as &dyn rusqlite::ToSql,
            &book,
            &full_plan["task"].as_str().unwrap_or(""),
            &full_plan.to_string(),
            &now_ms(),
        ],
    )?;
    Ok(hash)
}

pub fn load_frozen(db: &Db, plan_hash: &str) -> Option<Value> {
    db.q_json(
        "SELECT plan_json FROM skill_plan_snapshot WHERE plan_hash=?1",
        &[&plan_hash as &dyn rusqlite::ToSql],
    )
    .ok()?
    .first()
    .and_then(|r| serde_json::from_str(r["planJson"].as_str()?).ok())
}

/// 去掉模板全文的展示视图（前端预览 / 事件 / 回执）。
pub fn public_view(full_plan: &Value) -> Value {
    let mut v = full_plan.clone();
    if let Some(a) = v["skills"].as_array_mut() {
        for s in a.iter_mut() {
            if let Some(o) = s.as_object_mut() {
                let chars = o
                    .get("promptTemplate")
                    .and_then(Value::as_str)
                    .map(|t| t.chars().count())
                    .unwrap_or(0);
                o.remove("promptTemplate");
                o.insert("templateChars".into(), json!(chars));
            }
        }
    }
    for k in ["style", "humanize"] {
        if let Some(o) = v[k].as_object_mut() {
            let chars = o
                .get("text")
                .and_then(Value::as_str)
                .map(|t| t.chars().count())
                .unwrap_or(0);
            o.remove("text");
            o.insert("chars".into(), json!(chars));
        }
    }
    v
}

/// 确定性推荐（不调用模型）：只推荐真实存在、启用、模板非空且适用的技能，并说明原因。
pub fn recommend(db: &Db, book: &str, task: TaskKind, genre: &str) -> Value {
    let (primary, supports) = book_bindings(db, book, task);
    let mut out: Vec<Value> = Vec::new();
    let mut add = |r: Value, action: &str, reason: String| {
        if out.iter().any(|x| x["skillId"] == r["id"]) {
            return;
        }
        out.push(json!({"skillId": r["id"], "name": r["name"], "kind": r["kind"], "action": action, "reason": reason}));
    };
    let official = match task {
        TaskKind::Plot => Some("method.plot"),
        TaskKind::Outline => Some("method.outline"),
        TaskKind::Body => Some("method.body"),
        TaskKind::Humanize => Some("method.humanize.standard"),
        TaskKind::Review => Some("method.consistency_review"),
        _ => None,
    };
    if primary.is_empty() {
        if let Some(Lookup::Ok(r)) = official.map(|k| lookup(db, k)) {
            if applies(&r, task) {
                let n = r["name"].as_str().unwrap_or("").to_string();
                add(
                    r,
                    "set_primary",
                    format!(
                        "本书「{}」任务尚未设置主技能，官方写法「{}」可作为本次主技能",
                        task.label(),
                        n
                    ),
                );
            }
        }
    }
    let g = genre.trim();
    if !g.is_empty()
        && matches!(
            task,
            TaskKind::Plot | TaskKind::Outline | TaskKind::Body | TaskKind::Revise
        )
    {
        if let Ok(rows) = db.q_json(
            "SELECT * FROM skills WHERE enabled=1 AND kind='craft' AND name LIKE ?1 AND trim(prompt_template)<>''",
            &[&format!("%{}%", g) as &dyn rusqlite::ToSql],
        ) {
            for r in rows.iter().map(row) {
                let id = r["id"].as_str().unwrap_or("").to_string();
                if !supports.contains(&id) {
                    let n = r["name"].as_str().unwrap_or("").to_string();
                    add(r, "add_support", format!("作品题材为「{}」，题材库「{}」可补充类型套路（辅助叠加，不替代主技能）", g, n));
                }
            }
        }
    }
    json!(out)
}

#[cfg(test)]
#[path = "skill_resolver_tests.rs"]
mod tests;
