//! Crash/reload recovery for chat text only; never writes manuscript files.
use molan_core::{db::Db, stats};
use serde_json::json;
pub(super) struct Snapshot<'a> {
    db: &'a Db,
    id: String,
    session: String,
    context: String,
    latest: String,
    last: Option<std::time::Instant>,
}
impl<'a> Snapshot<'a> {
    pub(super) fn new(db: &'a Db, id: &str, session: &str, request: &str) -> Self {
        Self {
            db,
            id: id.into(),
            session: session.into(),
            context: json!({"requestId":request,"files":[],"skills":[]}).to_string(),
            latest: String::new(),
            last: None,
        }
    }
    pub(super) fn capture(&mut self, full: &str) {
        self.latest = full.into();
        if self.last.is_none_or(|t| t.elapsed().as_millis() >= 500) {
            self.flush();
            self.last = Some(std::time::Instant::now());
        }
    }
    fn flush(&self) {
        if self.latest.is_empty() {
            return;
        }
        if let Err(e)=self.db.exec("INSERT INTO messages(id,session_id,role,content,context_json,created_at,interrupted) SELECT ?1,?2,'assistant',?3,?4,?5,1 WHERE EXISTS(SELECT 1 FROM sessions s JOIN books b ON b.id=s.book_id WHERE s.id=?2 AND b.deleted_at IS NULL) ON CONFLICT(id) DO UPDATE SET content=excluded.content", &[&self.id as &dyn rusqlite::ToSql,&self.session,&self.latest,&self.context,&stats::now_ms()]) { eprintln!("[chat snapshot] {e}"); }
    }
}
impl Drop for Snapshot<'_> {
    fn drop(&mut self) {
        self.flush();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deleted_session_cannot_be_recreated_by_late_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let book = molan_core::books::create_book(&db, "测试", "悬疑", "第三人称");
        let session =
            molan_core::books::create_session(&db, book["id"].as_str().unwrap(), "测试").unwrap();
        let sid = session["id"].as_str().unwrap();
        let mut snapshot = Snapshot::new(&db, "late", sid, "request");
        snapshot.capture("首段");
        molan_core::books::delete_session(&db, sid).unwrap();
        snapshot.capture("迟到的尾段");
        drop(snapshot);
        assert!(db
            .q_json("SELECT id FROM messages WHERE session_id=?1", &[&sid])
            .unwrap()
            .is_empty());
    }
    #[test]
    fn checkpoints_are_one_incomplete_message_and_drop_flushes_tail() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let book = molan_core::books::create_book(&db, "测试", "悬疑", "第三人称");
        let session =
            molan_core::books::create_session(&db, book["id"].as_str().unwrap(), "测试").unwrap();
        let sid = session["id"].as_str().unwrap();
        {
            let mut snapshot = Snapshot::new(&db, "assistant-id", sid, "request-id");
            snapshot.capture("首段");
            let rows = db
                .q_json(
                    "SELECT content,interrupted FROM messages WHERE id='assistant-id'",
                    &[],
                )
                .unwrap();
            assert_eq!(rows[0]["content"], "首段");
            assert_eq!(rows[0]["interrupted"], 1);
            snapshot.capture("首段与尚未到节流时间的尾段");
        }
        let rows = db
            .q_json("SELECT content FROM messages WHERE session_id=?1", &[&sid])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["content"], "首段与尚未到节流时间的尾段");
        assert!(
            molan_core::files::scan_tree(&db, book["id"].as_str().unwrap())
                .as_array()
                .unwrap()
                .iter()
                .all(|g| g["files"].as_array().unwrap().is_empty())
        );
    }
}
