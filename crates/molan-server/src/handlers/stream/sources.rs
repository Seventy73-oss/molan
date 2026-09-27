// 书源下载：book_dl_* 与 DL_STATUS/DL_STOP，整本抓取存 data/下载的小说/。
//
// F24 修复要点：
// - 缺 url / 空正文 / 抓取失败 = 该章失败，计入 failed，绝不当完成；
// - 停止（DL_STOP）不是完成：终态 stopped，不写完成日志；
// - 终态区分 done / partial / failed，并给出 done/failed/total 计数；
// - 同名文件绝不 truncate 覆盖：用 create_new 择新名（书名_2.txt…）。
use super::super::AppState;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static DL_STATUS: Mutex<Option<Value>> = Mutex::new(None);
static DL_STOP: AtomicBool = AtomicBool::new(false);

fn dl_update(f: impl FnOnce(&mut Value)) {
    if let Ok(mut g) = DL_STATUS.lock() {
        if let Some(v) = g.as_mut() {
            f(v);
        }
    }
}

/// 安全文件名：过滤 Windows 非法字符与控制字符、去尾空白与点、避开保留设备名、空名兜底。
fn safe_file_stem(name: &str) -> String {
    let mut s = String::new();
    for c in name.chars() {
        // 用 matches! 逐字符判定，避免多层字符串转义出错
        if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            s.push('_');
        } else {
            s.push(c);
        }
    }
    let s: String = s
        .trim()
        .trim_end_matches(['.', ' '])
        .chars()
        .take(60)
        .collect();
    let s = s.trim().to_string();
    if s.is_empty() {
        return "未命名书籍".to_string();
    }
    let reserved = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem_upper = s
        .split('.')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    if reserved.contains(&stem_upper.as_str()) {
        format!("_{}", s)
    } else {
        s
    }
}

/// 以「绝不覆盖」的方式创建下载文件。
/// 冻结契约：同名文件必须**报错拒绝**，不做自动加后缀，避免静默产生第二份同名稿。
fn create_download_file(dir: &Path, stem: &str) -> Result<(PathBuf, std::fs::File)> {
    use std::io::Write as _;
    let cand = dir.join(format!("{}.txt", stem));
    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&cand)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow!("同名下载文件已存在，拒绝覆盖：{}", cand.display())
            } else {
                anyhow!("创建下载文件失败 {}: {}", cand.display(), e)
            }
        })?;
    // 一旦创建成功就立即落一个非空头：空文件不会被误认成有效下载
    f.write_all(format!("书名：{}\n", stem).as_bytes())?;
    Ok((cand, f))
}

/// 终态判定（纯函数）：空章节集绝不能算完成。
fn download_status(stopped: bool, done: usize, failed: usize, total: usize) -> &'static str {
    if total == 0 {
        return "failed";
    }
    if stopped {
        "stopped"
    } else if done == 0 && failed > 0 {
        "failed"
    } else if failed > 0 {
        "partial"
    } else {
        "done"
    }
}

/// 停止轮询：与在飞的抓取赛跑，stop 一旦置位立刻让抓取 future 被 drop（真停止）。
async fn wait_dl_stop() {
    while !DL_STOP.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn book_dl_loop(
    st: Arc<AppState>,
    source_id: String,
    book_name: String,
    chapters: Value,
) -> anyhow::Result<()> {
    let data_dir = st.root.join("data");
    let dir = data_dir.join("下载的小说");
    std::fs::create_dir_all(&dir)?;
    let stem = safe_file_stem(&book_name);
    let (path, mut out) = create_download_file(&dir, &stem)?;
    let fname = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download.txt")
        .to_string();
    dl_update(|v| {
        v["file"] = json!(fname);
    });
    use std::io::Write;
    out.write_all(format!("来源：书源 {}\n\n", source_id).as_bytes())?;

    let arr = chapters.as_array().cloned().unwrap_or_default();
    let total = arr.len();
    // 防御：即使被直接调用，空章节集也绝不进入完成路径。
    if total == 0 {
        anyhow::bail!("章节为空，拒绝下载");
    }
    let mut done = 0usize;
    let mut failed: Vec<Value> = Vec::new();
    let mut stopped = false;
    for (i, c) in arr.iter().enumerate() {
        if DL_STOP.load(Ordering::SeqCst) {
            stopped = true;
            break;
        }
        let name = c["name"].as_str().unwrap_or("未命名章节").to_string();
        let url = c["url"].as_str().unwrap_or("").trim().to_string();
        // 缺 url 不得计入完成
        if url.is_empty() {
            failed.push(json!({"index": i + 1, "name": name, "error": "缺少章节地址（url 为空）"}));
        } else {
            // 抓取与「停止」赛跑：stop 置位即 drop 在飞请求，不再等它返回。
            let fetched = tokio::select! {
                biased;
                _ = wait_dl_stop() => None,
                r = molan_sources::fetch_chapter_text(&data_dir, &source_id, &url) => Some(r),
            };
            let Some(res) = fetched else {
                stopped = true;
                break;
            };
            match res {
                Ok(text) if !text.trim().is_empty() => {
                    // 抓取返回后、写盘前再查一次停止：停止优先于未提交成果。
                    if DL_STOP.load(Ordering::SeqCst) {
                        stopped = true;
                        break;
                    }
                    out.write_all(format!("\n\n{}\n\n{}", name, text).as_bytes())?;
                    done += 1;
                }
                Ok(_) => {
                    failed.push(json!({"index": i + 1, "name": name, "error": "抓取到的正文为空"}))
                }
                Err(e) => {
                    failed.push(json!({"index": i + 1, "name": name, "error": format!("{}", e)}))
                }
            }
        }
        dl_update(|v| {
            v["cur"] = json!(i + 1);
            v["done"] = json!(done);
            v["failed"] = json!(failed);
        });
    }
    // 自然跑完也要再确认一次停止：请求过停止就不能报完成。
    if DL_STOP.load(Ordering::SeqCst) {
        stopped = true;
    }
    out.flush()?;
    out.sync_all()?;
    drop(out);

    let status = download_status(stopped, done, failed.len(), total);
    dl_update(|v| {
        v["done"] = json!(done);
        v["failed"] = json!(failed);
        v["stopped"] = json!(stopped);
        v["status"] = json!(status);
        v["complete"] = json!(status == "done");
        v["partial"] = json!(status == "partial");
    });
    if status == "done" {
        tracing::info!("书源下载完成：{}（{}/{} 章）", fname, done, total);
    } else {
        tracing::warn!(
            "书源下载未完成（{}）：{} 成功 {}/{} 章，失败 {} 章{}",
            status,
            fname,
            done,
            total,
            failed.len(),
            if stopped { "，用户已停止" } else { "" }
        );
    }
    Ok(())
}

/// book_dl_start：起一个后台下载任务（原 dispatch 分支体原样搬入）。
pub(crate) async fn book_dl_start(
    st: &Arc<AppState>,
    _cmd: &str,
    args: &Value,
    _tx: &tokio::sync::mpsc::Sender<String>,
) -> Result<Option<Value>> {
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();
    let source_id = s("sourceId");
    let book_name = s("bookName");
    let chapters = a("chapters").clone();
    if chapters.as_array().map(|a| a.is_empty()).unwrap_or(true) {
        return Err(anyhow!("章节为空"));
    }
    {
        let mut g = DL_STATUS.lock().unwrap();
        if g.as_ref()
            .map(|v| v["running"].as_bool().unwrap_or(false))
            .unwrap_or(false)
        {
            return Err(anyhow!("已有下载任务在运行"));
        }
        *g = Some(json!({
            "running": true, "bookName": book_name,
            "total": chapters.as_array().map(|x| x.len()).unwrap_or(0),
            "cur": 0, "done": 0, "failed": [], "file": "", "error": "",
            "stopping": false, "stopped": false, "partial": false, "complete": false,
        }));
    }
    DL_STOP.store(false, Ordering::SeqCst);
    let st2 = Arc::clone(st);
    tokio::spawn(async move {
        let res = book_dl_loop(st2, source_id, book_name, chapters).await;
        let mut g = DL_STATUS.lock().unwrap();
        if let Some(v) = g.as_mut() {
            v["running"] = json!(false);
            if let Err(e) = res {
                v["error"] = json!(format!("{}", e));
                v["status"] = json!("failed");
                v["complete"] = json!(false);
            }
        }
    });
    Ok(Some(json!({"ok": true})))
}

/// book_dl_status：查询下载进度（原 dispatch 分支体原样搬入）。
pub(crate) async fn book_dl_status(
    _st: &Arc<AppState>,
    _cmd: &str,
    _args: &Value,
    _tx: &tokio::sync::mpsc::Sender<String>,
) -> Result<Option<Value>> {
    let g = DL_STATUS.lock().unwrap();
    Ok(Some(g.clone().unwrap_or(json!({"running": false}))))
}

/// book_dl_stop：请求停止下载（原 dispatch 分支体原样搬入）。
/// 停止只是请求；终态由下载循环给出（stopped），绝不等于完成。
pub(crate) async fn book_dl_stop(
    _st: &Arc<AppState>,
    _cmd: &str,
    _args: &Value,
    _tx: &tokio::sync::mpsc::Sender<String>,
) -> Result<Option<Value>> {
    DL_STOP.store(true, Ordering::SeqCst);
    dl_update(|v| {
        v["stopping"] = json!(true);
    });
    Ok(Some(json!({"ok": true, "stopping": true})))
}

/// book_dl_get：读回已下载文件（原 dispatch 分支体原样搬入）。
pub(crate) async fn book_dl_get(
    st: &Arc<AppState>,
    _cmd: &str,
    args: &Value,
    _tx: &tokio::sync::mpsc::Sender<String>,
) -> Result<Option<Value>> {
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();
    // 绝对路径会顶掉 join 的 base（Rust Path::join 语义），必须先剥成纯文件名再拼接
    let raw = s("file");
    let fname = std::path::Path::new(&raw)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty() && !n.starts_with('.'))
        .ok_or_else(|| anyhow!("非法文件名"))?
        .to_string();
    let dir = st.root.join("data").join("下载的小说");
    let path = dir.join(&fname);
    let path = path
        .canonicalize()
        .map_err(|e| anyhow!("读取失败: {}", e))?;
    let dir_canon = dir.canonicalize().map_err(|e| anyhow!("读取失败: {}", e))?;
    if !path.starts_with(&dir_canon) {
        return Err(anyhow!("路径越界"));
    }
    let content = std::fs::read(&path).map_err(|e| anyhow!("读取失败: {}", e))?;
    Ok(Some(
        json!({"name": fname, "content": String::from_utf8_lossy(&content).to_string()}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_file_stem_blocks_traversal_and_reserved_names() {
        assert_eq!(safe_file_stem("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(safe_file_stem("a\u{0}b"), "a_b");
        assert_eq!(safe_file_stem("   "), "未命名书籍");
        assert_eq!(safe_file_stem("CON"), "_CON");
        assert_eq!(safe_file_stem("con.txt"), "_con.txt");
        assert_eq!(safe_file_stem("名字...  "), "名字");
        assert_eq!(safe_file_stem("a:b*c?d"), "a_b_c_d");
    }

    // 冻结契约：同名下载必须报错拒绝，不覆盖、也不静默加后缀。
    #[test]
    fn create_download_file_rejects_duplicate_instead_of_overwriting() {
        let dir = std::env::temp_dir().join(format!("molan-dl-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (p1, f1) = create_download_file(&dir, "书名").unwrap();
        drop(f1);
        std::fs::write(&p1, "旧稿").unwrap();
        let err = create_download_file(&dir, "书名").unwrap_err();
        assert!(format!("{}", err).contains("拒绝覆盖"));
        // 旧文件内容必须原样保留
        assert_eq!(std::fs::read_to_string(&p1).unwrap(), "旧稿");
        assert!(p1.ends_with("书名.txt"));
        assert!(!dir.join("书名_2.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 空章节集绝不能算完成；停止优先于完成。
    #[test]
    fn empty_chapter_set_is_never_done() {
        assert_eq!(download_status(false, 0, 0, 0), "failed");
        assert_eq!(download_status(false, 0, 1, 1), "failed");
        assert_eq!(download_status(false, 1, 1, 2), "partial");
        assert_eq!(download_status(false, 2, 0, 2), "done");
        assert_eq!(download_status(true, 2, 0, 2), "stopped");
        assert_eq!(download_status(true, 0, 0, 3), "stopped");
    }

    // 停止后不得把已完成章算成「完成」：即使全成功，stop 也是 stopped。
    #[test]
    fn stop_beats_done() {
        assert_ne!(download_status(true, 3, 0, 3), "done");
    }
}
