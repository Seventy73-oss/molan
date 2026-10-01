use super::*;

fn db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), None).unwrap();
    (dir, db)
}

#[allow(clippy::too_many_arguments)]
fn skill(
    db: &Db,
    id: &str,
    name: &str,
    kind: &str,
    usage: &str,
    targets: &str,
    tpl: &str,
    enabled: i64,
) {
    db.exec(
        "INSERT INTO skills(id,name,description,prompt_template,kind,source,enabled,builtin_key,usage_mode,origin,targets_json) VALUES(?1,?2,'',?3,?4,'',?5,NULL,?6,'user',?7)",
        &[&id as &dyn rusqlite::ToSql, &name, &tpl, &kind, &enabled, &usage, &targets],
    )
    .unwrap();
}

fn set(db: &Db, k: &str, v: &str) {
    db.exec(
        "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        &[&k as &dyn rusqlite::ToSql, &v],
    )
    .unwrap();
}

fn ids(plan: &Value) -> Vec<String> {
    plan["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect()
}

fn codes(plan: &Value) -> Vec<String> {
    plan["excluded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            format!(
                "{}:{}",
                s["id"].as_str().unwrap(),
                s["code"].as_str().unwrap()
            )
        })
        .collect()
}

fn base(db: &Db) {
    skill(
        db,
        "bp",
        "作品正文主卡",
        "user",
        "primary",
        r#"["body"]"#,
        "主卡模板",
        1,
    );
    skill(
        db,
        "bs",
        "作品辅助",
        "user",
        "support",
        r#"["body","revise"]"#,
        "辅助模板",
        1,
    );
    set(db, "book_primary_skill__B__body", "bp");
    set(db, "book_support_skills__B__body", r#"["bs"]"#);
}

#[test]
fn book_defaults_apply_without_selection() {
    let (_d, db) = db();
    base(&db);
    let p = resolve(&db, "B", TaskKind::Body, &Selection::default());
    assert_eq!(ids(&p), vec!["bp", "bs"]);
    assert_eq!(p["skills"][0]["role"], "primary");
    assert_eq!(p["skills"][0]["source"], "book_primary");
    assert!(p["excluded"].as_array().unwrap().is_empty());
}

#[test]
fn explicit_standalone_primary_replaces_book_primary() {
    let (_d, db) = db();
    base(&db);
    skill(
        &db,
        "x",
        "本次写法",
        "user",
        "standalone",
        r#"["body"]"#,
        "本次模板",
        1,
    );
    let sel = Selection {
        primary: Some("x".into()),
        ..Default::default()
    };
    let p = resolve(&db, "B", TaskKind::Body, &sel);
    assert_eq!(ids(&p), vec!["x", "bs"], "辅助仍叠加，作品主技能被替代");
    assert_eq!(codes(&p), vec!["bp:REPLACED"]);
    assert!(p["excluded"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("不改作品默认"));
    // 作品默认未被修改
    assert_eq!(book_bindings(&db, "B", TaskKind::Body).0, "bp");
}

#[test]
fn support_only_selection_keeps_book_primary() {
    let (_d, db) = db();
    base(&db);
    skill(
        &db,
        "s2",
        "本次辅助",
        "user",
        "support",
        r#"["body"]"#,
        "t",
        1,
    );
    let sel = Selection {
        supports: vec!["s2".into()],
        ..Default::default()
    };
    let p = resolve(&db, "B", TaskKind::Body, &sel);
    assert_eq!(ids(&p), vec!["bp", "s2", "bs"]);
}

#[test]
fn legacy_primary_mode_skill_still_suppresses_book_primary() {
    let (_d, db) = db();
    base(&db);
    skill(
        &db,
        "lp",
        "旧式主卡",
        "user",
        "primary",
        r#"["body"]"#,
        "t",
        1,
    );
    let sel = Selection {
        legacy: vec!["lp".into()],
        ..Default::default()
    };
    let p = resolve(&db, "B", TaskKind::Body, &sel);
    assert_eq!(ids(&p), vec!["lp", "bs"]);
    assert_eq!(codes(&p), vec!["bp:REPLACED"]);
}

#[test]
fn multiple_primaries_keep_first_and_explain() {
    let (_d, db) = db();
    skill(&db, "p1", "主一", "user", "primary", r#"["body"]"#, "t", 1);
    skill(&db, "p2", "主二", "user", "primary", r#"["body"]"#, "t", 1);
    let sel = Selection {
        legacy: vec!["p1".into(), "p2".into()],
        ..Default::default()
    };
    let p = resolve(&db, "B", TaskKind::Body, &sel);
    assert_eq!(ids(&p), vec!["p1"]);
    assert_eq!(codes(&p), vec!["p2:MULTI_PRIMARY"]);
}

#[test]
fn invalid_candidates_are_excluded_with_reasons() {
    let (_d, db) = db();
    skill(
        &db,
        "off",
        "停用卡",
        "user",
        "support",
        r#"["body"]"#,
        "t",
        0,
    );
    skill(
        &db,
        "empty",
        "空卡",
        "user",
        "support",
        r#"["body"]"#,
        "  ",
        1,
    );
    skill(
        &db,
        "rev",
        "审稿卡",
        "user",
        "primary",
        r#"["review"]"#,
        "t",
        1,
    );
    let sel = Selection {
        supports: vec!["off".into(), "empty".into(), "rev".into(), "ghost".into()],
        ..Default::default()
    };
    let p = resolve(&db, "B", TaskKind::Body, &sel);
    assert!(ids(&p).is_empty());
    assert_eq!(
        codes(&p),
        vec![
            "off:DISABLED",
            "empty:EMPTY_TEMPLATE",
            "rev:NOT_APPLICABLE",
            "ghost:NOT_FOUND"
        ]
    );
    assert!(p["excluded"][2]["reason"]
        .as_str()
        .unwrap()
        .contains("不适用于「正文」"));
}

#[test]
fn style_card_goes_to_style_channel_not_skills() {
    let (_d, db) = db();
    skill(&db, "st", "某文风", "style", "support", "[]", "文风模板", 1);
    skill(
        &db,
        "st2",
        "另一文风",
        "style",
        "support",
        "[]",
        "文风模板2",
        1,
    );
    let sel = Selection {
        supports: vec!["st".into(), "st2".into()],
        ..Default::default()
    };
    let p = resolve(&db, "B", TaskKind::Body, &sel);
    assert!(ids(&p).is_empty());
    assert_eq!(p["styleOverride"], "style:st");
    assert_eq!(codes(&p), vec!["st2:STYLE_CHANNEL"]);
    assert_eq!(p["notes"].as_array().unwrap().len(), 1);
}

#[test]
fn auto_match_cannot_replace_book_primary() {
    let (_d, db) = db();
    base(&db);
    skill(
        &db,
        "am",
        "自动主卡",
        "user",
        "primary",
        r#"["body"]"#,
        "t",
        1,
    );
    let sel = Selection {
        auto_matched: vec!["am".into()],
        ..Default::default()
    };
    let p = resolve(&db, "B", TaskKind::Body, &sel);
    assert_eq!(ids(&p), vec!["bp", "bs"]);
    assert_eq!(codes(&p), vec!["am:AUTO_MATCH_NO_REPLACE"]);
}

#[test]
fn duplicates_are_deduped_and_lookup_is_deterministic() {
    let (_d, db) = db();
    base(&db);
    let sel = Selection {
        supports: vec!["bs".into(), "作品辅助".into()],
        ..Default::default()
    };
    let p = resolve(&db, "B", TaskKind::Body, &sel);
    assert_eq!(ids(&p), vec!["bp", "bs"]);
    assert_eq!(p["skills"][1]["source"], "explicit");
}

#[test]
fn freeze_roundtrip_and_public_view_hides_templates() {
    let (_d, db) = db();
    base(&db);
    let p = resolve(&db, "B", TaskKind::Body, &Selection::default());
    let style = json!({"key": "off", "contentHash": "", "text": ""});
    let hum = json!({"method": "none", "contentHash": "", "text": ""});
    let h = plan_hash("B", &p, &style, &hum);
    let mut full = p.clone();
    full["planHash"] = json!(h);
    full["style"] = style;
    full["humanize"] = hum;
    assert_eq!(freeze(&db, "B", &full).unwrap(), h);
    assert_eq!(freeze(&db, "B", &full).unwrap(), h, "幂等");
    // 冻结后改技能模板：快照不变，新解析的 hash 改变
    db.exec(
        "UPDATE skills SET prompt_template='改过的主卡' WHERE id='bp'",
        &[],
    )
    .unwrap();
    let frozen = load_frozen(&db, &h).unwrap();
    assert_eq!(frozen["skills"][0]["promptTemplate"], "主卡模板");
    let p2 = resolve(&db, "B", TaskKind::Body, &Selection::default());
    assert_ne!(plan_hash("B", &p2, &full["style"], &full["humanize"]), h);
    let v = public_view(&frozen);
    assert!(v["skills"][0].get("promptTemplate").is_none());
    assert_eq!(v["skills"][0]["templateChars"], 4);
}

#[test]
fn recommend_only_existing_applicable_skills() {
    let (_d, db) = db();
    db.exec(
        "INSERT INTO skills(id,name,prompt_template,kind,enabled,builtin_key,usage_mode,origin,targets_json) VALUES('ob','展开正文写作','写正文','builtin',1,'method.body','primary','official','[\"body\"]')",
        &[],
    )
    .unwrap();
    skill(
        &db,
        "g",
        "题材库·玄幻",
        "craft",
        "support",
        "[]",
        "玄幻套路",
        1,
    );
    skill(
        &db,
        "g2",
        "题材库·都市",
        "craft",
        "support",
        "[]",
        "都市套路",
        1,
    );
    let r = recommend(&db, "B", TaskKind::Body, "玄幻");
    let names: Vec<&str> = r
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["展开正文写作", "题材库·玄幻"]);
    assert_eq!(r[0]["action"], "set_primary");
    assert!(recommend(&db, "B", TaskKind::Chat, "")
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn selection_from_args_reads_new_and_legacy_fields() {
    let s = Selection::from_args(&json!({
        "skills": ["a", {"id": "b"}],
        "skillSelection": {"primarySkillId": " p ", "supportSkillIds": "x, y", "styleKey": "off"},
        "humanizeOverride": "none"
    }));
    assert_eq!(s.primary.as_deref(), Some("p"));
    assert_eq!(s.supports, vec!["x", "y"]);
    assert_eq!(s.legacy, vec!["a", "b"]);
    assert_eq!(s.style.as_deref(), Some("off"));
    assert_eq!(s.humanize.as_deref(), Some("none"));
}

#[test]
fn editing_a_skill_does_not_invalidate_inflight_fingerprint_but_bindings_do() {
    let (_d, db) = db();
    let book = crate::books::create_book(&db, "书", "玄幻", "第三人称")["id"]
        .as_str()
        .unwrap()
        .to_string();
    skill(
        &db,
        "any",
        "无关技能",
        "user",
        "support",
        r#"["body"]"#,
        "旧模板",
        1,
    );
    let before = crate::continuity::input_fingerprint(&db, &book, 2).unwrap();
    db.exec(
        "UPDATE skills SET prompt_template='新模板' WHERE id='any'",
        &[],
    )
    .unwrap();
    assert_eq!(
        before,
        crate::continuity::input_fingerprint(&db, &book, 2).unwrap()
    );
    set(&db, &format!("book_primary_skill__{}__body", book), "any");
    assert_ne!(
        before,
        crate::continuity::input_fingerprint(&db, &book, 2).unwrap()
    );
}

/// DeepWrite 绑定是 resolver 的一个来源：只在子阶段开启，按任务适用性筛选、按 id 去重、只作用于本书。
#[test]
fn deepwrite_bindings_go_through_the_same_checks() {
    let (_d, db) = db();
    skill(
        &db,
        "hz",
        "去味法",
        "user",
        "support",
        r#"["humanize"]"#,
        "去味模板",
        1,
    );
    skill(
        &db,
        "bd",
        "正文写法",
        "user",
        "support",
        r#"["body"]"#,
        "正文模板",
        1,
    );
    skill(
        &db,
        "off",
        "停用卡",
        "user",
        "support",
        r#"["humanize"]"#,
        "模板",
        0,
    );
    for (b, s) in [("B", "hz"), ("B", "bd"), ("B", "off"), ("OTHER", "hz")] {
        db.exec(
            "INSERT INTO dw_book_skill(book_id,skill_id,enabled,updated_at) VALUES(?1,?2,1,0)",
            &[&b as &dyn rusqlite::ToSql, &s],
        )
        .unwrap();
    }
    set(&db, "book_support_skills__B__humanize", r#"["hz"]"#);
    let dw = Selection {
        deepwrite: true,
        ..Default::default()
    };
    let p = resolve(&db, "B", TaskKind::Humanize, &dw);
    assert_eq!(
        ids(&p),
        vec!["hz"],
        "作品辅助与 DeepWrite 绑定同一技能只注入一次"
    );
    let mut c = codes(&p);
    c.sort();
    assert_eq!(c, vec!["bd:NOT_APPLICABLE", "off:DISABLED"]);
    assert!(p["excluded"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["source"] == "deepwrite"));
    // 主任务（未开启 deepwrite）不受 DeepWrite 绑定影响
    let p = resolve(&db, "B", TaskKind::Body, &Selection::default());
    assert!(ids(&p).is_empty());
    // 只作用于本书
    let p = resolve(&db, "NOBOOK", TaskKind::Humanize, &dw);
    assert!(ids(&p).is_empty());
}
