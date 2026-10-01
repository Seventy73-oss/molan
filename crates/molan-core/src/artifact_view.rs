//! 产物卡片投影：交付记录 × 当前领域事实 → 状态、中文措辞与合法动作。
//!
//! 状态：generating / generated / interrupted / failed / discarded / base_changed /
//! saved / partial / pending_review / confirmed / approved / rejected / stale / conflict。
//! 不是线性流程：每类产物只暴露自己的合法动作，主动作与状态一一对应。

use crate::artifact::{a_target, deliveries, disk_hash, get, get_rev, items_of};
use crate::continuity::{approved_hash, content_hash};
use crate::db::{Db, REVIEW_GROUP};
use crate::task_kind::TaskKind;
use anyhow::Result;
use serde_json::{json, Value};

pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "book_setup" => "建书资料包",
        "outline_draft" => "细纲草稿",
        "body_draft" => "正文草稿",
        "revision" => "修改稿",
        "review_report" => "审稿报告",
        "humanize_rewrite" => "去AI味改稿",
        "skill_draft" => "技能草稿",
        "summary" => "总结",
        "plot_note" => "剧情推演",
        "distill_note" => "蒸馏笔记",
        "proposal" => "修改提案",
        "multi_file" => "多文件交付",
        _ => "文档",
    }
}

pub fn state_label(state: &str) -> &'static str {
    match state {
        "generating" => "生成中",
        "generated" => "已生成，尚未保存",
        "interrupted" => "生成中断，草稿已保留",
        "failed" => "生成失败",
        "discarded" => "已放弃",
        "base_changed" => "原文已变化，需重新比较",
        "saved" => "已保存",
        "partial" => "部分成功",
        "pending_review" => "已提交待审，尚未定稿",
        "confirmed" => "细纲已确认",
        "approved" => "已定稿",
        "rejected" => "已驳回",
        "stale" => "结果已失效",
        "conflict" => "冲突，未写入",
        _ => "未知",
    }
}

fn status_of(db: &Db, book: &str, ch: i64, name: &str) -> Option<String> {
    db.q_json(
        "SELECT status FROM pending_chapter WHERE book_id=?1 AND (review_file=?2 OR ch=?3) ORDER BY review_file=?2 DESC LIMIT 1",
        &[&book as &dyn rusqlite::ToSql, &name, &ch],
    )
    .ok()?
    .first()
    .and_then(|r| r["status"].as_str().map(str::to_string))
}

/// 单条目状态：取最「靠后」的成功交付，按领域事实复核；无成功交付则看最后一次失败。
fn item_state(db: &Db, book: &str, ds: &[&Value]) -> (String, String, Value) {
    let ok = |action: &str| {
        ds.iter().rev().find(|d| {
            d["action"] == action && matches!(d["status"].as_str(), Some("committed" | "noop"))
        })
    };
    let loc = |d: &Value| json!({"group": d["groupName"], "name": d["fileName"], "ch": d["ch"]});
    if let Some(d) = ok("reject") {
        return (
            "rejected".into(),
            "待审稿已驳回（可在回收站恢复）".into(),
            loc(d),
        );
    }
    if let Some(d) = ok("approve") {
        let name = d["fileName"].as_str().unwrap_or("");
        let h = d["afterHash"].as_str().unwrap_or("");
        let ch = d["ch"].as_i64().unwrap_or(0);
        return if disk_hash(db, book, "正文", name).as_deref() == Some(h) {
            let m = memory_note(db, book, ch, h);
            (
                "approved".into(),
                format!("已定稿到 正文/{}{}", name, m),
                loc(d),
            )
        } else {
            (
                "stale".into(),
                format!("正文/{} 在定稿后已被修改，此卡为旧版本", name),
                loc(d),
            )
        };
    }
    if let Some(d) = ok("submit_pending") {
        let name = d["fileName"].as_str().unwrap_or("");
        let ch = d["ch"].as_i64().unwrap_or(0);
        let h = d["afterHash"].as_str().unwrap_or("");
        return match status_of(db, book, ch, name).as_deref() {
            Some("pending") if disk_hash(db, book, REVIEW_GROUP, name).as_deref() == Some(h) => (
                "pending_review".into(),
                format!("已提交到 正文待审/{}，等待定稿", name),
                loc(d),
            ),
            Some("approved")
                if approved_hash(db, book, name).ok().flatten().as_deref() == Some(h) =>
            {
                let m = memory_note(db, book, ch, h);
                (
                    "approved".into(),
                    format!("已定稿到 正文/{}{}", name, m),
                    loc(d),
                )
            }
            Some("rejected") => ("rejected".into(), "待审稿已被驳回".into(), loc(d)),
            _ => (
                "stale".into(),
                format!("正文待审/{} 已变化或已被其他操作处理", name),
                loc(d),
            ),
        };
    }
    if let Some(d) = ok("confirm_outline") {
        let name = d["fileName"].as_str().unwrap_or("");
        let ch = d["ch"].as_i64().unwrap_or(0);
        let st = crate::outline_confirm::status_for(db, book, ch, name);
        return if st["status"] == "confirmed" && st["hash"] == d["afterHash"] {
            (
                "confirmed".into(),
                format!("细纲/{} 已确认，可起草第{}章正文", name, ch),
                loc(d),
            )
        } else {
            (
                "stale".into(),
                format!("细纲/{} 在确认后被修改，确认已失效", name),
                loc(d),
            )
        };
    }
    if let Some(d) = ok("save_skill") {
        return skill_saved_state(db, d);
    }
    if let Some(d) = ok("save") {
        let (g, n) = (
            d["groupName"].as_str().unwrap_or(""),
            d["fileName"].as_str().unwrap_or(""),
        );
        let receipt: Value =
            serde_json::from_str(d["detailJson"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
        return match disk_hash(db, book, g, n) {
            Some(h) if h == d["afterHash"].as_str().unwrap_or("") => {
                if receipt["index"] == "failed" {
                    (
                        "partial".into(),
                        format!(
                            "已保存到 {}/{}，但索引登记失败（文件完好，可重试登记）",
                            g, n
                        ),
                        loc(d),
                    )
                } else {
                    ("saved".into(), format!("已保存到 {}/{}", g, n), loc(d))
                }
            }
            Some(_) => (
                "stale".into(),
                format!("{}/{} 在保存后已被修改，此卡为旧版本", g, n),
                loc(d),
            ),
            None => ("stale".into(), format!("{}/{} 已不存在", g, n), loc(d)),
        };
    }
    if let Some(d) = ds.last() {
        let detail: Value =
            serde_json::from_str(d["detailJson"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
        let msg = detail["error"]["message"]
            .as_str()
            .or(detail["error"].as_str())
            .unwrap_or("未写入")
            .to_string();
        let st = if d["status"] == "conflict" {
            "conflict"
        } else {
            "failed"
        };
        return (st.into(), msg, loc(d));
    }
    ("generated".into(), String::new(), Value::Null)
}

/// 定稿后的记忆同步状态（memory_job 绑定正文 hash）：「定稿完成，记忆更新排队中」等务实措辞。
fn memory_note(db: &Db, book: &str, ch: i64, hash: &str) -> &'static str {
    let row = db
        .q_json(
            "SELECT status, source_hash FROM memory_job WHERE book_id=?1 AND ch=?2",
            &[&book as &dyn rusqlite::ToSql, &ch],
        )
        .ok()
        .and_then(|v| v.into_iter().next());
    match row {
        Some(r) if r["sourceHash"].as_str() == Some(hash) => match r["status"].as_str() {
            Some("done") => "，记忆已更新",
            Some("failed") => "，记忆更新失败（可在生产线重试）",
            _ => "，记忆更新排队中",
        },
        _ => "，记忆更新排队中",
    }
}

/// 技能草稿保存后的状态：技能模板仍等于保存时的 hash → 已保存；被改 / 删除 → 已失效。
fn skill_saved_state(db: &Db, d: &Value) -> (String, String, Value) {
    let detail: Value =
        serde_json::from_str(d["detailJson"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
    let sid = detail["skillId"].as_str().unwrap_or("");
    let at = json!({"group": "技能", "name": d["fileName"], "skillId": sid});
    let row = db
        .q_json(
            "SELECT name, prompt_template FROM skills WHERE id=?1",
            &[&sid as &dyn rusqlite::ToSql],
        )
        .ok()
        .and_then(|v| v.into_iter().next());
    match row {
        Some(r) => {
            let n = r["name"].as_str().unwrap_or("");
            if content_hash(r["promptTemplate"].as_str().unwrap_or(""))
                == d["afterHash"].as_str().unwrap_or("")
            {
                ("saved".into(), format!("已保存为技能「{}」", n), at)
            } else {
                (
                    "stale".into(),
                    format!("技能「{}」在保存后已修改，此卡为旧版本", n),
                    at,
                )
            }
        }
        None => ("stale".into(), "保存的技能已被删除".into(), at),
    }
}

fn action(id: &str, label: &str, primary: bool) -> Value {
    json!({"id": id, "label": label, "primary": primary})
}

/// 按产物类型与状态给出动作（id 由前端映射到 artifact_deliver 或本地操作）。
fn actions_for(kind: &str, task: TaskKind, scope: &str, state: &str, target: &Value) -> Vec<Value> {
    let mut v = Vec::new();
    let has_ch = target["ch"].as_i64().unwrap_or(0) > 0;
    match state {
        "generating" => v.push(action("stop", "停止生成", true)),
        "discarded" => {}
        "generated" | "interrupted" | "base_changed" | "conflict" | "failed" => {
            if state == "base_changed" || state == "conflict" {
                v.push(action("compare", "与当前原文比较", true));
            }
            match kind {
                "skill_draft" => v.push(action("save_skill", "保存为技能", v.is_empty())),
                "outline_draft" => v.push(action("save_outline", "保存到细纲", v.is_empty())),
                "body_draft" if has_ch => {
                    v.push(action("submit_pending", "提交待审", v.is_empty()))
                }
                _ if scope == "fragment" && target["start"].is_u64() => {
                    v.push(action("apply_selection", "替换选区", v.is_empty()));
                    v.push(action("insert_after", "插入到选区后", false));
                }
                _ if task.rewrites_target() && target["name"].is_string() => {
                    v.push(action("apply_replace", "替换全文", v.is_empty()));
                }
                _ => {}
            }
            if kind != "skill_draft" {
                v.push(action("save_as", "另存为文档", v.is_empty()));
            }
        }
        "saved" if kind == "skill_draft" => v.push(action("open_skill", "查看技能", true)),
        "stale" if kind == "skill_draft" => {
            v.push(action("open_skill", "查看当前技能", true));
            v.push(action("save_skill", "另存为新技能", false));
        }
        "saved" | "partial" => {
            if kind == "outline_draft" && has_ch {
                v.push(action("confirm_outline", "确认细纲", true));
            }
            v.push(action("open_file", "打开文件", v.is_empty()));
        }
        "confirmed" => {
            v.push(action("draft_body", "起草本章正文", true));
            v.push(action("open_file", "打开细纲", false));
        }
        "pending_review" => {
            v.push(action("approve", "定稿", true));
            v.push(action("reject", "驳回", false));
            v.push(action("open_file", "阅读待审稿", false));
        }
        "approved" => v.push(action("open_file", "打开正文", true)),
        "stale" | "rejected" => {
            v.push(action("compare", "与当前文件比较", true));
            v.push(action("save_as", "另存为文档", false));
        }
        _ => {}
    }
    if !matches!(state, "generating" | "discarded") {
        v.push(action("edit", "编辑后保存为新修订", false));
        v.push(action("copy", "复制全文", false));
        if matches!(
            state,
            "generated" | "interrupted" | "failed" | "conflict" | "base_changed"
        ) {
            v.push(action("discard", "放弃", false));
        }
    }
    v
}

/// 产物卡片视图。`with_content=false` 时只给 2000 字预览（列表用）。
pub fn view(db: &Db, id: &str, with_content: bool) -> Result<Value> {
    let a = get(db, id)?;
    let book = a["bookId"].as_str().unwrap_or("").to_string();
    let head = a["headRev"].as_i64().unwrap_or(1);
    let rev = get_rev(db, id, head)?;
    let mut items = items_of(&rev);
    let content = rev["content"].as_str().unwrap_or("").to_string();
    let multi = !items.is_empty();
    if !multi {
        items.push(json!({"title": a["title"], "content": content}));
    }
    let all = deliveries(db, id);
    let target = a_target(&a);
    let task = TaskKind::parse(a["task"].as_str().unwrap_or("chat")).unwrap_or(TaskKind::Chat);
    let kind = a["kind"].as_str().unwrap_or("document").to_string();
    let scope = a["scope"].as_str().unwrap_or("document").to_string();
    let lifecycle = a["lifecycle"].as_str().unwrap_or("generated").to_string();
    let mut item_views = Vec::new();
    let mut states: Vec<String> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let ds: Vec<&Value> = all
            .iter()
            .filter(|d| {
                d["itemIndex"].as_i64() == Some(i as i64) && d["rev"].as_i64() == Some(head)
            })
            .collect();
        let (mut st, mut note, loc) = item_state(db, &book, &ds);
        if st == "generated" {
            let prior = all.iter().any(|d| {
                d["itemIndex"].as_i64() == Some(i as i64)
                    && d["rev"].as_i64() != Some(head)
                    && d["status"] == "committed"
            });
            if prior {
                note = "上一修订已交付；当前修订尚未保存".into();
            }
            if !multi && target["baseHash"].is_string() && target["name"].is_string() {
                let cur = disk_hash(
                    db,
                    &book,
                    target["group"].as_str().unwrap_or(""),
                    target["name"].as_str().unwrap_or(""),
                );
                if cur.as_deref() != target["baseHash"].as_str() {
                    st = "base_changed".into();
                    note = "生成后原文已被修改：直接替换会覆盖新内容，请先比较".into();
                }
            }
        }
        let c = it["content"].as_str().unwrap_or("");
        item_views.push(json!({
            "index": i, "title": it["title"], "group": it["group"], "name": it["name"],
            "chars": c.chars().count(), "hash": content_hash(c), "state": st, "stateLabel": state_label(&st),
            "note": note, "location": loc,
            "content": if multi && !with_content { Value::Null } else if multi { json!(c) } else { Value::Null },
        }));
        states.push(st);
    }
    let overall = if matches!(
        lifecycle.as_str(),
        "generating" | "interrupted" | "failed" | "discarded"
    ) && states.iter().all(|s| s == "generated")
    {
        lifecycle.clone()
    } else if states.iter().all(|s| *s == states[0]) {
        states[0].clone()
    } else {
        "partial".into()
    };
    let summary = if multi {
        let ok = states
            .iter()
            .filter(|s| {
                matches!(
                    s.as_str(),
                    "saved" | "approved" | "confirmed" | "pending_review"
                )
            })
            .count();
        let bad = states
            .iter()
            .filter(|s| matches!(s.as_str(), "conflict" | "failed" | "stale"))
            .count();
        let mut parts = vec![format!("{}项成功", ok)];
        if bad > 0 {
            parts.push(format!("{}项冲突或失败", bad));
        }
        let todo = states.len() - ok - bad;
        if todo > 0 {
            parts.push(format!("{}项未保存", todo));
        }
        parts.join("，")
    } else {
        item_views[0]["note"].as_str().unwrap_or("").to_string()
    };
    let preview: String = content.chars().take(2000).collect();
    let provenance: Value =
        serde_json::from_str(a["provenanceJson"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
    let delivery_views: Vec<Value> = all
        .iter()
        .map(|d| {
            json!({
                "id": d["id"], "rev": d["rev"], "item": d["itemIndex"], "action": d["action"], "status": d["status"],
                "group": d["groupName"], "name": d["fileName"], "ch": d["ch"], "op": d["op"], "afterHash": d["afterHash"],
                "writeId": d["writeId"], "createdAt": d["createdAt"],
                "detail": serde_json::from_str::<Value>(d["detailJson"].as_str().unwrap_or("{}")).unwrap_or(json!({})),
            })
        })
        .collect();
    let mut out = json!({
        "id": id, "bookId": book, "sessionId": a["sessionId"], "messageId": a["messageId"], "runId": a["runId"],
        "kind": kind, "kindLabel": kind_label(&kind), "task": task.id(), "taskLabel": task.label(),
        "title": a["title"], "scope": scope, "format": a["format"], "lifecycle": lifecycle,
        "rev": head, "contentHash": rev["contentHash"], "chars": content.chars().count(),
        "origin": rev["origin"], "createdAt": a["createdAt"], "updatedAt": a["updatedAt"], "legacy": false,
    });
    let extra = json!({
        "content": if with_content { json!(content) } else { json!(preview) },
        "truncated": !with_content && content.chars().count() > 2000,
        "items": if multi { json!(item_views) } else { json!([]) },
        "item": if multi { Value::Null } else { item_views[0].clone() },
        "state": overall, "stateLabel": state_label(&overall), "summary": summary,
        "target": target, "provenance": provenance,
        "actions": actions_for(&kind, task, &scope, &overall, &target),
        "deliveries": delivery_views,
    });
    if let (Some(o), Some(e)) = (out.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    Ok(out)
}

/// 会话内全部产物（新模型 + 旧消息形状的只读投影），按时间排序。
pub fn list_for_session(db: &Db, book: &str, session: &str) -> Result<Value> {
    let rows = db.q_json(
        "SELECT id FROM artifact WHERE book_id=?1 AND session_id=?2 ORDER BY created_at",
        &[&book as &dyn rusqlite::ToSql, &session],
    )?;
    let mut out: Vec<Value> = rows
        .iter()
        .filter_map(|r| view(db, r["id"].as_str()?, false).ok())
        .collect();
    out.extend(crate::artifact_legacy::views_for_session(
        db, book, session,
    )?);
    out.sort_by_key(|v| v["createdAt"].as_i64().unwrap_or(0));
    Ok(json!(out))
}

/// 技能工坊的技能草稿（全局，不属于任何作品；最近优先，不含已放弃）。
pub fn list_skill_drafts(db: &Db, limit: i64) -> Result<Value> {
    let rows = db.q_json(
        "SELECT id FROM artifact WHERE kind='skill_draft' AND lifecycle<>'discarded' ORDER BY updated_at DESC LIMIT ?1",
        &[&limit.clamp(1, 100) as &dyn rusqlite::ToSql],
    )?;
    Ok(json!(rows
        .iter()
        .filter_map(|r| view(db, r["id"].as_str()?, true).ok())
        .collect::<Vec<_>>()))
}

/// 作品内产物（最近优先），可按状态过滤（如只看待处理）。
pub fn list_for_book(db: &Db, book: &str, limit: i64) -> Result<Value> {
    let rows = db.q_json(
        "SELECT id FROM artifact WHERE book_id=?1 AND lifecycle<>'discarded' ORDER BY updated_at DESC LIMIT ?2",
        &[&book as &dyn rusqlite::ToSql, &limit.clamp(1, 500)],
    )?;
    Ok(json!(rows
        .iter()
        .filter_map(|r| view(db, r["id"].as_str()?, false).ok())
        .collect::<Vec<_>>()))
}
