// molan-core: SQLite 层。与 Node 版 lib/db.js + handlers-core.js 行为对齐。
use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct Db {
    pub conn: Mutex<Connection>,
    /// 文件变更/导出串行锁。锁序固定为 fs_lock -> conn，绝不反向获取。
    pub fs_lock: Mutex<()>,
    pub data_dir: PathBuf,
    pub books_dir: PathBuf,
    pub versions_dir: PathBuf,
    pub trash_dir: PathBuf,
}

pub const GROUPS: [(&str, &str, &str, &str); 4] = [
    ("settings", "设定资料", "设定", "green"),
    ("outline", "章节细纲", "细纲", "amber"),
    ("chapters", "小说正文", "正文", "gray"),
    ("reference", "参考拆解", "参考", "muted"),
];

/// AI 自动写作产出先进"正文待审"组，人工接受后才转正到"正文"
pub const REVIEW_GROUP: &str = "正文待审";

impl Db {
    pub fn open(data_dir: &Path, seed: Option<&Path>) -> Result<Db> {
        std::fs::create_dir_all(data_dir)?;
        let db_path = data_dir.join("writerx.db");
        if !db_path.exists() {
            if let Some(s) = seed {
                if s.exists() && s != db_path {
                    // 先拷临时名再原子 rename：拷贝中途崩溃不会留下半写的主库被下次启动直接打开
                    let tmp = data_dir.join("writerx.db.seedtmp");
                    let _ = std::fs::remove_file(&tmp);
                    std::fs::copy(s, &tmp)?;
                    std::fs::rename(&tmp, &db_path).inspect_err(|_e| {
                        let _ = std::fs::remove_file(&tmp);
                    })?;
                }
            }
        }
        let conn = Connection::open(&db_path)?;
        // busy_timeout：并发写竞争时让后到者等待而非立即 SQLITE_BUSY 报错（WAL 下写锁仍唯一）
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=OFF; PRAGMA busy_timeout=5000;",
        )?;
        let db = Db {
            conn: Mutex::new(conn),
            fs_lock: Mutex::new(()),
            data_dir: data_dir.to_path_buf(),
            books_dir: data_dir.join("books"),
            versions_dir: data_dir.join("versions"),
            trash_dir: data_dir.join("trash"),
        };
        db.migrate()?;
        // 版本记忆相关新表（父代理 continuity 模块负责 schema/语义）
        crate::continuity::ensure_schema(&db)?;
        crate::deepwrite::ensure_schema(&db)?;
        crate::chapter_state::ensure_schema(&db)?;
        crate::facts::ensure_schema(&db)?;
        crate::ctx_manifest::ensure_schema(&db)?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        // 核心六表：seed 库可能不带（全新环境），缺失时补建，保证新部署可用
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS books (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL DEFAULT '',
                genre TEXT,
                pov TEXT,
                status TEXT,
                cover_char TEXT,
                word_count INTEGER NOT NULL DEFAULT 0,
                chapter_count INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER,
                updated_at INTEGER
            );
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                book_id TEXT,
                title TEXT,
                preview TEXT,
                msg_count INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER,
                updated_at INTEGER
            );
            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                session_id TEXT,
                role TEXT,
                content TEXT,
                context_json TEXT,
                steps_json TEXT,
                result_json TEXT,
                interrupted INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER
            );
            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT
            );
            CREATE TABLE IF NOT EXISTS word_stats (
                date TEXT PRIMARY KEY,
                start_words INTEGER NOT NULL DEFAULT 0,
                latest_words INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS skills (
                id TEXT PRIMARY KEY,
                name TEXT,
                description TEXT,
                prompt_template TEXT,
                kind TEXT,
                source TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                builtin_key TEXT,
                usage_mode TEXT,
                origin TEXT,
                targets_json TEXT
            );",
        )?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS book_meta (
                book_id TEXT PRIMARY KEY,
                style_json TEXT,
                humanize INTEGER NOT NULL DEFAULT 0,
                primary_skill TEXT,
                support_skills_json TEXT NOT NULL DEFAULT '[]',
                lead_text TEXT
            );",
        )?;
        // books 扩展列（deleted_at / deleted_reason）
        let has_deleted = |name: &str| -> bool {
            let mut stmt = conn.prepare("PRAGMA table_info(books)").unwrap();
            let cols: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            cols.iter().any(|c| c == name)
        };
        if !has_deleted("deleted_at") {
            let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN deleted_at INTEGER");
        }
        if !has_deleted("deleted_reason") {
            let _ = conn.execute_batch("ALTER TABLE books ADD COLUMN deleted_reason TEXT");
        }
        // sessions 扩展列：老库可能缺 created_at。缺列时 create_session 的 INSERT 会失败，
        // 会话从未真正落库（用户看到"会话消失"）；补列失败必须让启动报错，绝不带病运行。
        {
            let mut stmt = conn.prepare("PRAGMA table_info(sessions)")?;
            let cols: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .filter_map(|r| r.ok())
                .collect();
            if !cols.iter().any(|c| c == "created_at") {
                conn.execute_batch("ALTER TABLE sessions ADD COLUMN created_at INTEGER")?;
            }
        }
        // 自动写作任务持久化：进度落盘，容器重启后可查可续跑
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS auto_task (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                book_id TEXT NOT NULL,
                session_id TEXT,
                from_ch INTEGER NOT NULL DEFAULT 0,
                to_ch INTEGER NOT NULL DEFAULT 0,
                current_ch INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'running',
                error TEXT,
                created_at INTEGER,
                updated_at INTEGER
            );",
        )?;
        // 启动时把上次异常中断的 running 任务标记为 interrupted
        let _ = conn.execute(
            "UPDATE auto_task SET status='interrupted', error='服务重启中断，可从断点续跑', updated_at=?1 WHERE status='running'",
            rusqlite::params![chrono::Utc::now().timestamp_millis()],
        );
        // 审批队列：AI 生成待人工接受的章节
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pending_chapter (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                book_id TEXT NOT NULL,
                ch INTEGER NOT NULL,
                review_file TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at INTEGER,
                updated_at INTEGER,
                UNIQUE(book_id, ch)
            );",
        )?;
        // LLM 调用用量落账：每次调用的 token 消耗（细纲/正文/审核/重写/人物状态/伏笔/摘要/体检）
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS llm_call_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                book_id TEXT NOT NULL DEFAULT '',
                tag TEXT NOT NULL DEFAULT '',
                model TEXT NOT NULL DEFAULT '',
                prompt_tokens INTEGER NOT NULL DEFAULT 0,
                completion_tokens INTEGER NOT NULL DEFAULT 0,
                total_tokens INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        // 索引（messages/sessions 由种子库提供，全新空库可能不存在——失败忽略，不阻断启动）
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, created_at)",
            [],
        );
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_llm_call_log_ts ON llm_call_log(ts)",
            [],
        );
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_llm_call_log_book ON llm_call_log(book_id, ts)",
            [],
        );
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_sessions_book ON sessions(book_id)",
            [],
        );
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_pending_book_ch ON pending_chapter(book_id, ch)",
            [],
        );
        let _ = conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_auto_task_book ON auto_task(book_id, id)",
            [],
        );
        Ok(())
    }

    pub fn q_json(
        &self,
        sql: &str,
        params: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(sql)?;
        let col_count = stmt.column_count();
        let col_names: Vec<String> = (0..col_count)
            .map(|i| {
                stmt.column_name(i)
                    .map(|n| n.to_string())
                    .unwrap_or_default()
            })
            .collect();
        let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let mut obj = serde_json::Map::new();
            for (i, name) in col_names.iter().enumerate() {
                let v: serde_json::Value = match row.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                    rusqlite::types::ValueRef::Integer(n) => serde_json::Value::from(n),
                    rusqlite::types::ValueRef::Real(f) => serde_json::Value::from(f),
                    rusqlite::types::ValueRef::Text(t) => {
                        serde_json::Value::from(String::from_utf8_lossy(t).to_string())
                    }
                    rusqlite::types::ValueRef::Blob(b) => {
                        serde_json::Value::from(String::from_utf8_lossy(b).to_string())
                    }
                };
                obj.insert(snake_to_camel(name), v);
            }
            out.push(serde_json::Value::Object(obj));
        }
        Ok(out)
    }

    pub fn exec(&self, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(sql)?;
        Ok(stmt.execute(rusqlite::params_from_iter(params.iter()))?)
    }
}

pub fn snake_to_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut upper = false;
    for c in s.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_copy_is_atomic_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let seed_dir = tempfile::tempdir().unwrap();
        // 先造一个合法 seed 库
        let seed_path = seed_dir.path().join("writerx.db");
        {
            let c = Connection::open(&seed_path).unwrap();
            c.execute_batch("CREATE TABLE t(x);").unwrap();
        }
        let db = Db::open(dir.path(), Some(&seed_path)).unwrap();
        drop(db);
        // 主库已建、临时文件不得残留
        assert!(dir.path().join("writerx.db").exists());
        assert!(!dir.path().join("writerx.db.seedtmp").exists());
    }

    /// 老库 sessions 表缺 created_at 列时，open 必须补列，且 create_session 真正落库。
    /// （生产事故：缺列导致 INSERT 静默失败，新会话"消失"。）
    #[test]
    fn legacy_sessions_without_created_at_is_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("writerx.db");
        {
            let c = Connection::open(&db_path).unwrap();
            c.execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY, book_id TEXT NOT NULL, title TEXT,
                    preview TEXT, msg_count INTEGER DEFAULT 0, updated_at INTEGER
                );",
            )
            .unwrap();
        }
        let db = Db::open(dir.path(), None).unwrap();
        let book = crate::books::create_book(&db, "t", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        let s = crate::books::create_session(&db, &book, "s1").unwrap();
        let sid = s["id"].as_str().unwrap().to_string();
        let listed = crate::books::list_sessions(&db, &book);
        assert!(
            listed.as_array().unwrap().iter().any(|r| r["id"] == sid),
            "create_session 之后 list_sessions 必须能看到新会话"
        );
    }

    #[test]
    fn busy_timeout_pragma_is_applied() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let conn = db.conn.lock().unwrap();
        let v: i64 = conn
            .query_row("PRAGMA busy_timeout;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 5000);
    }
}
