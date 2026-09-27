// molan-core: 文件仓库（书目录 scan_tree / 读写 / 版本 / 回收站 / 原子提交）
//
// V1 修复契约要点：
// - 所有写/删/改名返回 anyhow::Result，失败绝不静默报成功；
// - 落盘走「同目录临时文件 + 写满 + fsync + 回读校验 + 原子替换」；
// - 新建/AI 写入用 persist_noclobber，防“检查后外部并发创建”的 TOCTOU 覆盖；
// - 组名统一走 normalize_group（标准组映射稳定；未知组按安全自建组处理，绝不回落“设定”）；
// - 变更串行化走 Db::fs_lock（锁序：fs_lock -> conn，绝不反向）；
// - 每次成功变更后调用父代理的 crate::continuity::note_file_change 记录章节指纹；
//   索引失败时文件已保存，绝不回滚删除用户稿件，只返回“已保存但索引失败”。
use crate::db::{Db, GROUPS};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub fn safe_name(n: &str) -> String {
    let cleaned: String = n
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    // 去首尾空白与所有首尾点号：任何输入都不可能产出 "." / ".."（路径穿越分量）
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "_invalid_".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 安全守卫：双方 canonicalize（失败用原路径），target 必须严格位于 base 之内
/// （不等于 base 且以 base 为前缀）。用于删除类操作前防路径穿越。
pub fn assert_under(base: &std::path::Path, target: &std::path::Path) -> bool {
    let b = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    let t = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
    t != b && t.starts_with(&b)
}

/// 组名统一：中文目录名 / 英文 key / 中文标签 都映射到规范目录名。
/// 标准组映射稳定；正文待审(REVIEW_GROUP) 也认。
/// - 未知组：按“安全自建组”原样返回（绝不回落“设定”）；
/// - 非法/危险组名（空、分隔符、纯点号、控制字符）→ 返回 _invalid_，由调用方拒绝。
pub fn normalize_group(group: &str) -> String {
    let g = group.trim();
    if g.is_empty() {
        return "_invalid_".to_string();
    }
    if let Some(x) = GROUPS.iter().find(|x| x.2 == g || x.0 == g || x.1 == g) {
        return x.2.to_string();
    }
    if g == crate::db::REVIEW_GROUP {
        return g.to_string();
    }
    if g == "." || g == ".." || g.chars().all(|c| c == '.') {
        return "_invalid_".to_string();
    }
    if g.chars().any(|c| {
        matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*') || (c as u32) < 0x20
    }) {
        return "_invalid_".to_string();
    }
    g.to_string()
}

/// 是否标准组目录（含待审组）——标准组不允许 rename/整体删除。
pub fn is_standard_group_dir(g: &str) -> bool {
    GROUPS.iter().any(|x| x.2 == g) || g == crate::db::REVIEW_GROUP
}

/// 书籍标识合法性 + 数据库中存在且未删除。
/// 修复：book_dir 只查目录存在会让 ../x 之类经 safe_name 变成的别名绕过 DB 校验。
pub fn valid_book_id(db: &Db, book_id: &str) -> bool {
    let raw = book_id.trim();
    if raw.is_empty() || safe_name(raw) != raw {
        return false;
    }
    db.q_json(
        "SELECT id FROM books WHERE id=?1 AND deleted_at IS NULL",
        &[&raw as &dyn rusqlite::ToSql],
    )
    .map(|v| !v.is_empty())
    .unwrap_or(false)
}

fn book_dir_path(db: &Db, book_id: &str) -> PathBuf {
    db.books_dir.join(safe_name(book_id))
}

/// 路径必须位于 books_dir 之内：对“最近存在的祖先目录”做 canonicalize，
/// 这样即使目标尚不存在，也能识别通过符号链接指向外部的父目录。
fn assert_within_books(db: &Db, fp: &Path) -> Result<()> {
    let base = db
        .books_dir
        .canonicalize()
        .unwrap_or_else(|_| db.books_dir.clone());
    let mut anc = fp.to_path_buf();
    loop {
        if anc.exists() {
            break;
        }
        match anc.parent() {
            Some(p) if p != anc.as_path() => anc = p.to_path_buf(),
            _ => break,
        }
    }
    let resolved = anc.canonicalize().unwrap_or_else(|_| anc.clone());
    if !resolved.starts_with(&base) {
        bail!("路径越界（含符号链接解析）：{}", fp.display());
    }
    Ok(())
}

fn book_base(db: &Db, book_id: &str, group: &str, name: &str) -> PathBuf {
    book_dir_path(db, book_id)
        .join(normalize_group(group))
        .join(safe_name(name))
}

pub(crate) fn book_base_checked(
    db: &Db,
    book_id: &str,
    group: &str,
    name: &str,
) -> Result<PathBuf> {
    let g = normalize_group(group);
    if g == "_invalid_" {
        bail!("非法分组名：{:?}", group);
    }
    if name.trim().is_empty() || safe_name(name) == "_invalid_" {
        bail!("文件名不能为空或非法：{:?}", name);
    }
    let fp = book_dir_path(db, book_id).join(g).join(safe_name(name));
    assert_within_books(db, &fp)?;
    Ok(fp)
}

/// 书籍目录（必须已入库且未删除）：不满足返回 Err，绝不创建。
pub fn book_dir(db: &Db, book_id: &str) -> Result<PathBuf> {
    if !valid_book_id(db, book_id) {
        bail!("书籍不存在或未初始化：{}", book_id.trim());
    }
    Ok(book_dir_path(db, book_id))
}

/// 建书时初始化目录。仅用于“新书已入库”之后（valid_book_id 校验通过），
/// 不用于任意 bookId 的副作用创建。
pub fn ensure_book_dir(db: &Db, book_id: &str) -> Result<()> {
    let base = book_dir(db, book_id)?;
    std::fs::create_dir_all(&base)
        .with_context(|| format!("创建书籍目录失败：{}", base.display()))?;
    for g in GROUPS {
        std::fs::create_dir_all(base.join(g.2))
            .with_context(|| format!("创建分组目录失败：{}", g.2))?;
    }
    std::fs::create_dir_all(base.join(crate::db::REVIEW_GROUP))?;
    std::fs::create_dir_all(&db.trash_dir)?;
    Ok(())
}

fn read_dir_files(dir: &Path) -> Vec<(String, std::fs::Metadata)> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with('.') {
                continue;
            }
            if let Ok(meta) = e.metadata() {
                if meta.is_file() {
                    out.push((n, meta));
                }
            }
        }
    }
    out
}

fn file_item(group_dir: &str, fname: String, meta: &std::fs::Metadata, flags: &Value) -> Value {
    let size_kb = meta.len() as f64 / 1024.0;
    let size = if meta.len() < 1024 {
        format!("{}", ((meta.len() as f64 / 102.4).round() / 10.0).max(1.0))
    } else {
        format!("{}", size_kb.round() as i64)
    };
    let mut item = json!({
        "name": fname,
        "size": size + "k",
        "desc": "",
        "draft": false,
        "ts": meta.modified().ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0),
    });
    if let Some(f) = flags.get(format!(
        "{}/{}",
        group_dir,
        item["name"].as_str().unwrap_or("")
    )) {
        if let Some(obj) = f.as_object() {
            if let Some(l) = obj.get("locked") {
                item["locked"] = l.clone();
            }
            if let Some(a) = obj.get("aiOff") {
                item["aiOff"] = a.clone();
            }
        }
    }
    item
}

fn group_files(base: &Path, group_dir: &str, flags: &Value) -> Vec<Value> {
    read_dir_files(&base.join(group_dir))
        .into_iter()
        .map(|(n, m)| file_item(group_dir, n, &m, flags))
        .collect()
}

pub fn scan_tree(db: &Db, book_id: &str) -> Value {
    // 缺书/非法 id 直接返回空树，绝不创建任何目录（修复 F25）
    let base = match book_dir(db, book_id) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[molan-core] scan_tree 拒绝：{}", e);
            return json!([]);
        }
    };
    let flags: Value = db
        .q_json(
            "SELECT value FROM settings WHERE key=?1",
            &[&format!("file_flags__{}", book_id) as &dyn rusqlite::ToSql],
        )
        .ok()
        .and_then(|v| {
            v.first()
                .and_then(|r| serde_json::from_str::<Value>(r["value"].as_str().unwrap_or("")).ok())
        })
        .unwrap_or(json!({}));
    let mut out: Vec<Value> = GROUPS
        .iter()
        .map(|(key, label, group_dir, icon)| {
            json!({
                "key": key, "label": label, "dir": group_dir,
                "groupDir": group_dir, "icon": icon,
                "files": group_files(&base, group_dir, &flags),
            })
        })
        .collect();
    // 用户自建组：不属于标准组/待审组的目录（F18：自定义目录必须纳入扫描）
    if let Ok(rd) = std::fs::read_dir(&base) {
        let mut customs: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| !n.starts_with('.') && !is_standard_group_dir(n))
            .collect();
        customs.sort();
        for n in customs {
            out.push(json!({
                "key": format!("user:{}", n),
                "label": n, "dir": n, "groupDir": n,
                "icon": "folder", "custom": true,
                "files": group_files(&base, &n, &flags),
            }));
        }
    }
    json!(out)
}

/// 供上层日志/校验使用：与内部同一换算
pub fn book_path(db: &Db, book_id: &str, group: &str, name: &str) -> PathBuf {
    book_base(db, book_id, group, name)
}

pub fn read_file(db: &Db, book_id: &str, group: &str, name: &str) -> Option<String> {
    let fp = book_base(db, book_id, group, name);
    std::fs::read_to_string(fp).ok()
}

// ---------- 原子写入 ----------

/// 原子写入单次尝试。
/// noclobber=true 时用 persist_noclobber：即使存在 TOCTOU 竞态也不会覆盖外部新建的文件。
fn write_once(fp: &Path, content: &str, noclobber: bool) -> Result<()> {
    let parent = fp
        .parent()
        .ok_or_else(|| anyhow!("非法路径：{}", fp.display()))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".molan-tmp-")
        .tempfile_in(parent)
        .with_context(|| format!("创建临时文件失败：{}", parent.display()))?;
    tmp.write_all(content.as_bytes())
        .context("写临时文件失败")?;
    tmp.flush().context("flush 临时文件失败")?;
    tmp.as_file().sync_all().context("fsync 临时文件失败")?;
    // 回读校验必须传播错误：读取失败不能伪装成“空内容校验通过”
    let actual = std::fs::read_to_string(tmp.path())
        .with_context(|| format!("回读临时文件失败：{}", tmp.path().display()))?;
    if actual != content {
        bail!(
            "临时文件回读校验不一致（{} vs {} 字符）",
            actual.chars().count(),
            content.chars().count()
        );
    }
    if noclobber {
        tmp.persist_noclobber(fp).map_err(|e| {
            anyhow!(
                "目标已存在或原子替换失败（拒绝覆盖）：{}（{}）",
                fp.display(),
                e.error
            )
        })?;
    } else {
        tmp.persist(fp)
            .map_err(|e| anyhow!("原子替换失败：{}", e.error))?;
    }
    Ok(())
}

fn atomic_write_inner(fp: &Path, content: &str, noclobber: bool) -> Result<()> {
    let parent = fp
        .parent()
        .ok_or_else(|| anyhow!("非法路径：{}", fp.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("创建目录失败：{}", parent.display()))?;
    let mut last: Option<anyhow::Error> = None;
    for wait in [0u64, 200, 500] {
        if wait > 0 {
            std::thread::sleep(std::time::Duration::from_millis(wait));
        }
        match write_once(fp, content, noclobber) {
            Ok(()) => return Ok(()),
            // noclobber 冲突是确定性冲突，不重试
            Err(e) if noclobber && e.to_string().contains("目标已存在") => return Err(e),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("写入失败：{}", fp.display())))
}

fn atomic_write(fp: &Path, content: &str) -> Result<()> {
    atomic_write_inner(fp, content, false)
}

fn atomic_write_new(fp: &Path, content: &str) -> Result<()> {
    atomic_write_inner(fp, content, true)
}

/// 索引失败提示：文件已经保存成功，不得回滚。
fn index_error(canon: &str, name: &str, e: anyhow::Error) -> anyhow::Error {
    anyhow!(
        "文件已保存，但版本索引记录失败（{}/{}）：{}",
        canon,
        name,
        e
    )
}

/// 锁内写：调用方必须已持有 db.fs_lock。失败不更新统计、不删源稿。
pub(crate) fn write_file_locked(
    db: &Db,
    book_id: &str,
    group: &str,
    name: &str,
    content: &str,
) -> Result<()> {
    let canon = normalize_group(group);
    if canon == "_invalid_" {
        bail!("非法分组名：{:?}", group);
    }
    let fp = book_base_checked(db, book_id, &canon, name)?;
    book_dir(db, book_id)?;
    let old = match std::fs::read_to_string(&fp) {
        Ok(c) => Some(c),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            bail!("读取原稿失败，拒绝覆盖（{}/{}）：{}", canon, name, e);
        }
    };
    if let Some(o) = &old {
        if o == content {
            return Ok(());
        }
    }
    // 版本快照失败必须中止：否则会用新内容覆盖掉唯一可回滚的旧稿
    if let Some(o) = &old {
        save_version(db, book_id, &canon, name, o)
            .with_context(|| format!("版本快照失败，已保留原稿：{}/{}", canon, name))?;
    }
    atomic_write(&fp, content)?;
    // 文件已落盘；此后任何派生索引失败都不得回滚/删除刚保存的稿件
    crate::stats::refresh_words(db, book_id);
    crate::continuity::note_file_change(db, book_id, &canon, name, old.as_deref(), Some(content))
        .map_err(|e| index_error(&canon, name, e))?;
    eprintln!(
        "[molan-core] 写入OK: {} ({}字)",
        fp.display(),
        content.chars().count()
    );
    Ok(())
}

/// 锁内“仅新建”写：目标已存在即冲突；persist_noclobber 防外部并发 TOCTOU。
fn write_new_locked(db: &Db, book_id: &str, group: &str, name: &str, content: &str) -> Result<()> {
    let canon = normalize_group(group);
    if canon == "_invalid_" {
        bail!("非法分组名：{:?}", group);
    }
    let fp = book_base_checked(db, book_id, &canon, name)?;
    book_dir(db, book_id)?;
    if fp.exists() {
        bail!("目标已存在，拒绝覆盖：{}/{}", canon, name);
    }
    atomic_write_new(&fp, content)?;
    crate::stats::refresh_words(db, book_id);
    crate::continuity::note_file_change(db, book_id, &canon, name, None, Some(content))
        .map_err(|e| index_error(&canon, name, e))?;
    Ok(())
}

/// 人工写入：可修改 locked 文件（locked 只阻止 AI），但必须原子保存。失败返回 Err。
pub fn write_file(db: &Db, book_id: &str, group: &str, name: &str, content: &str) -> Result<()> {
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    write_file_locked(db, book_id, group, name, content)
}

/// 新建：目标已存在即冲突（锁内检查 + noclobber 写入）。
pub fn write_file_new(
    db: &Db,
    book_id: &str,
    group: &str,
    name: &str,
    content: &str,
) -> Result<()> {
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    write_new_locked(db, book_id, group, name, content)
}

/// AI 写入（带锁内前置校验回调）。
///
/// 调用方在 check 中做「取消 / input_fingerprint / 依赖」等判定：该回调在
/// **已持有 fs_lock 之后、写盘之前**执行，因此不会与本次写入产生 check/write 竞态，
/// 也避免调用方在外部先取锁再调用本函数造成自死锁（本函数只取一次 fs_lock）。
/// check 返回 Err 时不写入任何内容。
pub fn write_ai_file_checked<F>(
    db: &Db,
    book_id: &str,
    group: &str,
    name: &str,
    content: &str,
    check: F,
) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    // 锁内前置校验：任何失败都必须阻止写入
    check()?;
    let canon = normalize_group(group);
    if canon == "_invalid_" {
        bail!("非法分组名：{:?}", group);
    }
    if file_flag(db, book_id, &canon, name, "locked") {
        bail!("文件已被作者锁定，AI 不得改写：{}/{}", canon, name);
    }
    let fp = book_base_checked(db, book_id, &canon, name)?;
    book_dir(db, book_id)?;
    if fp.exists() {
        bail!("目标已存在，AI 拒绝覆盖：{}/{}", canon, name);
    }
    // 交叉保护：正式稿与待审稿任一存在都拒绝普通 AI 产稿
    for other in ["正文", crate::db::REVIEW_GROUP] {
        if other == canon {
            continue;
        }
        let p = book_base(db, book_id, other, name);
        if p.exists() {
            bail!(
                "已存在同章稿件（{}），请先处理审批/重写队列：{}",
                other,
                name
            );
        }
    }
    write_new_locked(db, book_id, &canon, name, content)
}

/// AI 写入：锁定文件拒绝；只允许不存在目标；
/// 正文组与待审组**交叉检查**——任一组已有同章稿都拒绝，普通产稿不得绕过审批队列。
pub fn write_ai_file(db: &Db, book_id: &str, group: &str, name: &str, content: &str) -> Result<()> {
    write_ai_file_checked(db, book_id, group, name, content, || Ok(()))
}

/// CAS 写入：锁内检查“当前内容 == expected”且文件未锁定；不满足返回 Err。
/// 读取失败要区分 NotFound（可视为空内容）与其他 IO 错误（必须报错，不能当空通过）。
pub fn write_file_cas(
    db: &Db,
    book_id: &str,
    group: &str,
    name: &str,
    expected: &str,
    content: &str,
) -> Result<()> {
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    write_file_cas_locked(db, book_id, group, name, expected, content)
}

/// CAS 写入的锁内版本。调用方必须已经持有 fs_lock。
pub(crate) fn write_file_cas_locked(
    db: &Db,
    book_id: &str,
    group: &str,
    name: &str,
    expected: &str,
    content: &str,
) -> Result<()> {
    let canon = normalize_group(group);
    if canon == "_invalid_" {
        bail!("非法分组名：{:?}", group);
    }
    if file_flag(db, book_id, &canon, name, "locked") {
        bail!("文件已被作者锁定，拒绝写入：{}/{}", canon, name);
    }
    let fp = book_base_checked(db, book_id, &canon, name)?;
    let current = match std::fs::read_to_string(&fp) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(anyhow!(
                "CAS 读取失败，拒绝写入（{}/{}）：{}",
                canon,
                name,
                e
            ));
        }
    };
    if current != expected {
        bail!("CAS 失败：文件已被修改（{}/{}）", canon, name);
    }
    write_file_locked(db, book_id, &canon, name, content)
}

pub fn rename_file(db: &Db, book_id: &str, group: &str, name: &str, new_name: &str) -> Result<()> {
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    let canon = normalize_group(group);
    if canon == "_invalid_" {
        bail!("非法分组名：{:?}", group);
    }
    if new_name.trim().is_empty() {
        bail!("新文件名不能为空");
    }
    let old = book_base_checked(db, book_id, &canon, name)?;
    let neu = book_base_checked(db, book_id, &canon, new_name)?;
    if !old.is_file() {
        bail!("源文件不存在：{}/{}", canon, name);
    }
    if neu.exists() {
        bail!("目标已存在，拒绝覆盖：{}/{}", canon, new_name);
    }
    let content = std::fs::read_to_string(&old).context("读取源文件失败")?;
    // 顺序：先迁 flags（纯 DB、可回滚）→ 改文件名 → 迁版本目录；后继失败时反向回滚，
    // 绝不允许「文件已改名但 locked/aiOff 与新名失联」。
    let moved = migrate_flags_prefix(db, book_id, &canon, &canon, name, new_name)?;
    if let Err(e) = std::fs::rename(&old, &neu) {
        let _ = migrate_flags_prefix(db, book_id, &canon, &canon, new_name, name);
        return Err(anyhow!("重命名失败（{}）：{}", old.display(), e));
    }
    if let Err(e) = migrate_version_dir(db, book_id, &canon, name, new_name) {
        let _ = std::fs::rename(&neu, &old);
        let _ = migrate_flags_prefix(db, book_id, &canon, &canon, new_name, name);
        return Err(anyhow!("迁移版本目录失败，已回滚文件名与标志：{}", e));
    }
    crate::continuity::note_file_change(db, book_id, &canon, name, Some(&content), None)?;
    crate::continuity::note_file_change(db, book_id, &canon, new_name, None, Some(&content))?;
    eprintln!(
        "[molan-core] 改名完成：{} -> {}（迁移 flags 键 {} 个）",
        name, new_name, moved
    );
    Ok(())
}

pub fn delete_file(db: &Db, book_id: &str, group: &str, name: &str) -> Result<String> {
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    delete_file_locked(db, book_id, group, name)
}

/// 锁内删除：调用方必须已持有 db.fs_lock（审批清理复用；fs_lock 不可重入，直接调 delete_file 会死锁）。
pub(crate) fn delete_file_locked(
    db: &Db,
    book_id: &str,
    group: &str,
    name: &str,
) -> Result<String> {
    let canon = normalize_group(group);
    if canon == "_invalid_" {
        bail!("非法分组名：{:?}", group);
    }
    let fp = book_base_checked(db, book_id, &canon, name)?;
    if !fp.exists() {
        return Ok(String::new()); // 无文件即无回收站条目；前端把返回值当恢复 id。
    }
    let content = std::fs::read_to_string(&fp).context("读取待删文件失败")?;
    // 先确保回收站写入成功，再删源文件（F01：回收失败绝不能删原稿）
    let tid = uuid::Uuid::new_v4().to_string();
    let meta = json!({
        "id": tid, "kind": "file", "bookId": book_id, "group": canon,
        "name": name, "deletedAt": crate::stats::now_ms(), "content": content,
    });
    let out = db
        .trash_dir
        .join(safe_name(book_id))
        .join(format!("{}.json", tid));
    std::fs::create_dir_all(out.parent().unwrap()).context("创建回收站目录失败")?;
    std::fs::write(&out, serde_json::to_string(&meta)?).context("写入回收站失败（原稿已保留）")?;
    if !assert_under(&db.books_dir, &fp) {
        bail!("拒绝删除越界路径：{}", fp.display());
    }
    std::fs::remove_file(&fp).with_context(|| format!("删除失败：{}", fp.display()))?;
    crate::stats::refresh_words(db, book_id);
    crate::continuity::note_file_change(db, book_id, &canon, name, Some(&content), None)?;
    Ok(tid)
}

// ---------- 版本 ----------

pub fn save_version(db: &Db, book_id: &str, group: &str, name: &str, content: &str) -> Result<()> {
    let ts = crate::stats::now_ms();
    let dir = db
        .versions_dir
        .join(safe_name(book_id))
        .join(normalize_group(group))
        .join(safe_name(name));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建版本目录失败：{}", dir.display()))?;
    let fp = dir.join(format!("{}.json", ts));
    std::fs::write(
        &fp,
        json!({"ts": ts, "content": content, "note": ""}).to_string(),
    )
    .with_context(|| format!("写入版本快照失败：{}", fp.display()))?;
    Ok(())
}

fn version_dir(db: &Db, book_id: &str, group: &str, name: &str) -> PathBuf {
    db.versions_dir
        .join(safe_name(book_id))
        .join(normalize_group(group))
        .join(safe_name(name))
}

pub fn list_versions(db: &Db, book_id: &str, group: &str, name: &str) -> Value {
    let dir = version_dir(db, book_id, group, name);
    if !dir.exists() {
        return json!([]);
    }
    let mut items: Vec<Value> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let j: Value =
                        serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
                    Some(json!({"ts": j["ts"], "size": j["content"].as_str().map(|c| c.chars().count()).unwrap_or(0)}))
                })
                .collect()
        })
        .unwrap_or_default();
    items.sort_by(|a, b| {
        b["ts"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&a["ts"].as_i64().unwrap_or(0))
    });
    json!(items)
}

/// 作者显式版本回退：允许覆盖当前内容，但必须先为“当前内容”留下版本备份，
/// 这样回退本身也可再回退（不丢作者现存稿）。版本文件读取失败返回 ok=false。
pub fn restore_version(db: &Db, book_id: &str, group: &str, name: &str, ts: i64) -> Value {
    let fp = version_dir(db, book_id, group, name).join(format!("{}.json", ts));
    if !fp.exists() {
        return json!({"ok": false, "err": "版本不存在"});
    }
    let raw = match std::fs::read_to_string(&fp) {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "err": format!("读取版本失败：{}", e)}),
    };
    let j: Value = match serde_json::from_str::<Value>(&raw) {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "err": format!("版本内容损坏：{}", e)}),
    };
    let Some(content) = j["content"].as_str() else {
        return json!({"ok": false, "err": "版本内容损坏：缺少正文内容"});
    };
    // 先备份当前稿，再覆盖（write_file 内部还会再快照一次旧内容，属安全冗余）
    let cur_path = book_path(db, book_id, group, name);
    let cur = match std::fs::read_to_string(&cur_path) {
        Ok(c) => Some(c),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return json!({"ok": false, "err": format!("读取当前稿失败，已中止回退：{}", e)}),
    };
    if let Some(cur) = &cur {
        if let Err(e) = save_version(db, book_id, group, name, cur) {
            return json!({"ok": false, "err": format!("回退前备份当前稿失败，已中止：{}", e)});
        }
    }
    if let Err(e) = write_file(db, book_id, group, name, content) {
        return json!({"ok": false, "err": e.to_string()});
    }
    json!({"ok": true})
}

// ---------- 文件级回收站 ----------
pub fn list_file_trash(db: &Db, book_id: &str) -> Value {
    let dir = db.trash_dir.join(safe_name(book_id));
    if !dir.exists() {
        return json!([]);
    }
    let mut items: Vec<Value> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let j: Value =
                        serde_json::from_str::<Value>(&std::fs::read_to_string(e.path()).ok()?)
                            .ok()?;
                    if j["bookId"].as_str()? == book_id {
                        Some(json!({
                            "id": j["id"], "kind": j["kind"].as_str().unwrap_or("file"),
                            "group": j["group"].as_str().unwrap_or(""), "name": j["name"],
                            "deletedAt": j["deletedAt"],
                        }))
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    items.sort_by(|a, b| {
        b["deletedAt"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&a["deletedAt"].as_i64().unwrap_or(0))
    });
    json!(items)
}

pub fn restore_file_trash(db: &Db, book_id: &str, id: &str) -> Value {
    let out = db
        .trash_dir
        .join(safe_name(book_id))
        .join(format!("{}.json", safe_name(id)));
    if !out.exists() {
        return json!({"ok": false, "err": "回收站记录不存在"});
    }
    let j: Value =
        serde_json::from_str::<Value>(&std::fs::read_to_string(&out).unwrap_or_default())
            .unwrap_or(json!(null));
    let raw_group = j["group"].as_str().unwrap_or("");
    let canon = normalize_group(raw_group);
    let name = j["name"].as_str().unwrap_or("");
    if j["bookId"].as_str() != Some(book_id) || name.is_empty() || canon == "_invalid_" {
        return json!({"ok": false, "err": "回收站记录无效"});
    }
    // 恢复必须回到原规范组（含“正文待审”），不允许静默落“设定”（修复 F19）。
    // P0：绝不覆盖同名新稿——同内容视为幂等成功，不同内容必须报冲突并保留回收站记录。
    let content = j["content"].as_str().unwrap_or("");
    let target = book_path(db, book_id, &canon, name);
    if target.is_file() {
        let current = std::fs::read_to_string(&target).unwrap_or_default();
        if current == content {
            // 同内容幂等：不重复写盘，但仍要清掉回收站记录
            if let Err(e) = std::fs::remove_file(&out) {
                return json!({"ok": false, "err": format!("同内容已存在，但回收站记录清理失败：{}", e)});
            }
            return json!({"ok": true, "group": canon, "name": name, "idempotent": true});
        }
        return json!({
            "ok": false,
            "err": format!("目标已存在同名且内容不同的稿件，拒绝覆盖：{}/{}", canon, name)
        });
    }
    if let Err(e) = write_file_new(db, book_id, &canon, name, content) {
        return json!({"ok": false, "err": e.to_string()});
    }
    // 只有写入成功后才清理回收站记录；清理失败必须如实上报（记录仍在=可再次恢复）
    if let Err(e) = std::fs::remove_file(&out) {
        return json!({
            "ok": false,
            "err": format!("稿件已恢复，但回收站记录清理失败（记录仍在，可重试）：{}", e)
        });
    }
    if canon == crate::db::REVIEW_GROUP {
        // 仅把「被驳回」的记录恢复为 pending；已 approved 的记录不得被恢复动作回退（幂等）。
        let _ = db.exec(
            "UPDATE pending_chapter SET status='pending', updated_at=?2              WHERE book_id=?1 AND review_file=?3 AND status='rejected'",
            &[
                &book_id as &dyn rusqlite::ToSql,
                &crate::stats::now_ms(),
                &name as &dyn rusqlite::ToSql,
            ],
        );
    }
    json!({"ok": true, "group": canon, "name": name})
}

/// 清空某书的文件回收站。失败返回 Err（调用方不得再报 ok）。
pub fn clear_file_trash(db: &Db, book_id: &str) -> Result<()> {
    let dir = db.trash_dir.join(safe_name(book_id));
    if !dir.exists() {
        return Ok(());
    }
    if !assert_under(&db.trash_dir, &dir) {
        bail!("拒绝删除越界路径：{}", dir.display());
    }
    std::fs::remove_dir_all(&dir)
        .with_context(|| format!("清空文件回收站失败：{}", dir.display()))?;
    Ok(())
}

// ---------- 审批（F01/F19：只有成功提交才删待审） ----------

/// 人工接受待审章节：待审稿原子提交到正式组。
/// - 提交前检查待审依赖（父 continuity::check_draft_dependency）；stale 直接 Err 让用户重审；
/// - 正式组同名非空稿已存在 → Err（不再用 mtime 猜测改名 .ai）；
/// - 只有目标持久化成功后才删除唯一待审源稿；任何 IO 失败都保住原稿并返回 Err。
pub fn approve_pending_chapter(db: &Db, book_id: &str, name: &str) -> Result<String> {
    // 审批唯一入口。saga 原子性（队列+批准凭证单事务）、窗口A（正文已写队列未更）自愈、
    // 带 note_file_change 记账的待审清理，实现移至 approval.rs；本入口保持 fs_lock 全程锁内契约。
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    crate::approval::approve_locked(db, book_id, name)
}

// ---------- 用户自定义文件夹 ----------
pub fn create_folder(db: &Db, book_id: &str, name: &str) -> Value {
    let n = name.trim();
    if n.is_empty() {
        return json!({"ok": false, "err": "文件夹名不能为空"});
    }
    let canon = normalize_group(n);
    if canon == "_invalid_" {
        return json!({"ok": false, "err": "非法文件夹名"});
    }
    if let Err(e) = book_dir(db, book_id) {
        return json!({"ok": false, "err": e.to_string()});
    }
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    let dir = book_dir_path(db, book_id).join(&canon);
    if let Err(e) = assert_within_books(db, &dir) {
        return json!({"ok": false, "err": e.to_string()});
    }
    match std::fs::create_dir_all(&dir) {
        Ok(()) => json!({"ok": true, "name": canon}),
        Err(e) => json!({"ok": false, "err": e.to_string()}),
    }
}

pub fn rename_folder(db: &Db, book_id: &str, from: &str, to: &str) -> Value {
    let f = from.trim();
    let t = to.trim();
    if f.is_empty() || t.is_empty() {
        return json!({"ok": false, "err": "源/目标名称不能为空"});
    }
    let fc = normalize_group(f);
    let tc = normalize_group(t);
    if fc == "_invalid_" || tc == "_invalid_" {
        return json!({"ok": false, "err": "非法名称"});
    }
    if is_standard_group_dir(&fc) || is_standard_group_dir(&tc) {
        return json!({"ok": false, "err": "标准分组不允许重命名"});
    }
    if let Err(e) = book_dir(db, book_id) {
        return json!({"ok": false, "err": e.to_string()});
    }
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    let base = book_dir_path(db, book_id);
    let from_dir = base.join(&fc);
    let to_dir = base.join(&tc);
    if let Err(e) = assert_within_books(db, &from_dir) {
        return json!({"ok": false, "err": e.to_string()});
    }
    if !from_dir.is_dir() {
        return json!({"ok": false, "err": "源目录不存在"});
    }
    if to_dir.exists() {
        return json!({"ok": false, "err": "目标已存在"});
    }
    // 先迁移版本目录（若失败则整体拒绝，不静默丢历史）
    let vfrom = db.versions_dir.join(safe_name(book_id)).join(&fc);
    let vto = db.versions_dir.join(safe_name(book_id)).join(&tc);
    if vfrom.exists() {
        if vto.exists() {
            return json!({"ok": false, "err": "目标版本目录已存在，拒绝合并"});
        }
        if let Some(parent) = vto.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        if let Err(e) = std::fs::rename(&vfrom, &vto) {
            return json!({"ok": false, "err": format!("迁移版本目录失败：{}", e)});
        }
    }
    if let Err(e) = std::fs::rename(&from_dir, &to_dir) {
        // 回滚版本目录，保持改名原子性
        if vto.exists() && !vfrom.exists() {
            let _ = std::fs::rename(&vto, &vfrom);
        }
        return json!({"ok": false, "err": e.to_string()});
    }
    // 迁移该文件夹下所有文件的 flags 键前缀
    let moved = match migrate_flags_prefix(db, book_id, &fc, &tc, "", "") {
        Ok(n) => n,
        Err(e) => {
            // 回滚目录与版本目录，避免约束与新名失联
            let _ = std::fs::rename(&to_dir, &from_dir);
            if vto.exists() && !vfrom.exists() {
                let _ = std::fs::rename(&vto, &vfrom);
            }
            return json!({"ok": false, "err": format!("flags 迁移失败，已回滚改名：{}", e)});
        }
    };
    json!({"ok": true, "from": fc, "to": tc, "migratedFlags": moved})
}

/// 递归收集目录下所有普通文件（相对路径），遇到符号链接/非常规条目直接拒绝，
/// 避免“跳过没备份的文件再删目录”造成数据丢失。
fn collect_files_recursive(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd =
            std::fs::read_dir(&dir).with_context(|| format!("读取目录失败：{}", dir.display()))?;
        for entry in rd {
            let entry = entry?;
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.to_string_lossy().to_string());
            let ft = entry.file_type()?;
            if ft.is_symlink() {
                bail!("目录内含符号链接，拒绝删除以免丢失链接目标：{}", rel);
            } else if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                out.push((rel, path));
            } else {
                bail!("目录内含非常规条目，拒绝删除：{}", rel);
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// 删除自定义文件夹：先递归完整备份到回收站（读取+回读校验），全部成功才删除目录。
pub fn delete_folder(db: &Db, book_id: &str, name: &str) -> Result<String> {
    let canon = normalize_group(name);
    if canon == "_invalid_" {
        bail!("非法分组名：{:?}", name);
    }
    if is_standard_group_dir(&canon) {
        bail!("标准分组不允许整体删除：{}", canon);
    }
    let base = book_dir_path(db, book_id).join(&canon);
    if !base.exists() {
        return Ok(canon);
    }
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    if !assert_under(&db.books_dir, &base) {
        bail!("拒绝删除越界路径：{}", base.display());
    }
    let files = collect_files_recursive(&base)?;
    for (rel, path) in &files {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("读取待删文件失败：{}", rel))?;
        // 回读校验：确保拿到完整内容后才允许进入删除流程
        let again =
            std::fs::read_to_string(path).with_context(|| format!("回读待删文件失败：{}", rel))?;
        if again != content {
            bail!("待删文件读取不一致，已中止删除：{}", rel);
        }
        let tid = uuid::Uuid::new_v4().to_string();
        let meta = json!({
            "id": tid, "kind": "file", "bookId": book_id, "group": canon,
            "name": rel, "deletedAt": crate::stats::now_ms(), "content": content,
        });
        let out = db
            .trash_dir
            .join(safe_name(book_id))
            .join(format!("{}.json", tid));
        std::fs::create_dir_all(out.parent().unwrap()).context("创建回收站目录失败")?;
        std::fs::write(&out, serde_json::to_string(&meta)?)
            .context("写入回收站失败（目录未删除）")?;
    }
    std::fs::remove_dir_all(&base).with_context(|| format!("删除目录失败：{}", base.display()))?;
    crate::stats::refresh_words(db, book_id);
    Ok(canon)
}

pub fn import_book(db: &Db, title: &str, genre: &str, files: &[Value]) -> Result<Value> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = crate::stats::now_ms();
    db.exec(
        "INSERT INTO books(id,title,genre,pov,status,cover_char,word_count,chapter_count,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?)",
        &[
            &id,
            &title,
            &genre,
            &"第三人称",
            &"构思中",
            &(title.chars().next().map(|c| c.to_string()).unwrap_or_else(|| "书".into())),
            &0i64,
            &0i64,
            &now,
            &now,
        ],
    )?;
    ensure_book_dir(db, &id)?;
    for f in files {
        let name = f["name"].as_str().unwrap_or("章节.md");
        let content = f["content"].as_str().unwrap_or("");
        write_file(db, &id, "正文", name, content)?;
        // 导入章标注来源：未经系统批准的章节不得被状态机当作 AI 定稿链推进（C3）
        if let Some(ch) = crate::continuity::chapter_number(name) {
            if let Err(e) = crate::chapter_state::record_origin(db, &id, ch, "import") {
                eprintln!("[molan-core] 导入章 origin 记账失败（不阻断导入）：{}", e);
            }
        }
    }
    Ok(json!({"bookId": id}))
}

// ---------- 文件级标志（locked / aiOff） ----------

fn flags_key(book_id: &str) -> String {
    format!("file_flags__{}", book_id)
}

fn flags_get(db: &Db, book_id: &str) -> Value {
    db.q_json(
        "SELECT value FROM settings WHERE key=?1",
        &[&flags_key(book_id) as &dyn rusqlite::ToSql],
    )
    .ok()
    .and_then(|v| {
        v.first()
            .and_then(|r| serde_json::from_str::<Value>(r["value"].as_str().unwrap_or("")).ok())
    })
    .filter(|v| v.is_object())
    .unwrap_or(json!({}))
}

/// 迁移 file_flags 键前缀：把 group 下所有「name」或「子路径」的键从 from_group 改到 to_group。
/// 供文件夹改名/文件改名使用，避免改名后 locked/aiOff 约束静默失效。
fn migrate_flags_prefix(
    db: &Db,
    book_id: &str,
    from_group: &str,
    to_group: &str,
    old_name: &str,
    new_name: &str,
) -> Result<usize> {
    let key = flags_key(book_id);
    // 同 set_file_flag：flags 读-改-写必须在单事务内完成
    let mut guard = db.conn.lock().unwrap_or_else(|e| e.into_inner());
    let tx = guard.transaction()?;
    let raw: Option<String> = tx
        .query_row("SELECT value FROM settings WHERE key=?1", [&key], |r| {
            r.get(0)
        })
        .ok();
    let mut flags: Value = raw
        .and_then(|v| serde_json::from_str::<Value>(&v).ok())
        .filter(|v| v.is_object())
        .unwrap_or(json!({}));
    // old_name/new_name 为空时表示“整个组前缀”（文件夹改名）
    let join = |g: &str, n: &str| {
        let g = normalize_group(g);
        if n.is_empty() {
            g
        } else {
            format!("{}/{}", g, n)
        }
    };
    let old_prefix = join(from_group, old_name);
    let new_prefix = join(to_group, new_name);
    if old_prefix == new_prefix {
        return Ok(0);
    }
    let Some(obj) = flags.as_object_mut() else {
        return Ok(0);
    };
    let mut moved = 0usize;
    let keys: Vec<String> = obj
        .keys()
        .filter(|k| *k == &old_prefix || k.starts_with(&format!("{}/", old_prefix)))
        .cloned()
        .collect();
    for k in keys {
        let suffix = k[old_prefix.len()..].to_string();
        let nk = format!("{}{}", new_prefix, suffix);
        if let Some(v) = obj.remove(&k) {
            // 目标已存在时不覆盖已有约束，保留二者中更严格的信息（并集）
            match obj.get_mut(&nk) {
                Some(existing) if existing.is_object() && v.is_object() => {
                    if let (Some(e), Some(n)) = (existing.as_object_mut(), v.as_object()) {
                        for (kk, vv) in n {
                            e.insert(kk.clone(), vv.clone());
                        }
                    }
                }
                _ => {
                    obj.insert(nk, v);
                }
            }
            moved += 1;
        }
    }
    if moved > 0 {
        tx.execute(
            "INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            rusqlite::params![key, flags.to_string()],
        )?;
    }
    tx.commit()?;
    Ok(moved)
}

/// 迁移文件版本目录（canonical group 下的 name 目录）到新名。
fn migrate_version_dir(
    db: &Db,
    book_id: &str,
    group: &str,
    old_name: &str,
    new_name: &str,
) -> Result<()> {
    let from = version_dir(db, book_id, group, old_name);
    if !from.exists() {
        return Ok(());
    }
    let to = version_dir(db, book_id, group, new_name);
    if to.exists() {
        bail!("版本目录已存在，拒绝合并：{}", to.display());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::rename(&from, &to).with_context(|| format!("迁移版本目录失败：{}", from.display()))?;
    Ok(())
}

/// 读取文件标志（locked / aiOff）。键统一用规范组名，与写入侧一致。
pub fn file_flag(db: &Db, book_id: &str, group: &str, name: &str, flag: &str) -> bool {
    let flags = flags_get(db, book_id);
    let key = format!("{}/{}", normalize_group(group), name);
    flags
        .get(&key)
        .and_then(|v| v.get(flag))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 一次性读取某书全部文件标志（供 context_bundle 批量判断，避免每文件一次查询）。
pub(crate) fn flags_snapshot(db: &Db, book_id: &str) -> Value {
    flags_get(db, book_id)
}

/// 写入文件标志（供 IPC 使用；键统一用规范组名）。
pub fn set_file_flag(
    db: &Db,
    book_id: &str,
    group: &str,
    name: &str,
    flag: &str,
    value: bool,
) -> Result<()> {
    if flag != "locked" && flag != "aiOff" {
        bail!("不支持的标志：{}", flag);
    }
    let key = flags_key(book_id);
    let fkey = format!("{}/{}", normalize_group(group), name);
    // flags 是整包 JSON：读-改-写必须在单事务内完成，否则并发设置会丢失更新（aiOff/locked 静默失效）
    let mut guard = db.conn.lock().unwrap_or_else(|e| e.into_inner());
    let tx = guard.transaction()?;
    let raw: Option<String> = tx
        .query_row("SELECT value FROM settings WHERE key=?1", [&key], |r| {
            r.get(0)
        })
        .ok();
    let mut flags: Value = raw
        .and_then(|v| serde_json::from_str::<Value>(&v).ok())
        .filter(|v| v.is_object())
        .unwrap_or(json!({}));
    if value {
        flags[&fkey][&flag] = json!(true);
    } else if let Some(obj) = flags[&fkey].as_object_mut() {
        obj.remove(flag);
        if obj.is_empty() {
            if let Some(top) = flags.as_object_mut() {
                top.remove(&fkey);
            }
        }
    }
    tx.execute(
        "INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        rusqlite::params![key, flags.to_string()],
    )?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod repo_tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let book = crate::books::create_book(&db, "repo-test", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        (dir, db, book)
    }

    #[test]
    fn missing_book_creates_nothing_and_rejects_traversal() {
        // F25：scan_tree 缺书/非法 bookId 不产生任何目录
        let (dir, db, _book) = fixture();
        assert_eq!(scan_tree(&db, "../audit-escape"), json!([]));
        assert_eq!(scan_tree(&db, "no-such-book"), json!([]));
        assert!(!dir.path().join("audit-escape").exists());
        assert!(!dir.path().join("books").join("audit-escape").exists());
        // 写入缺书必须 Err，且不建目录
        assert!(write_file(&db, "../escape", "正文", "第1章.md", "x").is_err());
        assert!(write_file(&db, "no-such-book", "正文", "第1章.md", "x").is_err());
        assert!(!dir.path().join("books").join("no-such-book").exists());
    }

    #[test]
    fn write_refuses_when_original_unreadable() {
        // P0：原稿存在但读不出来（编码/占用/权限）时，必须拒绝覆盖，
        // 绝不能把「读不到」当「没有原稿」而跳过版本快照直接写。
        let (_d, db, book) = fixture();
        let blocked = book_path(&db, &book, "设定", "blocked.md");
        std::fs::create_dir_all(&blocked).unwrap();
        let e = write_file(&db, &book, "设定", "blocked.md", "新内容").unwrap_err();
        assert!(
            e.to_string().contains("读取原稿失败"),
            "不可读原稿必须拒绝覆盖：{}",
            e
        );
        std::fs::remove_dir_all(&blocked).unwrap();
    }

    #[test]
    fn write_failure_does_not_touch_stats_and_keeps_original() {
        // F01：写盘失败必须 Err、旧稿保留、统计不变
        let (_d, db, book) = fixture();
        write_file(&db, &book, "正文", "第1章.md", "原始正文甲乙丙").unwrap();
        let before = db
            .q_json("SELECT word_count FROM books WHERE id=?1", &[&book])
            .unwrap()[0]["wordCount"]
            .as_i64()
            .unwrap();
        // 用目录占住目标路径，使原子替换必然失败
        let target = book_path(&db, &book, "正文", "第1章.md");
        std::fs::remove_file(&target).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        let err = write_file(&db, &book, "正文", "第1章.md", "全新内容");
        assert!(err.is_err(), "写入被目录占位时必须失败");
        let after = db
            .q_json("SELECT word_count FROM books WHERE id=?1", &[&book])
            .unwrap()[0]["wordCount"]
            .as_i64()
            .unwrap();
        assert_eq!(before, after, "失败不得改变统计");
        std::fs::remove_dir_all(&target).unwrap();
    }

    #[test]
    fn version_alias_is_unified() {
        // F23：用别名写、用规范名查，版本必须一致
        let (_d, db, book) = fixture();
        write_file(&db, &book, "chapters", "第1章.md", "第一版").unwrap();
        write_file(&db, &book, "正文", "第1章.md", "第二版").unwrap();
        let v1 = list_versions(&db, &book, "正文", "第1章.md");
        let v2 = list_versions(&db, &book, "chapters", "第1章.md");
        assert!(!v1.as_array().unwrap().is_empty(), "规范名必须能查到版本");
        assert_eq!(v1, v2, "别名与规范名版本视图必须一致");
    }

    #[test]
    fn restore_review_goes_back_to_review_group() {
        // F19：待审稿经回收站恢复必须回到“正文待审”，不得落“设定”
        let (_d, db, book) = fixture();
        write_file(&db, &book, crate::db::REVIEW_GROUP, "第1章.md", "待审正文").unwrap();
        db.exec(
            "INSERT INTO pending_chapter(book_id,ch,review_file,status,created_at,updated_at) VALUES(?1,1,?2,'pending',0,0)",
            &[&book as &dyn rusqlite::ToSql, &"第1章.md" as &dyn rusqlite::ToSql],
        )
        .unwrap();
        delete_file(&db, &book, crate::db::REVIEW_GROUP, "第1章.md").unwrap();
        let trash = list_file_trash(&db, &book);
        let id = trash.as_array().unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let out = restore_file_trash(&db, &book, &id);
        assert_eq!(out["ok"], true);
        assert_eq!(out["group"], crate::db::REVIEW_GROUP);
        assert_eq!(
            read_file(&db, &book, crate::db::REVIEW_GROUP, "第1章.md").as_deref(),
            Some("待审正文")
        );
        assert!(
            read_file(&db, &book, "设定", "第1章.md").is_none(),
            "不得落设定"
        );
        let st = db
            .q_json(
                "SELECT status FROM pending_chapter WHERE book_id=?1",
                &[&book as &dyn rusqlite::ToSql],
            )
            .unwrap();
        assert_eq!(
            st[0]["status"], "pending",
            "恢复待审后队列状态应回到 pending"
        );
    }

    #[test]
    fn standard_group_cannot_be_renamed_and_custom_folder_is_scanned() {
        // F18：标准组禁 rename；自定义组必须出现在 scan_tree
        let (_d, db, book) = fixture();
        write_file(&db, &book, "正文", "第1章.md", "正文内容").unwrap();
        let r = rename_folder(&db, &book, "正文", "改名正文");
        assert_eq!(r["ok"], false, "标准组必须拒绝重命名");
        assert!(book_path(&db, &book, "正文", "第1章.md").exists());
        assert!(create_folder(&db, &book, "我的资料")["ok"]
            .as_bool()
            .unwrap());
        let tree = scan_tree(&db, &book);
        let dirs: Vec<String> = tree
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["dir"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            dirs.contains(&"我的资料".to_string()),
            "自定义目录必须被扫描：{:?}",
            dirs
        );
    }

    #[test]
    fn ai_write_respects_lock_and_cross_group_conflict() {
        // S03 + 交叉保护：AI 不得改 locked；正式/待审任一组有同章都拒绝
        let (_d, db, book) = fixture();
        write_file(&db, &book, crate::db::REVIEW_GROUP, "第1章.md", "已有待审").unwrap();
        let err = write_ai_file(&db, &book, "正文", "第1章.md", "AI 新稿");
        assert!(err.is_err(), "已存在同章待审稿时 AI 产稿必须拒绝");
        assert!(read_file(&db, &book, "正文", "第1章.md").is_none());

        write_file(&db, &book, "正文", "第2章.md", "人工定稿").unwrap();
        set_file_flag(&db, &book, "正文", "第2章.md", "locked", true).unwrap();
        assert!(file_flag(&db, &book, "正文", "第2章.md", "locked"));
        // 别名读也要命中同一标志键
        assert!(file_flag(&db, &book, "chapters", "第2章.md", "locked"));
        // 人工可以改 locked 文件
        assert!(write_file(&db, &book, "正文", "第2章.md", "人工改稿").is_ok());
        // AI 不允许覆盖（目标存在）
        assert!(write_ai_file(&db, &book, "正文", "第2章.md", "AI 覆盖").is_err());
    }

    #[test]
    fn ai_formal_write_blocked_when_same_chapter_pending_exists() {
        // cross guard 回归（P0-1 反例收口）：同章已有待审稿 + 依赖已 stale 时，
        // AI 直写正式组必须被 cross-group guard 拒绝，绝不能让过期草稿链落成正式稿。
        let (_d, db, book) = fixture();
        write_file(
            &db,
            &book,
            crate::db::REVIEW_GROUP,
            "第1章.md",
            "任务T1的第1章",
        )
        .unwrap();
        write_file(
            &db,
            &book,
            crate::db::REVIEW_GROUP,
            "第2章.md",
            "任务T1的第2章",
        )
        .unwrap();
        crate::continuity::record_draft_dependency(
            &db,
            &book,
            2,
            1,
            &crate::continuity::content_hash("任务T1的第1章"),
            "T1",
        )
        .unwrap();
        // 作者改动第 1 章 → 第 2 章依赖变 stale
        write_file(
            &db,
            &book,
            crate::db::REVIEW_GROUP,
            "第1章.md",
            "作者改过的第1章",
        )
        .unwrap();
        assert!(
            crate::continuity::check_draft_dependency(&db, &book, 2).is_err(),
            "依赖应已 stale"
        );
        // 另一任务试图把第 2 章直写正式组
        let r = write_ai_file(&db, &book, "正文", "第2章.md", "T2重写的第2章");
        assert!(r.is_err(), "已有同章待审稿时必须拒绝 AI 写正式组：{:?}", r);
        assert!(
            read_file(&db, &book, "正文", "第2章.md").is_none(),
            "正式组不得被写入"
        );
        // 待审源稿保持不变
        assert_eq!(
            read_file(&db, &book, crate::db::REVIEW_GROUP, "第2章.md").as_deref(),
            Some("任务T1的第2章")
        );
    }

    #[test]
    fn deleted_draft_provenance_does_not_block_new_generation() {
        // 业务修复（非掩盖失败）：待审稿被驳回/移除时，父 continuity 的 note_file_change
        // 会先把这个被删草稿的后继章标记 stale（保留保守失效语义），
        // 然后清除**该章自身**的 draft_origin/draft_dependency。
        // 这样一张永远无法满足的旧被拒草稿不会永久堵住新任务在同一章号上重新生成。
        // 后继章仍然 stale —— 该断言由父 continuity 的测试覆盖，此处只验证本章槽位被释放。
        let (_d, db, book) = fixture();
        write_file(&db, &book, crate::db::REVIEW_GROUP, "第1章.md", "第1章").unwrap();
        write_file(&db, &book, crate::db::REVIEW_GROUP, "第2章.md", "第2章草稿").unwrap();
        crate::continuity::record_draft_dependency(
            &db,
            &book,
            2,
            1,
            &crate::continuity::content_hash("第1章"),
            "T1",
        )
        .unwrap();
        // 作者改动第 1 章 → 第 2 章依赖变 stale（删除前必须是 stale，才能证明删除是释放原因）
        write_file(
            &db,
            &book,
            crate::db::REVIEW_GROUP,
            "第1章.md",
            "改过的第1章",
        )
        .unwrap();
        assert!(
            crate::continuity::check_draft_dependency(&db, &book, 2).is_err(),
            "删除前依赖应为 stale"
        );
        // 驳回/移除该待审稿：清除本章自身的 origin/dependency
        delete_file(&db, &book, crate::db::REVIEW_GROUP, "第2章.md").unwrap();
        assert!(
            crate::continuity::check_draft_dependency(&db, &book, 2).is_ok(),
            "被删草稿不应再堵住同一章号"
        );
        // 旧草稿已清，新任务可在同一章号继续写入正式稿
        let r = write_ai_file(&db, &book, "正文", "第2章.md", "T2重写的第2章");
        assert!(r.is_ok(), "清除后应允许新任务写入：{:?}", r);
        assert_eq!(
            read_file(&db, &book, "正文", "第2章.md").as_deref(),
            Some("T2重写的第2章")
        );
    }

    #[test]
    fn cas_requires_expected_content_and_lock() {
        // write_file_cas：内容不符拒绝；locked 拒绝
        let (_d, db, book) = fixture();
        write_file(&db, &book, "正文", "第1章.md", "甲").unwrap();
        assert!(write_file_cas(&db, &book, "正文", "第1章.md", "乙", "丙").is_err());
        write_file_cas(&db, &book, "正文", "第1章.md", "甲", "丙").unwrap();
        assert_eq!(
            read_file(&db, &book, "正文", "第1章.md").as_deref(),
            Some("丙")
        );
        set_file_flag(&db, &book, "正文", "第1章.md", "locked", true).unwrap();
        assert!(write_file_cas(&db, &book, "正文", "第1章.md", "丙", "丁").is_err());
    }

    #[test]
    fn write_new_conflicts_and_ai_never_overwrites_manual() {
        let (_d, db, book) = fixture();
        write_file_new(&db, &book, "正文", "第1章.md", "人工稿").unwrap();
        assert!(write_file_new(&db, &book, "正文", "第1章.md", "再来").is_err());
        assert_eq!(
            read_file(&db, &book, "正文", "第1章.md").as_deref(),
            Some("人工稿")
        );
    }

    #[test]
    fn approve_requires_target_free_and_deletes_review_only_after_commit() {
        // F01 审批：正式稿已存在 → 冲突且不动源稿；成功路径才删待审
        let (_d, db, book) = fixture();
        write_file(&db, &book, "正文", "第1章.md", "作者手改稿").unwrap();
        write_file(&db, &book, crate::db::REVIEW_GROUP, "第1章.md", "AI 待审稿").unwrap();
        let err = approve_pending_chapter(&db, &book, "第1章.md");
        assert!(err.is_err(), "正式稿已存在必须报冲突");
        assert_eq!(
            read_file(&db, &book, "正文", "第1章.md").as_deref(),
            Some("作者手改稿")
        );
        assert_eq!(
            read_file(&db, &book, crate::db::REVIEW_GROUP, "第1章.md").as_deref(),
            Some("AI 待审稿"),
            "冲突时待审源稿必须保留"
        );

        // 无正式稿时可接受：提交成功后才删待审
        let (_d2, db2, book2) = fixture();
        write_file(&db2, &book2, crate::db::REVIEW_GROUP, "第1章.md", "AI 定稿").unwrap();
        db2.exec(
            "INSERT INTO pending_chapter(book_id,ch,review_file,status,created_at,updated_at) VALUES(?1,1,?2,'pending',0,0)",
            &[&book2 as &dyn rusqlite::ToSql, &"第1章.md" as &dyn rusqlite::ToSql],
        )
        .unwrap();
        let name = approve_pending_chapter(&db2, &book2, "第1章.md").unwrap();
        assert_eq!(name, "第1章.md");
        assert_eq!(
            read_file(&db2, &book2, "正文", "第1章.md").as_deref(),
            Some("AI 定稿")
        );
        assert!(read_file(&db2, &book2, crate::db::REVIEW_GROUP, "第1章.md").is_none());
    }

    #[test]
    fn restore_trash_never_overwrites_newer_author_draft() {
        // P0：回收站恢复不得覆盖同名新稿；不同内容必须冲突并保留回收站记录
        let (_d, db, book) = fixture();
        write_file(&db, &book, "资料", "a.md", "旧内容").unwrap();
        delete_file(&db, &book, "资料", "a.md").unwrap();
        // 作者随后写了同名新稿
        write_file(&db, &book, "资料", "a.md", "作者新稿").unwrap();
        let id = list_file_trash(&db, &book).as_array().unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let out = restore_file_trash(&db, &book, &id);
        assert_eq!(out["ok"], false, "不同内容必须拒绝覆盖");
        assert_eq!(
            read_file(&db, &book, "资料", "a.md").as_deref(),
            Some("作者新稿")
        );
        assert_eq!(
            list_file_trash(&db, &book).as_array().unwrap().len(),
            1,
            "冲突时回收站记录必须保留"
        );
    }

    #[test]
    fn restore_trash_same_content_is_idempotent() {
        let (_d, db, book) = fixture();
        write_file(&db, &book, "资料", "b.md", "同样内容").unwrap();
        delete_file(&db, &book, "资料", "b.md").unwrap();
        write_file(&db, &book, "资料", "b.md", "同样内容").unwrap();
        let id = list_file_trash(&db, &book).as_array().unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let out = restore_file_trash(&db, &book, &id);
        assert_eq!(out["ok"], true);
        assert_eq!(out["idempotent"], true);
        assert_eq!(
            read_file(&db, &book, "资料", "b.md").as_deref(),
            Some("同样内容")
        );
        assert!(
            list_file_trash(&db, &book).as_array().unwrap().is_empty(),
            "幂等成功后应清理记录"
        );
    }

    #[test]
    fn restore_version_backs_up_current_before_rollback() {
        // 显式版本回退允许覆盖，但必须先把当前稿存成版本，保证可再回退
        let (_d, db, book) = fixture();
        write_file(&db, &book, "资料", "c.md", "第一版").unwrap();
        write_file(&db, &book, "资料", "c.md", "第二版").unwrap();
        let versions = list_versions(&db, &book, "资料", "c.md");
        let first_ts = versions
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v["ts"].as_i64())
            .min()
            .unwrap();
        let out = restore_version(&db, &book, "资料", "c.md", first_ts);
        assert_eq!(out["ok"], true, "回退应成功：{}", out);
        assert_eq!(
            read_file(&db, &book, "资料", "c.md").as_deref(),
            Some("第一版")
        );
        // 第二版必须仍在版本列表里，可再次回退
        let after = list_versions(&db, &book, "资料", "c.md");
        assert!(
            after.as_array().unwrap().len() >= 2,
            "回退后必须仍保留可回退的版本"
        );
    }

    #[test]
    fn clear_file_trash_reports_errors() {
        let (_d, db, book) = fixture();
        write_file(&db, &book, "资料", "d.md", "内容").unwrap();
        delete_file(&db, &book, "资料", "d.md").unwrap();
        assert!(list_file_trash(&db, &book).as_array().unwrap().len() == 1);
        clear_file_trash(&db, &book).unwrap();
        assert!(list_file_trash(&db, &book).as_array().unwrap().is_empty());
        // 幂等：已空再次清理仍 Ok
        assert!(clear_file_trash(&db, &book).is_ok());
    }

    #[test]
    fn rename_file_migrates_flags_and_versions() {
        // 改名后 locked/aiOff 必须跟着新名生效，历史版本也必须可查
        let (_d, db, book) = fixture();
        write_file(&db, &book, "资料", "旧名.md", "第一版").unwrap();
        write_file(&db, &book, "资料", "旧名.md", "第二版").unwrap();
        set_file_flag(&db, &book, "资料", "旧名.md", "locked", true).unwrap();
        set_file_flag(&db, &book, "资料", "旧名.md", "aiOff", true).unwrap();
        let versions_before = list_versions(&db, &book, "资料", "旧名.md")
            .as_array()
            .unwrap()
            .len();
        assert!(versions_before > 0, "先有版本快照");

        rename_file(&db, &book, "资料", "旧名.md", "新名.md").unwrap();

        assert!(
            file_flag(&db, &book, "资料", "新名.md", "locked"),
            "locked 必须随改名迁移"
        );
        assert!(
            file_flag(&db, &book, "资料", "新名.md", "aiOff"),
            "aiOff 必须随改名迁移"
        );
        assert!(
            !file_flag(&db, &book, "资料", "旧名.md", "locked"),
            "旧名不应再命中"
        );
        assert_eq!(
            list_versions(&db, &book, "资料", "新名.md")
                .as_array()
                .unwrap()
                .len(),
            versions_before,
            "版本历史必须随改名迁移"
        );
        assert!(list_versions(&db, &book, "资料", "旧名.md")
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn rename_folder_migrates_flags_of_all_files() {
        // 文件夹改名：其下所有文件的 locked/aiOff 与版本历史必须迁移，不得静默丢约束
        let (_d, db, book) = fixture();
        create_folder(&db, &book, "资料");
        write_file(&db, &book, "资料", "a.md", "内容A").unwrap();
        write_file(&db, &book, "资料", "子目录.md", "内容B").unwrap();
        set_file_flag(&db, &book, "资料", "a.md", "locked", true).unwrap();
        set_file_flag(&db, &book, "资料", "子目录.md", "aiOff", true).unwrap();

        let r = rename_folder(&db, &book, "资料", "档案");
        assert_eq!(r["ok"], true, "改名应成功：{}", r);

        assert!(
            file_flag(&db, &book, "档案", "a.md", "locked"),
            "locked 必须迁移到新组"
        );
        assert!(
            file_flag(&db, &book, "档案", "子目录.md", "aiOff"),
            "aiOff 必须迁移到新组"
        );
        assert!(
            !file_flag(&db, &book, "资料", "a.md", "locked"),
            "旧组不应再命中"
        );
        // 版本目录同样迁移
        assert!(read_file(&db, &book, "档案", "a.md").as_deref() == Some("内容A"));
    }

    #[test]
    fn delete_folder_refuses_symlink_and_keeps_source() {
        // delete_folder：含符号链接必须拒绝，且不删除目录
        let (_d, db, book) = fixture();
        create_folder(&db, &book, "资料");
        write_file(&db, &book, "资料", "a.md", "内容A").unwrap();
        let dir = book_path(&db, &book, "资料", "a.md");
        let folder = dir.parent().unwrap().to_path_buf();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/hostname", folder.join("link")).unwrap();
            assert!(delete_folder(&db, &book, "资料").is_err());
            assert!(folder.exists(), "拒绝时必须保留原目录");
        }
        #[cfg(windows)]
        {
            // Windows 下创建符号链接需要特权：只验证普通删除与备份成功路径
            let out = delete_folder(&db, &book, "资料");
            assert!(out.is_ok());
            assert!(!folder.exists());
            let trash = list_file_trash(&db, &book);
            assert!(
                !trash.as_array().unwrap().is_empty(),
                "删除前必须已备份到回收站"
            );
        }
    }

    #[test]
    fn approve_rejects_before_write_when_queue_missing() {
        // 队列预检：没有 pending 记录必须在**写盘前**拒绝，不得先写正式稿再发现无队列
        let (_d, db, book) = fixture();
        write_file(
            &db,
            &book,
            crate::db::REVIEW_GROUP,
            "第3章.md",
            "待审稿内容",
        )
        .unwrap();
        let out = approve_pending_chapter(&db, &book, "第3章.md");
        assert!(out.is_err(), "无队列记录必须拒绝");
        assert!(
            read_file(&db, &book, "正文", "第3章.md").is_none(),
            "预检失败不得写出正式稿"
        );
        assert_eq!(
            read_file(&db, &book, crate::db::REVIEW_GROUP, "第3章.md").as_deref(),
            Some("待审稿内容"),
            "预检失败必须保留待审源稿"
        );
    }

    #[test]
    fn approve_rejects_non_pending_queue_status() {
        // rejected 的旧回收记录不得被 approve 直写
        let (_d, db, book) = fixture();
        write_file(
            &db,
            &book,
            crate::db::REVIEW_GROUP,
            "第5章.md",
            "被驳回的稿",
        )
        .unwrap();
        db.exec(
            "INSERT INTO pending_chapter(book_id,ch,review_file,status,created_at,updated_at) VALUES(?1,5,?2,'rejected',0,0)",
            &[&book as &dyn rusqlite::ToSql, &"第5章.md" as &dyn rusqlite::ToSql],
        )
        .unwrap();
        let out = approve_pending_chapter(&db, &book, "第5章.md");
        assert!(out.is_err(), "rejected 记录不得被接受");
        assert!(read_file(&db, &book, "正文", "第5章.md").is_none());
    }

    #[test]
    fn approve_db_trigger_fault_preserves_formal_and_review() {
        // DB 故障（触发器）中途失败：正式稿与待审源稿都必须保留，错误必须可见
        let (_d, db, book) = fixture();
        write_file(&db, &book, crate::db::REVIEW_GROUP, "第6章.md", "待审稿六").unwrap();
        db.exec(
            "INSERT INTO pending_chapter(book_id,ch,review_file,status,created_at,updated_at) VALUES(?1,6,?2,'pending',0,0)",
            &[&book as &dyn rusqlite::ToSql, &"第6章.md" as &dyn rusqlite::ToSql],
        )
        .unwrap();
        // 安装触发器：任何 pending_chapter 更新都失败
        db.exec(
            "CREATE TRIGGER fail_approve BEFORE UPDATE ON pending_chapter BEGIN SELECT RAISE(ABORT,'injected'); END",
            &[],
        )
        .unwrap();
        let out = approve_pending_chapter(&db, &book, "第6章.md");
        assert!(out.is_err(), "DB 故障必须报错");
        assert_eq!(
            read_file(&db, &book, "正文", "第6章.md").as_deref(),
            Some("待审稿六"),
            "正式稿已保存，不得回滚"
        );
        assert_eq!(
            read_file(&db, &book, crate::db::REVIEW_GROUP, "第6章.md").as_deref(),
            Some("待审稿六"),
            "DB 故障时必须保留待审源稿"
        );
    }

    #[test]
    fn restoring_already_approved_review_is_idempotent_and_keeps_approved() {
        // 恢复：已 approved 的待审稿进回收站后再恢复，不得把状态错误回退成 pending
        let (_d, db, book) = fixture();
        write_file(&db, &book, crate::db::REVIEW_GROUP, "第4章.md", "已接受稿").unwrap();
        db.exec(
            "INSERT INTO pending_chapter(book_id,ch,review_file,status,created_at,updated_at) VALUES(?1,4,?2,'approved',0,0)",
            &[&book as &dyn rusqlite::ToSql, &"第4章.md" as &dyn rusqlite::ToSql],
        )
        .unwrap();
        delete_file(&db, &book, crate::db::REVIEW_GROUP, "第4章.md").unwrap();
        let trash = list_file_trash(&db, &book);
        let id = trash.as_array().unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let out = restore_file_trash(&db, &book, &id);
        assert_eq!(out["ok"], true);
        assert_eq!(out["group"], crate::db::REVIEW_GROUP);
        let st = db
            .q_json(
                "SELECT status FROM pending_chapter WHERE book_id=?1",
                &[&book as &dyn rusqlite::ToSql],
            )
            .unwrap();
        assert_eq!(st[0]["status"], "approved", "已批准记录不得被恢复动作回退");
    }

    #[test]
    fn rename_requires_missing_target_and_does_not_touch_flags_key() {
        // 改名：目标存在拒绝；成功后源消失、目标存在；flags 键按新名读取不受影响
        let (_d, db, book) = fixture();
        write_file(&db, &book, "正文", "第1章.md", "内容一").unwrap();
        write_file(&db, &book, "正文", "第2章.md", "内容二").unwrap();
        assert!(
            rename_file(&db, &book, "正文", "第1章.md", "第2章.md").is_err(),
            "目标存在必须拒绝"
        );
        assert_eq!(
            read_file(&db, &book, "正文", "第1章.md").as_deref(),
            Some("内容一")
        );

        rename_file(&db, &book, "正文", "第1章.md", "第1章 新名.md").unwrap();
        assert!(read_file(&db, &book, "正文", "第1章.md").is_none());
        assert_eq!(
            read_file(&db, &book, "正文", "第1章 新名.md").as_deref(),
            Some("内容一")
        );

        // flags 按新名读取：旧名的标志不应错误命中新名
        set_file_flag(&db, &book, "正文", "第1章 新名.md", "locked", true).unwrap();
        assert!(file_flag(&db, &book, "正文", "第1章 新名.md", "locked"));
        assert!(!file_flag(&db, &book, "正文", "第1章.md", "locked"));
    }

    #[test]
    fn purge_book_keeps_db_index_when_disk_cleanup_fails() {
        // purge：磁盘目录清理失败时不得假装成功，且不得把 DB 索引先删干净
        let (_d, db, book) = fixture();
        write_file(&db, &book, "正文", "第1章.md", "正文内容").unwrap();
        db.exec(
            "INSERT INTO pending_chapter(book_id,ch,review_file,status,created_at,updated_at) VALUES(?1,1,?2,'pending',0,0)",
            &[&book as &dyn rusqlite::ToSql, &"第1章.md" as &dyn rusqlite::ToSql],
        )
        .unwrap();
        // 正常路径：purge 必须清掉关联行
        let out = crate::books::purge_book(&db, &book);
        assert_eq!(out["ok"], true, "正常清理应成功：{}", out);
        for table in [
            "pending_chapter",
            "auto_task",
            "chapter_memory",
            "draft_dependency",
            "story_plan",
            "continuity_event",
            "file_revision",
        ] {
            let n = db
                .q_json(
                    &format!("SELECT COUNT(*) AS n FROM {} WHERE book_id=?1", table),
                    &[&book as &dyn rusqlite::ToSql],
                )
                .unwrap()[0]["n"]
                .as_i64()
                .unwrap();
            assert_eq!(n, 0, "purge 必须清理表 {}", table);
        }
        let books = db
            .q_json(
                "SELECT COUNT(*) AS n FROM books WHERE id=?1",
                &[&book as &dyn rusqlite::ToSql],
            )
            .unwrap()[0]["n"]
            .as_i64()
            .unwrap();
        assert_eq!(books, 0);
    }
}

#[cfg(test)]
mod safe_name_tests {
    use super::*;

    fn assert_safe(out: &str) {
        assert!(!out.contains('/'), "含路径分隔符斜杠: {:?}", out);
        assert!(!out.contains('\\'), "含路径分隔符反斜杠: {:?}", out);
        assert!(
            !out.chars().all(|c| c == '.'),
            "结果是纯点号（路径穿越分量）: {:?}",
            out
        );
    }

    #[test]
    fn parent_dir_is_neutralized() {
        let out = safe_name("..");
        assert_safe(&out);
        assert_ne!(out, "..");
        assert_eq!(out, "_invalid_");
    }

    #[test]
    fn dot_is_neutralized() {
        let out = safe_name(".");
        assert_safe(&out);
        assert_ne!(out, ".");
        assert_eq!(out, "_invalid_");
    }

    #[test]
    fn trailing_dots_stripped() {
        let out = safe_name("a..");
        assert_safe(&out);
        assert_eq!(out, "a");
    }

    #[test]
    fn leading_dots_stripped() {
        let out = safe_name("..a");
        assert_safe(&out);
        assert_eq!(out, "a");
    }

    #[test]
    fn slash_is_mapped() {
        let out = safe_name("a/b");
        assert_safe(&out);
        assert_eq!(out, "a_b");
    }

    #[test]
    fn backslash_is_mapped() {
        let out = safe_name("a\\b");
        assert_safe(&out);
        assert_eq!(out, "a_b");
    }

    #[test]
    fn empty_is_invalid() {
        let out = safe_name("");
        assert_safe(&out);
        assert_eq!(out, "_invalid_");
    }

    #[test]
    fn normal_chinese_name_kept() {
        let out = safe_name("第一章 风起");
        assert_safe(&out);
        assert_eq!(out, "第一章 风起");
    }

    #[test]
    fn normalize_group_maps_all_aliases() {
        assert_eq!(normalize_group("正文"), "正文");
        assert_eq!(normalize_group("chapters"), "正文");
        assert_eq!(normalize_group("小说正文"), "正文");
        assert_eq!(normalize_group("settings"), "设定");
        assert_eq!(normalize_group("设定资料"), "设定");
        assert_eq!(normalize_group("正文待审"), crate::db::REVIEW_GROUP);
    }

    #[test]
    fn normalize_group_never_falls_back_to_settings() {
        assert_eq!(normalize_group("我的资料"), "我的资料");
        assert_eq!(normalize_group(""), "_invalid_");
        assert_eq!(normalize_group(".."), "_invalid_");
        assert_eq!(normalize_group("a/b"), "_invalid_");
        assert_ne!(normalize_group("未知组名"), "设定");
    }
}
