//! 运行计划服务：SkillResolver + 文风通道 + 去AI味 → 冻结快照（planHash）。
//!
//! 聊天、Agent、单章起草、自动写作都从这里取「本次实际生效」的技能/文风/去味，
//! 前端预览（task_preview）与执行使用同一函数——预览不是授权凭证，执行时服务端重新解析并冻结。
//! 运行期间只读快照；作者编辑技能不会改变已开始的运行。

use anyhow::Result;
use molan_core::continuity::content_hash;
use molan_core::db::Db;
use molan_core::skill_resolver::{self, Selection};
use molan_core::task_kind::TaskKind;
use serde_json::{json, Value};
use std::path::Path;

fn book_genre(db: &Db, book: &str) -> Option<String> {
    db.q_json(
        "SELECT genre FROM books WHERE id=?1",
        &[&book as &dyn rusqlite::ToSql],
    )
    .ok()?
    .first()?["genre"]
        .as_str()
        .filter(|g| !g.trim().is_empty())
        .map(str::to_string)
}

fn enabled_template(db: &Db, key: &str) -> Option<(String, String)> {
    db.q_json(
        "SELECT name, prompt_template FROM skills WHERE (id=?1 OR builtin_key=?1) AND enabled=1 ORDER BY id=?1 DESC LIMIT 1",
        &[&key as &dyn rusqlite::ToSql],
    )
    .ok()?
    .first()
    .and_then(|r| {
        let t = r["promptTemplate"].as_str()?.to_string();
        (!t.trim().is_empty()).then(|| (r["name"].as_str().unwrap_or("").to_string(), t))
    })
}

/// 按文风键解析文本：返回 {key, label, source, text, contentHash, note}。
/// 与旧 `resolve_book_style` 语义一致，另修正：停用/空模板的文风卡不再生效（回落蒸馏文风并说明）。
pub(crate) fn style_for_key(
    db: &Db,
    root: &Path,
    book: &str,
    genre: Option<&str>,
    key: &str,
) -> Value {
    let distill = || {
        molan_core::books::get_book_style(db, book)
            .as_str()
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
    };
    let key = key.trim();
    let (label, source, text, note): (String, &str, Option<String>, String) = match key {
        "off" => ("不使用文风".into(), "off", None, String::new()),
        "" | "distill" => ("作品蒸馏文风".into(), "distill", distill(), String::new()),
        k if k.starts_with("style:") => match enabled_template(db, &k[6..]) {
            Some((name, t)) => (
                format!("文风卡「{}」", name),
                "skill",
                Some(t),
                String::new(),
            ),
            None => (
                "作品蒸馏文风".into(),
                "distill",
                distill(),
                "所选文风卡不存在、已停用或为空，已回落到作品蒸馏文风".into(),
            ),
        },
        k => {
            let gk = if k == "auto" {
                super::genre_key(genre.unwrap_or(""))
            } else {
                k.to_string()
            };
            match super::prompts(root)[format!("genre__{}", gk)]
                .as_str()
                .filter(|t| !t.trim().is_empty())
            {
                Some(t) => (
                    format!("题材文风（{}）", gk),
                    "genre",
                    Some(t.to_string()),
                    String::new(),
                ),
                None => (
                    "作品蒸馏文风".into(),
                    "distill",
                    distill(),
                    format!("题材文风「{}」不存在，已回落", gk),
                ),
            }
        }
    };
    let text = text.filter(|t| !t.trim().is_empty());
    json!({
        "key": key, "label": label, "source": source,
        "contentHash": text.as_deref().map(content_hash).unwrap_or_default(),
        "text": text.unwrap_or_default(), "note": note,
    })
}

/// 去AI味方法：本次覆盖 > 作品设置 > official:standard；none 关闭。skill:<id> 只接受启用且非空的技能。
pub(crate) fn humanize_for(db: &Db, book: &str, override_method: Option<&str>) -> Value {
    let mut method = override_method.unwrap_or("").trim().to_string();
    let from_override = !method.is_empty();
    if method.is_empty() {
        method = molan_llm::get_setting(db, &format!("book_humanize__{}", book))
            .trim()
            .to_string();
        if method == "null" {
            method.clear();
        }
    }
    if method.is_empty() {
        method = "official:standard".into();
    }
    let (label, text, note) = if method == "none" {
        ("不去AI味".to_string(), String::new(), String::new())
    } else if let Some(id) = method.strip_prefix("skill:") {
        match enabled_template(db, id) {
            Some((name, t)) => (format!("技能「{}」", name), t, String::new()),
            None => {
                method = "official:standard".into();
                let t = enabled_template(db, "method.humanize.standard")
                    .map(|x| x.1)
                    .unwrap_or_default();
                (
                    "官方标准去味".into(),
                    t,
                    "所选去味技能不存在或已停用，已回落官方标准".to_string(),
                )
            }
        }
    } else if method == "official:deep" {
        (
            "官方深度去味".into(),
            enabled_template(db, "method.humanize.deep")
                .map(|x| x.1)
                .unwrap_or_default(),
            String::new(),
        )
    } else {
        (
            "官方标准去味".into(),
            enabled_template(db, "method.humanize.standard")
                .map(|x| x.1)
                .unwrap_or_default(),
            String::new(),
        )
    };
    json!({"method": method, "label": label, "fromOverride": from_override,
           "contentHash": if text.is_empty() { String::new() } else { content_hash(&text) },
           "text": text, "note": note})
}

/// 解析并冻结完整计划（含模板全文）。返回的 JSON 可直接用于生成；`public_view` 给前端/事件。
pub(crate) fn build(
    db: &Db,
    root: &Path,
    book: &str,
    task: TaskKind,
    sel: &Selection,
) -> Result<Value> {
    let mut plan = skill_resolver::resolve(db, book, task, sel);
    let genre = book_genre(db, book);
    let style_key = plan["styleOverride"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| molan_llm::get_setting(db, &format!("book_style__{}", book)));
    let mut style = style_for_key(db, root, book, genre.as_deref(), &style_key);
    style["fromOverride"] = json!(plan["styleOverride"].is_string());
    let humanize = humanize_for(db, book, sel.humanize.as_deref());
    let hash = skill_resolver::plan_hash(book, &plan, &style, &humanize);
    plan["bookId"] = json!(book);
    plan["genre"] = json!(genre);
    plan["style"] = style;
    plan["humanize"] = humanize;
    plan["planHash"] = json!(hash);
    if !book.is_empty() {
        skill_resolver::freeze(db, book, &plan)?;
    }
    Ok(plan)
}

/// 子阶段计划（审稿后重写 = 修改、去AI味 = 去AI味、审稿 = 审稿）：按子阶段自己的任务解析技能，
/// 不继承主任务的显式主/辅技能（正文写作卡不进入去味/审稿），只继承文风与去味覆盖；
/// 叠加本书 DeepWrite 绑定（同样按任务适用性筛选）。同样冻结，planHash 可追溯。
pub(crate) fn stage_plan(
    db: &Db,
    root: &Path,
    book: &str,
    task: TaskKind,
    parent: &Selection,
) -> Value {
    let sel = Selection {
        style: parent.style.clone(),
        humanize: parent.humanize.clone(),
        deepwrite: true,
        ..Default::default()
    };
    build(db, root, book, task, &sel).unwrap_or(Value::Null)
}

/// 旧入口（聊天 / 场景）：按请求参数解析的「去AI味」「审稿」子阶段计划。
pub(crate) fn humanize_stage(db: &Db, root: &Path, book: &str, args: &Value) -> Value {
    stage_plan(
        db,
        root,
        book,
        TaskKind::Humanize,
        &Selection::from_args(args),
    )
}

pub(crate) fn review_stage(db: &Db, root: &Path, book: &str, args: &Value) -> Value {
    stage_plan(
        db,
        root,
        book,
        TaskKind::Review,
        &Selection::from_args(args),
    )
}

/// 子阶段提示中的技能块：已出现在提示里的模板不重复，单个模板最多 4000 字。
pub(crate) fn stage_skills_text(plan: &Value, existing: &str) -> String {
    let mut out = String::new();
    for s in skill_rows(plan) {
        let t: String = s["promptTemplate"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(4000)
            .collect();
        if t.trim().is_empty() || existing.contains(&t) {
            continue;
        }
        if out.is_empty() {
            out.push_str("【本阶段技能】");
        }
        out.push_str(&format!(
            "\n- {}：{}",
            s["name"].as_str().unwrap_or("技能"),
            t
        ));
    }
    out
}

/// 计划中的技能行（build_system 需要 name + promptTemplate）。
pub(crate) fn skill_rows(plan: &Value) -> Vec<Value> {
    plan["skills"].as_array().cloned().unwrap_or_default()
}

pub(crate) fn style_text(plan: &Value) -> Option<String> {
    plan["style"]["text"]
        .as_str()
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
}

pub(crate) fn humanize_method(plan: &Value) -> String {
    plan["humanize"]["method"]
        .as_str()
        .unwrap_or("official:standard")
        .to_string()
}

/// 用冻结计划拼系统提示（文风/技能/去味均取自计划，不再各自读最新设置）。
pub(crate) fn system_prompt(
    db: &Db,
    root: &Path,
    book: &str,
    plan: &Value,
    user_msg: &str,
    context: &str,
) -> String {
    super::build_system_with(
        db,
        root,
        book,
        plan["genre"].as_str(),
        style_text(plan).as_deref(),
        &skill_rows(plan),
        user_msg,
        context,
        &humanize_method(plan),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let id = molan_core::books::create_book(&db, "书", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        (dir, db, id)
    }

    fn skill(db: &Db, id: &str, kind: &str, tpl: &str, enabled: i64) {
        db.exec(
            "INSERT INTO skills(id,name,prompt_template,kind,enabled,usage_mode,origin,targets_json) VALUES(?1,?1,?2,?3,?4,'support','user','[\"body\"]')",
            &[&id as &dyn rusqlite::ToSql, &tpl, &kind, &enabled],
        )
        .unwrap();
    }

    #[test]
    fn disabled_style_card_falls_back_with_note() {
        let (d, db, b) = setup();
        skill(&db, "st", "style", "文风模板", 0);
        let v = style_for_key(&db, d.path(), &b, Some("玄幻"), "style:st");
        assert_eq!(v["source"], "distill");
        assert!(v["note"].as_str().unwrap().contains("已停用"));
        skill(&db, "st2", "style", "文风模板2", 1);
        let v = style_for_key(&db, d.path(), &b, Some("玄幻"), "style:st2");
        assert_eq!(v["source"], "skill");
        assert_eq!(v["text"], "文风模板2");
        assert_eq!(style_for_key(&db, d.path(), &b, None, "off")["text"], "");
    }

    #[test]
    fn humanize_override_and_disabled_skill_fallback() {
        let (_d, db, b) = setup();
        assert_eq!(humanize_for(&db, &b, Some("none"))["method"], "none");
        assert_eq!(humanize_for(&db, &b, None)["method"], "official:standard");
        skill(&db, "h", "user", "去味模板", 0);
        let v = humanize_for(&db, &b, Some("skill:h"));
        assert_eq!(v["method"], "official:standard");
        assert!(v["note"].as_str().unwrap().contains("已停用"));
    }

    #[test]
    fn build_freezes_and_hash_tracks_template_changes() {
        let (d, db, b) = setup();
        skill(&db, "s1", "user", "模板一", 1);
        let sel = Selection {
            supports: vec!["s1".into()],
            humanize: Some("none".into()),
            ..Default::default()
        };
        let p1 = build(&db, d.path(), &b, TaskKind::Body, &sel).unwrap();
        let h1 = p1["planHash"].as_str().unwrap().to_string();
        assert!(skill_resolver::load_frozen(&db, &h1).is_some());
        assert_eq!(skill_rows(&p1)[0]["promptTemplate"], "模板一");
        db.exec(
            "UPDATE skills SET prompt_template='模板二' WHERE id='s1'",
            &[],
        )
        .unwrap();
        let p2 = build(&db, d.path(), &b, TaskKind::Body, &sel).unwrap();
        assert_ne!(p2["planHash"], p1["planHash"]);
        assert_eq!(
            skill_resolver::load_frozen(&db, &h1).unwrap()["skills"][0]["promptTemplate"],
            "模板一"
        );
        let sys = system_prompt(&db, d.path(), &b, &p1, "写", "");
        assert!(sys.contains("【技能：s1】\n模板一"));
        assert!(!sys.contains("模板二"));
    }

    /// 子阶段按自己的任务解析：不继承主任务的正文主技能，保留去味覆盖，叠加 DeepWrite 绑定，并冻结。
    #[test]
    fn stage_plan_resolves_for_its_own_task_and_freezes() {
        let (d, db, b) = setup();
        skill(&db, "main", "user", "正文主卡", 1);
        db.exec(
            "INSERT INTO skills(id,name,prompt_template,kind,enabled,usage_mode,origin,targets_json) VALUES('hz','去味法','去味模板','user',1,'support','user','[\"humanize\"]')",
            &[],
        )
        .unwrap();
        molan_core::deepwrite::bind_skill(&db, &b, "hz", true).unwrap();
        let parent = Selection {
            primary: Some("main".into()),
            humanize: Some("none".into()),
            ..Default::default()
        };
        let p = stage_plan(&db, d.path(), &b, TaskKind::Humanize, &parent);
        let ids: Vec<&str> = p["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["hz"], "正文主卡不进入去味子阶段");
        assert_eq!(p["humanize"]["method"], "none", "去味覆盖沿用主任务选择");
        let frozen =
            molan_core::skill_resolver::load_frozen(&db, p["planHash"].as_str().unwrap()).unwrap();
        assert_eq!(frozen["skills"][0]["promptTemplate"], "去味模板");
        let text = stage_skills_text(&p, "已有提示");
        assert_eq!(text, "【本阶段技能】\n- 去味法：去味模板");
        assert_eq!(
            stage_skills_text(&p, "…去味模板…"),
            "",
            "已在提示中的模板不重复"
        );
    }
}
