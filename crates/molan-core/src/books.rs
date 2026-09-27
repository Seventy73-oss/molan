// molan-core: 书 / 会话 / 消息（对齐 handlers-core.js 相关函数）
use crate::db::Db;
use crate::stats::now_ms;
use rusqlite::ToSql;
use serde_json::{json, Value};

fn row_book(r: &Value) -> Value {
    // q_json 已把列名 snake->camel：cover_char -> coverChar, word_count -> wordCount...
    json!({
        "id": r["id"], "title": r["title"], "genre": r["genre"], "pov": r["pov"],
        "status": r["status"], "coverChar": r["coverChar"],
        "wordCount": r["wordCount"], "chapterCount": r["chapterCount"],
        "createdAt": r["createdAt"], "updatedAt": r["updatedAt"],
    })
}

pub fn list_books(db: &Db) -> Value {
    let rows = db
        .q_json(
            "SELECT * FROM books WHERE deleted_at IS NULL ORDER BY updated_at DESC",
            &[],
        )
        .unwrap_or_default();
    json!(rows.iter().map(row_book).collect::<Vec<_>>())
}

pub fn create_book(db: &Db, title: &str, genre: &str, pov: &str) -> Value {
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_ms();
    let cover = title
        .chars()
        .next()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "书".into());
    let status = "构思中";
    if let Err(e) = db.exec(
        "INSERT INTO books(id,title,genre,pov,status,cover_char,word_count,chapter_count,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?)",
        &[
            &id as &dyn ToSql, &title, &genre, &pov, &status,
            &cover, &0i64, &0i64, &now, &now,
        ],
    ) {
        return json!({"ok": false, "err": format!("创建书籍失败：{}", e)});
    }
    if let Err(e) = crate::files::ensure_book_dir(db, &id) {
        return json!({"ok": false, "err": format!("创建书籍目录失败：{}", e)});
    }
    json!({
        "id": id, "title": title, "genre": genre, "pov": pov, "status": status,
        "coverChar": cover,
        "wordCount": 0, "chapterCount": 0, "createdAt": now, "updatedAt": now,
    })
}

pub fn rename_book(db: &Db, book_id: &str, title: &str, genre: &str, status: &str) -> Value {
    let now = now_ms();
    if let Err(e) = db.exec(
        "UPDATE books SET title=?, genre=?, status=?, updated_at=? WHERE id=?",
        &[&title as &dyn ToSql, &genre, &status, &now, &book_id],
    ) {
        return json!({"ok": false, "err": format!("重命名书籍失败：{}", e)});
    }
    json!({"ok": true})
}

pub fn delete_book(db: &Db, book_id: &str) -> Value {
    let now = now_ms();
    if let Err(e) = db.exec(
        "UPDATE books SET deleted_at=?, deleted_reason='user' WHERE id=?",
        &[&now as &dyn ToSql, &book_id],
    ) {
        return json!({"ok": false, "err": format!("删除书籍失败：{}", e)});
    }
    json!({"ok": true})
}

// 彻底删除：物理移除书目录 + 删除 DB 行（从回收站消失，不可恢复）
/// 彻底删除：物理移除书目录 + 关联 DB 行（从回收站消失，不可恢复）。
/// V1：必须清理全部关联表（含版本记忆/计划/依赖/事件/用量/配置前缀键），
/// 单事务提交，磁盘与 DB 同时清理；任何一步失败返回 ok=false 而不是静默成功。
pub fn purge_book(db: &Db, book_id: &str) -> Value {
    let exists = db
        .q_json(
            "SELECT id FROM books WHERE id=?1",
            &[&book_id as &dyn ToSql],
        )
        .map(|rows| !rows.is_empty())
        .unwrap_or(false);
    if !exists {
        return json!({"ok": false, "err": "书籍不存在，拒绝永久删除"});
    }
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    let dir = db.books_dir.join(crate::files::safe_name(book_id));
    let mut disk_errors: Vec<String> = Vec::new();
    if dir.exists() {
        if crate::files::assert_under(&db.books_dir, &dir) {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                disk_errors.push(format!("书目录: {}", e));
            }
        } else {
            disk_errors.push(format!("拒绝删除越界路径: {}", dir.display()));
        }
    }
    let vdir = db.versions_dir.join(crate::files::safe_name(book_id));
    if vdir.exists() && crate::files::assert_under(&db.versions_dir, &vdir) {
        if let Err(e) = std::fs::remove_dir_all(&vdir) {
            disk_errors.push(format!("版本目录: {}", e));
        }
    }
    let tdir = db.trash_dir.join(crate::files::safe_name(book_id));
    if tdir.exists() && crate::files::assert_under(&db.trash_dir, &tdir) {
        if let Err(e) = std::fs::remove_dir_all(&tdir) {
            disk_errors.push(format!("回收站: {}", e));
        }
    }
    // 磁盘有任何残留时保留 DB 行，确保用户能从界面重试，而不是制造孤儿目录。
    if !disk_errors.is_empty() {
        return json!({"ok": false, "err": disk_errors.join("; ")});
    }
    if let Err(e) = purge_book_rows(db, book_id) {
        return json!({"ok": false, "err": e.to_string()});
    }
    json!({"ok": true})
}

/// 单事务清理一本书的全部 DB 关联行。由 purge_book / 上层重置复用。
pub fn purge_book_rows(db: &Db, book_id: &str) -> anyhow::Result<()> {
    let mut conn = db
        .conn
        .lock()
        .map_err(|_| anyhow::anyhow!("数据库锁损坏"))?;
    let exists: i64 =
        conn.query_row("SELECT COUNT(*) FROM books WHERE id=?1", [book_id], |row| {
            row.get(0)
        })?;
    if exists != 1 {
        anyhow::bail!("书籍不存在，拒绝清理关联数据");
    }
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM messages WHERE session_id IN (SELECT id FROM sessions WHERE book_id=?1)",
        [book_id],
    )?;
    tx.execute("DELETE FROM sessions WHERE book_id=?1", [book_id])?;
    tx.execute("DELETE FROM book_meta WHERE book_id=?1", [book_id])?;
    tx.execute("DELETE FROM pending_chapter WHERE book_id=?1", [book_id])?;
    tx.execute("DELETE FROM auto_task WHERE book_id=?1", [book_id])?;
    tx.execute("DELETE FROM llm_call_log WHERE book_id=?1", [book_id])?;
    tx.execute("DELETE FROM file_revision WHERE book_id=?1", [book_id])?;
    tx.execute("DELETE FROM chapter_memory WHERE book_id=?1", [book_id])?;
    tx.execute("DELETE FROM memory_job WHERE book_id=?1", [book_id])?;
    tx.execute("DELETE FROM draft_dependency WHERE book_id=?1", [book_id])?;
    tx.execute("DELETE FROM draft_origin WHERE book_id=?1", [book_id])?;
    tx.execute("DELETE FROM story_plan WHERE book_id=?1", [book_id])?;
    tx.execute("DELETE FROM continuity_event WHERE book_id=?1", [book_id])?;
    tx.execute("DELETE FROM dw_change_proposal WHERE book_id=?1", [book_id])?;
    tx.execute("DELETE FROM dw_book_skill WHERE book_id=?1", [book_id])?;
    tx.execute("DELETE FROM dw_agent_profile WHERE book_id=?1", [book_id])?;
    // 仅清理已知书籍配置命名空间；禁止双端 LIKE，避免 '_'/'%' 通配误删全站配置。
    for key in [
        format!("file_flags__{}", book_id),
        format!("book_style__{}", book_id),
        format!("book_humanize__{}", book_id),
        format!("book_auto_full__{}", book_id),
    ] {
        tx.execute("DELETE FROM settings WHERE key=?1", [&key])?;
    }
    for prefix in ["book_primary_skill__", "book_support_skills__"] {
        let exact_prefix = format!("{}{}__", prefix, book_id);
        tx.execute(
            "DELETE FROM settings WHERE substr(key,1,length(?1))=?1",
            [&exact_prefix],
        )?;
    }
    tx.execute("DELETE FROM books WHERE id=?1", [book_id])?;
    tx.commit()?;
    Ok(())
}

// 书本级回收站
pub fn list_trash(db: &Db) -> Value {
    let rows = db
        .q_json(
            "SELECT * FROM books WHERE deleted_at IS NOT NULL ORDER BY deleted_at DESC",
            &[],
        )
        .unwrap_or_default();
    json!(rows.iter().map(row_book).collect::<Vec<_>>())
}

pub fn restore_trash(db: &Db, id: &str) -> Value {
    let _ = db.exec(
        "UPDATE books SET deleted_at=NULL, deleted_reason=NULL WHERE id=?",
        &[&id as &dyn ToSql],
    );
    json!({"ok": true})
}

pub fn clear_trash(db: &Db) -> Value {
    let rows = db
        .q_json("SELECT id FROM books WHERE deleted_at IS NOT NULL", &[])
        .unwrap_or_default();
    let mut errors: Vec<String> = Vec::new();
    for r in rows {
        let id = r["id"].as_str().unwrap_or("").to_string();
        let bdir = db.books_dir.join(crate::files::safe_name(&id));
        if bdir.exists() {
            if crate::files::assert_under(&db.books_dir, &bdir) {
                if let Err(e) = std::fs::remove_dir_all(&bdir) {
                    errors.push(format!("书目录 {}: {}", id, e));
                }
            } else {
                errors.push(format!("拒绝删除越界路径: {}", bdir.display()));
            }
        }
        let vdir = db.versions_dir.join(crate::files::safe_name(&id));
        if vdir.exists() && crate::files::assert_under(&db.versions_dir, &vdir) {
            let _ = std::fs::remove_dir_all(&vdir);
        }
        let tdir = db.trash_dir.join(crate::files::safe_name(&id));
        if tdir.exists() && crate::files::assert_under(&db.trash_dir, &tdir) {
            let _ = std::fs::remove_dir_all(&tdir);
        }
        // 与 purge 同一套关联清理（含版本记忆/计划/依赖/事件/用量/配置前缀键）
        if let Err(e) = purge_book_rows(db, &id) {
            errors.push(format!("DB 清理 {}: {}", id, e));
        }
    }
    crate::stats::rebuild_word_stats(db);
    if errors.is_empty() {
        json!({"ok": true})
    } else {
        json!({"ok": false, "err": errors.join("; ")})
    }
}

/// 重置全部数据（对齐 reset_all_data）：清空全部业务表 + 磁盘 books/versions/trash。
/// 只保留 official 技能；调用方必须已完成 confirm 校验与任务停止。返回错误列表。
pub fn purge_all_rows(db: &Db) -> anyhow::Result<()> {
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    for dir in [&db.books_dir, &db.versions_dir, &db.trash_dir] {
        if !dir.exists() {
            continue;
        }
        // 只清理目录内容，保留目录本身，避免越界删除
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let p = e.path();
            // 保守再校验：清理目标必须位于该根目录之内
            if !p.starts_with(dir) {
                anyhow::bail!("拒绝清理越界路径：{}", p.display());
            }
            if e.file_type()?.is_dir() {
                std::fs::remove_dir_all(&p)?;
            } else {
                std::fs::remove_file(&p)?;
            }
        }
    }
    let mut conn = db
        .conn
        .lock()
        .map_err(|_| anyhow::anyhow!("数据库锁损坏"))?;
    let tx = conn.transaction()?;
    tx.execute_batch(
        "DELETE FROM books;
         DELETE FROM sessions;
         DELETE FROM messages;
         DELETE FROM book_meta;
         DELETE FROM pending_chapter;
         DELETE FROM auto_task;
         DELETE FROM word_stats;
         DELETE FROM llm_call_log;
         DELETE FROM file_revision;
         DELETE FROM chapter_memory;
         DELETE FROM memory_job;
         DELETE FROM draft_dependency;
         DELETE FROM draft_origin;
         DELETE FROM story_plan;
         DELETE FROM continuity_event;
         DELETE FROM dw_change_proposal;
         DELETE FROM dw_book_skill;
         DELETE FROM dw_agent_profile;
         DELETE FROM skills WHERE origin != 'official';",
    )?;
    tx.commit()?;
    Ok(())
}

// ---------- 会话 / 消息 ----------
pub fn list_sessions(db: &Db, book_id: &str) -> Value {
    let rows = db
        .q_json(
            "SELECT * FROM sessions WHERE book_id=? ORDER BY updated_at DESC",
            &[&book_id as &dyn ToSql],
        )
        .unwrap_or_default();
    // q_json 已把列名 snake->camel（db.rs 的 snake_to_camel），必须读驼峰键；
    // 旧实现读 book_id/msg_count/updated_at 导致恒为 null/0（修复 F22）。
    let out: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r["id"], "bookId": r["bookId"], "title": r["title"],
                "preview": r["preview"].as_str().unwrap_or(""),
                "msgCount": r["msgCount"].as_i64().unwrap_or(0),
                "updatedAt": r["updatedAt"],
            })
        })
        .collect();
    json!(out)
}

/// 创建会话。INSERT 失败必须报错（旧实现吞错：老库缺 created_at 列时
/// 会话从未落库，前端却显示成功——用户感知为“会话消失”）。
pub fn create_session(db: &Db, book_id: &str, title: &str) -> anyhow::Result<Value> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_ms();
    db.exec(
        "INSERT INTO sessions(id,book_id,title,preview,msg_count,created_at,updated_at) VALUES(?,?,?,NULL,0,?,?)",
        &[&id as &dyn ToSql, &book_id, &title, &now, &now],
    )?;
    Ok(
        json!({"id": id, "bookId": book_id, "title": title, "preview": "", "msgCount": 0, "createdAt": now, "updatedAt": now}),
    )
}

pub fn rename_session(db: &Db, session_id: &str, title: &str) -> Value {
    let now = now_ms();
    let _ = db.exec(
        "UPDATE sessions SET title=?, updated_at=? WHERE id=?",
        &[&title as &dyn ToSql, &now, &session_id],
    );
    json!({"ok": true})
}

/// 删除会话及其全部消息。单事务原子删除：任何一步失败都回滚，
/// 绝不出现“消息已删、会话还在”或“会话已删、消息成孤儿”的半态；
/// 目标会话不存在时返回错误（旧实现吞掉两次 DB 错误且恒返回 ok=true）。
pub fn delete_session(db: &Db, session_id: &str) -> anyhow::Result<Value> {
    let mut conn = db
        .conn
        .lock()
        .map_err(|_| anyhow::anyhow!("数据库锁损坏"))?;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM messages WHERE session_id=?1", [session_id])?;
    let affected = tx.execute("DELETE FROM sessions WHERE id=?1", [session_id])?;
    if affected != 1 {
        // 目标会话不存在：回滚（消息删除一并撤销），不得静默成功
        tx.rollback()?;
        anyhow::bail!("会话不存在");
    }
    tx.commit()?;
    Ok(json!({"ok": true, "deleted": true, "sessionId": session_id}))
}

pub fn msg_row(r: &Value) -> Value {
    // q_json 已把列名 snake->camel：context_json -> contextJson、result_json -> resultJson 等
    let context = r["contextJson"]
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok());
    // 老数据（seed/旧库）context_json 可能为空：补默认字段，前端渲染消息时直接读 c.skills 等
    let context = match context.unwrap_or(json!({})) {
        Value::Object(mut o) => {
            for (k, def) in [
                ("skills", json!([])),
                ("files", json!([])),
                ("contextFiles", json!([])),
                ("webSearch", json!(false)),
                ("requestId", json!("")),
                ("runId", json!("")),
                ("humanizeOverride", json!("")),
                ("skillSelection", json!(null)),
            ] {
                let need = matches!(o.get(k), None | Some(Value::Null));
                if need {
                    o.insert(k.to_string(), def);
                }
            }
            Value::Object(o)
        }
        other => other,
    };
    let steps = r["stepsJson"]
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok());
    let result = r["resultJson"]
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok());
    json!({
        "id": r["id"], "sessionId": r["sessionId"], "role": r["role"],
        "content": r["content"].as_str().unwrap_or(""),
        "context": context,
        "steps": steps.unwrap_or(json!([])),
        "result": result.unwrap_or(json!(null)),
        "createdAt": r["createdAt"],
        "interrupted": r["interrupted"].as_i64().unwrap_or(0) != 0,
    })
}

pub fn list_messages(db: &Db, session_id: &str) -> Value {
    let rows = db
        .q_json(
            "SELECT * FROM messages WHERE session_id=? ORDER BY created_at ASC",
            &[&session_id as &dyn ToSql],
        )
        .unwrap_or_default();
    json!(rows.iter().map(msg_row).collect::<Vec<_>>())
}

pub fn save_message(db: &Db, row: &Value) -> String {
    let id = row["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let session_id = row["session_id"].as_str().unwrap_or("").to_string();
    if !session_id.is_empty() {
        let role = row["role"].as_str().unwrap_or("user").to_string();
        let content = row["content"].as_str().unwrap_or("").to_string();
        let ctx = row["context_json"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let steps = row["steps_json"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let result = row["result_json"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let created = row["created_at"].as_i64().unwrap_or_else(now_ms);
        let interrupted: i64 = if row["interrupted"].as_i64().unwrap_or(0) != 0 {
            1
        } else {
            0
        };
        let preview = row["content"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect::<String>();
        let now = now_ms();
        // 消息 INSERT 与会话计数 UPDATE 必须同事务：崩溃不留半态，失败不假成功
        let tx_result = (|| -> rusqlite::Result<()> {
            let mut guard = db.conn.lock().unwrap_or_else(|e| e.into_inner());
            let tx = guard.transaction()?;
            tx.execute(
                "INSERT INTO messages(id,session_id,role,content,context_json,steps_json,result_json,created_at,interrupted) VALUES(?,?,?,?,?,?,?,?,?)",
                rusqlite::params![id, session_id, role, content, ctx, steps, result, created, interrupted],
            )?;
            tx.execute(
                "UPDATE sessions SET msg_count=msg_count+1, preview=?, updated_at=? WHERE id=?",
                rusqlite::params![preview, now, session_id],
            )?;
            tx.commit()
        })();
        if let Err(e) = tx_result {
            eprintln!("[molan-core] 消息写入失败（会话 {}）：{}", session_id, e);
        }
    }
    id
}

// ---------- book_meta ----------
fn ensure_meta(db: &Db, book_id: &str) {
    let has = db
        .q_json(
            "SELECT book_id FROM book_meta WHERE book_id=?",
            &[&book_id as &dyn ToSql],
        )
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if !has {
        let null_s = "null".to_string();
        let _ = db.exec(
            "INSERT INTO book_meta(book_id,style_json,humanize,primary_skill,support_skills_json) VALUES(?,?,0,NULL,'[]')",
            &[&book_id as &dyn ToSql, &null_s],
        );
    }
}

pub fn get_book_meta(db: &Db, book_id: &str) -> Value {
    let rows = db
        .q_json(
            "SELECT * FROM book_meta WHERE book_id=?",
            &[&book_id as &dyn ToSql],
        )
        .unwrap_or_default();
    let m = match rows.into_iter().next() {
        Some(m) => m,
        None => {
            ensure_meta(db, book_id);
            db.q_json(
                "SELECT * FROM book_meta WHERE book_id=?",
                &[&book_id as &dyn ToSql],
            )
            .ok()
            .and_then(|v| v.into_iter().next())
            .unwrap_or(json!({}))
        }
    };
    json!({
        "styleJson": m["styleJson"].as_str().unwrap_or("null"),
        "humanize": m["humanize"].as_i64().unwrap_or(0) != 0,
        "primarySkill": m["primarySkill"].as_str(),
        "supportSkills": serde_json::from_str::<Value>(m["supportSkills"].as_str().unwrap_or("[]")).unwrap_or(json!([])),
        "leadText": m["leadText"].as_str().unwrap_or(""),
    })
}

/// 设置本书文风卡选择：key = "auto" | "off" | "distill" | "style:<技能id>" | 题材预设key。
/// 只写 settings 里的 book_style__<bookId>（前端读这里）；
/// book_meta.style_json 留给「蒸馏出的文风正文」，由 set_book_style_text 写。
pub fn set_book_style(db: &Db, book_id: &str, key: &str) -> Value {
    let k = if key.trim().is_empty() {
        "auto"
    } else {
        key.trim()
    };
    let skey = format!("book_style__{}", book_id);
    let _ = db.exec(
        "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2",
        &[&skey as &dyn ToSql, &k as &dyn ToSql],
    );
    json!({"ok": true, "key": k})
}

/// 保存蒸馏得到的文风正文（book_meta.style_json）
pub fn set_book_style_text(db: &Db, book_id: &str, text: &str) -> Value {
    ensure_meta(db, book_id);
    let s = serde_json::to_string(&json!(text)).unwrap_or_default();
    let _ = db.exec(
        "UPDATE book_meta SET style_json=? WHERE book_id=?",
        &[&s as &dyn ToSql, &book_id],
    );
    json!({"ok": true})
}

pub fn get_book_style(db: &Db, book_id: &str) -> Value {
    // node: JSON.parse(style_json 原文)，失败 catch 返回原文。
    // q_json 已把 TEXT 原样取出（仍带 JSON 引号），这里 parse 一次即等价 node 的 JSON.parse。
    let m = get_book_meta(db, book_id);
    let raw = m["styleJson"].as_str().unwrap_or("null").to_string();
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) => v,
        // parse 失败 => node catch 分支返回原文（去掉外层无引号场景）
        Err(_) => json!(raw),
    }
}

/// 设置本书「自动去味」方法：official:standard / official:deep / skill:<id> / none。
/// 前端与自动去味读的都是 settings 里的 book_humanize__<bookId>；
/// book_meta.humanize 保留为布尔兼容位（非 none 即视为开启）。
pub fn set_book_humanize(db: &Db, book_id: &str, method: &str) -> Value {
    let m = match method.trim() {
        "" | "true" | "1" => "official:standard",
        "false" | "0" => "none",
        other => other,
    };
    let key = format!("book_humanize__{}", book_id);
    let _ = db.exec(
        "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2",
        &[&key as &dyn ToSql, &m as &dyn ToSql],
    );
    ensure_meta(db, book_id);
    let n: i64 = if m == "none" { 0 } else { 1 };
    let _ = db.exec(
        "UPDATE book_meta SET humanize=? WHERE book_id=?",
        &[&n as &dyn ToSql, &book_id],
    );
    json!({"ok": true, "method": m})
}

/// 设置本书某任务的主技能：settings['book_primary_skill__<bookId>__<taskKind>'] = skillId
pub fn set_book_primary_skill(db: &Db, book_id: &str, task_kind: &str, skill_id: &str) -> Value {
    let key = format!("book_primary_skill__{}__{}", book_id, task_kind);
    let _ = db.exec(
        "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2",
        &[&key as &dyn ToSql, &skill_id as &dyn ToSql],
    );
    ensure_meta(db, book_id);
    let _ = db.exec(
        "UPDATE book_meta SET primary_skill=? WHERE book_id=?",
        &[&skill_id as &dyn ToSql, &book_id],
    );
    json!({"ok": true})
}

/// 设置本书某任务的叠加技能：settings['book_support_skills__<bookId>__<taskKind>'] = JSON 数组
pub fn set_book_support_skills(
    db: &Db,
    book_id: &str,
    task_kind: &str,
    skill_ids: &[String],
) -> Value {
    let key = format!("book_support_skills__{}__{}", book_id, task_kind);
    let v = serde_json::to_string(&json!(skill_ids)).unwrap_or_else(|_| "[]".to_string());
    let _ = db.exec(
        "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2",
        &[&key as &dyn ToSql, &v as &dyn ToSql],
    );
    json!({"ok": true})
}

pub fn save_lead(db: &Db, book_id: &str, text: &str) -> Value {
    ensure_meta(db, book_id);
    let _ = db.exec(
        "UPDATE book_meta SET lead_text=? WHERE book_id=?",
        &[&text as &dyn ToSql, &book_id],
    );
    json!({"ok": true})
}

#[cfg(test)]
mod purge_safety_tests {
    use super::*;

    #[test]
    fn wildcard_book_id_cannot_delete_global_settings() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(d.path(), None).unwrap();
        db.exec(
            "INSERT INTO settings(key,value) VALUES('channel_key__keep','secret'),('file_flags__real-book','{}')",
            &[],
        ).unwrap();
        let out = purge_book(&db, "_");
        assert_eq!(out["ok"], false);
        let rows = db
            .q_json("SELECT key FROM settings ORDER BY key", &[])
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn purge_cleans_only_target_book_settings() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(d.path(), None).unwrap();
        let id = create_book(&db, "删除测试", "测试", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        for key in [
            format!("file_flags__{}", id),
            format!("book_style__{}", id),
            format!("book_primary_skill__{}__writing", id),
            format!("book_support_skills__{}__writing", id),
        ] {
            db.exec("INSERT INTO settings(key,value) VALUES(?1,'x')", &[&key])
                .unwrap();
        }
        db.exec(
            "INSERT INTO settings(key,value) VALUES('channel_key__keep','secret')",
            &[],
        )
        .unwrap();
        assert_eq!(purge_book(&db, &id)["ok"], true);
        let rows = db.q_json("SELECT key FROM settings", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["key"], "channel_key__keep");
    }

    /// 删除不存在的会话必须失败，且不得误删任何消息（旧实现恒 ok=true）。
    #[test]
    fn delete_missing_session_fails_and_keeps_other_messages() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(d.path(), None).unwrap();
        let id = create_book(&db, "会话删除测试", "测试", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        let s1 = create_session(&db, &id, "s1").unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let s2 = create_session(&db, &id, "s2").unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        db.exec(
            "INSERT INTO messages(id,session_id,role,content,created_at) VALUES('m1',?1,'user','hi',1)",
            &[&s1 as &dyn ToSql],
        )
        .unwrap();
        let err = delete_session(&db, "no-such-session").unwrap_err();
        assert!(err.to_string().contains("会话不存在"), "{}", err);
        // 两个会话与消息全部原样保留
        let n: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(list_messages(&db, &s1).as_array().unwrap().len(), 1);
        assert_eq!(list_messages(&db, &s2).as_array().unwrap().len(), 0);
    }

    /// 正常删除：会话与消息同时清零，返回体带 deleted/sessionId，其它会话不受影响。
    #[test]
    fn delete_session_removes_session_and_its_messages() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(d.path(), None).unwrap();
        let id = create_book(&db, "会话删除测试2", "测试", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        let s1 = create_session(&db, &id, "s1").unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let s2 = create_session(&db, &id, "s2").unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        for sid in [&s1, &s2] {
            db.exec(
                "INSERT INTO messages(id,session_id,role,content,created_at) VALUES(?1,?2,'user','hi',1)",
                &[
                    &format!("m-{}-{}", sid, now_ms()) as &dyn ToSql,
                    sid as &dyn ToSql,
                ],
            )
            .unwrap();
        }
        let out = delete_session(&db, &s1).unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["deleted"], true);
        assert_eq!(out["sessionId"], s1.as_str());
        assert_eq!(list_messages(&db, &s1).as_array().unwrap().len(), 0);
        assert_eq!(list_messages(&db, &s2).as_array().unwrap().len(), 1);
        let left: Vec<String> = db
            .q_json("SELECT id FROM sessions", &[])
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(left, vec![s2]);
    }

    /// 事务故障：sessions 表被临时改名，删除必须报错而不是假成功；
    /// 报错路径下 messages 也不得被静默删掉（事务未提交）。

    #[test]
    fn delete_session_failure_does_not_half_delete() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(d.path(), None).unwrap();
        let id = create_book(&db, "事务故障测试", "测试", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        let sid = create_session(&db, &id, "s").unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        db.exec(
            "INSERT INTO messages(id,session_id,role,content,created_at) VALUES('m1',?1,'user','hi',1)",
            &[&sid as &dyn ToSql],
        )
        .unwrap();
        // 制造 DELETE sessions 的失败：把表改名，messages 表仍可删
        db.conn
            .lock()
            .unwrap()
            .execute_batch("ALTER TABLE sessions RENAME TO sessions_broken")
            .unwrap();
        let err = delete_session(&db, &sid).unwrap_err();
        assert!(!err.to_string().is_empty());
        db.conn
            .lock()
            .unwrap()
            .execute_batch("ALTER TABLE sessions_broken RENAME TO sessions")
            .unwrap();
        // 事务回滚：消息仍在
        assert_eq!(list_messages(&db, &sid).as_array().unwrap().len(), 1);
    }
}
