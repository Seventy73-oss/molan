use super::*;
use crate::artifact_view::{list_for_session, view};

fn setup() -> (tempfile::TempDir, Db, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), None).unwrap();
    let id = crate::books::create_book(&db, "书", "玄幻", "第三人称")["id"]
        .as_str()
        .unwrap()
        .to_string();
    (dir, db, id)
}

fn new(book: &str, kind: &str, task: &str, content: &str, target: Value) -> NewArtifact {
    NewArtifact {
        book_id: book.into(),
        session_id: "S".into(),
        kind: kind.into(),
        task: task.into(),
        title: "t".into(),
        target,
        content: content.into(),
        ..Default::default()
    }
}

fn dv(art: &str, book: &str, action: &str, key: &str) -> Deliver {
    Deliver {
        artifact_id: art.into(),
        book_id: book.into(),
        action: action.into(),
        idempotency_key: key.into(),
        ..Default::default()
    }
}

fn ids(v: &Value) -> Vec<String> {
    v["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn outline_save_confirm_then_edit_goes_stale() {
    let (_d, db, b) = setup();
    let id = create(
        &db,
        &new(
            &b,
            "outline_draft",
            "outline",
            "# 第3章细纲\n冲突……",
            json!({"ch": 3}),
        ),
    )
    .unwrap();
    let v = view(&db, &id, false).unwrap();
    assert_eq!(v["state"], "generated");
    assert_eq!(v["stateLabel"], "已生成，尚未保存");
    assert_eq!(ids(&v)[0], "save_outline");
    let mut d = dv(&id, &b, "save", "k1");
    d.group = "细纲".into();
    d.name = "细纲_第3章.md".into();
    d.op = "create".into();
    let r = deliver(&db, &d).unwrap();
    assert_eq!(r["ok"], true, "{}", r);
    let v = view(&db, &id, false).unwrap();
    assert_eq!(v["state"], "saved");
    assert_eq!(v["summary"], "已保存到 细纲/细纲_第3章.md");
    assert_eq!(ids(&v)[0], "confirm_outline");
    let r = deliver(&db, &dv(&id, &b, "confirm_outline", "k2")).unwrap();
    assert_eq!(r["ok"], true, "{}", r);
    let v = view(&db, &id, false).unwrap();
    assert_eq!(v["state"], "confirmed");
    assert_eq!(ids(&v)[0], "draft_body");
    // 作者在编辑器里改了细纲：卡片如实变为失效
    crate::files::write_file(&db, &b, "细纲", "细纲_第3章.md", "改过的细纲").unwrap();
    let v = view(&db, &id, false).unwrap();
    assert_eq!(v["state"], "stale");
    assert!(v["summary"].as_str().unwrap().contains("确认已失效"));
}

#[test]
fn body_submit_approve_and_formal_edit_invalidates() {
    let (_d, db, b) = setup();
    let id = create(
        &db,
        &new(
            &b,
            "body_draft",
            "body",
            "第一章 正文内容",
            json!({"ch": 1}),
        ),
    )
    .unwrap();
    assert_eq!(ids(&view(&db, &id, false).unwrap())[0], "submit_pending");
    let r = deliver(&db, &dv(&id, &b, "submit_pending", "k1")).unwrap();
    assert_eq!(r["ok"], true, "{}", r);
    let v = view(&db, &id, false).unwrap();
    assert_eq!(v["state"], "pending_review");
    assert_eq!(v["stateLabel"], "已提交待审，尚未定稿");
    let r = deliver(&db, &dv(&id, &b, "approve", "k2")).unwrap();
    assert_eq!(r["ok"], true, "{}", r);
    let v = view(&db, &id, false).unwrap();
    assert_eq!(v["state"], "approved");
    // 定稿措辞带记忆同步状态：按正文 hash 绑定的 memory_job 如实反映排队 / 完成 / 失败
    assert!(
        v["summary"].as_str().unwrap().ends_with("，记忆更新排队中"),
        "{}",
        v["summary"]
    );
    let h = crate::continuity::content_hash("第一章 正文内容");
    let job = |status: &str, hash: &str| {
        db.exec(
            "INSERT INTO memory_job(book_id,ch,name,source_hash,status,updated_at) VALUES(?1,1,'第1章.md',?2,?3,0)
             ON CONFLICT(book_id,ch) DO UPDATE SET source_hash=excluded.source_hash,status=excluded.status",
            &[&b as &dyn rusqlite::ToSql, &hash, &status],
        )
        .unwrap();
        view(&db, &id, false).unwrap()["summary"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(job("done", &h).ends_with("，记忆已更新"));
    assert!(job("failed", &h).contains("记忆更新失败"));
    assert!(
        job("done", "other").ends_with("，记忆更新排队中"),
        "别的版本的记忆不算本稿已更新"
    );
    crate::files::write_file(&db, &b, "正文", "第1章.md", "作者重写").unwrap();
    assert_eq!(view(&db, &id, false).unwrap()["state"], "stale");
}

#[test]
fn approve_from_pending_panel_is_reflected() {
    let (_d, db, b) = setup();
    let id = create(
        &db,
        &new(&b, "body_draft", "body", "正文", json!({"ch": 2})),
    )
    .unwrap();
    deliver(&db, &dv(&id, &b, "submit_pending", "k1")).unwrap();
    crate::chapter_commit::approve(&db, &b, "", 2, None, "panel").unwrap();
    assert_eq!(view(&db, &id, false).unwrap()["state"], "approved");
}

#[test]
fn fragment_rules_and_base_change_detection() {
    let (_d, db, b) = setup();
    let base = "甲。被改的句子。乙。";
    crate::files::write_file(&db, &b, "设定", "a.md", base).unwrap();
    let start = crate::doc_write::utf16_len("甲。");
    let end = start + crate::doc_write::utf16_len("被改的句子。");
    let target = json!({"group": "设定", "name": "a.md", "baseHash": content_hash(base), "start": start, "end": end, "selectionText": "被改的句子。"});
    let mut n = new(&b, "revision", "revise", "改写后。", target);
    n.scope = "fragment".into();
    let id = create(&db, &n).unwrap();
    let v = view(&db, &id, false).unwrap();
    assert_eq!(v["state"], "generated");
    assert_eq!(ids(&v)[0], "apply_selection");
    // 片段不能整篇替换
    let mut d = dv(&id, &b, "save", "k1");
    d.group = "设定".into();
    d.name = "a.md".into();
    d.op = "replace".into();
    d.base_hash = Some(content_hash(base));
    assert!(deliver(&db, &d).unwrap_err().to_string().contains("片段"));
    // 选区替换成功
    d.op = "replace_range".into();
    d.start = Some(start);
    d.end = Some(end);
    d.expected = Some("被改的句子。".into());
    d.idempotency_key = "k2".into();
    let r = deliver(&db, &d).unwrap();
    assert_eq!(r["ok"], true, "{}", r);
    assert_eq!(
        crate::files::read_file(&db, &b, "设定", "a.md").as_deref(),
        Some("甲。改写后。乙。")
    );
    assert_eq!(view(&db, &id, false).unwrap()["state"], "saved");
    // 另一个基于旧基线的片段产物：原文已变 → base_changed，主动作为比较
    let mut n2 = new(
        &b,
        "revision",
        "revise",
        "另一版。",
        json!({"group": "设定", "name": "a.md", "baseHash": content_hash(base), "start": start, "end": end}),
    );
    n2.scope = "fragment".into();
    let id2 = create(&db, &n2).unwrap();
    let v2 = view(&db, &id2, false).unwrap();
    assert_eq!(v2["state"], "base_changed");
    assert_eq!(ids(&v2)[0], "compare");
}

#[test]
fn review_report_cannot_overwrite_target() {
    let (_d, db, b) = setup();
    crate::files::write_file(&db, &b, "正文", "第1章.md", "正文").unwrap();
    let id = create(
        &db,
        &new(&b, "review_report", "review", "审稿意见", json!({})),
    )
    .unwrap();
    let mut d = dv(&id, &b, "save", "k1");
    d.group = "正文".into();
    d.name = "第1章.md".into();
    d.op = "replace".into();
    d.base_hash = Some(content_hash("正文"));
    assert!(deliver(&db, &d)
        .unwrap_err()
        .to_string()
        .contains("只能另存"));
    d.group = "参考".into();
    d.name = "审稿_第1章.md".into();
    d.op = "create".into();
    d.base_hash = None;
    assert_eq!(deliver(&db, &d).unwrap()["ok"], true);
}

#[test]
fn multi_item_partial_success_is_reported_per_item() {
    let (_d, db, b) = setup();
    crate::files::write_file(&db, &b, "设定", "已有.md", "不同的旧内容").unwrap();
    let mut n = new(&b, "multi_file", "chat", "", json!({}));
    n.items = vec![
        json!({"title": "人物", "group": "设定", "name": "人物.md", "content": "人物表"}),
        json!({"title": "世界", "group": "设定", "name": "世界.md", "content": "世界观"}),
        json!({"title": "冲突", "group": "设定", "name": "已有.md", "content": "新内容"}),
    ];
    let id = create(&db, &n).unwrap();
    for (i, name) in ["人物.md", "世界.md", "已有.md"].iter().enumerate() {
        let mut d = dv(&id, &b, "save", &format!("k{}", i));
        d.item = i as i64;
        d.group = "设定".into();
        d.name = name.to_string();
        d.op = "create".into();
        deliver(&db, &d).unwrap();
    }
    let v = view(&db, &id, false).unwrap();
    assert_eq!(v["state"], "partial");
    assert_eq!(v["summary"], "2项成功，1项冲突或失败");
    assert_eq!(v["items"][2]["state"], "conflict");
    assert_eq!(
        crate::files::read_file(&db, &b, "设定", "已有.md").as_deref(),
        Some("不同的旧内容"),
        "同名异内容不覆盖"
    );
}

#[test]
fn revise_creates_new_revision_and_detects_concurrent_edit() {
    let (_d, db, b) = setup();
    let id = create(&db, &new(&b, "review_report", "review", "原始", json!({}))).unwrap();
    assert_eq!(revise(&db, &id, &b, 1, "改过", None, "").unwrap(), 2);
    let e = revise(&db, &id, &b, 1, "另一个窗口", None, "").unwrap_err();
    assert!(e.to_string().starts_with("REV_CONFLICT"));
    let v = view(&db, &id, true).unwrap();
    assert_eq!(v["content"], "改过");
    assert_eq!(v["origin"], "user_edit");
    assert_eq!(
        get_rev(&db, &id, 1).unwrap()["content"],
        "原始",
        "保留原生成内容"
    );
    assert!(revise(&db, &id, "other-book", 2, "x", None, "").is_err());
}

#[test]
fn deliver_is_idempotent_per_key() {
    let (_d, db, b) = setup();
    let id = create(&db, &new(&b, "plot_note", "plot", "推演", json!({}))).unwrap();
    let mut d = dv(&id, &b, "save", "click-1");
    d.group = "参考".into();
    d.name = "推演.md".into();
    d.op = "create".into();
    let r1 = deliver(&db, &d).unwrap();
    let r2 = deliver(&db, &d).unwrap();
    assert_eq!(r2["replayed"], true);
    assert_eq!(r1["deliveryId"], r2["deliveryId"]);
    assert_eq!(deliveries(&db, &id).len(), 1);
}

#[test]
fn legacy_doc_message_is_projected_with_real_state() {
    let (_d, db, b) = setup();
    db.exec(
        "INSERT INTO sessions(id,book_id,title,created_at,updated_at) VALUES('S',?1,'s',0,0)",
        &[&b as &dyn rusqlite::ToSql],
    )
    .unwrap();
    let result = json!({"doc": {"group": "设定", "name": "旧卡.md", "content": "旧内容"}});
    db.exec(
        "INSERT INTO messages(id,session_id,role,content,result_json,created_at) VALUES('m1','S','assistant','x',?1,5)",
        &[&result.to_string() as &dyn rusqlite::ToSql],
    )
    .unwrap();
    let list = list_for_session(&db, &b, "S").unwrap();
    assert_eq!(list[0]["legacy"], true);
    assert_eq!(list[0]["state"], "generated");
    assert_eq!(list[0]["actions"][0]["id"], "legacy_save_doc");
    crate::files::write_file(&db, &b, "设定", "旧卡.md", "旧内容").unwrap();
    let saved = json!({"doc": {"group": "设定", "name": "旧卡.md", "content": "旧内容"}, "savedDocs": [{"group": "设定", "name": "旧卡.md"}], "saved": true});
    db.exec(
        "UPDATE messages SET result_json=?1 WHERE id='m1'",
        &[&saved.to_string() as &dyn rusqlite::ToSql],
    )
    .unwrap();
    assert_eq!(list_for_session(&db, &b, "S").unwrap()[0]["state"], "saved");
    crate::files::write_file(&db, &b, "设定", "旧卡.md", "被改").unwrap();
    assert_eq!(
        list_for_session(&db, &b, "S").unwrap()[0]["state"],
        "stale",
        "旧卡不会永远绿色"
    );
}
