//! 整本书导入（txt / 文件夹 / zip 共用）。
use crate::db::Db;
use crate::stats::now_ms;
use rusqlite::ToSql;
use serde_json::{json, Value};

/// 导入整本书（txt / 文件夹 / zip 共用）：建书 → 逐章写入「正文」并标注 import 来源。
/// 文件系统与数据库没有全局事务：任一章写入失败时，已建的半成品作品整体移入作品回收站（可恢复），
/// 并报告失败的文件，绝不留下残缺作品冒充导入成功。
pub fn import_book(db: &Db, title: &str, genre: &str, files: &[Value]) -> anyhow::Result<Value> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_ms();
    let cover = title
        .chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "书".into());
    db.exec(
        "INSERT INTO books(id,title,genre,pov,status,cover_char,word_count,chapter_count,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?)",
        &[&id as &dyn ToSql, &title, &genre, &"第三人称", &"构思中", &cover, &0i64, &0i64, &now, &now],
    )?;
    let written = (|| -> anyhow::Result<usize> {
        crate::files::ensure_book_dir(db, &id)?;
        let mut n = 0;
        for f in files {
            let name = f["name"].as_str().unwrap_or("章节.md");
            let content = f["content"].as_str().unwrap_or("");
            #[cfg(test)]
            if tests::FAIL_ON.with(|x| x.borrow().as_deref() == Some(name)) {
                anyhow::bail!("「{}」写入失败：测试注入故障", name);
            }
            crate::files::write_file(db, &id, "正文", name, content)
                .map_err(|e| anyhow::anyhow!("「{}」写入失败：{}", name, e))?;
            n += 1;
            // 导入章标注来源：未经系统批准的章节不得被状态机当作 AI 定稿链推进（C3）
            if let Some(ch) = crate::continuity::chapter_number(name) {
                if let Err(e) = crate::chapter_state::record_origin(db, &id, ch, "import") {
                    eprintln!("[molan-core] 导入章 origin 记账失败（不阻断导入）：{}", e);
                }
            }
        }
        Ok(n)
    })();
    match written {
        Ok(_) => Ok(json!({"bookId": id})),
        Err(e) => {
            let _ = db.exec(
                "UPDATE books SET deleted_at=?, deleted_reason='import_failed' WHERE id=?",
                &[&now_ms() as &dyn ToSql, &id],
            );
            anyhow::bail!("导入未完成：{}。已写入的部分随作品《{}》移入作品回收站（可恢复），书库中不会出现残缺作品。", e, title)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        pub(super) static FAIL_ON: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    }

    #[test]
    fn failed_import_moves_partial_book_to_trash() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let ok = import_book(
            &db,
            "好书",
            "玄幻",
            &[json!({"name": "第1章.md", "content": "正文一"})],
        )
        .unwrap();
        let id = ok["bookId"].as_str().unwrap();
        assert_eq!(
            crate::files::read_file(&db, id, "正文", "第1章.md").as_deref(),
            Some("正文一")
        );
        // 第二章写入失败（注入故障）：整本移入回收站，书库不出现残缺作品
        FAIL_ON.with(|x| *x.borrow_mut() = Some("第2章.md".into()));
        let e = import_book(
            &db,
            "坏书",
            "玄幻",
            &[
                json!({"name": "第1章.md", "content": "一"}),
                json!({"name": "第2章.md", "content": "二"}),
            ],
        )
        .unwrap_err();
        assert!(e.to_string().contains("移入作品回收站"), "{}", e);
        let live = db
            .q_json("SELECT title FROM books WHERE deleted_at IS NULL", &[])
            .unwrap();
        assert!(live.iter().all(|r| r["title"] != "坏书"));
        let trashed = db
            .q_json("SELECT deleted_reason FROM books WHERE title='坏书'", &[])
            .unwrap();
        assert_eq!(trashed[0]["deletedReason"], "import_failed");
    }
}
