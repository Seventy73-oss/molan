//! Paper Studio 契约 v2 的 IPC 命令（全部为加法；旧命令语义与返回结构不变）。
//!
//! - app_info：新前端启动握手（契约版本、任务目录、鉴权模式）；
//! - doc_read / doc_write / doc_history：DocumentWriteService（带 hash 的读取、五种写入、回执）；
//! - task_preview / skill_recommend：发起前的「实际生效计划」预览与确定性推荐（预览不是授权凭证）；
//! - artifact_list / artifact_get / artifact_revise / artifact_deliver：产物卡片的数据与交付；
//! - skill_draft / skill_drafts / skill_draft_revise / skill_draft_save / skill_draft_discard：技能工坊草稿产物。
use crate::AppState;
use anyhow::{anyhow, Result};
use molan_core::{artifact, artifact_view, doc_write, skill_resolver, task_kind};
use serde_json::{json, Value};
use std::sync::Arc;

pub const CONTRACT_VERSION: i64 = 2;

#[path = "skill_draft.rs"]
pub(crate) mod skill_draft;

fn s(args: &Value, k: &str) -> String {
    args.get(k)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn need_book(st: &AppState, args: &Value) -> Result<String> {
    let b = s(args, "bookId");
    if b.is_empty() || !molan_core::files::valid_book_id(&st.db, &b) {
        return Err(anyhow!("作品不存在或已删除"));
    }
    Ok(b)
}

/// 命中 v2 命令返回 Some(结果)；否则 None 交回旧分发。
pub async fn dispatch(
    st: &Arc<AppState>,
    cmd: &str,
    args: &Value,
    tx: &tokio::sync::mpsc::Sender<String>,
) -> Option<Result<Option<Value>>> {
    let out = match cmd {
        "app_info" => Ok(json!({
            "app": "molan", "ui": "paper-studio", "contract": CONTRACT_VERSION,
            "version": env!("CARGO_PKG_VERSION"), "authRequired": st.auth_required(),
            "tasks": task_kind::catalog(),
            "writeOps": ["create", "replace", "append", "insert", "replace_range"],
        })),
        "doc_read" => need_book(st, args)
            .and_then(|b| doc_write::read_doc(&st.db, &b, &s(args, "group"), &s(args, "name"))),
        "doc_write" => doc_write::WritePlan::from_args(args).and_then(|p| {
            // 服务端实测写入耗时（含 CAS、原子写、快照、索引、账本两阶段）
            let t = std::time::Instant::now();
            let mut r = doc_write::execute(&st.db, &p)?.to_json();
            r["durationMs"] = json!(t.elapsed().as_millis() as u64);
            Ok(r)
        }),
        "doc_history" => need_book(st, args).and_then(|b| {
            doc_write::history(
                &st.db,
                &b,
                &s(args, "group"),
                &s(args, "name"),
                args["limit"].as_i64().unwrap_or(30),
            )
        }),
        "task_preview" => task_preview(st, args),
        "skill_recommend" => need_book(st, args).and_then(|b| {
            let task = task_kind::parse_or_chat(&s(args, "task")).map_err(|e| anyhow!(e))?;
            Ok(skill_resolver::recommend(
                &st.db,
                &b,
                task,
                &book_genre(st, &b),
            ))
        }),
        "artifact_list" => need_book(st, args).and_then(|b| {
            let sid = s(args, "sessionId");
            if sid.is_empty() {
                artifact_view::list_for_book(&st.db, &b, args["limit"].as_i64().unwrap_or(100))
            } else {
                artifact_view::list_for_session(&st.db, &b, &sid)
            }
        }),
        "artifact_get" => need_book(st, args).and_then(|b| {
            let v = artifact_view::view(&st.db, &s(args, "artifactId"), true)?;
            if v["bookId"].as_str() != Some(b.as_str()) {
                return Err(anyhow!("产物不属于当前作品"));
            }
            Ok(v)
        }),
        "artifact_revise" => need_book(st, args).and_then(|b| {
            let id = s(args, "artifactId");
            let items = args.get("items").and_then(Value::as_array).cloned();
            artifact::revise(
                &st.db,
                &id,
                &b,
                args["baseRev"].as_i64().unwrap_or(0),
                &s_raw(args, "content"),
                items,
                &s(args, "note"),
            )?;
            artifact_view::view(&st.db, &id, true)
        }),
        "artifact_deliver" => deliver(st, args),
        "read_version" => need_book(st, args).and_then(|b| read_version(st, &b, args)),
        "skill_revisions" => {
            molan_core::skill_rev::revisions(&st.db, &s(args, "id")).map(|v| json!(v))
        }
        "book_skill_bindings" => need_book(st, args).map(|b| {
            let mut out = serde_json::Map::new();
            for t in task_kind::ALL {
                let (primary, supports) = skill_resolver::book_bindings(&st.db, &b, t);
                out.insert(
                    t.id().into(),
                    json!({"primary": primary, "supports": supports}),
                );
            }
            Value::Object(out)
        }),
        "channel_test" => return Some(channel_test(st, args).await.map(Some)),
        "review_pending" => return Some(review_pending(st, args).await.map(Some)),
        "skill_draft"
        | "skill_drafts"
        | "skill_draft_revise"
        | "skill_draft_save"
        | "skill_draft_discard" => {
            return Some(skill_draft::dispatch(st, cmd, args).await.map(Some))
        }
        _ => return run_cmd(st, cmd, args, tx).await,
    };
    Some(out.map(Some))
}

/// 作者在待审区手动「审稿当前版本」：按「审稿」子阶段计划审当前待审稿，结论绑定其 hash 记入审稿账本。
/// 不改稿、不定稿；没有待审稿或稿件过短时如实报错。
async fn review_pending(st: &Arc<AppState>, args: &Value) -> Result<Value> {
    let book = need_book(st, args)?;
    let ch = args["ch"]
        .as_i64()
        .filter(|c| *c > 0)
        .ok_or_else(|| anyhow!("缺少章号"))?;
    let name = st
        .db
        .q_json(
            "SELECT review_file FROM pending_chapter WHERE book_id=?1 AND ch=?2 AND status='pending'",
            &[&book as &dyn rusqlite::ToSql, &ch],
        )?
        .first()
        .and_then(|r| r["reviewFile"].as_str().map(str::to_string))
        .ok_or_else(|| anyhow!("第{}章没有待审稿", ch))?;
    let plan = super::stream::run_plan::review_stage(&st.db, &st.root, &book, args);
    let saved = [format!("{} / {}", molan_core::db::REVIEW_GROUP, name)];
    let (_, summary) = super::stream::chapter_review::review_pending_saved(
        &st.db,
        &book,
        Some(ch),
        &saved,
        None,
        &plan,
    )
    .await
    .ok_or_else(|| anyhow!("待审稿过短（少于 300 字）或读取失败，未审稿"))?;
    let hash = summary["bodyHash"].as_str().unwrap_or("").to_string();
    Ok(
        json!({"review": molan_core::review_log::latest(&st.db, &book, ch, &hash), "summary": summary}),
    )
}

/// 内容字段不 trim（保留首尾换行与空格）。
fn s_raw(args: &Value, k: &str) -> String {
    args.get(k)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn book_genre(st: &AppState, book: &str) -> String {
    st.db
        .q_json(
            "SELECT genre FROM books WHERE id=?1",
            &[&book as &dyn rusqlite::ToSql],
        )
        .ok()
        .and_then(|r| {
            r.first()
                .and_then(|x| x["genre"].as_str().map(str::to_string))
        })
        .unwrap_or_default()
}

fn task_preview(st: &Arc<AppState>, args: &Value) -> Result<Value> {
    let book = need_book(st, args)?;
    let task = task_kind::parse_or_chat(&s(args, "task")).map_err(|e| anyhow!(e))?;
    let sel = skill_resolver::Selection::from_args(args);
    let plan = super::stream::run_plan::build(&st.db, &st.root, &book, task, &sel)?;
    let ctx = super::stream::context_builder::preview(
        &st.db,
        &book,
        task,
        &args["target"],
        &args["contextFiles"],
    );
    Ok(json!({
        "task": task.id(), "taskLabel": task.label(), "role": task.role(),
        "plan": skill_resolver::public_view(&plan),
        "context": ctx,
        "recommend": skill_resolver::recommend(&st.db, &book, task, &book_genre(st, &book)),
        "note": "预览仅供确认；执行时服务端会按同一规则重新解析并冻结本次计划。",
    }))
}

fn deliver(st: &Arc<AppState>, args: &Value) -> Result<Value> {
    let book = need_book(st, args)?;
    let d = artifact::Deliver::from_args(args);
    if d.book_id != book {
        return Err(anyhow!("bookId 不一致"));
    }
    let t = std::time::Instant::now();
    let r = artifact::deliver(&st.db, &d)?;
    let ms = t.elapsed().as_millis() as u64; // 交付（写入 / 提交 / 定稿）的服务端耗时
                                             // 定稿成功：章后处理（记忆/人物/伏笔）异步串行执行；失败只标记、可重试，定稿保留。
    if d.action == "approve"
        && r["ok"] == json!(true)
        && r["result"]["alreadyApproved"] != json!(true)
    {
        if let Some(ch) = r["result"]["ch"].as_i64().filter(|c| *c > 0) {
            super::spawn_post_approved(Arc::clone(st), &book, vec![ch]);
        }
    }
    let view = artifact_view::view(&st.db, &d.artifact_id, false)?;
    Ok(json!({"delivery": r, "artifact": view, "ms": ms}))
}

/// 版本快照全文（versions/<book>/<group>/<name>/<ts>.json），供版本比较与恢复前预览。
fn read_version(st: &AppState, book: &str, args: &Value) -> Result<Value> {
    use molan_core::files::{normalize_group, safe_name};
    let ts = args["ts"].as_i64().ok_or_else(|| anyhow!("缺少 ts"))?;
    let p = st
        .db
        .versions_dir
        .join(safe_name(book))
        .join(normalize_group(&s(args, "group")))
        .join(safe_name(&s(args, "name")))
        .join(format!("{}.json", ts));
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(&p).map_err(|_| anyhow!("版本不存在"))?)?;
    Ok(json!({"ts": ts, "content": v["content"].as_str().unwrap_or("")}))
}

/// 渠道连通测试：使用服务端保存的密钥（不回传到浏览器），一次极小请求。
async fn channel_test(st: &Arc<AppState>, args: &Value) -> Result<Value> {
    let id = s(args, "id");
    let (channels, _) = molan_llm::all_settings(&st.db);
    let ch = channels
        .iter()
        .find(|c| c["id"].as_str() == Some(id.as_str()))
        .ok_or_else(|| anyhow!("渠道不存在：{}", id))?;
    let model = Some(s(args, "model"))
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| ch["model"].as_str().unwrap_or("").to_string());
    let params = molan_llm::ChatParams {
        base_url: ch["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: molan_llm::channel_key(&st.db, &id),
        model: model.clone(),
        messages: vec![json!({"role": "user", "content": "回复\"OK\"两个字母即可"})],
        max_tokens: 16,
        reasoning_effort: String::new(),
        ..Default::default()
    };
    let t0 = std::time::Instant::now();
    Ok(match molan_llm::chat_once(params).await {
        Ok(out) => {
            json!({"ok": true, "model": model, "output": out.chars().take(200).collect::<String>(), "totalMs": t0.elapsed().as_millis() as i64})
        }
        Err(e) => {
            json!({"ok": false, "model": model, "output": e.to_string().chars().take(300).collect::<String>(), "totalMs": t0.elapsed().as_millis() as i64})
        }
    })
}

/// 运行相关命令（Agent 运行状态等）；未命中返回 None。
async fn run_cmd(
    st: &Arc<AppState>,
    cmd: &str,
    args: &Value,
    _tx: &tokio::sync::mpsc::Sender<String>,
) -> Option<Result<Option<Value>>> {
    match cmd {
        "run_status" => Some(super::stream::agent_loop::run_status(st, args).map(Some)),
        _ => None,
    }
}

#[cfg(test)]
#[path = "v2_tests.rs"]
mod tests;
