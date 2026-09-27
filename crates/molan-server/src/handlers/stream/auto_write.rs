// 自动写作引擎：任务运行态/实时进度日志/逐章生成（细纲→正文→审核→去味→落盘→章后处理）/空洞回补。
use super::super::AppState;
use super::build_system;
use super::{
    auto_book_context_for_chapter, effective_skills, extract_json_block, log_llm_usage, prompts,
    write_ai_file_async,
};
use crate::handlers::chapter_num_from_name;
use anyhow::anyhow;
use molan_core::files;
use molan_core::stats;
use molan_llm::ChatParams;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static AUTO_RUN: Mutex<Option<Value>> = Mutex::new(None);
static AUTO_STOP: AtomicBool = AtomicBool::new(false);
/// 自动写作在飞调用的取消令牌：auto_write_stop 时 cancel() → 当前那次 LLM HTTP 调用被掐断。
/// 任务自然结束时置 None（由 auto_write_loop 收尾负责）。
static AUTO_CANCEL: Mutex<Option<tokio_util::sync::CancellationToken>> = Mutex::new(None);

/// 读取当前任务的取消令牌（每个 ChatParams 都填 clone，保证任何一处 LLM 调用都能被掐断）
pub(crate) fn auto_cancel_token() -> Option<tokio_util::sync::CancellationToken> {
    AUTO_CANCEL.lock().ok().and_then(|g| g.clone())
}

/// 开一轮新任务：换新令牌（旧令牌若还挂着会被 drop，不影响新任务）
fn auto_cancel_arm() -> tokio_util::sync::CancellationToken {
    let token = tokio_util::sync::CancellationToken::new();
    if let Ok(mut g) = AUTO_CANCEL.lock() {
        *g = Some(token.clone());
    }
    token
}

/// 任务自然结束（含成功/失败/停止）后清空，防止下一轮误用已 cancel 的旧令牌
fn auto_cancel_clear() {
    if let Ok(mut g) = AUTO_CANCEL.lock() {
        *g = None;
    }
}

/// 提交/停止串行锁：取消屏障与落盘在同一把锁内完成，避免“停止已返回、正文仍写入”。
static AUTO_COMMIT: Mutex<()> = Mutex::new(());
/// 当前任务标识（task_id#bookId）：停止与状态按任务作用域判定，避免全局单槽误伤别的书
static AUTO_TASK_SCOPE: Mutex<String> = Mutex::new(String::new());

fn set_task_scope(task_id: i64, book_id: &str) {
    if let Ok(mut g) = AUTO_TASK_SCOPE.lock() {
        *g = format!("{}#{}", task_id, book_id);
    }
}
fn clear_task_scope() {
    if let Ok(mut g) = AUTO_TASK_SCOPE.lock() {
        g.clear();
    }
}
#[allow(dead_code)]
fn task_scope() -> String {
    AUTO_TASK_SCOPE
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default()
}

/// 当前任务是否已被取消（stop 标志或取消令牌任一命中）
fn is_cancelled_now() -> bool {
    AUTO_STOP.load(Ordering::SeqCst)
        || auto_cancel_token()
            .map(|t| t.is_cancelled())
            .unwrap_or(false)
}

/// 任务内内容指纹（SHA-256，与父 continuity::content_hash 同口径）：审核对象=最终文本绑定用
pub(crate) fn text_fingerprint(s: &str) -> String {
    molan_core::continuity::content_hash(s)
}

/// 提交结果：取消（未写）与写入成功/IO 失败严格区分，绝不把 IO 失败伪装成取消。
enum CommitOutcome {
    /// 已请求停止，一个字都没写
    Cancelled,
    /// 落盘成功
    Written,
    /// 落盘失败（IO/权限等），调用方必须按失败处理
    Failed(String),
    /// 生成期间依赖（设定/前文/技能）被修改：不得定稿，转待审
    DependencyChanged,
}

/// 提交前取消屏障 + 依赖 CAS + 串行落盘：与 auto_write_stop 在同一把锁下串行。
/// - 已请求停止（标志/令牌）→ Cancelled，一个字都不写；
/// - expected_inputs 为生成前冻结的依赖指纹，提交时重算并比较，不等 → DependencyChanged（转待审，不得定稿）；
/// - 落盘走 A 的 write_ai_file（锁定/已存在拒绝），写后回读确认。
async fn commit_chapter_file(
    st: &Arc<AppState>,
    book_id: &str,
    group: &str,
    name: &str,
    body: &str,
    expected_inputs: Option<(i64, String)>,
) -> CommitOutcome {
    let st2 = Arc::clone(st);
    let (b, g, n, c) = (
        book_id.to_string(),
        group.to_string(),
        name.to_string(),
        body.to_string(),
    );
    // 取消与依赖校验都在 A 的 fs_lock 内执行（write_ai_file_checked 的 check 回调），
    // 与作者/其他写入者处于同一临界区，消除 check/write 竞态。
    let cancel_seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dep_seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (cancel_flag, dep_flag) = (cancel_seen.clone(), dep_seen.clone());
    match tokio::task::spawn_blocking(move || {
        let _guard = match AUTO_COMMIT.lock() {
            Ok(x) => x,
            Err(poisoned) => poisoned.into_inner(),
        };
        if is_cancelled_now() {
            return CommitOutcome::Cancelled;
        }
        let expected = expected_inputs.clone();
        let cancel_flag2 = cancel_flag.clone();
        let dep_flag2 = dep_flag.clone();
        // 闭包需 move 捕获，故克隆句柄供锁内校验使用（外部仍保留 st2/b 做回读校验）
        let st3 = Arc::clone(&st2);
        let b3 = b.clone();
        let res = files::write_ai_file_checked(&st2.db, &b, &g, &n, &c, move || {
            // 锁内：再次确认未取消
            if is_cancelled_now() {
                cancel_flag2.store(true, Ordering::SeqCst);
                return Err(anyhow!("已请求停止"));
            }
            // 锁内：依赖指纹必须与生成前冻结值一致
            if let Some((target, expect)) = expected.as_ref() {
                let now = molan_core::continuity::input_fingerprint(&st3.db, &b3, *target)?;
                if now != *expect {
                    dep_flag2.store(true, Ordering::SeqCst);
                    return Err(anyhow!("依赖已变化"));
                }
            }
            Ok(())
        });
        if let Err(e) = res {
            if cancel_flag.load(Ordering::SeqCst) {
                return CommitOutcome::Cancelled;
            }
            if dep_flag.load(Ordering::SeqCst) {
                return CommitOutcome::DependencyChanged;
            }
            return CommitOutcome::Failed(format!("AI 写入被拒绝：{} / {} → {}", g, n, e));
        }
        let after = files::read_file(&st2.db, &b, &g, &n).unwrap_or_default();
        if after.trim().is_empty() || after != c {
            return CommitOutcome::Failed(format!("落盘校验失败：{} / {}", g, n));
        }
        CommitOutcome::Written
    })
    .await
    {
        Ok(x) => x,
        Err(e) => CommitOutcome::Failed(format!("写入任务失败：{}", e)),
    }
}

/// 生成前冻结依赖指纹（target = 本章 + 1，即包含本章自身及其全部前序输入）。
fn freeze_inputs(db: &molan_core::db::Db, book_id: &str, ch: i64) -> anyhow::Result<(i64, String)> {
    let target = ch + 1;
    let fp = molan_core::continuity::input_fingerprint(db, book_id, target)?;
    Ok((target, fp))
}

#[allow(dead_code)]
/// 仅当文件当前内容与调用方读到的 expected 一致时才写入（本地 CAS-lite）：
/// 人工/AI 已改动则放弃写入，绝不覆盖别人刚写的内容。
async fn write_if_unchanged(
    st: &Arc<AppState>,
    book_id: &str,
    group: &str,
    name: &str,
    expected: &str,
    content: &str,
) -> CommitOutcome {
    let now = files::read_file(&st.db, book_id, group, name).unwrap_or_default();
    if now != expected {
        return CommitOutcome::Failed(format!("文件已被改动，放弃覆盖：{} / {}", group, name));
    }
    commit_chapter_file(st, book_id, group, name, content, None).await
}

/// 自动写作实时进度日志（前端在对话流里轮询渲染进度卡片）
static AUTO_LOGS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
/// 日志属于哪本书：避免 A 书的进度卡片显示在 B 书的对话里
static AUTO_LOG_BOOK: Mutex<String> = Mutex::new(String::new());

pub(crate) fn auto_log_clear(book_id: &str) {
    if let Ok(mut g) = AUTO_LOGS.lock() {
        g.clear();
    }
    if let Ok(mut b) = AUTO_LOG_BOOK.lock() {
        *b = book_id.to_string();
    }
}
pub(crate) fn auto_log(ch: i64, step: &str, text: impl Into<String>) {
    if let Ok(mut g) = AUTO_LOGS.lock() {
        g.push(json!({"ts": stats::now_ms(), "ch": ch, "step": step, "text": text.into()}));
        let n = g.len();
        if n > 300 {
            g.drain(0..n - 300);
        }
    }
}
fn auto_logs() -> Vec<Value> {
    AUTO_LOGS.lock().map(|g| g.clone()).unwrap_or_default()
}
/// 只返回属于这本书的日志
pub(crate) fn auto_logs_for(book_id: &str) -> Vec<Value> {
    let owner = AUTO_LOG_BOOK.lock().map(|b| b.clone()).unwrap_or_default();
    if owner.is_empty() || owner != book_id {
        return Vec::new();
    }
    auto_logs()
}

// ---------- 自动任务持久化（auto_task 表） ----------
pub fn persist_auto_task(
    db: &molan_core::db::Db,
    task_id: i64,
    current: i64,
    status: &str,
    err: &str,
) {
    let _ = db.exec(
        "UPDATE auto_task SET current_ch=?2, status=?3, error=?4, updated_at=?5 WHERE id=?1",
        &[
            &task_id as &dyn rusqlite::ToSql,
            &current,
            &status,
            &err,
            &stats::now_ms(),
        ],
    );
}

/// 服务启动时把上次异常中断的 running 任务标记为 interrupted（已在 Db::open 迁移里统一处理）
#[allow(dead_code)]
pub fn recover_interrupted_auto_tasks(_db: &molan_core::db::Db) {}

/// 查某本书最近一条任务（供续跑与状态展示）
pub fn latest_auto_task(db: &molan_core::db::Db, book_id: &str) -> Option<Value> {
    ensure_task_schema(db);
    db.q_json(
        "SELECT id,book_id,session_id,from_ch,to_ch,current_ch,status,error,updated_at,full_auto FROM auto_task WHERE book_id=?1 ORDER BY id DESC LIMIT 1",
        &[&book_id as &dyn rusqlite::ToSql],
    )
    .ok()
    .and_then(|rows| rows.into_iter().next())
}

pub(crate) fn latest_chapter_num(db: &molan_core::db::Db, book_id: &str) -> i64 {
    let tree = files::scan_tree(db, book_id);
    let mut max = 0i64;
    if let Some(arr) = tree.as_array() {
        for g in arr {
            if g["dir"].as_str() != Some("正文") {
                continue;
            }
            if let Some(fs) = g["files"].as_array() {
                for f in fs {
                    if let Some(n) = chapter_num_from_name(f["name"].as_str().unwrap_or("")) {
                        max = max.max(n);
                    }
                }
            }
        }
    }
    max
}

pub(crate) fn save_auto_message(
    db: &molan_core::db::Db,
    session_id: &str,
    role: &str,
    content: &str,
) {
    if session_id.is_empty() || content.trim().is_empty() {
        return;
    }
    let ctx_json = "{}".to_string();
    let now = stats::now_ms();
    let _ = db.exec(
        "INSERT INTO messages(id,session_id,role,content,context_json,steps_json,result_json,created_at,interrupted) VALUES(?,?,?,?,?,NULL,NULL,?,0)",
        &[
            &(uuid::Uuid::new_v4().to_string()) as &dyn rusqlite::ToSql,
            &session_id, &role, &content, &ctx_json, &now,
        ],
    );
    let preview: String = content.chars().take(80).collect();
    let _ = db.exec(
        "UPDATE sessions SET msg_count=(SELECT COUNT(*) FROM messages WHERE session_id=?1), preview=?2, updated_at=?3 WHERE id=?1",
        &[&session_id as &dyn rusqlite::ToSql, &preview, &now],
    );
}

pub(crate) fn set_auto_progress(ch: i64) {
    if let Ok(mut g) = AUTO_RUN.lock() {
        if let Some(v) = g.as_mut() {
            v["current"] = json!(ch);
        }
    }
}

/// 任务表惰性补列：B 需要按任务持久化 full_auto 快照（resume 沿原任务模式）。
/// 只做向后兼容的 ALTER，不触碰 A 的 db.rs 所有权；已存在则静默跳过。
fn ensure_task_schema(db: &molan_core::db::Db) {
    let cols = db
        .q_json("PRAGMA table_info(auto_task)", &[])
        .unwrap_or_default();
    let has = cols.iter().any(|c| c["name"].as_str() == Some("full_auto"));
    if !has {
        let _ = db.exec(
            "ALTER TABLE auto_task ADD COLUMN full_auto INTEGER NOT NULL DEFAULT 0",
            &[],
        );
    }
}

/// 任务创建时冻结的全自动模式：任务运行期间不再受中途改设置影响（F05/R4）。
/// 无匹配运行任务时安全地按待审处理，绝不继承历史书级开关。
fn task_full_auto(_db: &molan_core::db::Db, book_id: &str) -> bool {
    if let Ok(g) = AUTO_RUN.lock() {
        if let Some(v) = g.as_ref() {
            if v["running"].as_bool().unwrap_or(false) && v["bookId"].as_str() == Some(book_id) {
                if let Some(b) = v["fullAuto"].as_bool() {
                    return b;
                }
            }
        }
    }
    false
}

/// 宽容提取模型返回里的第一个 JSON 对象
fn extract_json_loose(text: &str) -> Option<Value> {
    if let Some(v) = extract_json_block(text) {
        return Some(v);
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Value>(text[start..=end].trim()).ok()
}

/// 审核单次能覆盖的正文上限（字符）。超过即视为「未覆盖全文」，不得自动放行。
pub(crate) const REVIEW_BODY_BUDGET: usize = 9000;

/// 读取设定文件，但遵守 aiOff（作者禁止自动引用则返回空，绝不外发）。
fn read_asset_respecting_aioff(
    db: &molan_core::db::Db,
    book_id: &str,
    group: &str,
    name: &str,
) -> String {
    if files::file_flag(db, book_id, group, name, "aiOff") {
        return String::new();
    }
    files::read_file(db, book_id, group, name).unwrap_or_default()
}

/// 生成前冻结的「同任务上一章待审稿」：文本 + hash。
/// 细纲/正文/审核三个阶段共用同一份快照，登记依赖也用同一 hash，
/// 避免生成后重新 read 到已被改动的前稿。
#[derive(Clone)]
struct PendingSnapshot {
    text: String,
    hash: String,
}

impl PendingSnapshot {
    /// 供 prompt 使用的结尾片段（明确标注未审批草稿，不得当正式设定）
    fn block(&self) -> String {
        if self.text.trim().is_empty() {
            return String::new();
        }
        let t = self.text.chars().count();
        let tail: String = self.text.chars().skip(t.saturating_sub(700)).collect();
        format!(
            "【上一章待审稿结尾（未审批草稿，仅用于衔接核对，不得作为正式设定）】\n{}\n\n",
            tail
        )
    }
}

/// 生成前抓取同任务上一章待审稿（task/hash/aiOff 由父 continuity 精确校验）。
fn freeze_pending_snapshot(
    db: &molan_core::db::Db,
    book_id: &str,
    task_id: i64,
    prev_ch: i64,
) -> Option<PendingSnapshot> {
    let text = same_task_pending_text(db, book_id, task_id, prev_ch)?;
    if text.trim().is_empty() {
        return None;
    }
    let hash = text_fingerprint(&text);
    Some(PendingSnapshot { text, hash })
}

/// 组装审核用 user prompt（初次审核与最终稿复审共用同一套规则，保证"审核对象=最终文本"）。
/// 返回 (prompt, body_truncated)：body_truncated=true 表示正文超出单次预算、只覆盖了前段，
/// 调用方必须据此转人工待审，绝不允许「只审了前 9000 字」却自动定稿。
fn build_review_user(
    db: &molan_core::db::Db,
    book_id: &str,
    ch: i64,
    pending_block: &str,
    body: &str,
) -> (String, bool) {
    // 作者资产：统一遵守 aiOff
    let people = read_asset_respecting_aioff(db, book_id, "设定", "人物表.md");
    let ledger = read_asset_respecting_aioff(db, book_id, "设定", "伏笔台账.md");
    let prev = read_asset_respecting_aioff(db, book_id, "正文", &format!("第{}章.md", ch - 1));
    let prev_tail: String = {
        let t = prev.chars().count();
        prev.chars().skip(t.saturating_sub(700)).collect()
    };
    // 时间上下文完全用 C 的 temporal helper（只含 < ch 的已定稿记忆与已批准正文），
    // 不再直接读「前情摘要.md」——旧摘要可能覆盖到 ch 之后，属于未来信息，必须由 C 过滤。
    let temporal = auto_book_context_for_chapter(db, book_id, ch, false);
    // 待审连写：用调用方在生成前冻结的同任务上一章待审稿（不得在此处重新读取，
    // 否则登记依赖时读到的可能已是被改动过的稿）。
    let pending_note = pending_block.to_string();
    // 当前正史事实（穿帮硬对照，A3）：活跃事实+未裁决冲突的精简清单，
    // 审核据此抓伤势/位置/物品/伏笔状态/信息差穿帮，而不是只靠模型自觉。
    let facts_block = molan_core::facts::list_facts(db, book_id, "", "current", 200)
        .ok()
        .map(|v| {
            let mut lines: Vec<String> = Vec::new();
            for f in v["facts"].as_array().into_iter().flatten() {
                let st = f["subjectType"].as_str().unwrap_or("");
                let sid = f["subjectId"].as_str().unwrap_or("");
                let pred = f["predicate"].as_str().unwrap_or("");
                let val = f["value"].as_str().unwrap_or("");
                let tag = if f["state"].as_str() == Some("disputed") {
                    "·冲突未裁决"
                } else {
                    ""
                };
                let line = if st == "secret" {
                    let p: Value = serde_json::from_str(val).unwrap_or(Value::Null);
                    let join = |k: &str| {
                        p[k].as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str())
                                    .collect::<Vec<_>>()
                                    .join("、")
                            })
                            .unwrap_or_default()
                    };
                    format!(
                        "- [秘密{}] {}（知情：{}；不知情：{}）",
                        tag,
                        p["fact"].as_str().unwrap_or(val),
                        join("known_by"),
                        join("unknown_to")
                    )
                } else if st == "thread" {
                    format!("- [伏笔{}] {} = {}", tag, sid, val)
                } else {
                    format!("- [事实{}] {}·{} = {}", tag, sid, pred, val)
                };
                lines.push(line);
            }
            let mut s = lines.join("\n");
            if s.chars().count() > 1600 {
                s = s.chars().take(1600).collect();
                s.push_str("\n…（更多事实略，可用 list_story_facts 全查）");
            }
            s
        })
        .unwrap_or_default();
    let facts_block = if facts_block.is_empty() {
        "（暂无已入账事实）".to_string()
    } else {
        facts_block
    };
    let body_len = body.chars().count();
    let truncated = body_len > REVIEW_BODY_BUDGET;
    let prompt = format!(
        "【本章细纲】\n{}\n\n【人物表】\n{}\n\n【伏笔台账】\n{}\n\n【当前正史事实（穿帮硬对照）】\n{}\n\n【时间线上下文（截至第{}章）】\n{}\n\n【上一章结尾】\n{}\n\n{}【待审正文】\n{}\n\n\
审读要求：\n\
0) 先逐条对照【当前正史事实】：伤势/位置/持有物/伏笔状态/秘密知情范围，正文与之冲突即为硬伤，直接列出；\n\
1) 与细纲/人物表/上一章结尾比对，找硬伤：设定冲突、人物言行不一致、时间线错乱、前后矛盾、称呼错误；\n\
2) 判断是否是可读的小说正文（不是提纲、不是提问、不是解释说明）；\n\
3) 人称与视角是否统一；\n\
4) 跑偏检查：是否写了本章细纲以外的剧情、是否提前揭底未回收的伏笔、是否新增了档案/人物表里没有的重要人物或设定、是否重复解决上一章已解决的冲突；\n\
5) 时间线是否紧接着上一章结尾，有没有复述前文凑字数。\n\
只输出 JSON：{{\"ok\":true 或 false,\"issues\":[\"问题1\",\"问题2\"],\"fix\":\"给写手的具体修改指令，不超过150字\"}}\n\
没有硬伤时 ok=true、issues 为空数组。",
        outline_or_empty(db, book_id, ch),
        people.chars().take(900).collect::<String>(),
        ledger.chars().take(900).collect::<String>(),
        facts_block,
        ch - 1,
        temporal.chars().take(3000).collect::<String>(),
        prev_tail,
        pending_note,
        body.chars().take(REVIEW_BODY_BUDGET).collect::<String>(),
    );
    (prompt, truncated)
}

fn outline_or_empty(db: &molan_core::db::Db, book_id: &str, ch: i64) -> String {
    // 细纲同样是作者资产：标了 aiOff 就不得进入审核 prompt
    read_asset_respecting_aioff(db, book_id, "细纲", &format!("细纲_第{}章.md", ch))
        .chars()
        .take(1200)
        .collect()
}

/// 自动审核：与细纲/人物表/上一章结尾比对找硬伤；发现问题则按意见重写一轮。
/// 返回 (可选的重写稿, 人类可读的审核结论, 是否必须转人工待审)
/// 第三项为 true 表示审核链路本身不可信（JSON 两次解析失败 / 发现问题但重写失败保留原稿），
/// 此时即便全自动模式也必须走「正文待审」，不得直接写正式正文。
async fn review_and_fix_chapter(
    st: &Arc<AppState>,
    book_id: &str,
    ch: i64,
    body: &str,
    outline: &str,
    pending_block: &str,
) -> (Option<String>, String, bool) {
    let _ = outline; // 细纲现由 build_review_user 按章号统一读取，避免调用方与审核读取不一致
    let db = &st.db;
    let mut sys =
        "你是网文责编，审读一章正文。只输出一个 JSON 对象，不要解释、不要代码块标记。".to_string();
    if let Ok(project_agent) =
        molan_core::deepwrite::strict_agent_instructions(db, book_id, "continuity")
    {
        if !project_agent.is_empty() {
            sys.push_str("\n\n");
            sys.push_str(&project_agent);
        }
    }
    let (user, body_truncated) = build_review_user(db, book_id, ch, pending_block, body);
    if body_truncated {
        // 超单次预算：未覆盖全文，绝不自动放行（可人工审或后续分块实现）
        return (
            None,
            format!(
                "本章正文 {} 字超过单次审核预算 {} 字，未覆盖全文 → 转人工待审",
                body.chars().count(),
                REVIEW_BODY_BUDGET
            ),
            true,
        );
    }
    let Some(chn) = molan_llm::resolve_agent_channel(db, "review") else {
        // 无审核渠道 = 审核链路不可信，必须待审，绝不 Fail-Open
        return (None, "无可用审核渠道 → 转人工待审".to_string(), true);
    };
    let params = ChatParams {
        base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: chn["key"].as_str().unwrap_or("").to_string(),
        model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
        messages: vec![
            json!({"role": "system", "content": sys}),
            json!({"role": "user", "content": user}),
        ],
        temperature: 0.2,
        max_tokens: 1500,
        stream: false,
        no_thinking: true,
        reasoning_effort: String::new(),
        log_tag: "auto_review".to_string(),
        log_book: book_id.to_string(),
        cancel: auto_cancel_token(),
    };
    let raw = match molan_llm::chat_once_retry_logged_n(params.clone(), 2).await {
        Ok((x, usage)) => {
            log_llm_usage(
                db,
                book_id,
                "auto_review",
                chn["model"].as_str().unwrap_or(""),
                &usage,
            );
            x
        }
        // 审核调用失败（含 HTTP 400/网络错）：审核未完成，必须待审，绝不 Fail-Open
        Err(e) => return (None, format!("审核调用失败（{}）→ 转人工待审", e), true),
    };
    // 审核必须 Fail-Closed：输出无法解析成 JSON 时带反馈重问一次，仍失败则强制转人工
    let j = match extract_json_loose(&raw) {
        Some(j) => j,
        None => {
            let mut params2 = params.clone();
            params2.messages.push(
                json!({"role": "assistant", "content": raw.chars().take(500).collect::<String>()}),
            );
            params2.messages.push(json!({"role": "user", "content": "你上一条输出无法解析为 JSON，请只输出一个 JSON 对象"}));
            match molan_llm::chat_once_retry_logged_n(params2, 2).await {
                Ok((raw2, usage)) => {
                    log_llm_usage(
                        db,
                        book_id,
                        "auto_review",
                        chn["model"].as_str().unwrap_or(""),
                        &usage,
                    );
                    match extract_json_loose(&raw2) {
                        Some(j) => j,
                        None => {
                            return (
                                None,
                                "审核 JSON 两次解析失败 → 转人工待审".to_string(),
                                true,
                            )
                        }
                    }
                }
                Err(e) => {
                    return (
                        None,
                        format!("审核 JSON 解析失败且重问失败（{}）→ 转人工待审", e),
                        true,
                    );
                }
            }
        }
    };
    // Fail-Closed 严格解析：结构不完整（缺 ok / ok 非布尔 / issues 非数组）一律不可信
    let verdict = match parse_review_verdict(&j) {
        Some(v) => v,
        None => {
            return (
                None,
                "审核返回结构不完整（缺少布尔 ok 或 issues 数组）→ 转人工待审".to_string(),
                true,
            )
        }
    };
    let soft_note = if verdict.soft.is_empty() {
        String::new()
    } else {
        format!(
            "（另有{}条软建议：{}）",
            verdict.soft.len(),
            verdict
                .soft
                .iter()
                .take(2)
                .cloned()
                .collect::<Vec<_>>()
                .join("；")
        )
    };
    // 明确 ok=false 却没有任何 hard 问题：判定不可信，转人工
    if !verdict.ok && verdict.hard.is_empty() {
        return (
            None,
            "审核判不通过但未给出问题 → 转人工待审".to_string(),
            true,
        );
    }
    // schema 有效 + 无 hard 问题 → 通过（软建议只提示，不阻断）
    if verdict.ok && verdict.hard.is_empty() {
        return (None, format!("通过{}", soft_note), false);
    }
    // 有 hard 问题 → 走一轮受限重写，再对最终稿严格复审
    let issues = verdict.hard.clone();
    let reviewed_fp = text_fingerprint(body);
    let fix = j["fix"].as_str().unwrap_or("").trim().to_string();
    let list = issues
        .iter()
        .take(6)
        .enumerate()
        .map(|(i, s)| format!("{}. {}", i + 1, s))
        .collect::<Vec<_>>()
        .join("\n");
    // 一轮重写：保持情节事实，不新增设定
    let mut sys2 = "你是网文写手。按责编意见修订本章正文，保持情节、人物、时间线与原文一致，不新增设定。只输出完整正文。".to_string();
    if let Ok(project_agent) = molan_core::deepwrite::agent_instructions(db, book_id, "editor") {
        if !project_agent.is_empty() {
            sys2.push_str("\n\n");
            sys2.push_str(&project_agent);
        }
    }
    if let Ok(skills) = molan_core::deepwrite::skill_instructions(db, book_id, &sys2) {
        if !skills.is_empty() {
            sys2.push_str("\n\n");
            sys2.push_str(&skills);
        }
    }
    let user2 = format!(
        "【本章正文】\n{}\n\n【责编意见】\n{}\n{}\n\n请重写本章正文，第一行保持「第{}章 章名」，直接输出正文，不要任何说明。",
        body.chars().take(9000).collect::<String>(),
        list,
        if fix.is_empty() { String::new() } else { format!("总意见：{}", fix) },
        ch
    );
    let chn2 = match molan_llm::resolve_agent_channel(db, "chapter") {
        Some(c) => c,
        None => {
            return (
                None,
                format!("发现 {} 处问题，但无正文渠道可用，已保留原稿", issues.len()),
                true,
            )
        }
    };
    let max_tokens: i64 = molan_llm::get_setting(db, "max_tokens")
        .parse()
        .unwrap_or(8192);
    let params2 = ChatParams {
        base_url: chn2["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: chn2["key"].as_str().unwrap_or("").to_string(),
        model: chn2["model"]
            .as_str()
            .unwrap_or("deepseek-chat")
            .to_string(),
        messages: vec![
            json!({"role": "system", "content": sys2}),
            json!({"role": "user", "content": user2}),
        ],
        temperature: 0.7,
        max_tokens: if max_tokens == 0 { 8192 } else { max_tokens },
        stream: false,
        no_thinking: true,
        reasoning_effort: String::new(),
        log_tag: "auto_rewrite".to_string(),
        log_book: book_id.to_string(),
        cancel: auto_cancel_token(),
    };
    let t = match molan_llm::chat_once_retry_logged_n(params2, 2).await {
        Ok((fixed, usage)) => {
            log_llm_usage(
                db,
                book_id,
                "auto_rewrite",
                chn2["model"].as_str().unwrap_or(""),
                &usage,
            );
            let t = fixed.trim().to_string();
            if t.chars().count() < 300 {
                return (
                    None,
                    format!(
                        "发现 {} 处问题，重写稿过短（{}字）已保留原稿",
                        issues.len(),
                        t.chars().count()
                    ),
                    true,
                );
            }
            t
        }
        Err(e) => {
            return (
                None,
                format!("发现 {} 处问题，重写失败（{}）已保留原稿", issues.len(), e),
                true,
            )
        }
    };
    // F06：重写稿必须重新审核——内容变了旧审核结论作废，只按长度放行是假通过
    if text_fingerprint(&t) != reviewed_fp {
        match recheck_reviewed_text(st, book_id, ch, pending_block, &t).await {
            Ok(()) => {}
            Err(reason) => return (None, format!("重写稿复核未通过：{}", reason), true),
        }
    }
    let head = issues
        .iter()
        .take(2)
        .cloned()
        .collect::<Vec<_>>()
        .join("；");
    let head: String = head.chars().take(60).collect();
    (
        Some(t),
        format!(
            "发现 {} 处问题 → 已按意见重写并复核通过（{}）{}",
            issues.len(),
            head,
            soft_note
        ),
        false,
    )
}

/// 单次任务上限：按「区间长度」限制（最多 200 章），绝不按绝对章号截断。
pub(crate) const MAX_CHAPTERS_PER_TASK: i64 = 200;

/// 归一化章节范围：返回不超过 MAX_CHAPTERS_PER_TASK 的结束章号。
/// F09 修复点：旧实现 to.min(200) 会把 250~300 变成 250~200（倒置），这里按长度截断。
pub(crate) fn normalize_chapter_range(from: i64, to: i64) -> i64 {
    if to < from {
        return to;
    }
    let capped = from.saturating_add(MAX_CHAPTERS_PER_TASK - 1);
    to.min(capped)
}

/// 审核结论：hard=必须处理的问题，soft=不阻断的软建议。
pub(crate) struct ReviewVerdict {
    pub ok: bool,
    pub hard: Vec<String>,
    pub soft: Vec<String>,
}

/// 计算续跑范围：有缺章时由缺章集合的首尾驱动（主循环会自动跳过已有成果的章），
/// 无缺章时才回落到断点 current+1。
/// 例：缺章 [1]，current=2 → (1, 2)；这样"第1章缺、第2章已成功"能被真正补洞。
pub(crate) fn resume_range(resume_from: i64, resume_to: i64, holes: &[i64]) -> (i64, i64) {
    if let (Some(first), Some(last)) = (holes.first(), holes.last()) {
        let from = *first;
        let to = normalize_chapter_range(from, *last);
        return (from, to);
    }
    (resume_from, normalize_chapter_range(resume_from, resume_to))
}

/// 严格解析审核结论：仅当 ok 是布尔且 issues 是数组（元素为字符串）时才可信。
/// 缺字段、类型不符一律 None —— Fail-Closed。warnings 可选，只作软建议不阻断。
fn parse_review_verdict(j: &Value) -> Option<ReviewVerdict> {
    let obj = j.as_object()?;
    let ok = obj.get("ok")?.as_bool()?;
    let hard: Vec<String> = obj
        .get("issues")?
        .as_array()?
        .iter()
        .map(|x| x.as_str().map(|s| s.trim().to_string()))
        .collect::<Option<Vec<String>>>()?;
    let soft: Vec<String> = obj
        .get("warnings")
        .and_then(|w| w.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.trim().to_string())
                .collect()
        })
        .unwrap_or_default();
    Some(ReviewVerdict { ok, hard, soft })
}

/// 对「已被重写改变过的最终文本」重新执行同一套审核；返回 Err(原因) 表示不得定稿。
async fn recheck_reviewed_text(
    st: &Arc<AppState>,
    book_id: &str,
    ch: i64,
    pending_block: &str,
    text: &str,
) -> Result<(), String> {
    let db = &st.db;
    // 复核同样受单次预算约束：重写后正文若超预算，绝不能只审前段就当通过
    let (review_user, truncated) = build_review_user(db, book_id, ch, pending_block, text);
    if truncated {
        return Err(format!(
            "重写稿 {} 字超过单次审核预算 {} 字，未覆盖全文",
            text.chars().count(),
            REVIEW_BODY_BUDGET
        ));
    }
    let Some(chn) = molan_llm::resolve_agent_channel(db, "review") else {
        return Err("复核阶段无可用审核渠道".to_string());
    };
    let mut review_sys =
        "你是网文责编，审读一章正文。只输出一个 JSON 对象，不要解释、不要代码块标记。".to_string();
    if let Ok(extra) = molan_core::deepwrite::strict_agent_instructions(db, book_id, "continuity") {
        if !extra.is_empty() {
            review_sys.push_str("\n\n");
            review_sys.push_str(&extra);
        }
    }
    let params = ChatParams {
        base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: chn["key"].as_str().unwrap_or("").to_string(),
        model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
        messages: vec![
            json!({"role": "system", "content": review_sys}),
            json!({"role": "user", "content": review_user}),
        ],
        temperature: 0.2,
        max_tokens: 1500,
        stream: false,
        no_thinking: true,
        reasoning_effort: String::new(),
        log_tag: "auto_review_final".to_string(),
        log_book: book_id.to_string(),
        cancel: auto_cancel_token(),
    };
    let raw = molan_llm::chat_once_retry_logged_n(params, 2)
        .await
        .map(|(x, usage)| {
            log_llm_usage(
                db,
                book_id,
                "auto_review_final",
                chn["model"].as_str().unwrap_or(""),
                &usage,
            );
            x
        })
        .map_err(|e| format!("复核调用失败（{}）", e))?;
    let j = extract_json_loose(&raw).ok_or_else(|| "复核返回无法解析为 JSON".to_string())?;
    let v = parse_review_verdict(&j).ok_or_else(|| "复核返回结构不完整".to_string())?;
    if v.ok && v.hard.is_empty() {
        auto_log(ch, "review", format!("第{}章 重写稿复核通过", ch));
        Ok(())
    } else {
        Err(format!("ok={} hard={}", v.ok, v.hard.len()))
    }
}

// 包装层：失败时把错误落盘（进度保留最后持久值），面板/续跑可见
async fn auto_write_loop(
    st: Arc<AppState>,
    task_id: i64,
    book_id: String,
    session_id: String,
    from: i64,
    to: i64,
) -> anyhow::Result<()> {
    let res = auto_write_run(&st, task_id, &book_id, &session_id, from, to).await;
    if let Err(e) = &res {
        let current = AUTO_RUN
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|v| v["current"].as_i64()))
            .unwrap_or(0);
        persist_auto_task(&st.db, task_id, current, "failed", &format!("{}", e));
    }
    res
}

// ---------- 自动写作：单章完整流程（细纲 → 正文 → 审核 → 去味 → 落盘 → 章后处理） ----------
// 从主循环抽出的单章主体；失败/不合规不在此处动进度与熔断计数，由调用方决策。
enum ChapOutcome {
    Written,
    Rejected,
    GenFailed(String),
    /// 用户主动停止（auto_write_stop 掐断在飞调用）：既不算失败，也不算空洞
    Cancelled,
}

async fn write_one_chapter(
    st: &Arc<AppState>,
    book_id: &str,
    session_id: &str,
    ch: i64,
    pending_snapshot: Option<PendingSnapshot>,
) -> anyhow::Result<ChapOutcome> {
    let db = &st.db;
    auto_log(ch, "chapter", format!("第{}章 开始生成", ch));
    // 生成前已冻结的同任务上一章待审稿（文本+hash）；三个阶段共用同一份，绝不在生成后重新 read
    let pending_block = pending_snapshot
        .as_ref()
        .map(|p| p.block())
        .unwrap_or_default();
    let include_pending = pending_snapshot.is_some();

    // ---- 1) 本章细纲（缺失则自动生成）｜智能体：outline ----
    let outline_name = format!("细纲_第{}章.md", ch);
    let mut outline = files::read_file(db, book_id, "细纲", &outline_name).unwrap_or_default();
    if outline.trim().is_empty() && !task_full_auto(db, book_id) {
        auto_log(
            ch,
            "outline",
            format!(
                "第{}章尚无本章细纲；请作者先确认走向，本次不擅自补纲或写正文",
                ch
            ),
        );
        save_auto_message(
            db,
            session_id,
            "assistant",
            &format!(
                "第{}章缺少已确认细纲。请先在对话中确定本章目标、冲突和钩子，然后再确认写作。",
                ch
            ),
        );
        return Ok(ChapOutcome::Rejected);
    }
    if outline.trim().is_empty() {
        // 与正文同源：用 C 的统一目标章上下文（含 aiOff 过滤、仅 <ch 的前序、可选同任务待审稿），
        // 不再手拼档案/人物/上一章，避免细纲阶段绕过 aiOff 与时间轴约束。
        let outline_ctx = auto_book_context_for_chapter(db, book_id, ch, include_pending);
        // 细纲任务技能路由：主/辅助按 task=outline 解析，过滤禁用与空模板
        let outline_skills = effective_skills(db, book_id, "outline", &[]);
        let mut sys = String::from(
            "你是网文责编，为本章生成走向级细纲。只输出细纲正文（markdown），包含：本章目标 / 冲突与对手 / 看点与爽点 / 章末钩子。紧扣提供的设定、前情与上一章结尾，不得与人物表矛盾。不超过300字。",
        );
        let project_agent = molan_core::deepwrite::agent_instructions(db, book_id, "planner")
            .unwrap_or_else(|e| {
                auto_log(
                    ch,
                    "error",
                    format!("项目智能体提示注入失败，已降级：{}", e),
                );
                String::new()
            });
        if !project_agent.is_empty() {
            sys.push_str("\n\n");
            sys.push_str(&project_agent);
        }
        for sk in &outline_skills {
            if let Some(tpl) = sk["promptTemplate"].as_str() {
                if !tpl.trim().is_empty() {
                    sys.push_str(&format!(
                        "\n\n【技能：{}】\n{}",
                        sk["name"].as_str().unwrap_or(""),
                        tpl
                    ));
                }
            }
        }
        let bound_skills = super::deepwrite_bound_skills(db, book_id, "outline", &sys);
        if !bound_skills.is_empty() {
            sys.push_str("\n\n");
            sys.push_str(&bound_skills);
        }
        let user = format!(
            "{}\n\n{}请为【第{}章】生成细纲。",
            if outline_ctx.trim().is_empty() {
                "（暂无前文与设定，请按题材合理开篇）"
            } else {
                &outline_ctx
            },
            pending_block,
            ch
        );
        // 多智能体分工：outline 角色（未配置则回落活跃渠道）
        let chn = molan_llm::resolve_agent_channel(db, "outline")
            .ok_or_else(|| anyhow!("无可用模型渠道"))?;
        let params = ChatParams {
            base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
            api_key: chn["key"].as_str().unwrap_or("").to_string(),
            model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
            messages: vec![
                json!({"role": "system", "content": sys}),
                json!({"role": "user", "content": user}),
            ],
            temperature: 0.6,
            max_tokens: 8000,
            stream: false,
            // 自动细纲：核心创作任务，跟随全局 max 思考
            log_tag: "auto_outline".to_string(),
            log_book: book_id.to_string(),
            cancel: auto_cancel_token(),
            ..Default::default()
        };
        // 单章失败不终止整轮：记日志、返回 GenFailed 由调用方跳章/计连续失败
        match molan_llm::chat_once_retry_logged_n(params, 4).await {
            Ok((x, usage)) => {
                log_llm_usage(
                    db,
                    book_id,
                    "auto_outline",
                    chn["model"].as_str().unwrap_or(""),
                    &usage,
                );
                outline = x
            }
            Err(e) => {
                // 用户主动停止（auto_write_stop）：不是失败，不记「生成失败」错记，直接上报取消
                if molan_llm::is_cancelled_err(&e) || AUTO_STOP.load(Ordering::SeqCst) {
                    return Ok(ChapOutcome::Cancelled);
                }
                auto_log(ch, "error", format!("第{}章 生成失败：{}", ch, e));
                return Ok(ChapOutcome::GenFailed(e.to_string()));
            }
        }
        // 细纲是 AI 产物：走 AI 写入（锁定/已存在拒绝），失败必须显式处理
        if let Err(e) = write_ai_file_async(st, book_id, "细纲", &outline_name, &outline).await {
            auto_log(ch, "error", format!("第{}章 细纲写入失败：{}", ch, e));
            return Ok(ChapOutcome::GenFailed(format!("细纲写入失败：{}", e)));
        }
        save_auto_message(
            db,
            session_id,
            "user",
            &format!("【自动写作】第{}章细纲已生成", ch),
        );
        auto_log(ch, "outline", format!("第{}章 细纲已生成（自动补写）", ch));
    }

    // 细纲已存在（未走自动补写）同样记账，保证 outline_hash 不空缺
    if !outline.trim().is_empty() {
        let _ = molan_core::chapter_state::record_outline(
            db,
            book_id,
            ch,
            &molan_core::continuity::content_hash(&outline),
        );
    }

    // ---- 1.5) 冻结依赖指纹：细纲落盘后再抓，此后生成/审核/去味期间设定或前文若变化即失效 ----
    let frozen_inputs = freeze_inputs(db, book_id, ch).ok();

    // ---- 2) 正文（自动上下文已含本章细纲/档案/人物表/前情摘要/上章结尾）｜智能体：chapter ----
    let msg = format!("写第{}章正文", ch);
    // 用明确数值目标章构建上下文：前序已批准记忆 + 作者资产 + （可选）同任务上一章待审稿；
    // 绝不注入目标章之后的正文（F11），也绝不读任意其他任务草稿（F10）。
    let mut context = auto_book_context_for_chapter(db, book_id, ch, include_pending);
    let auto_ctx_present = !context.is_empty();
    if !auto_ctx_present {
        context = format!("【提醒】第{}章尚无前文与细纲，请按建书档案合理开篇。", ch);
    }
    // 待审连写：把生成前冻结的上一章待审稿显式追加进上下文（不是只靠布尔开关）
    if !pending_block.is_empty() {
        context.push_str("\n\n");
        context.push_str(&pending_block);
    }
    let book_genre = db
        .q_json(
            "SELECT genre FROM books WHERE id=?1",
            &[&book_id as &dyn rusqlite::ToSql],
        )
        .ok()
        .and_then(|v| {
            v.first()
                .and_then(|r| r["genre"].as_str().map(|x| x.to_string()))
        });
    let style: Option<String> = resolve_book_style(db, &st.root, book_id, book_genre.as_deref());
    // 技能路由（契约 C）：正文任务的主/辅助技能，过滤禁用与空模板，稳定去重
    let body_skills = effective_skills(db, book_id, "body", &[]);
    if !body_skills.is_empty() {
        let ids: Vec<String> = body_skills
            .iter()
            .map(|s| s["id"].as_str().unwrap_or("").to_string())
            .collect();
        auto_log(
            ch,
            "skill",
            format!("第{}章 生效技能：{}", ch, ids.join("、")),
        );
    }
    let mut sys = build_system(
        db,
        &st.root,
        book_id,
        book_genre.as_deref(),
        style.as_deref(),
        &body_skills,
        &msg,
        &context,
    );
    for role in ["character", "editor"] {
        let project_agent = molan_core::deepwrite::agent_instructions(db, book_id, role)
            .unwrap_or_else(|e| {
                auto_log(
                    ch,
                    "error",
                    format!("项目智能体提示注入失败，已降级：{}", e),
                );
                String::new()
            });
        if !project_agent.is_empty() {
            sys.push_str("\n\n");
            sys.push_str(&project_agent);
        }
    }
    let bound_skills = super::deepwrite_bound_skills(db, book_id, "body", &sys);
    if !bound_skills.is_empty() {
        sys.push_str("\n\n");
        sys.push_str(&bound_skills);
    }
    let max_tokens: i64 = molan_llm::get_setting(db, "max_tokens")
        .parse()
        .unwrap_or(8192);
    let max_tokens = if max_tokens == 0 { 8192 } else { max_tokens };
    // 多智能体分工：chapter 角色（未配置则回落活跃渠道）
    let chn =
        molan_llm::resolve_agent_channel(db, "chapter").ok_or_else(|| anyhow!("无可用模型渠道"))?;
    // 章节状态机 + 上下文清单（P0-2/P0-3）：只观测不阻断
    let _ = molan_core::chapter_state::record_generating(
        db,
        book_id,
        ch,
        &task_scope(),
        chn["model"].as_str().unwrap_or(""),
    );
    {
        let coverage = if ch > 1 {
            molan_core::continuity::chapter_context(db, book_id, ch)
                .ok()
                .map(|cx| {
                    json!({
                        "complete": cx["complete"], "missingCount": cx["missingCount"],
                        "stale": cx["stale"], "hidden": cx["hidden"],
                        "omittedEntries": cx["omittedEntries"],
                    })
                })
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let _ = molan_core::ctx_manifest::record(
            db,
            book_id,
            session_id,
            "auto_write:body",
            ch,
            chn["model"].as_str().unwrap_or(""),
            &context,
            coverage,
        );
    }
    let params = ChatParams {
        base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: chn["key"].as_str().unwrap_or("").to_string(),
        model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
        messages: vec![
            json!({"role": "system", "content": sys}),
            json!({"role": "user", "content": msg}),
        ],
        temperature: 0.7,
        max_tokens,
        stream: false,
        no_thinking: true,
        reasoning_effort: String::new(),
        log_tag: "auto_chapter".to_string(),
        log_book: book_id.to_string(),
        cancel: auto_cancel_token(),
    };
    let mut extra = String::new();
    let mut tries = 0;
    // 在循环内按需绑定，避免「首值从未被读」的多余赋值（loop 只可能以 break 带着 full 退出）
    let full = loop {
        tries += 1;
        let mut p2 = params.clone();
        if !extra.is_empty() {
            p2.messages.push(json!({"role": "user", "content": extra}));
        }
        let full = match molan_llm::chat_once_retry_logged_n(p2, 4).await {
            Ok((x, usage)) => {
                // loop 内每次尝试都是一次真实调用：每次成功都落账（含合规重试的每一次）
                log_llm_usage(
                    db,
                    book_id,
                    "auto_chapter",
                    chn["model"].as_str().unwrap_or(""),
                    &usage,
                );
                x
            }
            Err(e) => {
                // 用户主动停止（auto_write_stop）：不是失败，不记「生成失败」错记，直接上报取消
                if molan_llm::is_cancelled_err(&e) || AUTO_STOP.load(Ordering::SeqCst) {
                    return Ok(ChapOutcome::Cancelled);
                }
                auto_log(ch, "error", format!("第{}章 生成失败：{}", ch, e));
                return Ok(ChapOutcome::GenFailed(e.to_string()));
            }
        };
        let t = full.trim();
        let looks_like_question = t.ends_with('？')
            || t.contains("要直接开写")
            || t.contains("还是先就哪条线")
            || (t.contains("建议") && t.chars().count() < 900 && !t.contains("「"));
        if t.chars().count() >= 600 && !looks_like_question {
            break full;
        }
        if tries >= 3 {
            tracing::warn!(
                "自动写作：第{}章正文 {} 次尝试仍不合规（{}字）",
                ch,
                tries,
                t.chars().count()
            );
            break full;
        }
        extra = "你的上一条回复是在提问或给写作建议，不是正文。不要提问、不要列建议、不要讨论，直接输出本章完整正文：第一行「第N章 章名」，之后是连贯的小说正文。".to_string();
    };
    let mut body = if full.trim_start().starts_with('#') {
        full.clone()
    } else {
        format!("第{}章\n\n{}", ch, full.trim())
    };
    // 用任务创建时冻结的模式，避免中途改设置导致本任务策略漂移
    let full_auto = task_full_auto(db, book_id);
    if body.trim().chars().count() < 300 {
        let err = format!(
            "第{}章正文生成不合规（{}字），已跳过",
            ch,
            body.trim().chars().count()
        );
        tracing::warn!("自动写作：{}", err);
        auto_log(ch, "error", err.clone());
        let mut g = AUTO_RUN.lock().unwrap();
        if let Some(v) = g.as_mut() {
            v["error"] = json!(err);
        }
        return Ok(ChapOutcome::Rejected);
    }

    // ---- 2.35) 全自动前置门：依赖必须可验证（input_fingerprint 可取、前章记忆完整）----
    // 记忆不完整/缺章时绝不自动定稿，转待审并说明原因。
    let mut gate_block: Option<String> = None;
    if full_auto {
        if frozen_inputs.is_none() {
            gate_block = Some("依赖指纹不可用，无法保证依赖未变".to_string());
        }
        // 上一章草稿若已失效（被改动/来源不可信），本章不得自动定稿，转人工解决
        if gate_block.is_none() {
            if let Err(e) = molan_core::continuity::check_draft_dependency(db, book_id, ch) {
                gate_block = Some(format!("上章草稿依赖已失效（{}）→ 需人工重新审核", e));
            }
        }
        if gate_block.is_none() {
            match molan_core::continuity::chapter_context(db, book_id, ch) {
                Ok(ctx) => {
                    if !ctx["complete"].as_bool().unwrap_or(false) {
                        let missing = ctx["missing"].as_array().map(|a| a.len()).unwrap_or(0);
                        gate_block = Some(format!(
                            "前章记忆不完整（缺{}章/存在过期记忆）→ 需人工确认或重建记忆",
                            missing
                        ));
                    }
                }
                Err(e) => gate_block = Some(format!("前章记忆上下文不可用（{}）", e)),
            }
        }
        // 严格门禁（flag strict_continuity_gate，默认关；P0 阶段3）：
        // 存在未裁决的事实冲突时绝不自动定稿，转人工裁决。默认关 = 行为零变化。
        if gate_block.is_none()
            && molan_llm::get_setting(db, "strict_continuity_gate") == "1"
            && molan_core::facts::has_disputed(db, book_id)
        {
            gate_block = Some(
                "存在未裁决的事实冲突（get_fact_conflicts 查询/裁决后重试）→ 需人工处理"
                    .to_string(),
            );
        }
        if let Some(reason) = &gate_block {
            auto_log(
                ch,
                "review",
                format!("第{}章 全自动前置门未通过：{}", ch, reason),
            );
            save_auto_message(
                db,
                session_id,
                "user",
                &format!("【全自动】第{}章 前置门未通过：{} → 转待审", ch, reason),
            );
        }
    }
    let full_auto = full_auto && gate_block.is_none();

    // ---- 2.4) 自动审核（全自动模式）：与细纲/人物表/上一章比对，发现硬伤按意见重写一轮 ----
    let mut needs_review = false;
    // 审核所绑定的最终文本指纹：审核通过后任何改动（重写/去味）都会使它失配 → 触发复审
    let mut reviewed_fp = text_fingerprint(&body);
    if full_auto {
        auto_log(ch, "review", format!("第{}章 审核中…", ch));
        let (fixed, note, flagged) =
            review_and_fix_chapter(st, book_id, ch, &body, &outline, &pending_block).await;
        if let Some(f) = fixed {
            body = f;
        }
        needs_review = flagged;
        // 审核对象 = 审核后的实际文本（重写稿已被复核，其指纹即为有效审核对象）
        reviewed_fp = text_fingerprint(&body);
        // 章节状态机：审核结论入账（只观测不阻断）
        if needs_review {
            let _ = molan_core::chapter_state::record_review_failed(db, book_id, ch, &note);
        } else {
            let _ = molan_core::chapter_state::record_reviewed(db, book_id, ch, &note);
        }
        if needs_review {
            auto_log(ch, "error", format!("第{}章 审核未通过，转入待审", ch));
        }
        save_auto_message(
            db,
            session_id,
            "user",
            &format!("【全自动】第{}章 审核：{}", ch, note),
        );
        auto_log(ch, "review", format!("第{}章 审核：{}", ch, note));
    }

    // ---- 2.5) 去AI味审核层：读本书「自动去味」设置（默认官方标准），机器门 + 整合门 ----
    auto_log(ch, "deai", format!("第{}章 去AI味中…", ch));
    let (body, report) = auto_humanize(db, book_id, &body, None, "chapter").await;
    let deai_note = format!(
        "去AI味：{}分/{}（{}项·{}）",
        report["score"].as_i64().unwrap_or(0),
        if report["passed"].as_bool().unwrap_or(false) {
            "过"
        } else if report["score"].as_i64().unwrap_or(0) <= 50 {
            "压线"
        } else {
            "未过"
        },
        report["violations"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0),
        report["method"].as_str().unwrap_or("")
    );
    save_auto_message(
        db,
        session_id,
        "user",
        &format!(
            "【自动写作】第{}章 {}（机器门 {} 项：{}）",
            ch,
            deai_note,
            report["violations"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0),
            report["violations"]
                .as_array()
                .map(|a| a
                    .iter()
                    .filter_map(|x| x["rule"].as_str())
                    .take(3)
                    .collect::<Vec<_>>()
                    .join("、"))
                .unwrap_or_default()
        ),
    );
    auto_log(ch, "deai", format!("第{}章 {}", ch, deai_note));
    let _ = molan_core::chapter_state::record_humanized(
        db,
        book_id,
        ch,
        &molan_core::continuity::content_hash(&body),
    );

    // F06：去AI味改变了正文 → 旧审核结论作废，必须对最终文本重审
    if full_auto && !needs_review && text_fingerprint(&body) != reviewed_fp {
        auto_log(ch, "review", format!("第{}章 去味后最终稿复审…", ch));
        match recheck_reviewed_text(st, book_id, ch, &pending_block, &body).await {
            Ok(()) => {
                save_auto_message(
                    db,
                    session_id,
                    "user",
                    &format!("【全自动】第{}章 去味后复审：通过", ch),
                );
            }
            Err(reason) => {
                needs_review = true;
                auto_log(
                    ch,
                    "error",
                    format!("第{}章 去味后复审未通过（{}）→ 转待审", ch, reason),
                );
                save_auto_message(
                    db,
                    session_id,
                    "user",
                    &format!(
                        "【全自动】第{}章 去味后复审未通过（{}）→ 转待审",
                        ch, reason
                    ),
                );
            }
        }
    }

    // needs_review 时即使全自动也走待审分支：审核不可信的内容不得直接进正式正文
    if full_auto && !needs_review {
        // 全自动：提交前取消屏障 + 串行落盘；已请求停止则一个字都不写
        let name = format!("第{}章.md", ch);
        match commit_chapter_file(st, book_id, "正文", &name, &body, frozen_inputs.clone()).await
        {
            CommitOutcome::Written => {
                let _ = molan_core::chapter_state::record_save(
                    db,
                    book_id,
                    ch,
                    "正文",
                    &molan_core::continuity::content_hash(&body),
                );
            }
            CommitOutcome::Cancelled => {
                auto_log(
                    ch,
                    "stopped",
                    format!("第{}章 提交前检测到停止请求，未写入正文", ch),
                );
                return Ok(ChapOutcome::Cancelled);
            }
            CommitOutcome::DependencyChanged => {
                auto_log(
                    ch,
                    "error",
                    format!("第{}章 依赖已变化（设定/前文/技能被改），转待审", ch),
                );
                save_auto_message(
                    db,
                    session_id,
                    "user",
                    &format!("【全自动】第{}章 依赖已变化 → 转待审", ch),
                );
                // 落为待审，绝不定稿
                let _ = commit_chapter_file(
                    st,
                    book_id,
                    molan_core::db::REVIEW_GROUP,
                    &name,
                    &body,
                    None,
                )
                .await;
                let _ = db.exec(
                    "INSERT OR REPLACE INTO pending_chapter(book_id,ch,review_file,status,created_at) VALUES(?,?,?,?,?)",
                    &[&book_id as &dyn rusqlite::ToSql, &ch, &name, &"pending", &stats::now_ms()],
                );
                // 按磁盘实况记账：待审稿确实存在才记 DRAFT_REVIEW，再如实落到 STALE_DEPENDENCY
                if let Some(draft) =
                    files::read_file(db, book_id, molan_core::db::REVIEW_GROUP, &name)
                {
                    if !draft.trim().is_empty() {
                        let _ = molan_core::chapter_state::record_save(
                            db,
                            book_id,
                            ch,
                            molan_core::db::REVIEW_GROUP,
                            &molan_core::continuity::content_hash(&draft),
                        );
                    }
                }
                let _ = molan_core::chapter_state::record_stale_dependency(
                    db,
                    book_id,
                    ch,
                    json!({"note": "依赖已变化，定稿转待审"}),
                );
                return Ok(ChapOutcome::Rejected);
            }
            CommitOutcome::Failed(e) => {
                auto_log(ch, "error", format!("第{}章 定稿写入失败：{}", ch, e));
                return Ok(ChapOutcome::GenFailed(e));
            }
        }
        let final_hash = text_fingerprint(&body);
        // 不可变批准事件：区分"系统批准过此 hash"与"作者后来改稿"
        if let Err(e) = molan_core::continuity::record_approval(db, book_id, ch, &name, &final_hash)
        {
            auto_log(
                ch,
                "memory",
                format!("第{}章 定稿已保存，但批准事件记录失败：{}", ch, e),
            );
            save_auto_message(
                db,
                session_id,
                "user",
                &format!("【全自动】第{}章 定稿已保存，批准记录未同步（{}）", ch, e),
            );
        }
        let _ = molan_core::chapter_state::record_approved(
            db,
            book_id,
            ch,
            json!({"finalHash": final_hash, "by": "full_auto"}),
        );
        // 队列状态与正式稿对齐（DB 失败只告警，绝不删除已写正文）
        if let Err(e) = db.exec(
            "INSERT OR REPLACE INTO pending_chapter(book_id,ch,review_file,status,created_at) VALUES(?,?,?,?,?)",
            &[
                &book_id as &dyn rusqlite::ToSql,
                &ch,
                &name,
                &"approved",
                &stats::now_ms(),
            ],
        ) {
            auto_log(ch, "error", format!("第{}章 正文已写，但审批队列状态更新失败：{}", ch, e));
            save_auto_message(db, session_id, "user", &format!("【全自动】第{}章 正文已保存，队列状态未同步（{}）", ch, e));
        }
        save_auto_message(
            db,
            session_id,
            "user",
            &format!(
                "【全自动】第{}章 已写入正文/{}（{}字·审核hash {}）",
                ch,
                name,
                body.trim().chars().count(),
                &final_hash[..8]
            ),
        );
        auto_log(
            ch,
            "write",
            format!(
                "第{}章 已写入 正文/{}（{}字）",
                ch,
                name,
                body.trim().chars().count()
            ),
        );
        save_auto_message(db, session_id, "assistant", &body);
        // 全自动定稿 = 一次真实批准：按「实际批准章 + hash」事件化写正式记忆
        if let Err(e) = post_approved_chapter(st, book_id, ch).await {
            auto_log(
                ch,
                "memory",
                format!("第{}章 定稿已保存，正式记忆未同步：{}", ch, e),
            );
            save_auto_message(
                db,
                session_id,
                "user",
                &format!("【全自动】第{}章 定稿已保存，记忆未同步（{}）", ch, e),
            );
        }
    } else {
        // 审批流：AI 产出的正文先进「正文待审」组，人工接受后才转正到「正文」
        let name = format!("第{}章.md", ch);
        // 已有正式稿/待审稿时绝不覆盖（回补与主循环统一守卫）
        let existing = files::read_file(db, book_id, "正文", &name).unwrap_or_default();
        let pending_now =
            files::read_file(db, book_id, molan_core::db::REVIEW_GROUP, &name).unwrap_or_default();
        if !existing.trim().is_empty() || !pending_now.trim().is_empty() {
            auto_log(ch, "skip", format!("第{}章 已有正文或待审稿，放弃覆盖", ch));
            return Ok(ChapOutcome::Rejected);
        }
        match commit_chapter_file(
            st,
            book_id,
            molan_core::db::REVIEW_GROUP,
            &name,
            &body,
            None,
        )
        .await
        {
            CommitOutcome::Written => {
                let _ = molan_core::chapter_state::record_save(
                    db,
                    book_id,
                    ch,
                    molan_core::db::REVIEW_GROUP,
                    &molan_core::continuity::content_hash(&body),
                );
            }
            CommitOutcome::Cancelled => {
                auto_log(
                    ch,
                    "stopped",
                    format!("第{}章 提交前检测到停止请求，未写入待审", ch),
                );
                return Ok(ChapOutcome::Cancelled);
            }
            CommitOutcome::DependencyChanged => {
                auto_log(
                    ch,
                    "error",
                    format!("第{}章 待审写入前依赖已变化，放弃覆盖", ch),
                );
                return Ok(ChapOutcome::Rejected);
            }
            CommitOutcome::Failed(e) => {
                auto_log(ch, "error", format!("第{}章 待审写入失败：{}", ch, e));
                return Ok(ChapOutcome::GenFailed(e));
            }
        }
        save_auto_message(db, session_id, "assistant", &body);
        auto_log(
            ch,
            "write",
            format!(
                "第{}章 已存入 正文待审/{}（{}字）",
                ch,
                name,
                body.trim().chars().count()
            ),
        );
        // 记录到审批队列：队列是"可接受成果"的唯一入口，写失败不能算成功
        if let Err(e) = db.exec(
            "INSERT OR REPLACE INTO pending_chapter(book_id,ch,review_file,status,created_at) VALUES(?,?,?,?,?)",
            &[
                &book_id as &dyn rusqlite::ToSql,
                &ch,
                &name,
                &"pending",
                &stats::now_ms(),
            ],
        ) {
            auto_log(ch, "error", format!("第{}章 待审已写盘但队列登记失败：{}", ch, e));
            save_auto_message(db, session_id, "user", &format!("【自动写作】第{}章 待审稿已保存，但审批队列登记失败（{}）", ch, e));
            return Ok(ChapOutcome::GenFailed(format!("审批队列登记失败：{}", e)));
        }
        // 登记本章草稿的任务来源（精确 provenance）——必须在依赖记录之前，
        // 否则后续章的 pending_for_task 永远取不到上一章，待审连写会静默失效。
        let task_id = AUTO_RUN
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|v| v["taskId"].as_i64()))
            .unwrap_or(0);
        let body_hash = text_fingerprint(&body);
        if let Err(e) = molan_core::continuity::record_draft_origin(
            db,
            book_id,
            ch,
            &name,
            &body_hash,
            &task_id.to_string(),
        ) {
            auto_log(ch, "error", format!("第{}章 草稿来源登记失败：{}", ch, e));
        }
        // 待审连写依赖：用【生成前冻结】的上一章待审稿 hash 登记，
        // 绝不在生成后重新 read（那时前稿可能已被改动，读到的新 hash 会冒充旧输入）。
        if let Some(snap) = pending_snapshot.as_ref() {
            if ch > 1 {
                if let Err(e) = molan_core::continuity::record_draft_dependency(
                    db,
                    book_id,
                    ch,
                    ch - 1,
                    &snap.hash,
                    &task_id.to_string(),
                ) {
                    auto_log(ch, "error", format!("第{}章 待审依赖登记失败：{}", ch, e));
                }
            }
        }
    }

    // 章后正式记忆不在这里触发：普通写作完成后不得再跑「按最大正文章取料」的旧派生逻辑
    // （那会把目标章之后的正文读进摘要，造成未来信息泄漏并使已 valid 记忆过期）。
    // 正式记忆统一由 post_approved_chapter 按「实际批准章 + hash」事件化写入。
    Ok(ChapOutcome::Written)
}

/// 停止收尾（单点）：落「已手动停止」消息 + 停止日志 + persist(stopped)。
/// 顶部 AUTO_STOP 分支、取消分支、回补取消分支共用同一语义，避免三处漂移。
fn auto_write_stop_finish(
    db: &molan_core::db::Db,
    task_id: i64,
    session_id: &str,
    current: i64,
    err_note: &str,
) {
    save_auto_message(
        db,
        session_id,
        "user",
        &format!("【自动写作】已手动停止，完成至第{}章", current),
    );
    auto_log(current, "stopped", format!("已停止，完成至第{}章", current));
    persist_auto_task(db, task_id, current, "stopped", err_note);
}

async fn auto_write_run(
    st: &Arc<AppState>,
    task_id: i64,
    book_id: &str,
    session_id: &str,
    from: i64,
    to: i64,
) -> anyhow::Result<()> {
    let db = &st.db;
    let book_id = book_id.to_string();
    let session_id = session_id.to_string();
    // 每个写作任务开始时种子一次四角色，避免在每轮 LLM 调用里重复写入
    let _ = molan_core::deepwrite::ensure_book_agents(db, &book_id);
    let mut ch = if from > 0 {
        from
    } else {
        latest_chapter_num(db, &book_id) + 1
    };
    set_auto_progress(ch.saturating_sub(1));
    persist_auto_task(db, task_id, ch.saturating_sub(1), "running", "");
    save_auto_message(
        db,
        &session_id,
        "user",
        &format!("【自动写作】开始：第{}~{}章", ch, to),
    );
    // 连续失败计数：单章失败跳章继续，连续 3 章失败才判定任务失败（避免一次网络抖动整轮报废）
    let mut failed_streak = 0i32;
    // 空洞：生成失败/不合规被跳过的章节，主循环跑完后按章号升序回补
    let mut holes: Vec<i64> = Vec::new();
    // 因「已有正文/待审稿」被跳过的章数（用于完成消息如实汇报）
    let mut skipped = 0i64;

    while ch <= to {
        if AUTO_STOP.load(Ordering::SeqCst) {
            auto_write_stop_finish(db, task_id, &session_id, ch - 1, "");
            return Ok(());
        }
        // 已有正文则跳过
        let existing =
            files::read_file(db, &book_id, "正文", &format!("第{}章.md", ch)).unwrap_or_default();
        if !existing.trim().is_empty() {
            auto_log(ch, "skip", format!("第{}章 已有正文，跳过", ch));
            set_auto_progress(ch);
            persist_auto_task(db, task_id, ch, "running", "");
            skipped += 1;
            ch += 1;
            continue;
        }
        // 待审保护：该章已有人工未处理的待审稿时绝不重新生成——
        // 重新生成会覆盖旧稿且白烧调用；需重写请先在审批队列驳回该章
        let pending_draft = files::read_file(
            db,
            &book_id,
            molan_core::db::REVIEW_GROUP,
            &format!("第{}章.md", ch),
        )
        .unwrap_or_default();
        if !pending_draft.trim().is_empty() {
            auto_log(
                ch,
                "skip",
                format!("第{}章 已有待审稿，跳过（需重写请先在审批队列驳回）", ch),
            );
            set_auto_progress(ch);
            persist_auto_task(db, task_id, ch, "running", "");
            skipped += 1;
            ch += 1;
            continue;
        }

        // 生成前冻结同任务上一章待审稿：文本+hash 一次抓取，供细纲/正文/审核/依赖登记共用
        let pending_snapshot = freeze_pending_snapshot(db, &book_id, task_id, ch - 1);
        match write_one_chapter(st, &book_id, &session_id, ch, pending_snapshot).await {
            Ok(ChapOutcome::Written) => {
                // 本章完整产出并落盘成功：连续失败计数归零
                failed_streak = 0;
            }
            Ok(ChapOutcome::GenFailed(e)) => {
                failed_streak += 1;
                if failed_streak >= 3 {
                    // persist 用当前 AUTO_RUN.current 收窄竞态，语义等同原 persist(ch-1)
                    let cur = AUTO_RUN
                        .lock()
                        .ok()
                        .and_then(|g| g.as_ref().and_then(|v| v["current"].as_i64()))
                        .unwrap_or(0);
                    persist_auto_task(db, task_id, cur.max(ch - 1), "failed", &e);
                    return Err(anyhow!("{}", e));
                }
                holes.push(ch);
            }
            Ok(ChapOutcome::Rejected) => {
                holes.push(ch);
            }
            // 用户主动停止掐断了在飞调用：不计 failed_streak、不 push holes，直接走停止收尾
            Ok(ChapOutcome::Cancelled) => {
                let cur = AUTO_RUN
                    .lock()
                    .ok()
                    .and_then(|g| g.as_ref().and_then(|v| v["current"].as_i64()))
                    .unwrap_or(0);
                auto_write_stop_finish(db, task_id, &session_id, cur.max(ch - 1), "");
                return Ok(());
            }
            Err(e) => return Err(e),
        }

        set_auto_progress(ch);
        persist_auto_task(db, task_id, ch, "running", "");
        ch += 1;
    }

    // ---- 空洞回补：生成失败/不合规被跳过的章节，按章号升序再试一次 ----
    let mut backfilled: Vec<i64> = Vec::new();
    let mut remaining: Vec<i64> = Vec::new();
    if !holes.is_empty() && !task_full_auto(db, &book_id) {
        let list: Vec<String> = holes.iter().map(|x| x.to_string()).collect();
        persist_auto_task(
            db,
            task_id,
            to,
            "interrupted",
            &format!("等待作者确认第{}章细纲或稿件", list.join("、")),
        );
        return Ok(());
    }
    if !holes.is_empty() {
        let list: Vec<String> = holes.iter().map(|x| x.to_string()).collect();
        auto_log(
            to,
            "hole",
            format!("第{}章 生成失败，开始回补", list.join("、")),
        );
        save_auto_message(
            db,
            &session_id,
            "user",
            &format!(
                "【自动写作】以下章节未产出，开始回补：第{}章",
                list.join("、")
            ),
        );
        for hc in holes.iter().copied() {
            if AUTO_STOP.load(Ordering::SeqCst) {
                remaining.push(hc);
                continue;
            }
            let pending_snapshot = freeze_pending_snapshot(db, &book_id, task_id, hc - 1);
            match write_one_chapter(st, &book_id, &session_id, hc, pending_snapshot).await {
                Ok(ChapOutcome::Written) => {
                    backfilled.push(hc);
                    auto_log(hc, "hole", format!("第{}章 回补成功", hc));
                }
                Ok(ChapOutcome::GenFailed(e)) => {
                    auto_log(hc, "error", format!("第{}章 回补失败：{}", hc, e));
                    remaining.push(hc);
                }
                Ok(ChapOutcome::Rejected) => {
                    auto_log(hc, "error", format!("第{}章 回补失败：正文生成不合规", hc));
                    remaining.push(hc);
                }
                // 用户主动停止掐断了回补的在飞调用：不记「回补失败」错记，按停止语义收尾
                Ok(ChapOutcome::Cancelled) => {
                    remaining.push(hc);
                    auto_write_stop_finish(
                        db,
                        task_id,
                        &session_id,
                        to,
                        &format!("回补未完成，剩余空洞：第{}章", hc),
                    );
                    return Ok(());
                }
                // 无可用模型渠道这类致命错：原样向上传播（整个任务失败）
                Err(e) => return Err(e),
            }
        }
        if AUTO_STOP.load(Ordering::SeqCst) && !remaining.is_empty() {
            // 回补被手动停止中断：进度语义不变（不 backwards 移动），只标注剩余空洞
            let rem: Vec<String> = remaining.iter().map(|x| x.to_string()).collect();
            save_auto_message(
                db,
                &session_id,
                "user",
                &format!("【自动写作】回补被停止，仍缺：第{}章", rem.join("、")),
            );
            auto_log(
                to,
                "stopped",
                format!("回补被停止，仍缺第{}章", rem.join("、")),
            );
            persist_auto_task(
                db,
                task_id,
                to,
                "stopped",
                &format!("回补未完成，剩余空洞：第{}章", rem.join("、")),
            );
            return Ok(());
        }
    }
    if !backfilled.is_empty() {
        let ok_list: Vec<String> = backfilled.iter().map(|x| x.to_string()).collect();
        auto_log(
            to,
            "hole",
            format!(
                "回补完成：{}章成功（第{}章）",
                backfilled.len(),
                ok_list.join("、")
            ),
        );
    }

    let mut done_msg = if task_full_auto(db, &book_id) {
        // 如实汇报：范围内仍在「正文待审」的章不能算"直接写入正文"
        let mut pending_cnt = 0i64;
        for c in from..=to {
            let rv = files::read_file(
                db,
                &book_id,
                molan_core::db::REVIEW_GROUP,
                &format!("第{}章.md", c),
            )
            .unwrap_or_default();
            if !rv.trim().is_empty() {
                pending_cnt += 1;
            }
        }
        if pending_cnt > 0 {
            format!(
                "【全自动】完成：第{}~{}章（{}章未过审已转「正文待审」，其余写入正文）",
                from, to, pending_cnt
            )
        } else {
            format!(
                "【全自动】完成：第{}~{}章（已自动审核 + 自动去AI味 + 直接写入正文）",
                from, to
            )
        }
    } else {
        format!(
            "【自动写作】完成：第{}~{}章（正文在「正文待审」，请审阅）",
            from, to
        )
    };
    if skipped > 0 {
        done_msg.push_str(&format!("；跳过{}章（已有正文或待审稿）", skipped));
    }
    if !remaining.is_empty() {
        let failed: Vec<String> = remaining.iter().map(|x| x.to_string()).collect();
        done_msg.push_str(&format!(
            "；第{}章回补失败，可稍后从该章重新发起",
            failed.join("、")
        ));
    } else if !holes.is_empty() {
        done_msg.push_str("；空洞章节已全部回补");
    }
    // F08：完成必须按成果集合判定——范围内仍有空洞（无正文且无待审）时不得报 done
    let holes_left = collect_missing_chapters(db, &book_id, from, to);
    let final_status = if holes_left.is_empty() {
        "done"
    } else {
        "partial"
    };
    if !holes_left.is_empty() {
        let miss: Vec<String> = holes_left.iter().map(|x| x.to_string()).collect();
        done_msg.push_str(&format!(
            "；第{}章仍无正文与待审稿（未完成，可 auto_write_resume 补洞）",
            miss.join("、")
        ));
    }
    save_auto_message(db, &session_id, "user", &done_msg);
    auto_log(to, final_status, done_msg.clone());
    persist_auto_task(
        db,
        task_id,
        to,
        final_status,
        if final_status == "done" {
            ""
        } else {
            "存在缺章"
        },
    );
    Ok(())
}

/// 取本任务上一章待审稿正文（供上下文显式承接；非本任务返回 None）。
/// 依据父 continuity 的精确来源账本（draft_origin）：同时刻的手工草稿不构成任务来源。
fn same_task_pending_text(
    db: &molan_core::db::Db,
    book_id: &str,
    task_id: i64,
    prev_ch: i64,
) -> Option<String> {
    if prev_ch < 1 {
        return None;
    }
    molan_core::continuity::pending_for_task(db, book_id, prev_ch, &task_id.to_string())
        .ok()
        .flatten()
        .map(|(_, text)| text)
}

/// 收集范围内「既无正式正文、也无待审稿」的章节（成果集合判定用，不依赖 current 指针）
pub(crate) fn collect_missing_chapters(
    db: &molan_core::db::Db,
    book_id: &str,
    from: i64,
    to: i64,
) -> Vec<i64> {
    let mut miss = Vec::new();
    let mut ch = from.max(1);
    while ch <= to {
        let name = format!("第{}章.md", ch);
        let body = files::read_file(db, book_id, "正文", &name).unwrap_or_default();
        let pend =
            files::read_file(db, book_id, molan_core::db::REVIEW_GROUP, &name).unwrap_or_default();
        if body.trim().is_empty() && pend.trim().is_empty() {
            miss.push(ch);
        }
        ch += 1;
    }
    miss
}

// ---------- 章后正式记忆（按「实际批准章 + hash」事件化） ----------

/// 解析某章正式稿文件名（优先 第N章.md，其次 第N章.ai.md）。
fn resolve_approved_name(db: &molan_core::db::Db, book_id: &str, ch: i64) -> Option<String> {
    let plain = format!("第{}章.md", ch);
    if !files::read_file(db, book_id, "正文", &plain)
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        return Some(plain);
    }
    let ai = format!("第{}章.ai.md", ch);
    if !files::read_file(db, book_id, "正文", &ai)
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        return Some(ai);
    }
    None
}

/// 记忆抽取/复核的单章正文预算：30000 字覆盖几乎所有网文章节；
/// 超出时截断并明确标注（复核器据此知道所见非全文，覆盖度判断不会假装完整）。
const MEMORY_BODY_BUDGET: usize = 30000;

fn memory_body_with_note(body: &str) -> String {
    if body.chars().count() <= MEMORY_BODY_BUDGET {
        return body.to_string();
    }
    format!(
        "{}\n（正文共{}字，此处仅前{}字；复核覆盖度时按截断稿对待，不得假装看过全文）",
        body.chars().take(MEMORY_BODY_BUDGET).collect::<String>(),
        body.chars().count(),
        MEMORY_BODY_BUDGET
    )
}

fn mem_str(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").trim().to_string()
}

/// 证据包含检查的归一化：只剥离“纯排版字符”——空白（换行/空格）、引号样式（「」『』与“”‘’等价）、
/// markdown 强调符号（** 等）。文字内容本身必须逐字一致：概括、同义替换仍会被拒绝——那才是编造。
fn is_typographic_only(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '「' | '」' | '『' | '』' | '“' | '”' | '‘' | '’' | '"' | '\'' | '*' | '_' | '`'
        )
}

fn norm_ws(s: &str) -> String {
    s.chars().filter(|c| !is_typographic_only(*c)).collect()
}

fn source_contains_evidence(source: &str, evidence: &str) -> bool {
    if evidence.trim().is_empty() {
        return false;
    }
    if source.contains(evidence) {
        return true;
    }
    let (ns, ne) = (norm_ws(source), norm_ws(evidence));
    !ne.is_empty() && ns.contains(&ne)
}

/// 将仅排版差异的引文定位回真实原文，最终入库仍是原文的连续子串（含原始引号/markdown）。
fn canonical_evidence(source: &str, evidence: &str) -> Result<String, String> {
    if !source_contains_evidence(source, evidence) {
        return Err("证据不在原文中".to_string());
    }
    let quote = if source.contains(evidence) {
        evidence.to_string()
    } else {
        let chars: Vec<(usize, char)> = source
            .char_indices()
            .filter(|(_, c)| !is_typographic_only(*c))
            .collect();
        let needle: Vec<char> = norm_ws(evidence).chars().collect();
        let start = chars
            .windows(needle.len())
            .position(|w| w.iter().map(|(_, c)| *c).eq(needle.iter().copied()))
            .ok_or_else(|| "证据无法定位".to_string())?;
        let (end, last) = chars[start + needle.len() - 1];
        source[chars[start].0..end + last.len_utf8()].to_string()
    };
    if quote.trim().chars().count() < source.trim().chars().count().min(6) {
        return Err("证据过短：请从原文复制至少6个字的完整片段".to_string());
    }
    Ok(quote)
}

/// 严格校验抽取结果：任何字段缺失、类型不符或证据不是原文子串都判为失败。
/// 绝不"静默删掉坏条目后把剩余当 valid 提交"——那会把编造内容洗成可信记忆。
fn sanitize_memory_payload(source: &str, payload: &Value) -> Result<Value, String> {
    let summary = payload["summary"].as_str().unwrap_or("").trim().to_string();
    if summary.is_empty() {
        return Err("记忆缺少有效 summary".to_string());
    }
    let mut facts = Vec::new();
    for f in payload["facts"]
        .as_array()
        .ok_or_else(|| "记忆缺少 facts 数组".to_string())?
    {
        let (entity, field, value, evidence) = (
            mem_str(f, "entity"),
            mem_str(f, "field"),
            mem_str(f, "value"),
            mem_str(f, "evidence"),
        );
        if entity.is_empty() || field.is_empty() || value.is_empty() {
            return Err(format!("fact 字段不完整：{}", f));
        }
        if evidence.is_empty() {
            return Err(format!("fact 缺少原文证据：{} / {}", entity, field));
        }
        if !source_contains_evidence(source, &evidence) {
            return Err(format!(
                "fact 证据不在原文中：{} / {} → {}",
                entity, field, evidence
            ));
        }
        facts.push(json!({"entity": entity, "field": field, "value": value, "evidence": evidence}));
    }
    let mut threads = Vec::new();
    for t in payload["threads"]
        .as_array()
        .ok_or_else(|| "记忆缺少 threads 数组".to_string())?
    {
        let (id, state, evidence) = (
            mem_str(t, "id"),
            mem_str(t, "state"),
            mem_str(t, "evidence"),
        );
        if id.is_empty() {
            return Err("thread 缺少 id".to_string());
        }
        if !matches!(
            state.as_str(),
            "planted" | "advanced" | "partial" | "resolved" | "cancelled"
        ) {
            return Err(format!("thread 状态非法：{} / {}", id, state));
        }
        if evidence.is_empty() || !source_contains_evidence(source, &evidence) {
            return Err(format!("thread 证据不在原文中：{} → {}", id, evidence));
        }
        threads.push(json!({"id": id, "state": state, "evidence": evidence}));
    }
    let mut events = Vec::new();
    for e in payload["events"]
        .as_array()
        .ok_or_else(|| "记忆缺少 events 数组".to_string())?
    {
        let (description, evidence) = (mem_str(e, "description"), mem_str(e, "evidence"));
        if description.is_empty() {
            return Err("event 缺少 description".to_string());
        }
        if evidence.is_empty() || !source_contains_evidence(source, &evidence) {
            return Err(format!("event 证据不在原文中：{}", description));
        }
        events.push(json!({"description": description, "evidence": evidence}));
    }
    // 秘密账本（可选）：fact 非空 + 证据同规则；known_by/unknown_to 必须是字符串数组；
    // id 可选但存在时必须有效。任何不合规条目整条报错，绝不静默丢弃。
    let mut secrets = Vec::new();
    let mut secrets_present = false;
    if let Some(arr) = payload.get("secrets") {
        secrets_present = true;
        for s in arr
            .as_array()
            .ok_or_else(|| "secrets 必须是数组".to_string())?
        {
            let (fact, evidence) = (mem_str(s, "fact"), mem_str(s, "evidence"));
            if fact.is_empty() {
                return Err(format!("secret 缺少秘密内容：{}", s));
            }
            if evidence.is_empty() || !source_contains_evidence(source, &evidence) {
                return Err(format!("secret 证据不在原文中：{} → {}", fact, evidence));
            }
            for field in ["known_by", "unknown_to"] {
                if let Some(v) = s.get(field) {
                    let ok = v.as_array().is_some_and(|a| a.iter().all(Value::is_string));
                    if !ok {
                        return Err(format!("secret {} 必须是字符串数组", field));
                    }
                }
            }
            let id = mem_str(s, "id");
            if !id.is_empty() && id.chars().count() > 32 {
                return Err(format!("secret id 过长：{}", id));
            }
            secrets.push(json!({
                "id": id, "fact": fact,
                "known_by": s["known_by"].as_array().cloned().unwrap_or_default(),
                "unknown_to": s["unknown_to"].as_array().cloned().unwrap_or_default(),
                "evidence": evidence,
            }));
        }
    }
    // 空壳防线：原文有实质内容却抽不出任何事实/伏笔/事件，等于没抽——拒绝，
    // 不给"清空数组骗过校验"留路（极短章节才允许全空）。
    if facts.is_empty()
        && threads.is_empty()
        && events.is_empty()
        && secrets.is_empty()
        && source.trim().chars().count() >= 500
    {
        return Err("抽取结果为空但原文有实质内容：必须至少给出一条事实、伏笔或事件".to_string());
    }
    // 与最终存储层规则一致：纠正重试应捕获短证据；空白容错后存回真实原文。
    for (kind, items) in [
        ("fact", &mut facts),
        ("thread", &mut threads),
        ("event", &mut events),
        ("secret", &mut secrets),
    ] {
        for item in items {
            let quote = canonical_evidence(source, item["evidence"].as_str().unwrap_or(""))
                .map_err(|e| format!("{} {}", kind, e))?;
            item["evidence"] = json!(quote);
        }
    }
    let mut out = json!({"summary": summary, "facts": facts, "threads": threads, "events": events});
    if secrets_present {
        out["secrets"] = json!(secrets);
    }
    Ok(out)
}

/// 独立复核器：验证抽取结果的语义与覆盖（原文 substring 只证明"引用了原文"，
/// 不证明"值推理正确"）。返回 {passed: bool, errors: [...]}。
async fn verify_chapter_memory(
    st: &Arc<AppState>,
    book_id: &str,
    ch: i64,
    body: &str,
    payload: &Value,
) -> Result<Value, String> {
    let db = &st.db;
    let Some(chn) = molan_llm::resolve_agent_channel(db, "review")
        .or_else(|| molan_llm::resolve_agent_channel(db, "summary"))
    else {
        return Err("复核阶段无可用渠道".to_string());
    };
    let mut sys = "你是小说连续性复核员。核对「记忆抽取」是否忠于原文，并与已有正史一致：\
只输出一个 JSON 对象：{\"passed\":true 或 false,\"errors\":[\"问题\"]}。\
逐条检查：① 每条 fact 的 value 是否可由 evidence 原文推出（不得夸大、不得推测未写明的因果）；\
② entity/field 是否与原文一致（不得张冠李戴）；③ 是否遗漏了本章明确的关键状态变化（覆盖度）；\
④ thread 状态是否与原文相符（计划要回收≠已回收）；\
⑤ 与【已有正史】是否冲突：人物状态/位置/身份/能力/已回收伏笔若与正史矛盾，即使本章原文自洽也必须报错。\
注意：正史只包含本章之前的已定稿记忆，不得用后续章节反推本章。有任何一条不成立就 passed=false 并列出 errors。".to_string();
    if let Ok(extra) = molan_core::deepwrite::strict_agent_instructions(db, book_id, "continuity") {
        if !extra.is_empty() {
            sys.push_str("\n\n");
            sys.push_str(&extra);
        }
    }
    // 截止本章之前的有效正史（chapter_context 只纳入 < ch 的记忆，绝不含未来章）
    let (prior_canon, prior_complete, baseline_diag) =
        match molan_core::continuity::chapter_context(db, book_id, ch) {
            Ok(ctx) => {
                let complete = ctx["complete"].as_bool().unwrap_or(false);
                let text = ctx["text"].as_str().unwrap_or("").to_string();
                let diag = format!(
                    "缺{}章/过期{}章/隐藏{}章/预算省略{}条",
                    ctx["missingCount"].as_i64().unwrap_or(-1),
                    ctx["stale"].as_array().map(|a| a.len()).unwrap_or(0),
                    ctx["hidden"].as_array().map(|a| a.len()).unwrap_or(0),
                    ctx["omittedEntries"].as_i64().unwrap_or(-1),
                );
                let shown = if complete {
                    text
                } else {
                    format!(
                        "{}\n（注意：前序记忆不完整/存在过期记忆，本章记忆须标记为待复核）",
                        text
                    )
                };
                (shown, complete, diag)
            }
            Err(e) => (
                "（无可用前序正史）".to_string(),
                false,
                format!("前序正史读取失败：{}", e),
            ),
        };
    let user = format!(
        "【第{}章原文】\n{}\n\n【已有正史（截至第{}章，不含未来章）】\n{}\n\n【待核对记忆】\n{}",
        ch,
        memory_body_with_note(body),
        ch - 1,
        prior_canon,
        payload
    );
    let max_tokens: i64 = molan_llm::get_setting(db, "max_tokens")
        .parse()
        .unwrap_or(8192);
    let params = ChatParams {
        base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: chn["key"].as_str().unwrap_or("").to_string(),
        model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
        messages: vec![
            json!({"role": "system", "content": sys}),
            json!({"role": "user", "content": user}),
        ],
        temperature: 0.0,
        max_tokens: if max_tokens == 0 { 8192 } else { max_tokens },
        stream: false,
        no_thinking: true,
        reasoning_effort: String::new(),
        log_tag: "chapter_memory_verify".to_string(),
        log_book: book_id.to_string(),
        cancel: auto_cancel_token(),
    };
    let (raw, usage) = molan_llm::chat_once_retry_logged_n(params, 2)
        .await
        .map_err(|e| format!("记忆复核调用失败：{}", e))?;
    log_llm_usage(
        db,
        book_id,
        "chapter_memory_verify",
        chn["model"].as_str().unwrap_or(""),
        &usage,
    );
    let j = extract_json_loose(&raw).ok_or_else(|| "记忆复核返回无法解析为 JSON".to_string())?;
    let passed = j["passed"]
        .as_bool()
        .ok_or_else(|| "记忆复核缺少布尔 passed".to_string())?;
    let errors: Vec<String> = j["errors"]
        .as_array()
        .ok_or_else(|| "记忆复核缺少 errors 数组".to_string())?
        .iter()
        .map(|x| x.as_str().unwrap_or("").to_string())
        .collect();
    if passed && errors.is_empty() {
        // 前序记忆不完整（缺章/过期/隐藏）时，抽取可继续，但必须标记需人工复核——
        // 同源自洽不等于跨章一致，不能假装已与全书正史对齐。
        let review_required = !prior_complete;
        Ok(
            json!({"passed": true, "errors": [], "reviewRequired": review_required, "baseline": baseline_diag}),
        )
    } else {
        Err(format!("记忆复核未通过：{}", errors.join("；")))
    }
}

/// 用固定 schema 从「本章全文」抽取结构化记忆候选；未知留空，不猜不补。
async fn extract_chapter_memory(
    st: &Arc<AppState>,
    book_id: &str,
    ch: i64,
    name: &str,
    body: &str,
    hint: &str,
) -> anyhow::Result<Value> {
    let db = &st.db;
    let chn = molan_llm::resolve_agent_channel(db, "review")
        .or_else(|| molan_llm::resolve_agent_channel(db, "summary"))
        .ok_or_else(|| anyhow!("无可用模型渠道，无法抽取章节记忆"))?;
    let mut sys = "你是小说连续性记录员。只依据给定章节原文抽取事实，禁止推测、禁止补设定。\
只输出一个 JSON 对象，不要解释、不要代码块标记。字段固定为：\
{\"summary\":\"本章发生了什么，≤300字\",\
\"facts\":[{\"entity\":\"人物/地点/物品名\",\"field\":\"状态字段如位置/伤势/身份\",\"value\":\"新值\",\"evidence\":\"原文中逐字出现的证据句\"}],\
\"threads\":[{\"id\":\"伏笔或承诺的简短标识\",\"state\":\"planted|advanced|partial|resolved|cancelled\",\"evidence\":\"原文证据句\"}],\
\"events\":[{\"description\":\"本章关键事件\",\"evidence\":\"原文证据句\"}],\
\"secrets\":[{\"id\":\"秘密简短标识(可省略)\",\"fact\":\"秘密内容\",\"known_by\":[\"知情角色\"],\"unknown_to\":[\"不知情角色\"],\"evidence\":\"原文证据句\"}]}\
规则：evidence 必须是从原文中【连续逐字复制、至少6个字】的片段（不得改写、不得概括、不得用省略号拼接多处原文）；原文没写的一律留空数组，不要臆测。示例：若原文是「他从怀里数出五十文，刚才那是吓唬人的」，则 evidence 应写「他从怀里数出五十文」，而不是「陆沉舟用五十文封口」（这是概括，会被拒绝）。event 的 evidence 同样必须是原文句子，不能把 description 复述一遍当作证据。secrets 只记本章原文明确揭示的信息差（谁在场亲见/谁被告知/谁仍被蒙在鼓里），没有就留空数组，不得推测。".to_string();
    if let Ok(extra) = molan_core::deepwrite::strict_agent_instructions(db, book_id, "continuity") {
        if !extra.is_empty() {
            sys.push_str("\n\n");
            sys.push_str(&extra);
        }
    }
    let mut user = format!("【第{}章 {}】\n{}", ch, name, memory_body_with_note(body));
    if !hint.is_empty() {
        user.push_str(&format!("\n\n【上次校验反馈】{}\n请重新抽取完整 JSON。每条 evidence 必须从原文连续逐字复制，不得拼接、改写或概括。修正错误的同时保留本章关键状态变化与事件，不得通过省略关键内容规避校验。", hint));
    }
    let max_tokens: i64 = molan_llm::get_setting(db, "max_tokens")
        .parse()
        .unwrap_or(8192);
    let params = ChatParams {
        base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: chn["key"].as_str().unwrap_or("").to_string(),
        model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
        messages: vec![
            json!({"role": "system", "content": sys}),
            json!({"role": "user", "content": user}),
        ],
        temperature: 0.1,
        max_tokens: if max_tokens == 0 { 8192 } else { max_tokens },
        stream: false,
        no_thinking: true,
        reasoning_effort: String::new(),
        log_tag: "chapter_memory_extract".to_string(),
        log_book: book_id.to_string(),
        cancel: auto_cancel_token(),
    };
    let (raw, usage) = molan_llm::chat_once_retry_logged_n(params, 3).await?;
    log_llm_usage(
        db,
        book_id,
        "chapter_memory_extract",
        chn["model"].as_str().unwrap_or(""),
        &usage,
    );
    extract_json_loose(&raw).ok_or_else(|| anyhow!("记忆抽取返回无法解析为 JSON"))
}

/// 章后正式记忆入口：由人工审批成功（F）或全自动定稿按「实际批准章」调用。
/// - 只处理传入的 book/ch，绝不用「全书最大章号」替代；
/// - 幂等：同一章同一 hash 重复调用不重复生效（continuity 内部判重）；
/// - 不改写人物表 / 伏笔台账等作者手工资产；失败会写 memory_job 错误状态，不静默成功。
pub(crate) async fn post_approved_chapter(
    st: &Arc<AppState>,
    book_id: &str,
    ch: i64,
) -> anyhow::Result<()> {
    let db = &st.db;
    if book_id.is_empty() || ch <= 0 {
        return Err(anyhow!("post_approved_chapter 需要有效的 bookId 与 ch"));
    }
    let name = resolve_approved_name(db, book_id, ch)
        .ok_or_else(|| anyhow!("第{}章尚无正式正文，不能写入正式记忆", ch))?;
    // aiOff：在把全文送出去之前就拒绝，避免"先发全文再被父拒存"的越权外发
    if files::file_flag(db, book_id, "正文", &name, "aiOff") {
        let e = anyhow!("第{}章已标记 aiOff，禁止自动记忆抽取", ch);
        let _ = molan_core::continuity::mark_memory_error(db, book_id, ch, &format!("{}", e));
        return Err(e);
    }
    let body = files::read_file(db, book_id, "正文", &name).unwrap_or_default();
    if body.trim().is_empty() {
        return Err(anyhow!("第{}章正文为空，不能写入正式记忆", ch));
    }
    // 依赖未变：待审连写产物若上章草稿已变化，要求重新审核后再接受
    if let Err(e) = molan_core::continuity::check_draft_dependency(db, book_id, ch) {
        let _ = molan_core::continuity::mark_memory_error(db, book_id, ch, &format!("{}", e));
        return Err(e);
    }
    let hash = molan_core::continuity::content_hash(&body);
    // 抽取前锁定输入指纹：若抽取期间设定/前文/技能被并发修改，CAS 提交会失败，绝不覆盖 stale 记忆
    let expected_inputs = match molan_core::continuity::input_fingerprint(db, book_id, ch + 1) {
        Ok(v) => v,
        Err(e) => {
            let msg = anyhow!("第{}章记忆输入指纹不可用：{}", ch, e);
            let _ = molan_core::continuity::mark_memory_error(db, book_id, ch, &format!("{}", msg));
            return Err(msg);
        }
    };
    // 已有同 hash 有效记忆 → 幂等返回，不重复抽取、不重复计费
    if let Ok(v) = molan_core::continuity::memory_status(db, book_id) {
        let dup = v["memories"].as_array().into_iter().flatten().any(|m| {
            m["ch"].as_i64() == Some(ch)
                && m["sourceHash"].as_str() == Some(hash.as_str())
                && m["status"].as_str() == Some("valid")
        });
        if dup {
            auto_log(
                ch,
                "memory",
                format!(
                    "第{}章 正式记忆已是最新（hash {}）",
                    ch,
                    &hash[..8.min(hash.len())]
                ),
            );
            return Ok(());
        }
    }
    // 抽取 → 严格校验 → 独立语义复核 的纠正闭环：任一环节给出具体拒绝原因，
    // 就把原因作为反馈重新抽取（最多 3 轮：首次 + 2 次纠正）。仍失败则如实标记，绝不放行。
    let mut hint = String::new();
    let mut checked: Option<Value> = None;
    let mut last_err = String::new();
    for attempt in 0..3 {
        let payload = match extract_chapter_memory(st, book_id, ch, &name, &body, &hint).await {
            Ok(p) => p,
            Err(e) => {
                let _ =
                    molan_core::continuity::mark_memory_error(db, book_id, ch, &format!("{}", e));
                return Err(e);
            }
        };
        // 严格校验：任何字段缺失/证据非原文子串都判失败，绝不静默剔除后当 valid
        let candidate = match sanitize_memory_payload(&body, &payload) {
            Ok(v) => v,
            Err(e) => {
                last_err = format!("记忆校验失败：{}", e);
                hint = e;
                auto_log(
                    ch,
                    "memory",
                    format!("第{}章 第{}轮校验未通过：{}", ch, attempt + 1, last_err),
                );
                continue;
            }
        };
        // 独立语义复核：substring 只证明引用原文，不证明推理正确；
        // 同时对照截止本章的已有正史，避免"本章自洽但与前文冲突"被当有效记忆。
        match verify_chapter_memory(st, book_id, ch, &body, &candidate).await {
            Ok(v) => {
                if v["reviewRequired"].as_bool().unwrap_or(false) {
                    // 前序记忆不完整：不得写成 valid 记忆（来源稿保留，仅记忆失败，可重建）
                    let msg = anyhow!(
                        "第{}章前序记忆不完整/存在过期记忆（{}），本章记忆未获可信基线，暂不写入 valid",
                        ch,
                        v["baseline"].as_str().unwrap_or("基线详情不可用")
                    );
                    let _ = molan_core::continuity::mark_memory_error(
                        db,
                        book_id,
                        ch,
                        &format!("{}", msg),
                    );
                    auto_log(
                        ch,
                        "memory",
                        format!(
                            "第{}章 前序记忆不完整 → 记忆标记失败待重建（来源稿保留）",
                            ch
                        ),
                    );
                    return Err(msg);
                }
                checked = Some(candidate);
                break;
            }
            Err(e) => {
                last_err = format!("记忆语义复核未通过：{}", e);
                hint = e;
                auto_log(
                    ch,
                    "memory",
                    format!("第{}章 第{}轮语义复核未通过：{}", ch, attempt + 1, last_err),
                );
                continue;
            }
        }
    }
    let Some(checked) = checked else {
        let msg = anyhow!("第{}章{}（经3轮抽取纠正仍失败）", ch, last_err);
        let _ = molan_core::continuity::mark_memory_error(db, book_id, ch, &format!("{}", msg));
        return Err(msg);
    };
    if let Err(e) = molan_core::continuity::record_chapter_memory_cas(
        db,
        book_id,
        ch,
        &name,
        &hash,
        &expected_inputs,
        &checked,
    ) {
        let _ = molan_core::continuity::mark_memory_error(db, book_id, ch, &format!("{}", e));
        return Err(e);
    }
    auto_log(
        ch,
        "memory",
        format!(
            "第{}章 正式记忆已写入（hash {}，事实{}条/伏笔{}条/事件{}条）",
            ch,
            &hash[..8.min(hash.len())],
            checked["facts"].as_array().map(|a| a.len()).unwrap_or(0),
            checked["threads"].as_array().map(|a| a.len()).unwrap_or(0),
            checked["events"].as_array().map(|a| a.len()).unwrap_or(0),
        ),
    );
    // 说明：这里不再调用旧 refresh_rolling_summary（它按"最大正文章"取料且可能覆盖作者文件）。
    // 前情摘要改为由新记忆上下文派生，且失败不影响已提交的正式记忆。
    Ok(())
}

// ---------- dispatch 分支体（原样搬入；闭包 a/s 按原语义展开为局部辅助闭包） ----------

/// auto_write_start：起一轮自动写作（原 dispatch 分支体原样搬入）。
pub(crate) async fn auto_write_start(
    st: &Arc<AppState>,
    _cmd: &str,
    args: &Value,
    _tx: &tokio::sync::mpsc::Sender<String>,
) -> anyhow::Result<Option<Value>> {
    let db = &st.db;
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();
    let book_id = s("bookId");
    // 入口校验：书必须真实存在，否则整条流水线会在幽灵目录上跑完并留下脏数据
    if book_id.is_empty() {
        return Err(anyhow!("缺少 bookId"));
    }
    let book_exists = db
        .q_json(
            "SELECT 1 FROM books WHERE id=?1 LIMIT 1",
            &[&book_id as &dyn rusqlite::ToSql],
        )
        .ok()
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if !book_exists {
        return Err(anyhow!("书不存在：{}", book_id));
    }
    let session_id = s("sessionId");
    let from = a("fromCh").as_i64().unwrap_or(0);
    let to = a("toCh").as_i64().unwrap_or(0);
    if to <= 0 {
        return Err(anyhow!("缺少 toCh（写到第几章）"));
    }
    // 先归一化范围，再按「区间长度」限制单次上限（F09：不再用绝对章号 to.min(200) 造成倒置）
    let from_norm = if from > 0 {
        from
    } else {
        latest_chapter_num(db, &book_id) + 1
    };
    let to_norm = normalize_chapter_range(from_norm, to);
    if to_norm < from_norm {
        return Err(anyhow!("章节范围无效：第{}~{}章", from_norm, to_norm));
    }
    // 超长区间必须显式拒绝，不能静默截断成与实际授权不同的范围
    if to - from_norm + 1 > MAX_CHAPTERS_PER_TASK {
        return Err(anyhow!(
            "单次最多 {} 章，本次请求第{}~{}章共{}章；请拆分后重试（服务端不会静默截断）",
            MAX_CHAPTERS_PER_TASK,
            from_norm,
            to,
            to - from_norm + 1
        ));
    }
    // 全自动模式：V1 契约——仅当本次请求明确 fullAuto=true 才全自动；省略即 false。
    // 绝不用本书历史 flag 兜底，否则某次 true 会让以后所有任务静默定稿。
    let full_auto_snapshot = a("fullAuto").as_bool().unwrap_or(false);
    if a("confirmed").as_bool() != Some(true) {
        return Err(anyhow!("请先确认本次写作范围和待审模式"));
    }
    if full_auto_snapshot && a("confirmAuto").as_bool() != Some(true) {
        return Err(anyhow!("全自动定稿需要再次明确确认"));
    }
    if !full_auto_snapshot && to_norm != from_norm {
        return Err(anyhow!(
            "逐章待审每次只允许生成一章；下一章请由作者再次确认"
        ));
    }
    if session_id.is_empty() {
        return Err(anyhow!("缺少有效会话，已停止写作以免记录丢失"));
    }
    let session_rows = db.q_json(
        "SELECT book_id FROM sessions WHERE id=?1",
        &[&session_id as &dyn rusqlite::ToSql],
    )?;
    if session_rows.first().and_then(|row| row["bookId"].as_str()) != Some(book_id.as_str()) {
        return Err(anyhow!("会话不存在或不属于本书，已停止写作"));
    }
    ensure_task_schema(db);
    {
        // 检查与占位必须在同一把锁内完成，否则并发两个请求都能通过检查（TOCTOU 双开）
        let mut g = AUTO_RUN.lock().unwrap();
        if g.as_ref()
            .map(|v| v["running"].as_bool().unwrap_or(false))
            .unwrap_or(false)
        {
            return Err(anyhow!("已有自动写作任务在运行，可先 auto_write_stop"));
        }
        *g = Some(json!({"running": true, "pending": true, "bookId": book_id}));
    }
    // 全自动授权仅存于 auto_task.full_auto 任务快照；不再写书级 settings 死开关，避免将来被误读成默认全自动。
    let from = from_norm;
    let to = to_norm;
    // 任务落盘：进度可查、重启可续跑
    let now = stats::now_ms();
    let start_ch = from;
    auto_log_clear(&book_id);
    auto_log(
        start_ch,
        "start",
        format!(
            "开始：第{}~{}章 · {}",
            start_ch,
            to,
            if full_auto_snapshot {
                "全自动（审核 → 去AI味 → 写入正文）"
            } else {
                "待审模式"
            }
        ),
    );
    // 落盘失败必须把前面的占位撤回，否则任务永远卡在 running=true
    if let Err(e) = db.exec(
                "INSERT INTO auto_task(book_id,session_id,from_ch,to_ch,current_ch,created_at,updated_at,full_auto) VALUES(?,?,?,?,?,?,?,?)",
                &[
                    &book_id as &dyn rusqlite::ToSql,
                    &session_id,
                    &from,
                    &to,
                    &(start_ch - 1),
                    &now,
                    &now,
                    &(if full_auto_snapshot { 1i64 } else { 0i64 }) as &dyn rusqlite::ToSql,
                ],
            ) {
                if let Ok(mut g) = AUTO_RUN.lock() {
                    *g = None;
                }
                return Err(e);
            }
    let task_id = db
        .q_json(
            "SELECT id FROM auto_task WHERE book_id=?1 ORDER BY id DESC LIMIT 1",
            &[&book_id as &dyn rusqlite::ToSql],
        )
        .ok()
        .and_then(|v| v.first().and_then(|r| r["id"].as_i64()))
        .unwrap_or(0);
    {
        let mut g = AUTO_RUN.lock().unwrap();
        *g = Some(json!({
            "bookId": book_id, "sessionId": session_id, "taskId": task_id,
            "from": from, "to": to, "current": 0,
            "running": true, "error": "",
            // 冻结任务模式：运行期间改设置不影响本任务
            "fullAuto": full_auto_snapshot,
        }));
    }
    AUTO_STOP.store(false, Ordering::SeqCst);
    auto_cancel_arm();
    let scope_book = book_id.clone();
    set_task_scope(task_id, &scope_book);
    let st2 = Arc::clone(st);
    tokio::spawn(async move {
        let res = auto_write_loop(st2, task_id, book_id, session_id, from, to).await;
        if let Err(e) = res {
            tracing::error!("自动写作任务失败：{}", e);
        }
        // 任务自然结束（成功/失败/停止）：清空取消令牌，再清空内存态
        auto_cancel_clear();
        clear_task_scope();
        // 清空内存态（任务结果已在 auto_task 表落盘，状态查询走 db 回退）
        if let Ok(mut g) = AUTO_RUN.lock() {
            *g = None;
        }
    });
    Ok(Some(json!({"ok": true, "taskId": task_id})))
}

/// auto_write_resume：从上次断点续跑（原 dispatch 分支体原样搬入）。
pub(crate) async fn auto_write_resume(
    st: &Arc<AppState>,
    _cmd: &str,
    args: &Value,
    _tx: &tokio::sync::mpsc::Sender<String>,
) -> anyhow::Result<Option<Value>> {
    let db = &st.db;
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();
    let book_id = s("bookId");
    let task = latest_auto_task(db, &book_id).ok_or_else(|| anyhow!("该书没有历史自动写作任务"))?;
    if task["sessionId"].as_str().unwrap_or("").is_empty() {
        return Err(anyhow!("旧任务没有关联会话，请手动启动新任务"));
    }
    if a("confirmed").as_bool() != Some(true) {
        return Err(anyhow!("续跑前请先确认原任务范围与模式"));
    }
    if task["fullAuto"].as_i64() == Some(1) && a("confirmAuto").as_bool() != Some(true) {
        return Err(anyhow!("续跑全自动定稿须再次明确确认，不能继承旧授权"));
    }
    {
        // 检查与占位同一把锁：避免与 auto_write_start 并发双开
        let mut g = AUTO_RUN.lock().unwrap();
        if g.as_ref()
            .map(|v| v["running"].as_bool().unwrap_or(false))
            .unwrap_or(false)
        {
            return Err(anyhow!("已有自动写作任务在运行，可先 auto_write_stop"));
        }
        *g = Some(json!({"running": true, "pending": true, "bookId": book_id}));
    }
    // F09：范围按区间长度归一，不用绝对章号截断；resume 只补真正缺的章
    ensure_task_schema(db);
    // 模式沿原任务快照（latest_auto_task 已 SELECT full_auto）：老任务无该列默认待审，绝不回读本书 flag
    let resume_full_auto = task["fullAuto"].as_i64().map(|x| x == 1).unwrap_or(false);
    let resume_from = task["currentCh"].as_i64().unwrap_or(0) + 1;
    let resume_to = task["toCh"].as_i64().unwrap_or(0);
    // resume 不能只从 current+1 起：必须扫描原任务范围内真正的缺章（正文与待审都空），
    // 否则"第1章缺、第2章成功"这类空洞会被跳过并再次假完成。
    let task_from = task["fromCh"].as_i64().unwrap_or(0);
    let holes = if resume_to >= task_from && task_from > 0 {
        collect_missing_chapters(db, &book_id, task_from, resume_to)
    } else {
        Vec::new()
    };
    let session_id = task["sessionId"].as_str().unwrap_or("").to_string();
    // 关键：真正的续跑范围由「缺章集合」驱动，而不是 current+1。
    // 主循环会自动跳过已有正文/待审稿的章，因此用缺章的首尾区间即可精确补洞。
    let (from, to) = resume_range(resume_from, resume_to, &holes);
    if !resume_full_auto && from < to {
        if let Ok(mut g) = AUTO_RUN.lock() {
            *g = None;
        }
        return Err(anyhow!("旧待审任务包含多个未写章节；请逐章重新确认并启动"));
    }
    if !holes.is_empty() {
        let list: Vec<String> = holes.iter().map(|x| x.to_string()).collect();
        save_auto_message(
            db,
            &session_id,
            "user",
            &format!(
                "【自动写作】续跑检测到缺章：第{}章，将按缺章补写",
                list.join("、")
            ),
        );
    }
    if from > to {
        if let Ok(mut g) = AUTO_RUN.lock() {
            *g = None;
        }
        // 无缺章且断点已到末尾：如实说明，不再谎报"已完成"
        return Ok(Some(json!({
            "ok": false,
            "message": "没有待补章节（范围内正文与待审均已存在）",
            "from": from,
            "to": to,
        })));
    }
    auto_log_clear(&book_id);
    auto_log(from, "start", format!("续跑：第{}~{}章", from, to));
    let now = stats::now_ms();
    // 落盘失败必须撤回占位，否则永远卡在 running=true
    if let Err(e) = db.exec(
                "INSERT INTO auto_task(book_id,session_id,from_ch,to_ch,current_ch,created_at,updated_at,full_auto) VALUES(?,?,?,?,?,?,?,?)",
                &[
                    &book_id as &dyn rusqlite::ToSql,
                    &session_id,
                    &from,
                    &to,
                    &(from - 1),
                    &now,
                    &now,
                    &(if resume_full_auto { 1i64 } else { 0i64 }) as &dyn rusqlite::ToSql,
                ],
            ) {
                if let Ok(mut g) = AUTO_RUN.lock() {
                    *g = None;
                }
                return Err(e);
            }
    let task_id = db
        .q_json(
            "SELECT id FROM auto_task WHERE book_id=?1 ORDER BY id DESC LIMIT 1",
            &[&book_id as &dyn rusqlite::ToSql],
        )
        .ok()
        .and_then(|v| v.first().and_then(|r| r["id"].as_i64()))
        .unwrap_or(0);
    {
        let mut g = AUTO_RUN.lock().unwrap();
        *g = Some(json!({
            "bookId": book_id, "sessionId": session_id, "taskId": task_id,
            "from": from, "to": to, "current": from - 1,
            "running": true, "error": "", "resumed": true,
            // 沿原任务冻结模式，不受本书当前 flag 影响
            "fullAuto": resume_full_auto,
        }));
    }
    AUTO_STOP.store(false, Ordering::SeqCst);
    auto_cancel_arm();
    let scope_book = book_id.clone();
    set_task_scope(task_id, &scope_book);
    let st2 = Arc::clone(st);
    tokio::spawn(async move {
        let res = auto_write_loop(st2, task_id, book_id, session_id, from, to).await;
        if let Err(e) = res {
            tracing::error!("自动写作任务失败：{}", e);
        }
        // 任务自然结束（成功/失败/停止）：清空取消令牌，再清空内存态
        auto_cancel_clear();
        clear_task_scope();
        // 清空内存态（任务结果已在 auto_task 表落盘，状态查询走 db 回退）
        if let Ok(mut g) = AUTO_RUN.lock() {
            *g = None;
        }
    });
    Ok(Some(
        json!({"ok": true, "taskId": task_id, "from": from, "to": to}),
    ))
}

/// auto_write_last_task：最近一条任务（原 dispatch 分支体原样搬入）。
pub(crate) async fn auto_write_last_task(
    st: &Arc<AppState>,
    _cmd: &str,
    args: &Value,
    _tx: &tokio::sync::mpsc::Sender<String>,
) -> anyhow::Result<Option<Value>> {
    let db = &st.db;
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();
    let book_id = s("bookId");
    Ok(Some(
        latest_auto_task(db, &book_id).unwrap_or(json!({"running": false})),
    ))
}

/// auto_write_status：任务状态/日志（原 dispatch 分支体原样搬入）。
pub(crate) async fn auto_write_status(
    st: &Arc<AppState>,
    _cmd: &str,
    args: &Value,
    _tx: &tokio::sync::mpsc::Sender<String>,
) -> anyhow::Result<Option<Value>> {
    let db = &st.db;
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();
    // 内存态优先（运行中）；空闲时回退查持久化任务（重启后仍能看到上次进度/错误）
    {
        let g = AUTO_RUN.lock().unwrap();
        if let Some(v) = g.as_ref() {
            if v["running"].as_bool().unwrap_or(false) {
                let mut vv = v.clone();
                let bid = vv["bookId"].as_str().unwrap_or("").to_string();
                // 运行中：以任务快照为准，不读本书 flag（避免 UI 显示与实际策略不一致）
                let fa = vv["fullAuto"].as_bool().unwrap_or(false);
                if let Some(o) = vv.as_object_mut() {
                    o.insert("fullAuto".to_string(), json!(fa));
                    o.insert("logs".to_string(), json!(auto_logs_for(&bid)));
                }
                return Ok(Some(vv));
            }
        }
    }
    let book_id = s("bookId");
    if !book_id.is_empty() {
        if let Some(t) = latest_auto_task(db, &book_id) {
            // 用任务快照的模式（无 snapshot 的老任务默认 false），不读本书 flag
            let fa = t["fullAuto"].as_i64().map(|x| x == 1).unwrap_or(false);
            // 以磁盘成果复核 DB 状态：DB 说 done 但仍有缺章时必须降级为 partial，
            // 绝不把缺章任务展示成完成。
            let from = t["fromCh"].as_i64().unwrap_or(0);
            let to = t["toCh"].as_i64().unwrap_or(0);
            let db_status = t["status"].as_str().unwrap_or("").to_string();
            let holes = if to >= from && from > 0 {
                collect_missing_chapters(db, &book_id, from, to)
            } else {
                Vec::new()
            };
            let status = if db_status == "done" && !holes.is_empty() {
                "partial".to_string()
            } else {
                db_status
            };
            return Ok(Some(json!({
                "bookId": book_id,
                "taskId": t["id"],
                "from": t["fromCh"], "to": t["toCh"], "current": t["currentCh"],
                "running": false,
                "status": status,
                "dbStatus": t["status"],
                "holes": holes,
                "error": t["error"],
                "fullAuto": fa,
                "resumable": status != "done",
                "logs": auto_logs_for(&book_id),
            })));
        }
    }
    let g = AUTO_RUN.lock().unwrap();
    let mut v = g.clone().unwrap_or(json!({"running": false}));
    if let Some(o) = v.as_object_mut() {
        let bid = o
            .get("bookId")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        o.insert("logs".to_string(), json!(auto_logs_for(&bid)));
    }
    Ok(Some(v))
}

/// auto_write_stop：请求停止（原 dispatch 分支体原样搬入）。
pub(crate) async fn auto_write_stop(
    st: &Arc<AppState>,
    _cmd: &str,
    args: &Value,
    _tx: &tokio::sync::mpsc::Sender<String>,
) -> anyhow::Result<Option<Value>> {
    let _ = st;
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let want_book = a("bookId").as_str().unwrap_or("").to_string();
    // 停止必须带书籍作用域；空 bookId 绝不能跨书停止别人的任务。
    if want_book.is_empty() {
        return Ok(Some(
            json!({"ok": false, "stopping": false, "reason": "必须指定任务所属书籍"}),
        ));
    }
    {
        let running_book = AUTO_RUN
            .lock()
            .ok()
            .and_then(|g| {
                g.as_ref().and_then(|v| {
                    if v["running"].as_bool().unwrap_or(false) {
                        v["bookId"].as_str().map(|x| x.to_string())
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_default();
        if running_book.is_empty() {
            return Ok(Some(
                json!({"ok": false, "stopping": false, "reason": "当前没有运行中的自动写作任务"}),
            ));
        }
        if running_book != want_book {
            return Ok(Some(
                json!({"ok": false, "stopping": false, "reason": "运行中的任务属于另一本书，未停止"}),
            ));
        }
    }
    AUTO_STOP.store(true, Ordering::SeqCst);
    // 立即掐断在飞的那次 LLM 调用（可能 60s+），不再等章边界；AUTO_STOP 原语义保留（章边界也停）
    if let Some(t) = auto_cancel_token() {
        t.cancel();
    }
    auto_log(0, "stop", "已请求停止，正在中断当前生成…");
    Ok(Some(json!({"ok": true, "stopping": true})))
}

/// 解析「自动去味」方法：显式 override > 本书设置 book_humanize__<bookId> > official:standard。
/// 返回 (方法标识, 方法提示词)；方法为 "none" 时提示词为空 = 关闭自动去味。
pub(crate) fn resolve_humanize(
    db: &molan_core::db::Db,
    book_id: &str,
    override_method: Option<&str>,
) -> (String, String) {
    let mut method = override_method.unwrap_or("").trim().to_string();
    if method.is_empty() && !book_id.is_empty() {
        method = molan_llm::get_setting(db, &format!("book_humanize__{}", book_id));
        if method == "null" {
            method.clear();
        }
    }
    if method.is_empty() {
        method = "official:standard".to_string();
    }
    if method == "none" {
        return (method, String::new());
    }
    let prompt = if let Some(sid) = method.strip_prefix("skill:") {
        db.q_json(
            "SELECT prompt_template FROM skills WHERE id=?1 OR builtin_key=?1",
            &[&sid as &dyn rusqlite::ToSql],
        )
        .ok()
        .and_then(|v| {
            v.first()
                .and_then(|r| r["promptTemplate"].as_str().map(|x| x.to_string()))
        })
        .unwrap_or_default()
    } else {
        let key = if method == "official:deep" {
            "method.humanize.deep"
        } else {
            "method.humanize.standard"
        };
        db.q_json(
            "SELECT prompt_template FROM skills WHERE builtin_key=?1",
            &[&key as &dyn rusqlite::ToSql],
        )
        .ok()
        .and_then(|v| {
            v.first()
                .and_then(|r| r["promptTemplate"].as_str().map(|x| x.to_string()))
        })
        .unwrap_or_default()
    };
    (method, prompt)
}

/// 生成正文后的自动审核去AI味：机器门评分 → 未过线则按所选方法修（最多 2 轮）。
/// 任何异常都退回原文，绝不阻断生成。返回 (最终正文, 审核报告)。
pub(crate) async fn auto_humanize(
    db: &molan_core::db::Db,
    book_id: &str,
    body: &str,
    override_method: Option<&str>,
    agent: &str,
) -> (String, Value) {
    let (method, method_prompt) = resolve_humanize(db, book_id, override_method);
    let mut report = molan_core::deai::score_text(body);
    if method == "none" {
        report["skipped"] = json!("自动去味已关闭");
        return (body.to_string(), report);
    }
    let mut cur = body.to_string();
    let mut round = 0i64;
    let mut shrink_retry = false;
    while round < 2 {
        let viols = report["violations"].as_array().cloned().unwrap_or_default();
        // v2.4：凡是不及格（>32）且有命中项即触发修复。原实现只认 tier1/2/5，
        // 导致只有 tier3/tier4（酒馆腔/模板句/明喻·副词·眼神密度等）命中时「报告超线却不去味」。
        let blocking = !viols.is_empty();
        if report["passed"].as_bool().unwrap_or(true) || !blocking {
            break;
        }
        let Some(chn) =
            molan_llm::resolve_agent_channel(db, agent).or_else(|| molan_llm::active_channel(db))
        else {
            break;
        };
        let max_tokens: i64 = molan_llm::get_setting(db, "max_tokens")
            .parse()
            .unwrap_or(8192);
        let max_tokens = if max_tokens == 0 { 8192 } else { max_tokens };
        // 结构类问题必须允许调句长/并段，否则修不掉；其余情况保持「不动结构」的保守红线
        let structural = report["violations"]
            .as_array()
            .map(|a| a.iter().any(|v| v["tier"].as_i64() == Some(5)))
            .unwrap_or(false);
        let mut sys = String::from(
            "你是网文成章质检引擎（去AI味整合门）。对给定正文做去AI味改写。\n\
             【保真红线】人物、事实、数字、时间、因果、信息点一项不能丢；只改「怎么说」不改「说什么」。\n",
        );
        if structural {
            sys.push_str(
                "【允许】为修复「短句主导 / 句长过短 / 段落过碎 / 段落过于均匀」：可以把碎段并成整段（但段落数不得少于原文的三分之二）、把短句并成长句；目标平均句长约 30 字、平均段长约 60 字，不许出现 80 字以上的超长句。\n\
                 【禁止】增删剧情、美化文采、修正错字与标点瑕疵（瑕疵=真人指纹）。\n",
            );
        } else {
            sys.push_str(
                "【禁止】重写未命中的段落、调整段落结构、增删剧情、美化文采、修正错字与标点瑕疵（瑕疵=真人指纹）。\n",
            );
        }
        sys.push_str("【输出】只输出改写后的正文全文（保留原有章节标题行），不加解释、不加报告、不加代码块。");
        if !method_prompt.is_empty() {
            sys.push_str("\n\n");
            sys.push_str(&method_prompt);
        }
        if let Ok(extra) = molan_core::deepwrite::agent_instructions(db, book_id, "editor") {
            if !extra.is_empty() {
                sys.push_str("\n\n");
                sys.push_str(&extra);
            }
        }
        if let Ok(skills) = molan_core::deepwrite::skill_instructions(db, book_id, &sys) {
            if !skills.is_empty() {
                sys.push_str("\n\n");
                sys.push_str(&skills);
            }
        }
        if shrink_retry {
            sys.push_str("\n【本次特别要求】上一版修复稿因删减过多被退回：本次必须完整保留全部信息与情节细节，只改表达，字数不得少于原文的 95%。");
        }
        let user = format!(
            "{}\n\n【初稿】\n{}",
            molan_core::deai::build_fix_instruction(&report),
            cur
        );
        let params = ChatParams {
            base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
            api_key: chn["key"].as_str().unwrap_or("").to_string(),
            model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
            messages: vec![
                json!({"role": "system", "content": sys}),
                json!({"role": "user", "content": user}),
            ],
            temperature: 0.6,
            max_tokens,
            stream: false,
            no_thinking: true,
            reasoning_effort: String::new(),
            log_tag: "auto_deai".to_string(),
            log_book: book_id.to_string(),
            cancel: auto_cancel_token(),
        };
        let (fixed, usage) = molan_llm::chat_once_retry_logged_n(params, 3)
            .await
            .unwrap_or_default();
        log_llm_usage(
            db,
            book_id,
            "auto_deai",
            chn["model"].as_str().unwrap_or(""),
            &usage,
        );
        let old_n = cur.chars().count();
        let new_n = fixed.trim().chars().count();
        if fixed.trim().is_empty() || new_n * 10 < old_n * 8 {
            // v2.4：长度异常不再直接放弃——先让它带着「不得删减内容」的要求重试一轮
            if !shrink_retry {
                shrink_retry = true;
                round += 1;
                continue;
            }
            tracing::warn!(
                "自动去味：修复稿长度异常（{}→{}字），保留原稿",
                old_n,
                new_n
            );
            break;
        }
        cur = fixed.trim().to_string();
        report = molan_core::deai::score_text(&cur);
        round += 1;
    }
    report["rounds"] = json!(round);
    report["method"] = json!(method);
    (cur, report)
}

pub(crate) fn genre_key(genre: &str) -> String {
    let map: &[(&str, &str)] = &[
        ("玄幻", "xuanhuan"),
        ("城市", "dushi"),
        ("都市", "dushi"),
        ("古言", "guyan"),
        ("古代", "guyan"),
        ("科幻", "kehuan"),
        ("末世", "moshi"),
        ("历史", "lishi"),
        ("快穿", "kuaichuan"),
        ("穿书", "kuaichuan"),
        ("年代", "niandai"),
        ("种田", "niandai"),
        ("悬疑", "xuanyi"),
        ("惊悚", "xuanyi"),
        ("言情", "yanqing"),
        ("游戏", "youxi"),
        ("无限流", "youxi"),
        ("高武", "gaowu"),
        ("大女主", "danvzhu"),
        ("女频", "nvshengcun"),
        ("男频", "tongyong"),
    ];
    for (re, k) in map {
        if genre.contains(re) {
            return k.to_string();
        }
    }
    "tongyong".into()
}

/// 服务端解析本书文风（前端不传 style，只传 skills）：
/// settings['book_style__<bookId>'] =
///   "style:<技能id>" → 该文风卡技能的 prompt_template
///   "distill"        → book_meta.style_json（蒸馏出的原作文风）
///   "off"            → 不注入
///   "auto" / 题材key → 题材卡 genre__<key>
/// 未设置时回落 book_meta.style_json（老数据兼容）。
pub(crate) fn resolve_book_style(
    db: &molan_core::db::Db,
    root: &std::path::Path,
    book_id: &str,
    genre: Option<&str>,
) -> Option<String> {
    if book_id.is_empty() {
        return None;
    }
    let distill_text = || {
        molan_core::books::get_book_style(db, book_id)
            .as_str()
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
    };
    let key = molan_llm::get_setting(db, &format!("book_style__{}", book_id));
    let key = key.trim().to_string();
    if key.is_empty() {
        return distill_text();
    }
    if key == "off" {
        return None;
    }
    if key == "distill" {
        return distill_text();
    }
    if let Some(sid) = key.strip_prefix("style:") {
        let t = db
            .q_json(
                "SELECT prompt_template FROM skills WHERE id=?1 OR builtin_key=?1",
                &[&sid as &dyn rusqlite::ToSql],
            )
            .ok()
            .and_then(|v| {
                v.first()
                    .and_then(|r| r["promptTemplate"].as_str().map(|x| x.to_string()))
            })
            .unwrap_or_default();
        if !t.trim().is_empty() {
            return Some(t);
        }
        return distill_text();
    }
    let k = if key == "auto" {
        genre_key(genre.unwrap_or(""))
    } else {
        key.clone()
    };
    let p = prompts(root);
    if let Some(t) = p[format!("genre__{}", k)].as_str() {
        if !t.trim().is_empty() {
            return Some(t.to_string());
        }
    }
    distill_text()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn review_verdict_is_fail_closed() {
        let pass = parse_review_verdict(&v(r#"{"ok":true,"issues":[]}"#)).unwrap();
        assert!(pass.ok && pass.hard.is_empty());
        // ok=false 且无 hard 问题：ok=false 必须保留（调用方据此转人工）
        let neg = parse_review_verdict(&v(r#"{"ok":false,"issues":[]}"#)).unwrap();
        assert!(!neg.ok && neg.hard.is_empty());
        // 缺 ok / 缺 issues / 类型不符 一律不可信
        assert!(parse_review_verdict(&v(r#"{"issues":[]}"#)).is_none());
        assert!(parse_review_verdict(&v(r#"{"ok":true}"#)).is_none());
        assert!(parse_review_verdict(&v(r#"{"ok":"true","issues":[]}"#)).is_none());
        assert!(parse_review_verdict(&v(r#"{"ok":true,"issues":""}"#)).is_none());
        assert!(parse_review_verdict(&v(r#"{"ok":true,"issues":[1]}"#)).is_none());
        assert!(parse_review_verdict(&v("{}")).is_none());
        // hard 问题保留，且软建议单独成列、不混入 hard
        let hard = parse_review_verdict(&v(
            r#"{"ok":false,"issues":["人物死亡冲突"],"warnings":["节奏略慢"]}"#,
        ))
        .unwrap();
        assert_eq!(hard.hard, vec!["人物死亡冲突".to_string()]);
        assert_eq!(hard.soft, vec!["节奏略慢".to_string()]);
    }

    #[test]
    fn range_cap_is_by_length_not_absolute() {
        // F09：250~300 只有 51 章，属于用户授权范围，必须原样保留（不得被绝对章号截成 250~200）
        assert_eq!(normalize_chapter_range(250, 300), 300);
        // 真正超过 200 章长度的区间才按长度截断：250~600 → 250~449
        assert_eq!(
            normalize_chapter_range(250, 600),
            250 + MAX_CHAPTERS_PER_TASK - 1
        );
        assert_eq!(normalize_chapter_range(1, 50), 50);
        assert_eq!(normalize_chapter_range(1, 200), 200);
        assert_eq!(normalize_chapter_range(1, 201), 200);
        // 倒置输入保持倒置，交由调用方报错
        assert_eq!(normalize_chapter_range(300, 250), 250);
    }

    #[test]
    fn evidence_is_canonicalized_and_short_quotes_are_rejected() {
        let source = "李三左臂中箭，\n退回城中。";
        assert_eq!(
            canonical_evidence(source, "左臂中箭，退回城中").unwrap(),
            "左臂中箭，\n退回城中"
        );
        assert!(canonical_evidence(source, "中箭").is_err());
        assert!(canonical_evidence(source, "").is_err());
        let p = json!({"summary":"李三退城", "facts":[], "threads":[], "events":[{"description":"退城", "evidence":"左臂中箭，退回城中"}]});
        let checked = sanitize_memory_payload(source, &p).unwrap();
        assert!(source.contains(checked["events"][0]["evidence"].as_str().unwrap()));
    }

    #[test]
    fn memory_body_budget_marks_truncation_honestly() {
        let short = "一".repeat(100);
        assert_eq!(memory_body_with_note(&short), short);
        let long = "字".repeat(MEMORY_BODY_BUDGET + 500);
        let out = memory_body_with_note(&long);
        assert!(out.contains(&format!("正文共{}字", MEMORY_BODY_BUDGET + 500)));
        assert!(out.contains("不得假装看过全文"));
        assert!(out.chars().count() < long.chars().count() + 100);
    }

    #[test]
    fn empty_payload_is_rejected_for_substantive_chapter() {
        // 原文有实质内容却交空壳 → 必须拒（防止清空数组骗过校验）；极短章允许空。
        let source = "李".repeat(600);
        let empty = v(r#"{"summary":"有摘要","facts":[],"threads":[],"events":[]}"#);
        assert!(sanitize_memory_payload(&source, &empty).is_err());
        let tiny = "李三左臂中箭，退回城中。";
        assert!(sanitize_memory_payload(tiny, &empty).is_ok());
    }

    #[test]
    fn evidence_matching_only_tolerates_whitespace() {
        let source = "李三左臂中箭，\n退回城中。";
        assert!(source_contains_evidence(source, "李三左臂中箭"));
        assert!(source_contains_evidence(source, "左臂中箭，退回城中"));
        assert!(!source_contains_evidence(source, "李三右臂中箭"));
        assert!(!source_contains_evidence(source, "李三受伤后回城"));
        assert!(!source_contains_evidence(source, ""));
        assert!(!source_contains_evidence(source, " \n\t"));
        assert!(!source_contains_evidence("", ""));
    }

    #[test]
    fn evidence_matching_tolerates_quote_style_and_markdown() {
        // 模型常见的纯排版替换：弯引号→直角引号、丢掉 markdown 强调符号——文字逐字相同，应通过并定位回原文
        let source = "裂纹还在“陆”字压着的那一角。地上三个字：**不走。**";
        assert!(source_contains_evidence(
            source,
            "裂纹还在「陆」字压着的那一角"
        ));
        assert!(source_contains_evidence(source, "地上三个字：不走。"));
        assert_eq!(
            canonical_evidence(source, "裂纹还在「陆」字压着的那一角").unwrap(),
            "裂纹还在“陆”字压着的那一角"
        );
        // 定位以最后一个内容字符收尾：开头的 markdown 符号包含在原文切片内，尾部 ** 在“。”之后，不属于引文
        assert_eq!(
            canonical_evidence(source, "地上三个字：不走。").unwrap(),
            "地上三个字：**不走。"
        );
        // 但文字内容不同仍是编造，必须拒绝
        assert!(!source_contains_evidence(
            source,
            "裂纹还在「谢」字压着的那一角"
        ));
    }

    #[test]
    fn memory_payload_rejects_bad_evidence_instead_of_dropping() {
        let source = "第1章 开头\n李三左臂中箭，退回城中。";
        let bad = v(
            r#"{"summary":"s","facts":[{"entity":"李三","field":"伤势","value":"右臂中箭","evidence":"李三右臂中箭"}],"threads":[],"events":[]}"#,
        );
        assert!(sanitize_memory_payload(source, &bad).is_err());
        let missing = v(
            r#"{"summary":"s","facts":[{"entity":"李三","field":"伤势","value":"中箭","evidence":""}],"threads":[],"events":[]}"#,
        );
        assert!(sanitize_memory_payload(source, &missing).is_err());
        let bad_thread = v(
            r#"{"summary":"s","facts":[],"threads":[{"id":"t1","state":"done","evidence":"李三左臂中箭"}],"events":[]}"#,
        );
        assert!(sanitize_memory_payload(source, &bad_thread).is_err());
        let good = v(
            r#"{"summary":"李三受伤退城","facts":[{"entity":"李三","field":"伤势","value":"左臂中箭","evidence":"李三左臂中箭"}],"threads":[{"id":"箭伤来源","state":"planted","evidence":"李三左臂中箭"}],"events":[{"description":"退回城中","evidence":"中箭，退回城中"}]}"#,
        );
        let ok = sanitize_memory_payload(source, &good).expect("应通过");
        assert_eq!(ok["facts"].as_array().unwrap().len(), 1);
        assert_eq!(ok["threads"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn resume_range_is_driven_by_holes() {
        // 缺章 [1]，断点 current=2 → 必须从 1 开始补，而不是 current+1=3
        assert_eq!(resume_range(3, 5, &[1]), (1, 1));
        // 多个缺章：首尾区间覆盖全部空洞
        assert_eq!(resume_range(4, 10, &[1, 3, 5]), (1, 5));
        // 无缺章：回落到断点范围
        assert_eq!(resume_range(3, 5, &[]), (3, 5));
        // 缺章跨越超长区间：仍按长度上限截断
        assert_eq!(
            resume_range(4, 10, &[1, 600]),
            (1, 1 + MAX_CHAPTERS_PER_TASK - 1)
        );
    }

    #[test]
    fn fingerprint_is_stable_and_content_bound() {
        let a = text_fingerprint("第一章正文");
        assert_eq!(a, text_fingerprint("第一章正文"));
        assert_ne!(a, text_fingerprint("第一章正文 "));
        assert_eq!(a.len(), 64);
    }
}
