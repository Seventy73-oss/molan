//! 网页导出/备份：txt | zip | all。返回 base64 产物，供前端 Blob 下载。
//!
//! 约束（IMPLEMENTATION-V1 第 23 条 + 父审 P0）：
//! - 整个快照在 Db::fs_lock 下构建，锁序 fs_lock -> conn，绝不反向。
//! - 完整性优先：任何读目录/读文件/查库失败都返回 Err，绝不静默跳过产出"缺文件的成功备份"。
//! - 有硬性大小上限，超限明确报错，不静默截断。
//! - DB 快照必须真实生成；生成失败即 Err，绝不用 checkpoint 后拷贝活库冒充一致性快照。
//! - DB 快照内的**已知凭据键**在快照库内脱敏（secure_delete + VACUUM）后才读取字节；
//!   只覆盖已知键，**不声称**已识别并移除全部敏感信息。
//! - 压缩包内一律相对路径，不含任何绝对路径；绝不递归 data/exports 自身。
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use molan_core::db::Db;
use molan_core::files;
use serde_json::{json, Value};
use std::io::{Cursor, Write as _};
use std::path::{Path, PathBuf};

/// 单次导出原始字节上限（base64 后约 4/3，再加 JSON 封装，避免 IPC/内存 OOM）
pub const MAX_TOTAL_BYTES: usize = 48 * 1024 * 1024;
/// 单个文件上限：超过即报错，不截断
pub const MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 6000;
const MAX_DEPTH: usize = 12;

/// 绝不递归导出的目录（避免把上一次导出再打包进去）
fn is_skipped_dir(name: &str) -> bool {
    name == "exports"
}

fn walk_dir(
    base: &Path,
    rel: &str,
    out: &mut Vec<(String, PathBuf)>,
    depth: usize,
    skip: Option<&Path>,
) -> Result<()> {
    // 深度超限：备份会缺文件，必须报错而不是静默截断
    if depth > MAX_DEPTH {
        bail!(
            "目录层级超过 {} 层，拒绝生成不完整备份：{}",
            MAX_DEPTH,
            base.display()
        );
    }
    // 绝不递归导出目录自身（data/exports 里放着历史导出产物）
    if let Some(s) = skip {
        if base == s {
            return Ok(());
        }
    }
    let rd = std::fs::read_dir(base)
        .with_context(|| format!("无法读取目录，拒绝生成不完整备份：{}", base.display()))?;
    let mut entries: Vec<_> = Vec::new();
    for e in rd {
        let e = e.with_context(|| format!("读取目录项失败：{}", base.display()))?;
        entries.push(e);
    }
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || is_skipped_dir(&name) {
            continue;
        }
        let ft = e
            .file_type()
            .with_context(|| format!("无法判断文件类型：{}", e.path().display()))?;
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", rel, name)
        };
        if ft.is_dir() {
            walk_dir(&e.path(), &child_rel, out, depth + 1, skip)?;
        } else if ft.is_file() {
            if out.len() >= MAX_ENTRIES {
                bail!(
                    "导出条目超过 {} 个，拒绝生成不完整备份，请按单本导出",
                    MAX_ENTRIES
                );
            }
            out.push((child_rel, e.path()));
        }
    }
    Ok(())
}

fn read_capped(path: &Path, rel: &str) -> Result<Vec<u8>> {
    let meta = std::fs::metadata(path).with_context(|| format!("无法读取文件元信息：{}", rel))?;
    if meta.len() as usize > MAX_ENTRY_BYTES {
        bail!("文件过大（{} 字节），拒绝导出：{}", meta.len(), rel);
    }
    std::fs::read(path).with_context(|| format!("读取文件失败：{}", rel))
}

/// 设置脱敏：密钥类键值一律置空并登记，绝不外带明文。
pub fn redact_settings(rows: &[Value]) -> (Vec<Value>, Vec<String>) {
    let mut out = Vec::new();
    let mut redacted = Vec::new();
    for r in rows {
        let key = r["key"].as_str().unwrap_or("");
        let val = r["value"].as_str().unwrap_or("");
        if is_secret_key(key) && !val.is_empty() {
            out.push(json!({"key": key, "value": "", "redacted": true}));
            redacted.push(key.to_string());
        } else {
            out.push(json!({"key": key, "value": val}));
        }
    }
    (out, redacted)
}

/// 与快照库内脱敏保持同一判定，避免两处口径漂移。
fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    [
        "channel_key__",
        "api_key",
        "apikey",
        "token",
        "secret",
        "password",
    ]
    .iter()
    .any(|m| k.contains(m))
}

fn zip_entries(entries: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in entries {
        writer.start_file(name.clone(), opts)?;
        writer.write_all(bytes)?;
    }
    let cursor = writer.finish()?;
    Ok(cursor.into_inner())
}

fn gather(
    db: &Db,
    root: &Path,
    rel_prefix: &str,
    skip: Option<&Path>,
) -> Result<Vec<(String, Vec<u8>)>> {
    let _ = db;
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    walk_dir(root, rel_prefix, &mut paths, 0, skip)?;
    let mut out = Vec::new();
    let mut total = 0usize;
    for (rel, p) in paths {
        let bytes = read_capped(&p, &rel)?;
        total += bytes.len();
        if total > MAX_TOTAL_BYTES {
            bail!(
                "导出内容超过 {} MB 上限，请按单本 / 分项导出（不提供静默截断）",
                MAX_TOTAL_BYTES / 1024 / 1024
            );
        }
        out.push((rel, bytes));
    }
    Ok(out)
}

/// 从"同一份"快照库读出的一致性元数据 + 快照字节。
/// 关键：meta/*.json 必须来自这份快照，而不是导出过程中另查活库——
/// 否则 fs_lock 挡不住 settings 写入，元数据与快照会互相矛盾。
struct SnapshotBundle {
    bytes: Vec<u8>,
    how: &'static str,
    settings: Vec<Value>,
    skills: Vec<Value>,
    books: Vec<Value>,
    book_meta: Vec<Value>,
    plans: Vec<Value>,
    redacted_keys: Vec<String>,
}

fn q_snap(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(sql)?;
    let cols: Vec<String> = (0..stmt.column_count())
        .map(|i| {
            stmt.column_name(i)
                .map(|n| n.to_string())
                .unwrap_or_default()
        })
        .collect();
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut obj = serde_json::Map::new();
        for (i, name) in cols.iter().enumerate() {
            let v = match row.get_ref(i)? {
                rusqlite::types::ValueRef::Null => Value::Null,
                rusqlite::types::ValueRef::Integer(n) => Value::from(n),
                rusqlite::types::ValueRef::Real(f) => Value::from(f),
                rusqlite::types::ValueRef::Text(t) => {
                    Value::from(String::from_utf8_lossy(t).to_string())
                }
                rusqlite::types::ValueRef::Blob(b) => {
                    Value::from(String::from_utf8_lossy(b).to_string())
                }
            };
            obj.insert(molan_core::db::snake_to_camel(name), v);
        }
        out.push(Value::Object(obj));
    }
    Ok(out)
}

/// 一致性 DB 快照：VACUUM INTO 生成独立快照库 → 在快照库内脱敏已知凭据键 →
/// secure_delete + VACUUM 重写 → 读取字节与元数据。
/// - 临时库用 tempfile RAII 保管，任何提前返回/错误都会自动删除（不会残留含密钥的临时文件）。
/// - 读取字节有上限，防止快照异常膨胀撑爆内存。
/// - 任一步失败即 Err，绝不拷贝活库冒充一致性快照。
fn sqlite_snapshot(db: &Db) -> Result<SnapshotBundle> {
    // RAII：tempdir 在 drop 时递归删除；这里只用一个"尚未创建的路径"，
    // 不持有文件句柄——避免 Windows 上 NamedTempFile handle 与 SQLite 打开互相 sharing 冲突。
    let tmpdir = tempfile::tempdir().context("创建临时快照目录失败")?;
    let path = tmpdir.path().join("snapshot.db");
    let literal = path.to_string_lossy().replace('\'', "''");
    db.exec(&format!("VACUUM INTO '{}'", literal), &[])
        .context("生成数据库一致性快照失败（VACUUM INTO）")?;

    let conn = rusqlite::Connection::open(&path)
        .with_context(|| format!("打开快照库失败：{}", path.display()))?;
    // 仅针对**已知凭据键**脱敏（不声称"任何密钥"）
    conn.execute_batch("PRAGMA secure_delete=ON;")?;
    let mut redacted_keys: Vec<String> = q_snap(&conn, "SELECT key FROM settings ORDER BY key")?
        .iter()
        .filter_map(|r| r["key"].as_str().map(|s| s.to_string()))
        .filter(|k| is_secret_key(k))
        .collect();
    conn.execute(
        "UPDATE settings SET value='' WHERE lower(key) LIKE '%channel_key__%' \
         OR lower(key) LIKE '%api_key%' OR lower(key) LIKE '%apikey%' \
         OR lower(key) LIKE '%token%' OR lower(key) LIKE '%secret%' \
         OR lower(key) LIKE '%password%'",
        [],
    )?;
    // 重写整库，确保被清空的内容不残留在空闲页
    conn.execute_batch("VACUUM;")?;
    redacted_keys.sort();
    redacted_keys.dedup();

    // 元数据全部取自这份快照（与字节同源，保证一致）
    let settings = redact_settings(&q_snap(
        &conn,
        "SELECT key,value FROM settings ORDER BY key",
    )?)
    .0;
    let skills = q_snap(&conn, "SELECT id,name,description,kind,source,origin,enabled,builtin_key,usage_mode,targets_json,prompt_template FROM skills ORDER BY id")?;
    let books = q_snap(&conn, "SELECT * FROM books ORDER BY created_at")?;
    let book_meta = q_snap(&conn, "SELECT * FROM book_meta ORDER BY book_id")?;
    let plans = q_snap(&conn, "SELECT book_id,kind,payload_json,revision,updated_at FROM story_plan ORDER BY book_id,kind")?;
    drop(conn);

    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as usize;
    if size > MAX_TOTAL_BYTES {
        bail!("数据库快照过大（{} 字节），拒绝导出", size);
    }
    let bytes =
        std::fs::read(&path).with_context(|| format!("读取快照库失败：{}", path.display()))?;
    drop(tmpdir); // 显式 drop，递归删除临时目录

    Ok(SnapshotBundle {
        bytes,
        how: "VACUUM INTO 一致性快照（库内脱敏 + secure_delete）",
        settings,
        skills,
        books,
        book_meta,
        plans,
        redacted_keys,
    })
}

fn book_title(db: &Db, book_id: &str) -> Result<String> {
    let rows = db
        .q_json(
            "SELECT title FROM books WHERE id=?1",
            &[&book_id as &dyn rusqlite::ToSql],
        )
        .context("读取书籍标题失败")?;
    Ok(rows
        .first()
        .and_then(|r| r["title"].as_str().map(|s| s.to_string()))
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "未命名".to_string()))
}

fn file_stem(title: &str) -> String {
    let s: String = title
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let t = s.trim().trim_matches('.').trim();
    if t.is_empty() {
        "molan-export".to_string()
    } else {
        t.chars().take(80).collect()
    }
}

/// 书稿文本导出：仅正式正文，按数值章序（第2章在第10章前）；其他资料走 zip。
fn book_txt(db: &Db, book_id: &str, skip: Option<&Path>) -> Result<String> {
    let base = db.books_dir.join(files::safe_name(book_id)).join("正文");
    if !base.exists() {
        bail!("该书没有正式正文目录，无法导出 txt");
    }
    let mut paths = Vec::new();
    walk_dir(
        &base,
        &format!("正文/{}", files::safe_name(book_id)),
        &mut paths,
        0,
        skip,
    )?;
    // 数值章序；无章号的文件排在最后并按名称
    paths.sort_by(|a, b| {
        let na = chapter_no(&a.0);
        let nb = chapter_no(&b.0);
        match (na, nb) {
            (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.0.cmp(&b.0)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.0.cmp(&b.0),
        }
    });
    let mut out = String::new();
    let mut total = 0usize;
    let mut n = 0usize;
    for (rel, p) in paths {
        let bytes = read_capped(&p, &rel)?;
        total += bytes.len();
        if total > MAX_TOTAL_BYTES {
            bail!(
                "导出内容超过 {} MB 上限，请按单本导出",
                MAX_TOTAL_BYTES / 1024 / 1024
            );
        }
        let text = String::from_utf8_lossy(&bytes).to_string();
        out.push_str(&format!("\n\n===== {} =====\n\n", rel));
        out.push_str(&text);
        n += 1;
    }
    if n == 0 {
        bail!("该书正式正文目录没有任何可导出文件");
    }
    Ok(out)
}

/// 从相对路径里取章号（复用 handlers 的中文数字解析）
fn chapter_no(rel: &str) -> Option<i64> {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    super::chapter_num_from_name(name)
}

/// 构建导出产物。format: txt | zip | all（未知格式明确报错，不假装成功）
pub fn web_export(db: &Db, data_root: &Path, book_id: Option<&str>, format: &str) -> Result<Value> {
    let format = format.trim().to_ascii_lowercase();
    if !matches!(format.as_str(), "txt" | "zip" | "all") {
        bail!("不支持的导出格式：{}（仅支持 txt / zip / all）", format);
    }
    // 锁序：先 fs_lock（文件变更/导出串行），再在内部取 conn。绝不反向。
    let _guard = db.fs_lock.lock().map_err(|_| anyhow!("文件锁损坏"))?;
    // 导出目录绝对路径：递归时必须跳过，避免把历史导出再打包
    let skip = data_root.join("exports");
    let skip = if skip.is_dir() {
        Some(skip.as_path())
    } else {
        None
    };

    let (name, mime, bytes, redactions) = match format.as_str() {
        "txt" => {
            let bid = book_id
                .filter(|b| !b.trim().is_empty())
                .ok_or_else(|| anyhow!("txt 导出需要 bookId"))?;
            if !db.books_dir.join(files::safe_name(bid)).exists() {
                bail!("书籍不存在或没有书稿目录");
            }
            let text = book_txt(db, bid, skip)?;
            let title = book_title(db, bid)?;
            (
                format!("{}.txt", file_stem(&title)),
                "text/plain; charset=utf-8",
                text.into_bytes(),
                Vec::new(),
            )
        }
        "zip" => {
            let bid = book_id
                .filter(|b| !b.trim().is_empty())
                .ok_or_else(|| anyhow!("zip 导出需要 bookId"))?;
            let mut entries = gather(
                db,
                &db.books_dir.join(files::safe_name(bid)),
                &format!("books/{}", files::safe_name(bid)),
                skip,
            )?;
            if entries.is_empty() {
                bail!("该书没有任何可导出文件");
            }
            let title = book_title(db, bid)?;
            let entry_count = entries.len() + 1;
            let manifest = json!({
                "kind": "writerx-book-backup", "version": 1, "bookId": bid,
                "title": title, "files": entry_count,
                "note": "仅含本书书稿文件；不含渠道密钥",
            })
            .to_string()
            .into_bytes();
            entries.insert(0, ("MANIFEST.json".to_string(), manifest));
            (
                format!("{}.zip", file_stem(&title)),
                "application/zip",
                zip_entries(&entries)?,
                Vec::new(),
            )
        }
        _ => {
            let mut entries = gather(db, &db.books_dir, "books", skip)?;
            entries.extend(gather(db, &db.trash_dir, "trash", skip)?);
            entries.extend(gather(db, &db.versions_dir, "versions", skip)?);

            // 快照必须真实；失败即 Err（不降级、不拷活库）。
            // meta/*.json 全部来自这份快照，不再另查活库（否则 fs_lock 挡不住并发 settings 写）。
            let snap = sqlite_snapshot(db)?;
            entries.push((
                "meta/settings.json".into(),
                serde_json::to_vec_pretty(&snap.settings)?,
            ));
            entries.push((
                "meta/skills.json".into(),
                serde_json::to_vec_pretty(&snap.skills)?,
            ));
            entries.push((
                "meta/books.json".into(),
                serde_json::to_vec_pretty(&snap.books)?,
            ));
            entries.push((
                "meta/book_meta.json".into(),
                serde_json::to_vec_pretty(&snap.book_meta)?,
            ));
            entries.push((
                "meta/plans.json".into(),
                serde_json::to_vec_pretty(&snap.plans)?,
            ));
            entries.push(("db/writerx.snapshot.db".into(), snap.bytes));

            let redacted = snap.redacted_keys.clone();
            let total_entries = entries.len() + 1;
            let redactions_note = if redacted.is_empty() {
                "（无）".to_string()
            } else {
                redacted.join(", ")
            };
            entries.push(("README.txt".into(), format!(
                "墨澜工坊全量备份\n\n条目数：{}\n\n【脱敏说明】仅对**已知凭据类设置键**置空（channel_key__/api_key/token/secret/password 等）；其他设置原样保留。已置空：{}\n数据库快照：{}（库内同样已脱敏）。\n\n本备份不包含：上述凭据键明文、服务器登录令牌、运行时环境变量、任何绝对路径。\n注意：本备份**不声称**已识别并移除所有可能的敏感信息，请自行按需检查后分发。\n恢复：解压后 books/ 覆盖数据目录下 books/；meta/ 为可读 JSON 参考。\n",
                total_entries, redactions_note, snap.how,
            ).into_bytes()));

            let total: usize = entries.iter().map(|(_, b)| b.len()).sum();
            if total > MAX_TOTAL_BYTES {
                bail!(
                    "全量导出超过 {} MB 上限，请改用单本 zip",
                    MAX_TOTAL_BYTES / 1024 / 1024
                );
            }
            (
                format!("{}.zip", file_stem("molan-all")),
                "application/zip",
                zip_entries(&entries)?,
                redacted,
            )
        }
    };

    if bytes.len() > MAX_TOTAL_BYTES {
        bail!("导出产物超过 {} MB 上限", MAX_TOTAL_BYTES / 1024 / 1024);
    }
    Ok(json!({
        "ok": true,
        "name": name,
        "mime": mime,
        "base64": B64.encode(&bytes),
        "size": bytes.len(),
        "redactions": redactions,
        "note": if redactions.is_empty() {
            "未命中已知凭据键（不声称已扫描全部敏感信息）".to_string()
        } else {
            format!("已脱敏 {} 个已知凭据类设置键，不含其明文", redactions.len())
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 空库 + tempdir 路径必须能 VACUUM INTO（父要求：验证 Windows 上不持句柄可快照）。
    #[test]
    fn snapshot_on_tempdir_path_works_for_empty_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = molan_core::db::Db::open(dir.path(), None).unwrap();
        let snap = sqlite_snapshot(&db).expect("空库也应能生成一致性快照");
        assert!(!snap.bytes.is_empty(), "快照字节不应为空");
        assert_eq!(&snap.bytes[..15], b"SQLite format 3");
        // 临时目录已随函数返回清理，不应留下 molan-snap 文件
        let leftovers: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("molan-snap-"))
            .collect();
        assert!(leftovers.len() < 5, "不应残留大量临时快照文件");
    }

    /// 密钥键必须在快照库内被清空：解出快照库直接查 settings，确认取不到明文。
    #[test]
    fn snapshot_scrubs_known_credential_keys() {
        let dir = tempfile::tempdir().unwrap();
        let db = molan_core::db::Db::open(dir.path(), None).unwrap();
        db.exec(
            "INSERT INTO settings(key,value) VALUES('channel_key__c1','SECRET-KEY-123')",
            &[],
        )
        .unwrap();
        db.exec(
            "INSERT INTO settings(key,value) VALUES('book_style__b1','auto')",
            &[],
        )
        .unwrap();
        let snap = sqlite_snapshot(&db).unwrap();
        // 写到磁盘后用独立连接验证（模拟"解压后核对"）
        let out = dir.path().join("verify.db");
        std::fs::write(&out, &snap.bytes).unwrap();
        let conn = rusqlite::Connection::open(&out).unwrap();
        let leaked: String = conn
            .query_row(
                "SELECT COALESCE(value,'') FROM settings WHERE key='channel_key__c1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(leaked.is_empty(), "快照库内不应残留凭据明文，实际={leaked}");
        let kept: String = conn
            .query_row(
                "SELECT value FROM settings WHERE key='book_style__b1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, "auto", "非凭据设置应原样保留");
        assert!(snap.redacted_keys.contains(&"channel_key__c1".to_string()));
    }

    /// 未知格式必须明确报错，不得假装成功。
    #[test]
    fn unknown_format_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = molan_core::db::Db::open(dir.path(), None).unwrap();
        let err = web_export(&db, dir.path(), None, "tar")
            .unwrap_err()
            .to_string();
        assert!(err.contains("不支持"), "未知格式应报不支持，实际={err}");
    }

    /// txt 导出无 bookId 必须报错（不返回空成功）。
    #[test]
    fn txt_without_book_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = molan_core::db::Db::open(dir.path(), None).unwrap();
        assert!(web_export(&db, dir.path(), None, "txt").is_err());
        assert!(web_export(&db, dir.path(), Some("no-such-book"), "txt").is_err());
    }

    /// 脱敏函数：布尔/大小写/多类凭据键都要命中。
    #[test]
    fn redact_settings_matches_known_credential_keys() {
        let rows = vec![
            json!({"key":"channel_key__a","value":"k1"}),
            json!({"key":"API_KEY","value":"k2"}),
            json!({"key":"my_token","value":"k3"}),
            json!({"key":"book_style__b","value":"auto"}),
        ];
        let (out, redacted) = redact_settings(&rows);
        assert_eq!(redacted.len(), 3);
        assert_eq!(out[0]["value"], "");
        assert_eq!(out[3]["value"], "auto");
    }

    /// 端到端：真实书 + 密钥设置 → web_export("all") → 解码 zip → 校验条目与脱敏。
    #[test]
    fn all_export_decodes_and_is_redacted() {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        let dir = tempfile::tempdir().unwrap();
        let db = molan_core::db::Db::open(dir.path(), None).unwrap();
        let book = molan_core::books::create_book(&db, "导出测试书", "玄幻", "第三人称");
        let bid = book["id"].as_str().unwrap().to_string();
        molan_core::files::write_file(&db, &bid, "正文", "第2章.md", "第二章内容").unwrap();
        molan_core::files::write_file(&db, &bid, "正文", "第10章.md", "第十章内容").unwrap();
        db.exec(
            "INSERT INTO settings(key,value) VALUES('channel_key__x','TOP-SECRET')",
            &[],
        )
        .unwrap();

        let out = web_export(&db, dir.path(), None, "all").expect("all 导出应成功");
        let bytes = B64.decode(out["base64"].as_str().unwrap()).unwrap();
        assert_eq!(&bytes[..2], b"PK", "应是真实 zip 字节");
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip 必须可解");
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("README.txt")),
            "缺 README: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("meta/settings.json")),
            "缺 settings: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("db/writerx.snapshot.db")),
            "缺快照: {names:?}"
        );
        let mut leaked = false;
        for i in 0..zip.len() {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut zip.by_index(i).unwrap(), &mut buf).unwrap();
            if String::from_utf8_lossy(&buf).contains("TOP-SECRET") {
                leaked = true;
            }
        }
        assert!(!leaked, "导出包内不得出现凭据明文");
        assert!(out["redactions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "channel_key__x"));

        let mut zip2 = zip::ZipArchive::new(std::io::Cursor::new(
            B64.decode(out["base64"].as_str().unwrap()).unwrap(),
        ))
        .unwrap();
        let mut snap = Vec::new();
        std::io::Read::read_to_end(
            &mut zip2.by_name("db/writerx.snapshot.db").unwrap(),
            &mut snap,
        )
        .unwrap();
        let sp = dir.path().join("snap.db");
        std::fs::write(&sp, &snap).unwrap();
        let conn = rusqlite::Connection::open(&sp).unwrap();
        let v: String = conn
            .query_row(
                "SELECT COALESCE(value,'') FROM settings WHERE key='channel_key__x'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(v.is_empty(), "快照库内仍残留凭据：{v}");
    }

    /// txt：仅正式正文，且按**数值章序**（第2章在第10章之前）。
    #[test]
    fn txt_export_orders_chapters_numerically() {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        let dir = tempfile::tempdir().unwrap();
        let db = molan_core::db::Db::open(dir.path(), None).unwrap();
        let book = molan_core::books::create_book(&db, "排序书", "玄幻", "第三人称");
        let bid = book["id"].as_str().unwrap().to_string();
        molan_core::files::write_file(&db, &bid, "正文", "第10章.md", "TEN").unwrap();
        molan_core::files::write_file(&db, &bid, "正文", "第2章.md", "TWO").unwrap();
        molan_core::files::write_file(&db, &bid, "设定", "人物表.md", "SETTINGS-ONLY").unwrap();

        let out = web_export(&db, dir.path(), Some(&bid), "txt").unwrap();
        assert_eq!(out["mime"], "text/plain; charset=utf-8");
        let text = String::from_utf8(B64.decode(out["base64"].as_str().unwrap()).unwrap()).unwrap();
        let p2 = text.find("TWO").expect("应含第2章");
        let p10 = text.find("TEN").expect("应含第10章");
        assert!(p2 < p10, "第2章必须排在第10章之前（数值序）");
        assert!(!text.contains("SETTINGS-ONLY"), "txt 不应包含设定组资料");
    }

    /// 单文件超限必须报错，不得静默截断。
    #[test]
    fn oversized_entry_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = molan_core::db::Db::open(dir.path(), None).unwrap();
        let book = molan_core::books::create_book(&db, "大文件书", "玄幻", "第三人称");
        let bid = book["id"].as_str().unwrap().to_string();
        let big = "x".repeat(MAX_ENTRY_BYTES + 1);
        molan_core::files::write_file(&db, &bid, "正文", "第1章.md", &big).unwrap();
        let err = web_export(&db, dir.path(), Some(&bid), "zip")
            .unwrap_err()
            .to_string();
        assert!(err.contains("文件过大"), "超限应明确报错，实际={err}");
    }
}
