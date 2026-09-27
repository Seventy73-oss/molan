// molan-core: 字数统计 / 时间工具
//
// V1 契约：统计是派生数据，必须“按书以磁盘正式稿为准重算”，
// 删除/恢复/改名/零字变更都要反映，today = latest - start（与 week 同口径）。
use crate::db::Db;
use serde_json::json;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 正式章节文件判定：第N章.md（排除 AI 来源副本与场景拼稿）。
fn is_chapter_file(name: &str) -> bool {
    if !name.ends_with(".md") || name.ends_with(".ai.md") || name.starts_with("场景_") {
        return false;
    }
    crate::continuity::chapter_number(name).is_some()
}

fn count_chars(content: &str) -> i64 {
    content.chars().filter(|c| !c.is_whitespace()).count() as i64
}

/// 磁盘正式稿总字数（不含空白），以“正文”目录为准。
pub fn book_chars(db: &Db, book_id: &str) -> i64 {
    let dir = db
        .books_dir
        .join(crate::files::safe_name(book_id))
        .join("正文");
    let mut chars = 0i64;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().to_string();
            if !is_chapter_file(&name) {
                continue;
            }
            if let Ok(c) = std::fs::read_to_string(e.path()) {
                chars += count_chars(&c);
            }
        }
    }
    chars
}

pub fn chapter_count(db: &Db, book_id: &str) -> i64 {
    let dir = db
        .books_dir
        .join(crate::files::safe_name(book_id))
        .join("正文");
    std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| {
                    // 必须是普通文件：目录/链接即使叫「第N章.md」也不算章节
                    e.file_type().map(|t| t.is_file()).unwrap_or(false)
                        && is_chapter_file(&e.file_name().to_string_lossy())
                })
                .count() as i64
        })
        .unwrap_or(0)
}

/// 单次锁内完成：本书按磁盘重算 + 全局日账更新。
/// word_stats 是**跨书全局**日账（date 为 PK），latest 必须是所有未删除书的合计，
/// 不能写成单本字数（否则 A/B 两书交替写入会让今日增量来回跳）。
/// 新日基线取“写前全局总量”，这样当天首写的新增字数会被正确计入。
pub fn refresh_words(db: &Db, book_id: &str) {
    if book_id.is_empty() {
        return;
    }
    let d = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let chars = book_chars(db, book_id);
    let ch_count = chapter_count(db, book_id);
    let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
    // 本书写前存量与全局写前总量（用于本次增量与当日基线）
    let old: i64 = conn
        .query_row(
            "SELECT COALESCE(word_count,0) FROM books WHERE id=?",
            [&book_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let prev_total: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(word_count),0) FROM books WHERE deleted_at IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let delta = chars - old;
    let _ = conn.execute(
        "UPDATE books SET word_count=?, chapter_count=?, updated_at=? WHERE id=?",
        rusqlite::params![chars, ch_count, now_ms(), book_id],
    );
    let new_total = prev_total + delta;
    let existing: Option<i64> = conn
        .query_row(
            "SELECT latest_words FROM word_stats WHERE date=?",
            [&d],
            |r| r.get(0),
        )
        .ok();
    if existing.is_none() {
        // 当日首写：基线 = 写前全局总量（不是写完后的值），否则首章新增会被吞掉
        let _ = conn.execute(
            "INSERT INTO word_stats(date,start_words,latest_words) VALUES(?,?,?)",
            rusqlite::params![&d, prev_total, new_total],
        );
    } else {
        let _ = conn.execute(
            "UPDATE word_stats SET latest_words=? WHERE date=?",
            rusqlite::params![new_total, &d],
        );
    }
}

/// 兼容旧调用方：历史上这里做“增量累加”。增量无法表达删除与负值（审计 F21），
/// 现按契约退化为“按书重算”，保留签名以免打断尚未迁移的调用点。
pub fn refresh_words_delta(db: &Db, book_id: &str, _delta: i64) {
    refresh_words(db, book_id);
}

/// 跨书全局日账：重算给定书籍并把当日 latest 置为所有未删除书的磁盘字数合计。
pub fn refresh_words_all(db: &Db) {
    let rows = db
        .q_json("SELECT id FROM books WHERE deleted_at IS NULL", &[])
        .unwrap_or_default();
    let mut total = 0i64;
    for r in &rows {
        let id = r["id"].as_str().unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let chars = book_chars(db, id);
        let ch_count = chapter_count(db, id);
        total += chars;
        let _ = db.exec(
            "UPDATE books SET word_count=?, chapter_count=? WHERE id=?",
            &[&chars as &dyn rusqlite::ToSql, &ch_count, &id],
        );
    }
    let d = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
    let existing: Option<i64> = conn
        .query_row(
            "SELECT latest_words FROM word_stats WHERE date=?",
            [&d],
            |r| r.get(0),
        )
        .ok();
    if existing.is_none() {
        // 全量重建：基线取昨日 latest（无则视为与今日相同，避免凭空造出增量）
        let prev: Option<i64> = conn
            .query_row(
                "SELECT latest_words FROM word_stats WHERE date<? ORDER BY date DESC LIMIT 1",
                [&d],
                |r| r.get(0),
            )
            .ok();
        let start = prev.unwrap_or(total);
        let _ = conn.execute(
            "INSERT INTO word_stats(date,start_words,latest_words) VALUES(?,?,?)",
            rusqlite::params![&d, start, total],
        );
    } else {
        let _ = conn.execute(
            "UPDATE word_stats SET latest_words=? WHERE date=?",
            rusqlite::params![total, &d],
        );
    }
}

/// 用磁盘真实值重建整张日账表（清空后按当日重算），供 reset/purge 等清理后对齐。
pub fn rebuild_word_stats(db: &Db) {
    let _ = db.exec("DELETE FROM word_stats", &[]);
    refresh_words_all(db);
}

pub fn get_writing_stats(db: &Db) -> serde_json::Value {
    let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
    let daily: Vec<serde_json::Value> = {
        let mut stmt = match conn.prepare(
            "SELECT date, start_words, latest_words FROM word_stats ORDER BY date DESC LIMIT 7",
        ) {
            Ok(s) => s,
            Err(_) => return json!({"totalWords": 0, "todayWords": 0, "weekWords": 0, "daily": []}),
        };
        stmt.query_map([], |r| {
            Ok(json!({
                "date": r.get::<_, String>(0)?,
                "startWords": r.get::<_, i64>(1)?,
                "latestWords": r.get::<_, i64>(2)?,
            }))
        })
        .map(|it| it.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    };
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    // today = latest - start（与 week 同口径；旧实现直接返回 latest 造成“基线100+新增5=105”）
    let today_words: i64 = daily
        .iter()
        .find(|d| d["date"].as_str() == Some(today.as_str()))
        .map(|d| {
            (d["latestWords"].as_i64().unwrap_or(0) - d["startWords"].as_i64().unwrap_or(0)).max(0)
        })
        .unwrap_or(0);
    let total: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(word_count),0) FROM books WHERE deleted_at IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let week: i64 = daily
        .iter()
        .map(|d| {
            (d["latestWords"].as_i64().unwrap_or(0) - d["startWords"].as_i64().unwrap_or(0)).max(0)
        })
        .sum();
    json!({"totalWords": total, "todayWords": today_words, "weekWords": week, "daily": daily})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let book = crate::books::create_book(&db, "stats-test", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        (dir, db, book)
    }

    #[test]
    fn write_delete_and_alias_all_update_stats() {
        let (_d, db, book) = fixture();
        // 用英文 key 写入：落盘“正文”，统计必须同步（修复 F21 组别名漏统计）
        crate::files::write_file(&db, &book, "chapters", "第1章.md", "甲乙丙丁").unwrap();
        let row = db
            .q_json(
                "SELECT word_count, chapter_count FROM books WHERE id=?1",
                &[&book],
            )
            .unwrap();
        assert_eq!(row[0]["wordCount"].as_i64().unwrap(), 4);
        assert_eq!(row[0]["chapterCount"].as_i64().unwrap(), 1);
        // 用中文标签写入同样生效
        crate::files::write_file(&db, &book, "小说正文", "第2章.md", "戊己").unwrap();
        let row = db
            .q_json(
                "SELECT word_count, chapter_count FROM books WHERE id=?1",
                &[&book],
            )
            .unwrap();
        assert_eq!(row[0]["wordCount"].as_i64().unwrap(), 6);
        assert_eq!(row[0]["chapterCount"].as_i64().unwrap(), 2);
        // 删除必须回退统计（修复 F21 删除不减）
        crate::files::delete_file(&db, &book, "正文", "第1章.md").unwrap();
        let row = db
            .q_json(
                "SELECT word_count, chapter_count FROM books WHERE id=?1",
                &[&book],
            )
            .unwrap();
        assert_eq!(row[0]["wordCount"].as_i64().unwrap(), 2);
        assert_eq!(row[0]["chapterCount"].as_i64().unwrap(), 1);
    }

    #[test]
    fn ai_copy_and_scene_are_not_chapters() {
        let (_d, db, book) = fixture();
        crate::files::write_file(&db, &book, "正文", "第1章.md", "甲乙丙").unwrap();
        crate::files::write_file(&db, &book, "正文", "第1章.ai.md", "甲乙丙丁戊己庚").unwrap();
        crate::files::write_file(&db, &book, "正文", "场景_abc.md", "场景文本").unwrap();
        let row = db
            .q_json(
                "SELECT word_count, chapter_count FROM books WHERE id=?1",
                &[&book],
            )
            .unwrap();
        assert_eq!(row[0]["chapterCount"].as_i64().unwrap(), 1);
        assert_eq!(row[0]["wordCount"].as_i64().unwrap(), 3);
    }

    #[test]
    fn today_is_delta_not_absolute() {
        let (_d, db, book) = fixture();
        crate::files::write_file(&db, &book, "正文", "第1章.md", "甲乙丙丁戊").unwrap();
        let s = get_writing_stats(&db);
        assert_eq!(s["totalWords"].as_i64().unwrap(), 5);
        // 当日基线 = 写前全局总量(0)，today 为本次新增 5（不是绝对值语义）
        assert_eq!(s["todayWords"].as_i64().unwrap(), 5);
    }

    #[test]
    fn global_day_ledger_is_not_per_book_and_first_write_counts() {
        // P0 回归：word_stats 是全局日账；A/B 两书交替写入不得让 today 来回跳
        let (_d, db, a) = fixture();
        let b = crate::books::create_book(&db, "第二本", "都市", "第一人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        // A 首写 100 字：当日基线=写前全局总量(0)，today 应为 100
        crate::files::write_file(&db, &a, "正文", "第1章.md", &"甲".repeat(100)).unwrap();
        let s1 = get_writing_stats(&db);
        assert_eq!(s1["totalWords"].as_i64().unwrap(), 100);
        assert_eq!(
            s1["todayWords"].as_i64().unwrap(),
            100,
            "首章新增必须计入当日"
        );
        // B 写 10 字：全局 today 应为 110，而不是被覆盖成 10
        crate::files::write_file(&db, &b, "正文", "第1章.md", &"乙".repeat(10)).unwrap();
        let s2 = get_writing_stats(&db);
        assert_eq!(s2["totalWords"].as_i64().unwrap(), 110);
        assert_eq!(s2["todayWords"].as_i64().unwrap(), 110, "跨书日账必须累计");
        // A 再写：today 继续单调累计，不得回落到 A 的单书字数
        crate::files::write_file(&db, &a, "正文", "第2章.md", &"丙".repeat(5)).unwrap();
        let s3 = get_writing_stats(&db);
        assert_eq!(s3["totalWords"].as_i64().unwrap(), 115);
        assert_eq!(
            s3["todayWords"].as_i64().unwrap(),
            115,
            "两书交替后 today 不得跳变"
        );
    }

    #[test]
    fn directory_named_like_chapter_is_not_counted() {
        // chapter_count 必须要求是普通文件
        let (_d, db, book) = fixture();
        let dir = db.books_dir.join(&book).join("正文").join("第9章.md");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(chapter_count(&db, &book), 0, "目录不得计为章节");
        crate::files::write_file(&db, &book, "正文", "第1章.md", "甲乙丙").unwrap();
        assert_eq!(chapter_count(&db, &book), 1);
    }
}
