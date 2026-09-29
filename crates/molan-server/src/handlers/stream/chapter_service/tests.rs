use super::*;

fn fixture() -> (tempfile::TempDir, Db, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), None).unwrap();
    db.exec("INSERT INTO settings(key,value) VALUES('channels','[{\"id\":\"mock\",\"label\":\"Mock\",\"baseUrl\":\"mock://\",\"model\":\"mock-model\"}]')", &[]).unwrap();
    db.exec(
        "INSERT INTO settings(key,value) VALUES('active_channel','mock')",
        &[],
    )
    .unwrap();
    let book = molan_core::books::create_book(&db, "svc-test", "玄幻", "第三人称")["id"]
        .as_str()
        .unwrap()
        .to_string();
    files::write_file(&db, &book, "设定", "建书档案.md", "定位：测试档案").unwrap();
    files::write_file(&db, &book, "设定", "大纲.md", "全书大纲：测试").unwrap();
    (dir, db, book)
}

fn confirmed_outline(db: &Db, book: &str, ch: i64) {
    let n = format!("细纲_第{}章.md", ch);
    files::write_file(
        db,
        book,
        "细纲",
        &n,
        &format!("第{}章细纲：主角入场，冲突升级，章末钩子。", ch),
    )
    .unwrap();
    outline_confirm::confirm(db, book, ch, &n, None).unwrap();
}

fn test_state(dir: &tempfile::TempDir, db: Db) -> Arc<AppState> {
    Arc::new(AppState {
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

/// 走 agent_tools 的异步分发入口（与 agent_loop 同一调用面）：mock 渠道全流程。
#[tokio::test]
async fn draft_lands_in_review_with_bound_receipt() {
    let (_dir, db, book) = fixture();
    confirmed_outline(&db, &book, 1);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let cancel = CancellationToken::new();
    let r = super::super::agent_tools::dispatch_tool_io(
        &db,
        &book,
        "draft_chapter_body",
        &json!({"ch": 1}),
        &cancel,
        &tx,
        "1",
    )
    .await
    .unwrap();
    drop(tx);
    drain.await.unwrap();
    assert_eq!(r["kind"], "body_draft");
    let name = r["name"].as_str().unwrap().to_string();
    let hash = r["hash"].as_str().unwrap().to_string();
    assert_eq!(hash.len(), 64);
    let landed =
        files::read_file(&db, &book, molan_core::db::REVIEW_GROUP, &name).expect("待审稿必须落盘");
    assert_eq!(
        continuity::content_hash(&landed),
        hash,
        "回执 hash 必须绑定落盘内容"
    );
    // 手动线绝不直接写正式稿
    assert!(files::read_file(&db, &book, "正文", &name).is_none());
    let q = db
        .q_json(
            "SELECT status FROM pending_chapter WHERE book_id=?1 AND ch=1",
            &[&book as &dyn rusqlite::ToSql],
        )
        .unwrap();
    assert_eq!(q[0]["status"], "pending");
    // 审核报告绑定落盘 hash（mock 审核通过）
    assert_eq!(r["review"]["bodyHash"].as_str().unwrap(), hash);
    assert_eq!(r["review"]["ok"], json!(true));
    // 上下文清单含技能快照列（§6/§8.4 可追责）
    let man = db
        .q_json(
            "SELECT skills_json FROM context_manifest WHERE book_id=?1 AND command='manual_draft' AND target_ch=1",
            &[&book as &dyn rusqlite::ToSql],
        )
        .unwrap();
    // TEXT 列经 q_json 回传为字符串：解析后再断言（与 coverage_json 消费方式一致）
    let snap: Value =
        serde_json::from_str(man[0]["skillsJson"].as_str().unwrap_or("null")).unwrap();
    assert!(
        !man.is_empty() && snap.is_array(),
        "skills_json 应为数组: {:?}",
        man
    );
    // 草稿溯源登记 manual-draft；重复起草必须被拒
    let o = db
        .q_json(
            "SELECT task_id FROM draft_origin WHERE book_id=?1 AND ch=1",
            &[&book as &dyn rusqlite::ToSql],
        )
        .unwrap();
    assert_eq!(o[0]["taskId"], "manual-draft");
    let (tx2, _rx2) = tokio::sync::mpsc::channel::<String>(64);
    let e = super::super::agent_tools::dispatch_tool_io(
        &db,
        &book,
        "draft_chapter_body",
        &json!({"ch": 1}),
        &cancel,
        &tx2,
        "1",
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("已有待审稿"), "{}", e);
}

/// 前置门：细纲未确认 / 未确认但已有文件 / 已有正式稿，一律拒绝且不落盘。
#[tokio::test]
async fn draft_preflight_rejects_unconfirmed_or_existing() {
    let (_dir, db, book) = fixture();
    let (tx, _rx) = tokio::sync::mpsc::channel::<String>(64);
    let cancel = CancellationToken::new();
    let e = super::super::agent_tools::dispatch_tool_io(
        &db,
        &book,
        "draft_chapter_body",
        &json!({"ch":1}),
        &cancel,
        &tx,
        "1",
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("还没有细纲"), "{}", e);
    files::write_file(
        &db,
        &book,
        "细纲",
        "细纲_第1章.md",
        "第1章细纲：未确认版本内容。",
    )
    .unwrap();
    let e = super::super::agent_tools::dispatch_tool_io(
        &db,
        &book,
        "draft_chapter_body",
        &json!({"ch":1}),
        &cancel,
        &tx,
        "1",
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("未确认"), "{}", e);
    outline_confirm::confirm(&db, &book, 1, "细纲_第1章.md", None).unwrap();
    files::write_file(&db, &book, "正文", "第1章.md", "已有正式稿").unwrap();
    let e = super::super::agent_tools::dispatch_tool_io(
        &db,
        &book,
        "draft_chapter_body",
        &json!({"ch":1}),
        &cancel,
        &tx,
        "1",
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("已有正式稿"), "{}", e);
    assert!(files::read_file(&db, &book, molan_core::db::REVIEW_GROUP, "第1章.md").is_none());
}

/// 定稿绑定 hash + 幂等重入 + sweep 对「已定稿但记忆 pending」的章补发抽取（mock 下如实失败）。
#[tokio::test]
async fn finalize_binds_hash_and_sweep_syncs_memory() {
    let (dir, db, book) = fixture();
    let body = "第1章正式内容。".repeat(40);
    files::write_file(&db, &book, molan_core::db::REVIEW_GROUP, "第1章.md", &body).unwrap();
    crate::handlers::register_review_queue(&db, &book, "第1章.md", &body).unwrap();
    let hash = continuity::content_hash(&body);
    assert!(finalize_draft(&db, &book, 1, "aa").is_err());
    let r = finalize_draft(&db, &book, 1, &hash).unwrap();
    assert_eq!(r["kind"], "body_finalized");
    assert!(files::read_file(&db, &book, "正文", "第1章.md").is_some());
    let r2 = finalize_draft(&db, &book, 1, &hash).unwrap();
    assert_eq!(r2["alreadyApproved"], json!(true));
    assert_eq!(r2["verified"], "approved-receipt-hash-match");
    let st = test_state(&dir, db);
    sweep_pending_memory(&st, &book).await;
    let mut last = String::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let rows = st
            .db
            .q_json(
                "SELECT status FROM memory_job WHERE book_id=?1 AND ch=1 ORDER BY rowid",
                &[&book as &dyn rusqlite::ToSql],
            )
            .unwrap();
        last = rows
            .last()
            .and_then(|r| r["status"].as_str())
            .unwrap_or("")
            .to_string();
        if !last.is_empty() && last != "pending" {
            break;
        }
    }
    assert_eq!(last, "failed", "mock 下记忆抽取必须如实失败，绝不静默成功");
}
