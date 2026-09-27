//! 上下文清单（长篇稳定性 P0-3）：每次生成实际注入了什么，落库可查。
//!
//! 解决"关键设定被静默截掉，模型仍然流畅生成"的追责问题：
//! 每轮记录注入块清单（标签/字数）、记忆覆盖度（缺失/过期/隐藏/超预算）、
//! 模型与目标章。只观测不阻断；记录失败绝不影响生成。
use crate::{db::Db, stats::now_ms};
use anyhow::{anyhow, Result};
use rusqlite::params;
use serde_json::{json, Value};

pub fn ensure_schema(db: &Db) -> Result<()> {
    db.conn
        .lock()
        .map_err(|_| anyhow!("数据库锁损坏"))?
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS context_manifest (
                id TEXT PRIMARY KEY,
                ts INTEGER NOT NULL,
                book_id TEXT NOT NULL DEFAULT '',
                session_id TEXT NOT NULL DEFAULT '',
                command TEXT NOT NULL DEFAULT '',
                target_ch INTEGER NOT NULL DEFAULT 0,
                model TEXT NOT NULL DEFAULT '',
                total_chars INTEGER NOT NULL DEFAULT 0,
                blocks_json TEXT NOT NULL DEFAULT '[]',
                coverage_json TEXT NOT NULL DEFAULT 'null');
             CREATE INDEX IF NOT EXISTS idx_ctx_manifest_book ON context_manifest(book_id, ts);
             CREATE INDEX IF NOT EXISTS idx_ctx_manifest_session ON context_manifest(session_id, ts);",
        )?;
    Ok(())
}

/// 按「【标签】」头切块统计字数。无标签前缀归入 (prefix)。
/// 标签即上下文装配处的注入点标记（如 【全局设定·世界观】【已定稿章节记忆】）。
pub fn block_inventory(text: &str) -> Value {
    let mut blocks: Vec<(String, usize)> = Vec::new();
    let mut cur_label = String::from("(prefix)");
    let mut cur_chars = 0usize;
    let flush = |label: &str, chars: usize, blocks: &mut Vec<(String, usize)>| {
        if chars > 0 {
            if let Some(b) = blocks.iter_mut().find(|(l, _)| l == label) {
                b.1 += chars;
            } else {
                blocks.push((label.to_string(), chars));
            }
        }
    };
    for line in text.split('\n') {
        let t = line.trim_start();
        // strip_prefix 代替 [1..]：中文括号是多字节字符，直接按字节下标切会 panic
        let label = if let Some(rest) = t.strip_prefix('【') {
            rest.find('】').filter(|&i| i <= 80).map(|i| {
                let inner = &rest[..i];
                // 标签只取到第一个分隔符，避免过长（如 【全局设定·世界观（文件名）】→ 全局设定·世界观）
                inner.split('（').next().unwrap_or(inner).to_string()
            })
        } else {
            None
        };
        if let Some(l) = label {
            flush(&cur_label, cur_chars, &mut blocks);
            cur_label = l;
            cur_chars = line.chars().count() + 1;
        } else {
            cur_chars += line.chars().count() + 1;
        }
    }
    flush(&cur_label, cur_chars, &mut blocks);
    blocks.retain(|(_, c)| *c > 0);
    blocks.sort_by_key(|b| std::cmp::Reverse(b.1));
    blocks.truncate(64);
    json!(blocks
        .iter()
        .map(|(l, c)| json!({"label": l, "chars": c}))
        .collect::<Vec<_>>())
}

/// 记录一轮生成的上下文清单，返回清单 id。调用方必须容忍失败（let _ =）。
#[allow(clippy::too_many_arguments)]
pub fn record(
    db: &Db,
    book: &str,
    session: &str,
    command: &str,
    target_ch: i64,
    model: &str,
    context_text: &str,
    coverage: Value,
) -> Result<String> {
    ensure_schema(db)?;
    let id = uuid::Uuid::new_v4().to_string();
    let blocks = block_inventory(context_text);
    let total = context_text.chars().count() as i64;
    db.exec(
        "INSERT INTO context_manifest(id,ts,book_id,session_id,command,target_ch,model,total_chars,blocks_json,coverage_json)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            &id,
            now_ms(),
            book,
            session,
            command,
            target_ch,
            model,
            total,
            blocks.to_string(),
            coverage.to_string()
        ],
    )?;
    Ok(id)
}

/// 列表：按书或会话过滤，新的在前。不带 blocks 全文（太大），只带统计。
pub fn list(db: &Db, book: &str, session: &str, limit: i64) -> Result<Value> {
    ensure_schema(db)?;
    let lim = limit.clamp(1, 200);
    let (sql, refs): (String, Vec<Box<dyn rusqlite::ToSql>>) = if !book.is_empty() {
        (
            "SELECT id,ts,book_id,session_id,command,target_ch,model,total_chars,coverage_json
             FROM context_manifest WHERE book_id=?1 ORDER BY ts DESC LIMIT ?2"
                .to_string(),
            vec![Box::new(book.to_string()), Box::new(lim)],
        )
    } else {
        (
            "SELECT id,ts,book_id,session_id,command,target_ch,model,total_chars,coverage_json
             FROM context_manifest WHERE session_id=?1 ORDER BY ts DESC LIMIT ?2"
                .to_string(),
            vec![Box::new(session.to_string()), Box::new(lim)],
        )
    };
    let params: Vec<&dyn rusqlite::ToSql> = refs.iter().map(|b| b.as_ref()).collect();
    let rows = db.q_json(&sql, &params)?;
    Ok(json!({"ok": true, "manifests": rows}))
}

/// 单条详情（含注入块清单）。
pub fn get(db: &Db, id: &str) -> Result<Value> {
    ensure_schema(db)?;
    let rows = db.q_json("SELECT * FROM context_manifest WHERE id=?1", &[&id])?;
    let Some(row) = rows.first() else {
        anyhow::bail!("清单不存在：{}", id);
    };
    let mut out = row.clone();
    if let Some(b) = out["blocksJson"].as_str() {
        out["blocks"] = serde_json::from_str(b).unwrap_or(Value::Null);
    }
    if let Some(c) = out["coverageJson"].as_str() {
        out["coverage"] = serde_json::from_str(c).unwrap_or(Value::Null);
    }
    Ok(json!({"ok": true, "manifest": out}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_inventory_groups_by_label_and_counts_chars() {
        let text = "前言两行
第二行
【全局设定·世界观（世界观.md）】
世界内容
【已定稿章节记忆（截至第2章）】
记忆内容一
记忆内容二
";
        let inv = block_inventory(text);
        let arr = inv.as_array().unwrap();
        let get = |l: &str| {
            arr.iter().find(|b| b["label"] == l).unwrap()["chars"]
                .as_i64()
                .unwrap()
        };
        assert_eq!(get("(prefix)"), 4 + 1 + 3 + 1);
        assert_eq!(
            get("全局设定·世界观"),
            "【全局设定·世界观（世界观.md）】".chars().count() as i64 + 1 + 4 + 1
        );
        assert_eq!(
            get("已定稿章节记忆"),
            "【已定稿章节记忆（截至第2章）】".chars().count() as i64
                + 1
                + 5
                + 1
                + 5
                + 1
                + 1 // 结尾换行多出一个空行段
        );
        // 按字数降序
        assert!(arr[0]["chars"].as_i64() >= arr[1]["chars"].as_i64());
    }

    #[test]
    fn record_and_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let id = record(
            &db,
            "book1",
            "sess1",
            "chat_stream",
            3,
            "deepseek-chat",
            "【全局设定·世界观】
内容
",
            json!({"complete": true}),
        )
        .unwrap();
        let g = get(&db, &id).unwrap();
        assert_eq!(g["manifest"]["targetCh"], json!(3));
        assert_eq!(g["manifest"]["model"], json!("deepseek-chat"));
        assert!(g["manifest"]["blocks"].as_array().unwrap().len() == 1);
        let l = list(&db, "book1", "", 10).unwrap();
        assert_eq!(l["manifests"].as_array().unwrap().len(), 1);
        let l2 = list(&db, "", "sess1", 10).unwrap();
        assert_eq!(l2["manifests"].as_array().unwrap().len(), 1);
    }
}
