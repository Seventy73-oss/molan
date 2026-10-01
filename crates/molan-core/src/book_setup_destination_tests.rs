use super::*;
use crate::books::{create_book, create_session, list_books, list_messages, list_sessions};
use rusqlite::ToSql;

fn open() -> (tempfile::TempDir, Db) {
    let d = tempfile::tempdir().unwrap();
    let db = Db::open(d.path(), None).unwrap();
    (d, db)
}
fn book(db: &Db, title: &str) -> String {
    create_book(db, title, "历史", "第三人称")["id"]
        .as_str()
        .unwrap()
        .to_string()
}
fn session(db: &Db, book_id: &str) -> String {
    create_session(db, book_id, "共创").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}
fn files2() -> Value {
    json!([{"group":"设定","name":"建书档案.md","content":"档案内容"},
           {"group":"设定","name":"第1章细纲.md","content":"细纲内容"}])
}
fn insert_card(db: &Db, sid: &str, id: &str, files: &Value) {
    let result =
        json!({"bookSetup":{"titles":["新作品"],"genre":"历史","saved":false,"files":files}});
    db.exec(
        "INSERT INTO messages(id,session_id,role,content,result_json,created_at) VALUES(?1,?2,'assistant','预览',?3,1)",
        &[&id as &dyn ToSql, &sid as &dyn ToSql, &result.to_string() as &dyn ToSql],
    ).unwrap();
}
fn card(db: &Db, id: &str) -> Value {
    let rows = db
        .q_json(
            "SELECT result_json FROM messages WHERE id=?1",
            &[&id as &dyn ToSql],
        )
        .unwrap();
    serde_json::from_str(rows[0]["resultJson"].as_str().unwrap()).unwrap()
}
fn books_len(db: &Db) -> usize {
    list_books(db).as_array().unwrap().len()
}
fn title_of(db: &Db, id: &str) -> String {
    list_books(db)
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == id)
        .unwrap()["title"]
        .as_str()
        .unwrap()
        .to_string()
}
fn read(db: &Db, b: &str, g: &str, n: &str) -> Option<String> {
    files::read_file(db, b, g, n)
}
fn req(i: usize, g: &str, n: &str) -> Value {
    json!({"index":i,"group":g,"name":n})
}
fn dest_new(t: &str) -> Value {
    json!({"mode":"new","title":t})
}
fn dest_existing(id: &str) -> Value {
    json!({"mode":"existing","bookId":id})
}
fn save(db: &Db, src: &str, msg: &str, dest: Value, files: Value) -> Result<Value> {
    let args = json!({"sourceBookId":src,"messageId":msg,"confirmed":true,"destination":dest,"files":files});
    save_selection(db, &args)
}

/// 新目标独立于源作品；源标题/文件不变；内容取自持久化卡片而非客户端。
#[test]
fn new_destination_is_independent_and_source_untouched() {
    let (_d, db) = open();
    let src = book(&db, "源作品");
    let sid = session(&db, &src);
    insert_card(&db, &sid, "m1", &files2());
    files::write_file(&db, &src, "设定", "原有资料.md", "旧资料").unwrap();
    // 客户端 content 是伪造的，必须被忽略。
    let forged = json!([{"index":0,"group":"设定","name":"建书档案.md","content":"伪造内容"}]);
    let out = save(&db, &src, "m1", dest_new("新作品"), forged).unwrap();
    assert_eq!(out["ok"], true);
    assert_eq!(out["destination"]["created"], true);
    assert_eq!(out["complete"], false);
    assert_eq!(out["savedFiles"].as_array().unwrap().len(), 1);
    let target = out["destination"]["bookId"].as_str().unwrap().to_string();
    assert_ne!(target, src);
    assert_eq!(
        read(&db, &target, "设定", "建书档案.md").as_deref(),
        Some("档案内容")
    );
    // 源作品标题/文件不变，且没有把资料写进源作品。
    assert_eq!(title_of(&db, &src), "源作品");
    assert_eq!(
        read(&db, &src, "设定", "原有资料.md").as_deref(),
        Some("旧资料")
    );
    assert!(read(&db, &src, "设定", "建书档案.md").is_none());
    // 新书有可重开的目标会话与一条确认记录。
    let sessions = list_sessions(&db, &target);
    let tsid = sessions[0]["id"].as_str().unwrap();
    assert!(!list_messages(&db, tsid).as_array().unwrap().is_empty());
    // 源卡持久化绑定与 saved=false。
    let c = card(&db, "m1");
    assert_eq!(c["bookSetup"]["destination"]["bookId"], json!(target));
    assert_eq!(c["bookSetup"]["saved"], json!(false));
    assert_eq!(c["bookSetup"]["files"], files2());
}

/// existing 必须显式 bookId、绝不改名；显式目录覆盖源卡建议目录。
#[test]
fn existing_destination_is_explicit_and_outline_group_wins() {
    let (_d, db) = open();
    let src = book(&db, "源A");
    let exist = book(&db, "已有作品");
    let sid = session(&db, &src);
    insert_card(&db, &sid, "m1", &files2());
    // 不显式 bookId 的 existing 必须拒绝，且不得写入任何文件。
    let err = save(
        &db,
        &src,
        "m1",
        json!({"mode":"existing"}),
        json!([req(0, "设定", "建书档案.md")]),
    )
    .unwrap_err();
    assert!(err.to_string().contains("目标作品"), "{}", err);
    assert!(read(&db, &exist, "设定", "建书档案.md").is_none());
    // 源卡第 2 项标成「设定」，作者显式请求写入「细纲」。
    let files = json!([
        req(0, "设定", "建书档案.md"),
        req(1, "细纲", "第1章细纲.md")
    ]);
    let out = save(&db, &src, "m1", dest_existing(&exist), files).unwrap();
    assert_eq!(out["destination"]["bookId"], json!(exist));
    assert_eq!(out["destination"]["created"], false);
    assert_eq!(out["complete"], true);
    assert_eq!(out["savedFiles"][1]["group"], json!("细纲"));
    assert_eq!(title_of(&db, &exist), "已有作品");
    assert_eq!(
        read(&db, &exist, "设定", "建书档案.md").as_deref(),
        Some("档案内容")
    );
    assert_eq!(
        read(&db, &exist, "细纲", "第1章细纲.md").as_deref(),
        Some("细纲内容")
    );
    assert!(read(&db, &exist, "设定", "第1章细纲.md").is_none());
    assert_eq!(books_len(&db), 2);
}

/// 跨书消息必须拒绝，且不得创建新书或写入任何文件。
#[test]
fn message_from_other_book_is_rejected_without_creating_any_book() {
    let (_d, db) = open();
    let src = book(&db, "源C");
    let other = book(&db, "别的书");
    let sid = session(&db, &src);
    insert_card(&db, &sid, "m1", &files2());
    let before = books_len(&db);
    let files = json!([req(0, "设定", "建书档案.md")]);
    let err = save(&db, &other, "m1", dest_new("不该创建"), files).unwrap_err();
    assert!(err.to_string().contains("不属于"), "{}", err);
    assert_eq!(books_len(&db), before);
    assert!(read(&db, &src, "设定", "建书档案.md").is_none());
}

/// 重复 new 请求只建一次书；改用 existing+已绑定 ID 重试幂等。
#[test]
fn repeated_new_request_binds_once_and_retry_is_idempotent() {
    let (_d, db) = open();
    let src = book(&db, "源D");
    let sid = session(&db, &src);
    insert_card(&db, &sid, "m1", &files2());
    let out1 = save(
        &db,
        &src,
        "m1",
        dest_new("只建一次"),
        json!([req(0, "设定", "建书档案.md")]),
    )
    .unwrap();
    let target = out1["destination"]["bookId"].as_str().unwrap().to_string();
    let after = books_len(&db);
    assert_eq!(after, 2);
    // 双击：再次以 new 提交必须被绑定拒绝，绝不建第二本。
    let again = save(
        &db,
        &src,
        "m1",
        dest_new("只建一次"),
        json!([req(0, "设定", "建书档案.md")]),
    )
    .unwrap_err();
    assert!(again.to_string().contains("绑定"), "{}", again);
    assert_eq!(books_len(&db), after);
    // 客户端改用 existing + 已绑定 ID：幂等成功，不新建书。
    let files = json!([
        req(0, "设定", "建书档案.md"),
        req(1, "细纲", "第1章细纲.md")
    ]);
    let out2 = save(&db, &src, "m1", dest_existing(&target), files).unwrap();
    assert_eq!(out2["destination"]["bookId"], json!(target));
    assert_eq!(out2["complete"], true);
    assert_eq!(books_len(&db), after);
    assert_eq!(card(&db, "m1")["bookSetup"]["saved"], json!(true));
}

/// 同名异内容冲突绝不覆盖；绑定仍持久化，重试不换目标。
#[test]
fn conflicting_existing_file_is_never_overwritten() {
    let (_d, db) = open();
    let src = book(&db, "源E");
    let exist = book(&db, "目标E");
    let sid = session(&db, &src);
    insert_card(&db, &sid, "m1", &files2());
    files::write_file(&db, &exist, "设定", "建书档案.md", "作者现有内容").unwrap();
    let files = json!([req(0, "设定", "建书档案.md")]);
    let err = save(&db, &src, "m1", dest_existing(&exist), files).unwrap_err();
    assert!(err.to_string().contains("拒绝覆盖"), "{}", err);
    assert_eq!(
        read(&db, &exist, "设定", "建书档案.md").as_deref(),
        Some("作者现有内容")
    );
    let c = card(&db, "m1");
    assert_eq!(c["bookSetup"]["saved"], json!(false));
    assert!(c["bookSetup"]["savedFiles"]
        .as_array()
        .map(|a| a.is_empty())
        .unwrap_or(true));
    assert_eq!(c["bookSetup"]["destination"]["bookId"], json!(exist));
}

/// 部分保存必须持久化已完成项；saved 只在全部完成后才为 true；重试可继续。
#[test]
fn partial_save_is_durable_and_saved_only_when_complete() {
    let (_d, db) = open();
    let src = book(&db, "源F");
    let exist = book(&db, "目标F");
    let sid = session(&db, &src);
    insert_card(&db, &sid, "m1", &files2());
    files::write_file(&db, &exist, "细纲", "第2章细纲.md", "作者旧稿").unwrap();
    // 第 1 项成功、第 2 项冲突 => 部分保存落盘，saved 不得提前 true。
    let files = json!([
        req(0, "设定", "建书档案.md"),
        req(1, "细纲", "第2章细纲.md")
    ]);
    let err = save(&db, &src, "m1", dest_existing(&exist), files).unwrap_err();
    assert!(err.to_string().contains("拒绝覆盖"), "{}", err);
    let c = card(&db, "m1");
    assert_eq!(c["bookSetup"]["saved"], json!(false));
    assert_eq!(c["bookSetup"]["savedFiles"].as_array().unwrap().len(), 1);
    assert_eq!(c["bookSetup"]["savedFiles"][0]["index"], json!(0));
    assert_eq!(
        read(&db, &exist, "设定", "建书档案.md").as_deref(),
        Some("档案内容")
    );
    assert_eq!(
        read(&db, &exist, "细纲", "第2章细纲.md").as_deref(),
        Some("作者旧稿")
    );
    // 已保存项不得改存别处。
    let moved = save(
        &db,
        &src,
        "m1",
        dest_existing(&exist),
        json!([req(0, "设定", "改名.md")]),
    )
    .unwrap_err();
    assert!(moved.to_string().contains("不得改存"), "{}", moved);
    // 重试：未保存项改到不冲突的文件名，最终 complete=true。
    let out = save(
        &db,
        &src,
        "m1",
        dest_existing(&exist),
        json!([req(1, "细纲", "第3章细纲.md")]),
    )
    .unwrap();
    assert_eq!(out["complete"], true);
    assert_eq!(out["savedFiles"].as_array().unwrap().len(), 2);
    assert_eq!(card(&db, "m1")["bookSetup"]["saved"], json!(true));
    assert_eq!(
        read(&db, &exist, "细纲", "第3章细纲.md").as_deref(),
        Some("细纲内容")
    );
}

/// 非法/越界/重复/危险目录与未确认请求必须预先拒绝，不产生任何副作用。
#[test]
fn unsafe_requests_are_rejected_before_any_write() {
    let (_d, db) = open();
    let src = book(&db, "源G");
    let sid = session(&db, &src);
    insert_card(&db, &sid, "m1", &files2());
    let before = books_len(&db);
    let cases: Vec<(Value, &str)> = vec![
        (json!([req(0, "正文", "a.md")]), "设定/细纲/参考"),
        (json!([req(0, "正文待审", "a.md")]), "设定/细纲/参考"),
        (json!([req(0, "细纲", "../逃逸.md")]), "非法字符"),
        (
            json!([req(0, "细纲", "a.md"), req(0, "细纲", "b.md")]),
            "重复",
        ),
        (json!([req(9, "细纲", "a.md")]), "越界"),
        // 同一目标路径被两项不同内容占用。
        (
            json!([req(0, "细纲", "同.md"), req(1, "细纲", "同.md")]),
            "同一目标路径",
        ),
    ];
    for (files, needle) in cases {
        let err = save(&db, &src, "m1", dest_new("不该建的书"), files).unwrap_err();
        assert!(err.to_string().contains(needle), "{}", err);
    }
    assert_eq!(books_len(&db), before);
    assert!(read(&db, &src, "细纲", "a.md").is_none());
    // 未确认也拒绝。
    let args = json!({"sourceBookId":src,"messageId":"m1","confirmed":false,
        "destination":dest_new("不该建的书"),"files":[req(0,"细纲","a.md")]});
    let err = save_selection(&db, &args).unwrap_err();
    assert!(err.to_string().contains("确认"), "{}", err);
    assert_eq!(books_len(&db), before);
}

/// 全部保存后 saved=true，且 result 其他字段（extra/titles/files）保持不变。
#[test]
fn full_save_marks_saved_and_keeps_other_result_fields() {
    let (_d, db) = open();
    let src = book(&db, "源H");
    let sid = session(&db, &src);
    let result = json!({"bookSetup":{"titles":["新作品"],"genre":"历史","saved":false,"files":files2()},
                        "extra":{"keep":true}});
    db.exec(
        "INSERT INTO messages(id,session_id,role,content,result_json,created_at) VALUES('m1',?1,'assistant','预览',?2,1)",
        &[&sid as &dyn ToSql, &result.to_string() as &dyn ToSql],
    ).unwrap();
    let files = json!([
        req(0, "设定", "建书档案.md"),
        req(1, "细纲", "第1章细纲.md")
    ]);
    let out = save(&db, &src, "m1", dest_new("新作品"), files).unwrap();
    assert_eq!(out["complete"], true);
    let c = card(&db, "m1");
    assert_eq!(c["bookSetup"]["saved"], json!(true));
    assert_eq!(c["extra"]["keep"], json!(true));
    assert_eq!(c["bookSetup"]["titles"][0], json!("新作品"));
    assert_eq!(c["bookSetup"]["files"], files2());
}
