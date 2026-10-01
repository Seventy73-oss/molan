use super::super::auto_write::resolve_humanize;
use super::*;
use molan_core::db::Db;

fn v(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

/// 审稿子阶段：作品为「审稿」绑定的技能进入审稿提示（附协议硬约束），正文写作卡不进入。
#[test]
fn review_prompt_uses_review_stage_skills_under_protocol_guard() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), None).unwrap();
    let book = molan_core::books::create_book(&db, "书", "玄幻", "第三人称")["id"]
        .as_str()
        .unwrap()
        .to_string();
    db.exec("INSERT INTO skills(id,name,prompt_template,kind,enabled,usage_mode,origin,targets_json) VALUES('rv','设定审查法','重点查设定矛盾','user',1,'support','user','[\"review\"]'),('bd','正文写法','多写对白','user',1,'support','user','[\"body\"]')", &[]).unwrap();
    set(
        &db,
        &format!("book_support_skills__{}__review", book),
        r#"["rv","bd"]"#,
    );
    let plan = super::super::run_plan::stage_plan(
        &db,
        dir.path(),
        &book,
        molan_core::task_kind::TaskKind::Review,
        &Default::default(),
    );
    let sys = review_system(&db, &book, &plan);
    assert!(sys.starts_with("你是网文责编，审读一章正文。只输出一个 JSON 对象"));
    assert!(sys.contains("- 设定审查法：重点查设定矛盾"));
    assert!(sys.contains("【协议硬约束】"));
    assert!(!sys.contains("多写对白"), "正文写作卡不得进入审稿提示");
    assert_eq!(plan["excluded"][0]["code"], "NOT_APPLICABLE");
    // 无审稿技能时提示保持原样（不出现空技能块）
    let bare = review_system(&db, &book, &Value::Null);
    assert!(!bare.contains("本阶段技能") && !bare.contains("协议硬约束"));
}

fn set(db: &Db, key: &str, val: &str) {
    db.exec(
        "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        &[&key as &dyn rusqlite::ToSql, &val as &dyn rusqlite::ToSql],
    )
    .unwrap();
}

/// mock 渠道（channels=true）或无渠道的一本书测试库。
fn fixture(channels: bool) -> (tempfile::TempDir, Db, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), None).unwrap();
    if channels {
        set(
            &db,
            "channels",
            r#"[{"id":"mock","label":"Mock","baseUrl":"mock://","model":"mock-model"}]"#,
        );
        set(&db, "active_channel", "mock");
    }
    let book = molan_core::books::create_book(&db, "review-test", "玄幻", "第三人称")["id"]
        .as_str()
        .unwrap()
        .to_string();
    (dir, db, book)
}

fn is_untrusted(j: &str) -> bool {
    matches!(classify_verdict(&v(j)), ReviewDecision::Untrusted(_))
}

/// T4b 验收①：humanizeOverride 三态统一（表驱动）。
/// "" → 书级回落（缺失再 official:standard）；"none" → 明确关闭且压过书级；official/skill 显式。
#[test]
fn humanize_override_three_states_are_unified() {
    let (_d, db, book) = fixture(true);
    for (id, name, tpl, bk) in [
        ("s-std", "标准", "STD", "method.humanize.standard"),
        ("s-deep", "深度", "DEEP", "method.humanize.deep"),
        ("my-style", "我的", "SKILL", ""),
    ] {
        db.exec(
            "INSERT INTO skills(id,name,prompt_template,kind,enabled,builtin_key) VALUES(?1,?2,?3,'humanize',1,?4)",
            &[&id as &dyn rusqlite::ToSql, &name, &tpl, &bk],
        )
        .unwrap();
    }
    let key = format!("book_humanize__{}", book);
    // (override, 书级设置, 期望方法, 期望取到提示词)
    let cases: &[(Option<&str>, &str, &str, bool)] = &[
        (None, "official:deep", "official:deep", true),
        (Some(""), "official:deep", "official:deep", true),
        (Some("   "), "official:deep", "official:deep", true),
        // "none" = 明确关闭，压过书级默认
        (Some("none"), "official:deep", "none", false),
        (None, "none", "none", false),
        // official:* / skill:<id> 显式，压过书级
        (
            Some("official:standard"),
            "official:deep",
            "official:standard",
            true,
        ),
        (
            Some("skill:my-style"),
            "official:deep",
            "skill:my-style",
            true,
        ),
        // 书级缺失 / 字面 "null" → 再缺省 official:standard
        (None, "", "official:standard", true),
        (None, "null", "official:standard", true),
    ];
    for (ov, book_set, want_method, want_prompt) in cases {
        db.exec(
            "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            &[&key as &dyn rusqlite::ToSql, &book_set as &dyn rusqlite::ToSql],
        )
        .unwrap();
        let (method, prompt) = resolve_humanize(&db, &book, *ov);
        assert_eq!(
            method, *want_method,
            "override={:?} book={:?}",
            ov, book_set
        );
        assert_eq!(
            !prompt.is_empty(),
            *want_prompt,
            "override={:?} book={:?}",
            ov,
            book_set
        );
    }
    // 具体提示词内容也须取对（防 builtin_key 映射漂移）
    db.exec(
        "DELETE FROM settings WHERE key=?1",
        &[&key as &dyn rusqlite::ToSql],
    )
    .unwrap();
    assert_eq!(resolve_humanize(&db, &book, None).1, "STD");
    assert_eq!(
        resolve_humanize(&db, &book, Some("official:deep")).1,
        "DEEP"
    );
    assert_eq!(
        resolve_humanize(&db, &book, Some("skill:my-style")).1,
        "SKILL"
    );
}

/// T4b 验收②-a：review_body 通过分支（mock:// 的确定性审核 JSON）。
#[tokio::test]
async fn review_body_passes_on_mock_channel() {
    let (_d, db, book) = fixture(true);
    let body = format!("# 第1章 测试\n\n{}", "少年推开山门。".repeat(60));
    let out = review_body(&db, &book, 1, "", &body, None, &Value::Null).await;
    assert!(!out.flagged, "mock 审核应可信：{}", out.note);
    assert!(out.ok && out.issues.is_empty(), "{}", out.note);
    assert!(out.note.starts_with("通过"));
}

/// T4b 验收②-b：渠道缺失 → fail-closed（不得冒充通过）。
#[tokio::test]
async fn review_body_fails_closed_without_review_channel() {
    let (_d, db, book) = fixture(false);
    let out = review_body(
        &db,
        &book,
        1,
        "",
        "# 第1章 测试\n\n正文内容。",
        None,
        &Value::Null,
    )
    .await;
    assert!(out.flagged && !out.ok, "{}", out.note);
    assert!(out.note.contains("无可用审核渠道"), "{}", out.note);
}

/// T4b 验收②-c：超单次预算（未覆盖全文）→ fail-closed，绝不「只审前段」放行。
#[tokio::test]
async fn review_body_fails_closed_over_budget() {
    let (_d, db, book) = fixture(true);
    let body = "字".repeat(REVIEW_BODY_BUDGET + 1);
    let out = review_body(&db, &book, 1, "", &body, None, &Value::Null).await;
    assert!(out.flagged && !out.ok, "{}", out.note);
    assert!(out.note.contains("超过单次审核预算"), "{}", out.note);
    assert_eq!(REVIEW_BODY_BUDGET, 9000);
}

/// T4b 验收②-d：不通过 / 解析失败 / 结构不完整（mock 固定通过，故在纯判定层覆盖，
/// 与 auto_write 既有 parse_review_verdict 测试同层）。
#[test]
fn verdict_classification_is_fail_closed() {
    assert!(matches!(
        classify_verdict(&v(r#"{"ok":true,"issues":[]}"#)),
        ReviewDecision::Pass { .. }
    ));
    match classify_verdict(&v(r#"{"ok":false,"issues":["人物死亡冲突"],"fix":"改掉"}"#)) {
        ReviewDecision::Fix { ok, hard, fix, .. } => {
            assert!(!ok);
            assert_eq!(hard, vec!["人物死亡冲突".to_string()]);
            assert_eq!(fix, "改掉");
        }
        _ => panic!("ok=false + issues 应判为需重写"),
    }
    assert!(is_untrusted(r#"{"ok":false,"issues":[]}"#));
    assert!(is_untrusted("{}"));
    assert!(is_untrusted(r#"{"ok":"true","issues":[]}"#));
    assert!(is_untrusted(r#"{"ok":true,"issues":""}"#));
    assert!(is_untrusted(r#"{"ok":true,"issues":[1]}"#));
    assert!(extract_json_loose("模型跑题，没有任何 JSON").is_none());
    match classify_verdict(&v(r#"{"ok":true,"issues":[],"warnings":["节奏略慢"]}"#)) {
        ReviewDecision::Pass { note } => assert!(note.contains("软建议"), "{}", note),
        _ => panic!("软建议不应阻断通过"),
    }
}

fn test_state(dir: &tempfile::TempDir, db: Db) -> std::sync::Arc<crate::AppState> {
    std::sync::Arc::new(crate::AppState {
        db,
        web_dir: dir.path().join("web"),
        web_dir_canon: dir.path().join("web"),
        root: dir.path().to_path_buf(),
        auth_token: None,
        sessions: crate::auth::SessionStore::with_defaults(),
        login_limiter: crate::auth::LoginLimiter::with_defaults(),
        trusted_scheme: "http".to_string(),
        secure_cookies: false,
    })
}

async fn collect_chat(
    st: &std::sync::Arc<crate::AppState>,
    args: &Value,
) -> (anyhow::Result<Option<Value>>, Vec<Value>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(256);
    let a = args.clone();
    let runner = {
        let st = st.clone();
        async move {
            let r = super::super::chat::chat_stream(&st, "chat_stream", &a, &tx).await;
            drop(tx);
            r
        }
    };
    let collector = async {
        let mut events = Vec::new();
        while let Some(line) = rx.recv().await {
            if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                events.push(v["e"].clone());
            }
        }
        events
    };
    tokio::join!(runner, collector)
}

/// T4b 验收③：explicit-task 正文（≥300 字）落盘成功后必须发 review 事件，
/// bodyHash 必须绑定「落盘内容」而不是别的文本。
#[tokio::test]
async fn explicit_body_save_emits_review_event() {
    let (dir, db, book) = fixture(true);
    set(&db, &format!("book_auto_save__{}", book), "auto");
    let sid = molan_core::books::create_session(&db, &book, "审读会话").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let st = test_state(&dir, db);
    let a = json!({
        "onEvent": "__CHANNEL__:1", "sessionId": sid, "bookId": book,
        "message": "写第1章正文", "task": "body",
        "writeIntent": "explicit-task", "requestId": "req-review-1",
    });
    let (res, events) = collect_chat(&st, &a).await;
    res.unwrap().unwrap();
    assert!(
        events.iter().any(|e| e["type"] == "saved"),
        "explicit-task 正文应落盘：{:?}",
        events
    );
    let reviews: Vec<&Value> = events.iter().filter(|e| e["type"] == "review").collect();
    assert_eq!(reviews.len(), 1, "应恰好一条 review 事件：{:?}", events);
    assert_eq!(reviews[0]["ok"], json!(true), "{:?}", reviews[0]);
    assert_eq!(reviews[0]["issues"], json!([]));
    let hash = reviews[0]["bodyHash"].as_str().unwrap_or("");
    assert_eq!(hash.len(), 64, "bodyHash 应为 SHA-256：{}", hash);
    let landed =
        molan_core::files::read_file(&st.db, &book, molan_core::db::REVIEW_GROUP, "第1章.md")
            .expect("待审稿应已落盘");
    assert_eq!(hash, molan_core::continuity::content_hash(&landed));
}

/// T4b 验收③：preview 不审核（无 saved、无 review 事件）。
#[tokio::test]
async fn preview_body_is_not_reviewed() {
    let (dir, db, book) = fixture(true);
    set(&db, &format!("book_auto_save__{}", book), "auto");
    let sid = molan_core::books::create_session(&db, &book, "预览会话").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let st = test_state(&dir, db);
    let a = json!({
        "onEvent": "__CHANNEL__:1", "sessionId": sid, "bookId": book,
        "message": "写第1章正文", "task": "body",
        "writeIntent": "preview", "requestId": "req-preview-1",
    });
    let (_res, events) = collect_chat(&st, &a).await;
    assert!(
        !events.iter().any(|e| e["type"] == "review"),
        "preview 不得发 review 事件：{:?}",
        events
    );
    assert!(!events.iter().any(|e| e["type"] == "saved"));
    assert!(
        molan_core::files::read_file(&st.db, &book, molan_core::db::REVIEW_GROUP, "第1章.md")
            .is_none()
    );
}
