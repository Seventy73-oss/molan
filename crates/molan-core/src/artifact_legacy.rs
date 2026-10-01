//! 旧消息形状 → 产物卡片的只读投影（兼容适配层，不改旧数据）。
//!
//! 覆盖：`result.doc/docs`（保存文档卡）、`result.bookSetup`（建书资料包）、
//! Agent `steps_json[].artifact`（proposal / outline_saved / outline_confirmed / body_draft / body_finalized）。
//! 状态同样按磁盘/队列/回执复核：旧卡指向的文件被改过就显示「已失效」。
//! 动作只映射到既有、已验证的旧 IPC（save_doc / save_book_setup_selection / dw_* / approve_chapter），
//! 内容仍由服务端从持久化消息读取，前端不能借旧卡写入任意内容。

use crate::artifact::disk_hash;
use crate::artifact_view::{kind_label, state_label};
use crate::continuity::{approved_hash, content_hash};
use crate::db::{Db, REVIEW_GROUP};
use anyhow::Result;
use serde_json::{json, Value};

fn act(id: &str, label: &str, primary: bool) -> Value {
    json!({"id": id, "label": label, "primary": primary})
}

fn parse(v: &Value) -> Value {
    serde_json::from_str(v.as_str().unwrap_or("")).unwrap_or(Value::Null)
}

#[allow(clippy::too_many_arguments)]
fn card(
    msg: &Value,
    n: usize,
    kind: &str,
    title: &str,
    state: &str,
    summary: String,
    content: &str,
    items: Vec<Value>,
    actions: Vec<Value>,
    extra: Value,
) -> Value {
    let preview: String = content.chars().take(2000).collect();
    json!({
        "id": format!("legacy:{}:{}", msg["id"].as_str().unwrap_or(""), n),
        "legacy": true, "messageId": msg["id"], "sessionId": msg["sessionId"],
        "kind": kind, "kindLabel": kind_label(kind), "title": title, "scope": "document",
        "state": state, "stateLabel": state_label(state), "summary": summary,
        "content": preview, "chars": content.chars().count(), "truncated": content.chars().count() > 2000,
        "items": items, "actions": actions, "deliveries": [], "legacyRef": extra,
        "createdAt": msg["createdAt"], "rev": 1, "provenance": {}, "target": {},
    })
}

/// 保存文档卡：saved_output 的分组映射（正文/空 → 正文待审）。
fn doc_group(g: &str) -> String {
    match g.trim() {
        "" | "正文" => REVIEW_GROUP.to_string(),
        other => other.to_string(),
    }
}

fn docs_card(db: &Db, book: &str, msg: &Value, result: &Value) -> Option<Value> {
    let docs: Vec<Value> = match (&result["docs"], &result["doc"]) {
        (Value::Array(a), _) if !a.is_empty() => a.clone(),
        (_, d) if d.is_object() => vec![d.clone()],
        _ => return None,
    };
    let fallback = msg["content"].as_str().unwrap_or("");
    let saved_list = result["savedDocs"].as_array().cloned().unwrap_or_default();
    let mut items = Vec::new();
    let mut states = Vec::new();
    let mut joined = String::new();
    for d in &docs {
        let content = d["content"]
            .as_str()
            .filter(|c| !c.trim().is_empty())
            .unwrap_or(if docs.len() == 1 { fallback } else { "" });
        let (g, n) = (
            doc_group(d["group"].as_str().unwrap_or("")),
            d["name"].as_str().unwrap_or("").trim().to_string(),
        );
        let saved = saved_list
            .iter()
            .any(|s| s["name"].as_str() == Some(n.as_str()));
        let st = match (saved, disk_hash(db, book, &g, &n)) {
            (true, Some(h)) if h == content_hash(content) => "saved",
            (true, _) => "stale",
            (false, _) => "generated",
        };
        states.push(st);
        joined.push_str(content);
        items.push(json!({"title": d["title"].as_str().unwrap_or(&n), "group": g, "name": n, "chars": content.chars().count(), "state": st, "stateLabel": state_label(st)}));
    }
    let state = if states.iter().all(|s| *s == states[0]) {
        states[0]
    } else {
        "partial"
    };
    let actions = if states.contains(&"generated") {
        vec![act("legacy_save_doc", "保存到书稿", true)]
    } else {
        vec![]
    };
    let title = docs[0]["title"]
        .as_str()
        .or(docs[0]["name"].as_str())
        .unwrap_or("文档")
        .to_string();
    let summary = items
        .iter()
        .map(|i| {
            format!(
                "{}/{}：{}",
                i["group"].as_str().unwrap_or(""),
                i["name"].as_str().unwrap_or(""),
                i["stateLabel"].as_str().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("；");
    Some(card(
        msg,
        0,
        if docs.len() > 1 {
            "multi_file"
        } else {
            "document"
        },
        &title,
        state,
        summary,
        &joined,
        items,
        actions,
        json!({"type": "doc"}),
    ))
}

fn setup_card(db: &Db, msg: &Value, result: &Value) -> Option<Value> {
    let bs = &result["bookSetup"];
    let files = bs["files"].as_array()?;
    let saved = bs["savedFiles"].as_array().cloned().unwrap_or_default();
    let dest_book = bs["destination"]["bookId"].as_str().unwrap_or("");
    let mut items = Vec::new();
    let mut ok = 0;
    for (i, f) in files.iter().enumerate() {
        let content = f["content"].as_str().unwrap_or("");
        let rec = saved.iter().find(|s| s["index"].as_u64() == Some(i as u64));
        let st = match rec {
            Some(r) => match disk_hash(
                db,
                dest_book,
                r["group"].as_str().unwrap_or(""),
                r["name"].as_str().unwrap_or(""),
            ) {
                Some(h) if h == content_hash(content) => "saved",
                _ => "stale",
            },
            None => "generated",
        };
        if st == "saved" {
            ok += 1;
        }
        items.push(json!({"index": i, "title": f["title"].as_str().or(f["name"].as_str()).unwrap_or(""), "group": rec.map(|r| r["group"].clone()).unwrap_or(f["group"].clone()), "name": rec.map(|r| r["name"].clone()).unwrap_or(f["name"].clone()), "chars": content.chars().count(), "state": st, "stateLabel": state_label(st)}));
    }
    let state = if ok == files.len() {
        "saved"
    } else if ok == 0 && saved.is_empty() {
        "generated"
    } else {
        "partial"
    };
    let summary = format!("{}项已保存，{}项未保存或已失效", ok, files.len() - ok);
    let actions = if state != "saved" {
        vec![act("legacy_book_setup", "选择目标并保存", true)]
    } else {
        vec![]
    };
    let all: String = files
        .iter()
        .filter_map(|f| f["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    Some(card(
        msg,
        1,
        "book_setup",
        "建书资料包",
        state,
        summary,
        &all,
        items,
        actions,
        json!({"type": "bookSetup", "destination": bs["destination"]}),
    ))
}

fn agent_cards(db: &Db, book: &str, msg: &Value) -> Vec<Value> {
    let steps = parse(&msg["stepsJson"]);
    let mut out = Vec::new();
    for (i, s) in steps.as_array().into_iter().flatten().enumerate() {
        let a = &s["artifact"];
        let k = a["kind"].as_str().unwrap_or("");
        if k.is_empty() {
            continue;
        }
        let ch = a["ch"].as_i64().unwrap_or(0);
        let name = a["name"]
            .as_str()
            .or(a["finalName"].as_str())
            .unwrap_or("")
            .to_string();
        let hash = a["hash"].as_str().unwrap_or("");
        let (kind, state, summary, actions, content): (&str, &str, String, Vec<Value>, String) =
            match k {
                "outline_saved" | "outline_confirmed" => {
                    let st = crate::outline_confirm::status_for(db, book, ch, &name);
                    let s2 = match st["status"].as_str() {
                        Some("confirmed")
                            if st["hash"].as_str() == Some(hash) || k == "outline_saved" =>
                        {
                            "confirmed"
                        }
                        Some("saved") if st["hash"].as_str() == Some(hash) => "saved",
                        _ => "stale",
                    };
                    let acts = if s2 == "saved" {
                        vec![act("legacy_confirm_outline", "确认细纲", true)]
                    } else {
                        vec![]
                    };
                    let text = crate::files::read_file(db, book, "细纲", &name).unwrap_or_default();
                    ("outline_draft", s2, format!("细纲/{}", name), acts, text)
                }
                "body_draft" => {
                    let pend = db
                        .q_json(
                            "SELECT status FROM pending_chapter WHERE book_id=?1 AND ch=?2",
                            &[&book as &dyn rusqlite::ToSql, &ch],
                        )
                        .ok()
                        .and_then(|r| {
                            r.first()
                                .and_then(|x| x["status"].as_str().map(str::to_string))
                        });
                    let st = match pend.as_deref() {
                        Some("pending")
                            if disk_hash(db, book, REVIEW_GROUP, &name).as_deref()
                                == Some(hash) =>
                        {
                            "pending_review"
                        }
                        Some("approved")
                            if approved_hash(db, book, &name).ok().flatten().as_deref()
                                == Some(hash) =>
                        {
                            "approved"
                        }
                        Some("rejected") => "rejected",
                        _ => "stale",
                    };
                    let acts = if st == "pending_review" {
                        vec![
                            act("legacy_approve", "定稿", true),
                            act("legacy_reject", "驳回", false),
                        ]
                    } else {
                        vec![]
                    };
                    let text =
                        crate::files::read_file(db, book, REVIEW_GROUP, &name).unwrap_or_default();
                    ("body_draft", st, format!("正文待审/{}", name), acts, text)
                }
                "body_finalized" => {
                    let st = if disk_hash(db, book, "正文", &name).as_deref() == Some(hash) {
                        "approved"
                    } else {
                        "stale"
                    };
                    (
                        "body_draft",
                        st,
                        format!("正文/{}", name),
                        vec![],
                        String::new(),
                    )
                }
                "proposal" => {
                    let pid = a["proposalId"].as_str().unwrap_or("");
                    let row = db.q_json("SELECT status, group_name, file_name, proposed_content FROM dw_change_proposal WHERE id=?1", &[&pid as &dyn rusqlite::ToSql]).ok().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null);
                    let proposed = row["proposedContent"].as_str().unwrap_or("").to_string();
                    let st = match row["status"].as_str() {
                        Some("pending") => "generated",
                        Some("accepted")
                            if disk_hash(
                                db,
                                book,
                                row["groupName"].as_str().unwrap_or(""),
                                row["fileName"].as_str().unwrap_or(""),
                            )
                            .as_deref()
                                == Some(&content_hash(&proposed)) =>
                        {
                            "saved"
                        }
                        Some("accepted") => "stale",
                        Some("rejected") => "rejected",
                        _ => "stale",
                    };
                    let acts = if st == "generated" {
                        vec![
                            act("legacy_accept_proposal", "接受提案", true),
                            act("legacy_reject_proposal", "拒绝", false),
                        ]
                    } else {
                        vec![]
                    };
                    (
                        "proposal",
                        st,
                        format!(
                            "{}/{}：{}",
                            row["groupName"].as_str().unwrap_or(""),
                            row["fileName"].as_str().unwrap_or(""),
                            a["summary"].as_str().unwrap_or("")
                        ),
                        acts,
                        proposed,
                    )
                }
                _ => continue,
            };
        out.push(card(
            msg,
            10 + i,
            kind,
            &name,
            state,
            summary,
            &content,
            vec![],
            actions,
            json!({"type": "agent", "artifact": a}),
        ));
    }
    out
}

/// 会话内旧形状卡片（已被新产物引用的消息跳过，避免重复）。
pub fn views_for_session(db: &Db, book: &str, session: &str) -> Result<Vec<Value>> {
    let msgs = db.q_json(
        "SELECT m.* FROM messages m WHERE m.session_id=?1 AND m.role='assistant'
           AND NOT EXISTS (SELECT 1 FROM artifact a WHERE a.message_id=m.id AND a.message_id<>'')
         ORDER BY m.created_at",
        &[&session as &dyn rusqlite::ToSql],
    )?;
    let mut out = Vec::new();
    for m in msgs {
        let result = parse(&m["resultJson"]);
        if let Some(c) = docs_card(db, book, &m, &result) {
            out.push(c);
        }
        if let Some(c) = setup_card(db, &m, &result) {
            out.push(c);
        }
        out.extend(agent_cards(db, book, &m));
    }
    Ok(out)
}
