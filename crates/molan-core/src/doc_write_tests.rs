//! DocumentWriteService 回归测试：真实临时库 + 真实文件，逐字核对磁盘内容。
use super::*;
use crate::db::Db;

fn setup() -> (tempfile::TempDir, Db, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), None).unwrap();
    let id = crate::books::create_book(&db, "测试书", "玄幻", "第三人称")["id"]
        .as_str()
        .unwrap()
        .to_string();
    (dir, db, id)
}

fn plan(book: &str, op: WriteOp, name: &str, content: &str, key: &str) -> WritePlan {
    WritePlan {
        book_id: book.into(),
        group: "设定".into(),
        name: name.into(),
        op,
        base_hash: None,
        content: content.into(),
        start: None,
        end: None,
        expected: None,
        actor: Actor::User,
        idempotency_key: key.into(),
        source: Value::Null,
    }
}

fn disk(db: &Db, book: &str, name: &str) -> Option<String> {
    files::read_checked(db, book, "设定", name).unwrap()
}

const RICH: &str = "第一段：中文，标点。\r\n\r\n  缩进两格  \n😀 emoji 与 e\u{301} 组合字符\n```\n合法代码块内容\n```\n";

#[test]
fn create_preserves_bytes_exactly_and_reports_hash() {
    let (_d, db, b) = setup();
    let r = execute(&db, &plan(&b, WriteOp::Create, "人物.md", RICH, "k1")).unwrap();
    assert_eq!(r.commit, "committed", "{:?}", r.error);
    assert_eq!(r.index, "ok");
    assert_eq!(disk(&db, &b, "人物.md").as_deref(), Some(RICH));
    assert_eq!(r.after_hash.as_deref(), Some(content_hash(RICH).as_str()));
    assert_eq!(r.before_hash, None);
    assert!(r.revision.is_some());
    let read = read_doc(&db, &b, "设定", "人物.md").unwrap();
    assert_eq!(read["exists"], true);
    assert_eq!(read["hash"], json!(content_hash(RICH)));
    assert_eq!(read["utf16Len"], json!(utf16_len(RICH)));
}

#[test]
fn read_distinguishes_missing_from_empty() {
    let (_d, db, b) = setup();
    assert_eq!(read_doc(&db, &b, "设定", "无.md").unwrap()["exists"], false);
    execute(&db, &plan(&b, WriteOp::Create, "空.md", "", "k")).unwrap();
    let r = read_doc(&db, &b, "设定", "空.md").unwrap();
    assert_eq!(r["exists"], true);
    assert_eq!(r["content"], "");
}

#[test]
fn create_never_overwrites_different_content() {
    let (_d, db, b) = setup();
    execute(&db, &plan(&b, WriteOp::Create, "a.md", "原文", "k1")).unwrap();
    let r = execute(&db, &plan(&b, WriteOp::Create, "a.md", "新内容", "k2")).unwrap();
    assert_eq!(r.commit, "conflict");
    assert_eq!(r.error.unwrap().code, "TARGET_EXISTS");
    assert_eq!(disk(&db, &b, "a.md").as_deref(), Some("原文"));
    // 同名同内容 = 幂等 noop
    let same = execute(&db, &plan(&b, WriteOp::Create, "a.md", "原文", "k3")).unwrap();
    assert_eq!(same.commit, "noop");
}

#[test]
fn replace_requires_matching_base() {
    let (_d, db, b) = setup();
    execute(&db, &plan(&b, WriteOp::Create, "a.md", "v1", "k1")).unwrap();
    let mut p = plan(&b, WriteOp::Replace, "a.md", "v2", "k2");
    let r = execute(&db, &p).unwrap();
    assert_eq!(r.error.unwrap().code, "BASE_REQUIRED");
    p.base_hash = Some(content_hash("stale"));
    p.idempotency_key = "k3".into();
    let r = execute(&db, &p).unwrap();
    assert_eq!(r.commit, "conflict");
    let e = r.error.unwrap();
    assert_eq!(e.code, "BASE_CHANGED");
    assert_eq!(e.current_hash.as_deref(), Some(content_hash("v1").as_str()));
    assert_eq!(disk(&db, &b, "a.md").as_deref(), Some("v1"));
    p.base_hash = Some(content_hash("v1"));
    p.idempotency_key = "k4".into();
    let r = execute(&db, &p).unwrap();
    assert_eq!(r.commit, "committed");
    assert_eq!(r.before_hash.as_deref(), Some(content_hash("v1").as_str()));
    assert_eq!(disk(&db, &b, "a.md").as_deref(), Some("v2"));
}

#[test]
fn insert_and_append_use_utf16_offsets() {
    let (_d, db, b) = setup();
    let base = "😀甲e\u{301}乙";
    execute(&db, &plan(&b, WriteOp::Create, "a.md", base, "k1")).unwrap();
    // 😀 占 2 个 UTF-16 码元：偏移 2 = 「甲」之前
    let mut p = plan(&b, WriteOp::Insert, "a.md", "【插】", "k2");
    p.base_hash = Some(content_hash(base));
    p.start = Some(2);
    assert_eq!(execute(&db, &p).unwrap().commit, "committed");
    let after = "😀【插】甲e\u{301}乙";
    assert_eq!(disk(&db, &b, "a.md").as_deref(), Some(after));
    // 偏移 1 落在代理对中间：拒绝且不写
    let mut bad = plan(&b, WriteOp::Insert, "a.md", "X", "k3");
    bad.base_hash = Some(content_hash(after));
    bad.start = Some(1);
    let r = execute(&db, &bad).unwrap();
    assert_eq!(r.error.unwrap().code, "RANGE_INVALID");
    assert_eq!(disk(&db, &b, "a.md").as_deref(), Some(after));
    // 追加：逐字拼接，不自作主张加换行
    let mut ap = plan(&b, WriteOp::Append, "a.md", "\n\n尾段", "k4");
    ap.base_hash = Some(content_hash(after));
    assert_eq!(execute(&db, &ap).unwrap().commit, "committed");
    assert_eq!(disk(&db, &b, "a.md").unwrap(), format!("{}\n\n尾段", after));
}

#[test]
fn replace_range_only_touches_fragment_and_checks_anchor() {
    let (_d, db, b) = setup();
    let base = "开头。\n被选中的一句话。\n结尾😀。";
    execute(&db, &plan(&b, WriteOp::Create, "a.md", base, "k1")).unwrap();
    let start = utf16_len("开头。\n");
    let end = start + utf16_len("被选中的一句话。");
    let mut p = plan(&b, WriteOp::ReplaceRange, "a.md", "改写后的句子！", "k2");
    p.base_hash = Some(content_hash(base));
    p.start = Some(start);
    p.end = Some(end);
    p.expected = Some("被选中的另一句。".into());
    let r = execute(&db, &p).unwrap();
    assert_eq!(r.commit, "conflict");
    assert_eq!(r.error.unwrap().code, "SELECTION_CHANGED");
    p.expected = Some("被选中的一句话。".into());
    p.idempotency_key = "k3".into();
    assert_eq!(execute(&db, &p).unwrap().commit, "committed");
    assert_eq!(
        disk(&db, &b, "a.md").as_deref(),
        Some("开头。\n改写后的句子！\n结尾😀。")
    );
}

#[test]
fn idempotent_replay_does_not_double_append() {
    let (_d, db, b) = setup();
    execute(&db, &plan(&b, WriteOp::Create, "a.md", "A", "k1")).unwrap();
    let mut p = plan(&b, WriteOp::Append, "a.md", "B", "same-click");
    p.base_hash = Some(content_hash("A"));
    let r1 = execute(&db, &p).unwrap();
    let r2 = execute(&db, &p).unwrap();
    assert_eq!(r1.commit, "committed");
    assert!(r2.replayed);
    assert_eq!(r2.write_id, r1.write_id);
    assert_eq!(disk(&db, &b, "a.md").as_deref(), Some("AB"));
    // 同键不同内容：拒绝
    let mut other = p.clone();
    other.content = "C".into();
    assert_eq!(
        execute(&db, &other).unwrap().error.unwrap().code,
        "IDEMPOTENCY_MISMATCH"
    );
    assert_eq!(disk(&db, &b, "a.md").as_deref(), Some("AB"));
}

#[test]
fn crash_after_prepare_retries_cleanly() {
    let (_d, db, b) = setup();
    execute(&db, &plan(&b, WriteOp::Create, "a.md", "A", "k1")).unwrap();
    let mut p = plan(&b, WriteOp::Append, "a.md", "B", "k2");
    p.base_hash = Some(content_hash("A"));
    FAULT.with(|f| f.set(Some("after_prepare")));
    assert!(execute(&db, &p).is_err());
    assert_eq!(
        disk(&db, &b, "a.md").as_deref(),
        Some("A"),
        "未落盘时原文不变"
    );
    let r = execute(&db, &p).unwrap();
    assert_eq!(r.commit, "committed");
    assert!(!r.recovered);
    assert_eq!(disk(&db, &b, "a.md").as_deref(), Some("AB"));
}

#[test]
fn crash_after_file_write_reconciles_without_rewriting() {
    let (_d, db, b) = setup();
    execute(&db, &plan(&b, WriteOp::Create, "a.md", "A", "k1")).unwrap();
    let mut p = plan(&b, WriteOp::Append, "a.md", "B", "k2");
    p.base_hash = Some(content_hash("A"));
    FAULT.with(|f| f.set(Some("after_file_write")));
    assert!(execute(&db, &p).is_err());
    assert_eq!(disk(&db, &b, "a.md").as_deref(), Some("AB"), "文件已写入");
    let r = execute(&db, &p).unwrap();
    assert_eq!(r.commit, "committed");
    assert!(r.recovered, "重试只补记账本");
    assert_eq!(disk(&db, &b, "a.md").as_deref(), Some("AB"), "不重复追加");
}

#[test]
fn startup_recover_settles_prepared_rows() {
    let (_d, db, b) = setup();
    execute(&db, &plan(&b, WriteOp::Create, "a.md", "A", "k1")).unwrap();
    let mut p = plan(&b, WriteOp::Replace, "a.md", "A2", "k2");
    p.base_hash = Some(content_hash("A"));
    FAULT.with(|f| f.set(Some("after_file_write")));
    assert!(execute(&db, &p).is_err());
    let mut q = plan(&b, WriteOp::Create, "b.md", "B", "k3");
    q.source = json!({"artifactId": "x"});
    FAULT.with(|f| f.set(Some("after_prepare")));
    assert!(execute(&db, &q).is_err());
    assert_eq!(recover(&db).unwrap(), 2);
    let phases = db
        .q_json(
            "SELECT idem_key, phase FROM doc_write_log ORDER BY idem_key",
            &[],
        )
        .unwrap();
    let get = |k: &str| {
        phases
            .iter()
            .find(|r| r["idemKey"] == k)
            .map(|r| r["phase"].as_str().unwrap().to_string())
    };
    assert_eq!(get("k2").as_deref(), Some("committed"));
    assert_eq!(get("k3").as_deref(), Some("aborted"));
    assert_eq!(disk(&db, &b, "b.md"), None);
}

#[test]
fn ai_actor_respects_lock_user_does_not() {
    let (_d, db, b) = setup();
    execute(&db, &plan(&b, WriteOp::Create, "a.md", "A", "k1")).unwrap();
    files::set_file_flag(&db, &b, "设定", "a.md", "locked", true).unwrap();
    let mut p = plan(&b, WriteOp::Replace, "a.md", "AI", "k2");
    p.base_hash = Some(content_hash("A"));
    p.actor = Actor::Ai;
    assert_eq!(execute(&db, &p).unwrap().error.unwrap().code, "LOCKED");
    p.actor = Actor::User;
    p.idempotency_key = "k3".into();
    assert_eq!(execute(&db, &p).unwrap().commit, "committed");
}

#[test]
fn review_group_and_bad_names_are_rejected() {
    let (_d, db, b) = setup();
    let mut p = plan(&b, WriteOp::Create, "第1章.md", "x", "k1");
    p.group = "正文待审".into();
    assert_eq!(
        execute(&db, &p).unwrap().error.unwrap().code,
        "GROUP_FORBIDDEN"
    );
    let bad = plan(&b, WriteOp::Create, "../x.md", "x", "k2");
    assert_eq!(
        execute(&db, &bad).unwrap().error.unwrap().code,
        "NAME_INVALID"
    );
    let mut nob = plan("no-such-book", WriteOp::Create, "a.md", "x", "k3");
    nob.group = "设定".into();
    assert_eq!(
        execute(&db, &nob).unwrap().error.unwrap().code,
        "BOOK_INVALID"
    );
}

#[test]
fn index_failure_after_write_is_partial_success() {
    let (_d, db, b) = setup();
    {
        let c = db.conn.lock().unwrap();
        c.execute_batch(
            "CREATE TRIGGER fail_ce BEFORE INSERT ON continuity_event BEGIN SELECT RAISE(ABORT,'boom'); END;",
        )
        .unwrap();
    }
    let r = execute(&db, &plan(&b, WriteOp::Create, "a.md", "正文内容", "k1")).unwrap();
    assert_eq!(r.commit, "committed", "文件已写入");
    assert_eq!(r.index, "failed");
    assert!(r.index_error.unwrap().contains("文件已保存"));
    assert_eq!(disk(&db, &b, "a.md").as_deref(), Some("正文内容"));
}

#[test]
fn utf16_helpers() {
    let s = "a😀b";
    assert_eq!(utf16_len(s), 4);
    assert_eq!(utf16_to_byte(s, 0), Some(0));
    assert_eq!(utf16_to_byte(s, 1), Some(1));
    assert_eq!(utf16_to_byte(s, 2), None);
    assert_eq!(utf16_to_byte(s, 3), Some(5));
    assert_eq!(utf16_to_byte(s, 4), Some(6));
    assert_eq!(utf16_to_byte(s, 5), None);
    assert_eq!(byte_to_utf16(s, 5), 3);
    assert_eq!(utf16_to_byte("", 0), Some(0));
}

#[test]
fn history_lists_committed_receipts() {
    let (_d, db, b) = setup();
    execute(&db, &plan(&b, WriteOp::Create, "a.md", "A", "k1")).unwrap();
    let mut p = plan(&b, WriteOp::Replace, "a.md", "B", "k2");
    p.base_hash = Some(content_hash("A"));
    execute(&db, &p).unwrap();
    let h = history(&db, &b, "设定", "a.md", 10).unwrap();
    assert_eq!(h.as_array().unwrap().len(), 2);
    assert_eq!(h[0]["op"], "replace");
}

#[test]
fn same_millisecond_snapshots_never_overwrite_each_other() {
    let (_d, db, b) = setup();
    for v in ["v1", "v2", "v3"] {
        crate::files::save_version(&db, &b, "设定", "a.md", v).unwrap();
    }
    let list = crate::files::list_versions(&db, &b, "设定", "a.md");
    let ts: Vec<i64> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["ts"].as_i64().unwrap())
        .collect();
    assert_eq!(ts.len(), 3, "三份快照必须都保留：{:?}", ts);
    let dir = db.versions_dir.join(&b).join("设定").join("a.md");
    let mut got: Vec<String> = ts
        .iter()
        .map(|t| {
            let j: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(dir.join(format!("{}.json", t))).unwrap(),
            )
            .unwrap();
            assert_eq!(j["ts"].as_i64(), Some(*t), "文件名与内容 ts 一致");
            j["content"].as_str().unwrap().to_string()
        })
        .collect();
    got.sort();
    assert_eq!(got, ["v1", "v2", "v3"]);
}
