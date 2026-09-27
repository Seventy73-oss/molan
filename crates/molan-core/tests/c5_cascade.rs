//! C5 回归：draft_dependency 级联失效不得被历史残留 stale 行反复放大。
//!
//! 旧 SQL 以「全书 MIN(stale ch)」为级联起点：一条从未重新登记的历史 stale 行
//! 会让之后每次任意章节变更把整条下游全部重新打 stale。修复后级联只从
//! **本次变更**打 stale 的行（updated_at=now）起算；重新登记依赖可覆写回 valid。
use molan_core::continuity::{content_hash, record_draft_dependency};
use molan_core::{books, db::Db, files};

fn fixture() -> (tempfile::TempDir, Db, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), None).unwrap();
    let book = books::create_book(&db, "c5-test", "玄幻", "第三人称")["id"]
        .as_str()
        .unwrap()
        .to_string();
    (dir, db, book)
}

fn dep_status(db: &Db, book: &str, ch: i64) -> Option<String> {
    db.q_json(
        "SELECT status FROM draft_dependency WHERE book_id=?1 AND ch=?2",
        &[&book, &ch],
    )
    .unwrap()
    .first()
    .and_then(|r| r["status"].as_str().map(str::to_string))
}

#[test]
fn historical_stale_row_does_not_amplify_later_change() {
    let (_d, db, book) = fixture();
    // 历史残留：ch=3 的依赖很早以前被打 stale，从未重新登记
    db.exec(
        "INSERT INTO draft_dependency(book_id,ch,parent_ch,parent_hash,task_id,status,updated_at) VALUES(?1,3,2,'h2','task-old','stale',123)",
        &[&book],
    )
    .unwrap();
    // 现行有效依赖：ch=5 与 ch=8
    files::write_file(&db, &book, "正文", "第4章.md", "四章定稿").unwrap();
    record_draft_dependency(&db, &book, 5, 4, &content_hash("四章定稿"), "task-a").unwrap();
    files::write_file(&db, &book, "正文", "第7章.md", "七章定稿").unwrap();
    record_draft_dependency(&db, &book, 8, 7, &content_hash("七章定稿"), "task-b").unwrap();
    assert_eq!(dep_status(&db, &book, 5).as_deref(), Some("valid"));
    assert_eq!(dep_status(&db, &book, 8).as_deref(), Some("valid"));

    // 本次变更发生在 ch=10：先建立 ch=11 依赖，再改第10章正文
    files::write_file(&db, &book, "正文", "第10章.md", "十章初稿").unwrap();
    record_draft_dependency(&db, &book, 11, 10, &content_hash("十章初稿"), "task-c").unwrap();
    files::write_file(&db, &book, "正文", "第10章.md", "十章修订稿").unwrap();

    // 直接下游 ch=11 必须 stale；无关的 ch=5/8 不得被历史 stale 行放大波及
    assert_eq!(dep_status(&db, &book, 11).as_deref(), Some("stale"));
    assert_eq!(
        dep_status(&db, &book, 5).as_deref(),
        Some("valid"),
        "历史 stale 行不得放大本次变更（C5）"
    );
    assert_eq!(dep_status(&db, &book, 8).as_deref(), Some("valid"));
    assert_eq!(dep_status(&db, &book, 3).as_deref(), Some("stale"));

    // 重新登记依赖 → 覆写回 valid（ch=11 的父稿现为修订稿）
    record_draft_dependency(&db, &book, 11, 10, &content_hash("十章修订稿"), "task-c").unwrap();
    assert_eq!(dep_status(&db, &book, 11).as_deref(), Some("valid"));
}

#[test]
fn same_content_promotion_does_not_stale_dependents() {
    // 内容一致的「待审→正式」提升不是语义变更：下游依赖保持 valid
    let (_d, db, book) = fixture();
    files::write_file(&db, &book, "正文", "第1章.md", "一章定稿").unwrap();
    record_draft_dependency(&db, &book, 2, 1, &content_hash("一章定稿"), "task-a").unwrap();
    // 待审稿重写为同内容（触发 note_file_change，正文未动）
    files::write_file(&db, &book, "正文待审", "第2章.md", "二章待审").unwrap();
    files::write_file(&db, &book, "正文待审", "第2章.md", "二章待审改").unwrap();
    assert_eq!(
        dep_status(&db, &book, 2).as_deref(),
        Some("valid"),
        "改动待审稿自身不得 stale 自己对第1章的依赖"
    );
}
