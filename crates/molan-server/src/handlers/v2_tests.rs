//! v2 IPC 契约测试：经真实 handlers::dispatch（与 /ipc/:cmd 同一分发）验证新命令、
//! 旧命令兼容与安全修复；并把规范化后的返回形状写入 contracts/fixtures，供前端契约测试共用。
//!
//! 契约漂移检测：fixture 已存在时逐字比较（规范化后），不一致即失败；
//! 有意变更契约时设置 MOLAN_UPDATE_CONTRACTS=1 重新生成，并同步前端类型与测试。
use crate::AppState;
use molan_core::continuity::content_hash;
use molan_core::db::Db;
use serde_json::{json, Value};
use std::sync::Arc;

fn state() -> (tempfile::TempDir, Arc<AppState>, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), None).unwrap();
    db.exec("INSERT INTO settings(key,value) VALUES('channels','[{\"id\":\"mock\",\"label\":\"Mock\",\"baseUrl\":\"mock://\",\"model\":\"mock-model\"}]')", &[]).unwrap();
    db.exec(
        "INSERT INTO settings(key,value) VALUES('active_channel','mock')",
        &[],
    )
    .unwrap();
    let book = molan_core::books::create_book(&db, "契约测试", "玄幻", "第三人称")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let st = Arc::new(AppState {
        db,
        web_dir: dir.path().join("web"),
        web_dir_canon: dir.path().join("web"),
        root: dir.path().to_path_buf(),
        auth_token: None,
        sessions: crate::auth::SessionStore::with_defaults(),
        login_limiter: crate::auth::LoginLimiter::with_defaults(),
        trusted_scheme: "http".into(),
        secure_cookies: false,
    });
    (dir, st, book)
}

async fn ipc(st: &Arc<AppState>, cmd: &str, args: Value) -> anyhow::Result<Value> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(512);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let r = crate::handlers::dispatch(st, cmd, &args, &tx).await;
    drop(tx);
    let _ = drain.await;
    r.map(|v| v.unwrap_or(Value::Null))
}

/// 字符串内嵌的 UUID（如 artifact:<uuid>:key）替换为 <uuid>。
fn mask_uuids(s: &str) -> String {
    let b = s.as_bytes();
    let is_uuid = |w: &[u8]| {
        w.len() == 36
            && w.iter().enumerate().all(|(i, c)| match i {
                8 | 13 | 18 | 23 => *c == b'-',
                _ => c.is_ascii_hexdigit(),
            })
    };
    let (mut out, mut i) = (String::new(), 0);
    while i < b.len() {
        if i + 36 <= b.len() && is_uuid(&b[i..i + 36]) {
            out.push_str("<uuid>");
            i += 36;
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// 规范化易变字段（id/时间/hash），只保留结构与稳定值。
fn normalize(v: &Value) -> Value {
    const IDS: [&str; 13] = [
        "id",
        "artifactId",
        "writeId",
        "runId",
        "deliveryId",
        "messageId",
        "sessionId",
        "bookId",
        "requestId",
        "manifestId",
        "idempotencyKey",
        "trashId",
        "skillId",
    ];
    const TS: [&str; 6] = [
        "createdAt",
        "updatedAt",
        "ts",
        "confirmedAt",
        "appliedAt",
        "mtime",
    ];
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| {
                    let uuid_like = x
                        .as_str()
                        .is_some_and(|t| t.len() == 36 && t.matches('-').count() == 4);
                    let n = if uuid_like && IDS.contains(&k.as_str()) {
                        json!("<id>")
                    } else if x.is_number()
                        && (TS.contains(&k.as_str()) || k.ends_with("Ms") || k == "ms")
                    {
                        // 时间戳与耗时（firstTokenMs / totalMs …）随运行变化
                        json!(0)
                    } else if x.is_string()
                        && (k.ends_with("Hash") || k == "hash")
                        && x.as_str().unwrap().len() == 64
                    {
                        json!("<sha256>")
                    } else {
                        normalize(x)
                    };
                    (k.clone(), n)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(normalize).collect()),
        Value::String(x) => json!(mask_uuids(x)),
        other => other.clone(),
    }
}

fn fixture(name: &str, v: &Value) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/fixtures");
    let path = dir.join(format!("{}.json", name));
    let text = serde_json::to_string_pretty(&normalize(v)).unwrap() + "\n";
    let update = std::env::var("MOLAN_UPDATE_CONTRACTS").is_ok_and(|x| x == "1");
    if update || !path.exists() {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, text).unwrap();
        return;
    }
    let old = std::fs::read_to_string(&path)
        .unwrap()
        .replace("\r\n", "\n");
    assert_eq!(
        old, text,
        "契约 {} 变化：确认是有意变更后以 MOLAN_UPDATE_CONTRACTS=1 重新生成，并同步前端类型",
        name
    );
}

#[tokio::test]
async fn app_info_and_task_catalog() {
    let (_d, st, _b) = state();
    let v = ipc(&st, "app_info", json!({})).await.unwrap();
    assert_eq!(v["contract"], 2);
    assert_eq!(v["tasks"].as_array().unwrap().len(), 9);
    fixture(
        "app_info",
        &json!({"contract": v["contract"], "ui": v["ui"], "tasks": v["tasks"], "writeOps": v["writeOps"]}),
    );
}

#[tokio::test]
async fn doc_read_write_roundtrip_and_conflict_receipts() {
    let (_d, st, b) = state();
    let r = ipc(&st, "doc_write", json!({"bookId": b, "group": "设定", "name": "人物.md", "op": "create", "content": "阿青\n", "idempotencyKey": "k1"})).await.unwrap();
    assert_eq!(r["commit"], "committed");
    fixture("write_receipt_committed", &r);
    let doc = ipc(
        &st,
        "doc_read",
        json!({"bookId": b, "group": "设定", "name": "人物.md"}),
    )
    .await
    .unwrap();
    assert_eq!(doc["hash"], json!(content_hash("阿青\n")));
    fixture("doc_read", &doc);
    let conflict = ipc(&st, "doc_write", json!({"bookId": b, "group": "设定", "name": "人物.md", "op": "replace", "baseHash": "stale", "content": "x", "idempotencyKey": "k2"})).await.unwrap();
    assert_eq!(conflict["commit"], "conflict");
    assert_eq!(conflict["error"]["code"], "BASE_CHANGED");
    fixture("write_receipt_conflict", &conflict);
    // 缺幂等键：参数错误（不是回执）
    assert!(ipc(
        &st,
        "doc_write",
        json!({"bookId": b, "group": "设定", "name": "a.md", "op": "create", "content": "x"})
    )
    .await
    .is_err());
    let hist = ipc(
        &st,
        "doc_history",
        json!({"bookId": b, "group": "设定", "name": "人物.md"}),
    )
    .await
    .unwrap();
    assert_eq!(hist.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn task_preview_shows_effective_plan_and_context() {
    let (_d, st, b) = state();
    st.db.exec("INSERT INTO skills(id,name,prompt_template,kind,enabled,usage_mode,origin,targets_json) VALUES('bp','作品主卡','模板','user',1,'primary','user','[\"body\"]'),('x','本次写法','模板','user',1,'standalone','user','[\"body\"]')", &[]).unwrap();
    ipc(
        &st,
        "set_book_primary_skill",
        json!({"bookId": b, "taskKind": "chapter", "skillId": "bp"}),
    )
    .await
    .unwrap();
    let key = format!("book_primary_skill__{}__body", b);
    assert_eq!(
        molan_llm::get_setting(&st.db, &key),
        "bp",
        "chapter 别名必须落在 body 键"
    );
    let v = ipc(&st, "task_preview", json!({"bookId": b, "task": "body", "skillSelection": {"primarySkillId": "x"}, "target": {"ch": 1}})).await.unwrap();
    assert_eq!(v["plan"]["skills"][0]["id"], "x");
    assert_eq!(v["plan"]["excluded"][0]["code"], "REPLACED");
    assert!(
        v["plan"]["skills"][0].get("promptTemplate").is_none(),
        "预览不下发模板全文"
    );
    assert!(
        v["context"]["blockers"][0]
            .as_str()
            .unwrap()
            .contains("细纲"),
        "无细纲必须提示"
    );
    fixture("task_preview", &v);
    // 预览与执行同源：作者附带的资料按 agent_turn 的 contextFiles 规则出现在预览里
    molan_core::files::write_file(&st.db, &b, "设定", "人物.md", "林岚").unwrap();
    let v = ipc(&st, "task_preview", json!({"bookId": b, "task": "chat", "contextFiles": [{"group": "设定", "name": "人物.md"}]})).await.unwrap();
    assert!(
        v["context"]["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["label"] == "资料：设定/人物.md" && x.get("skipped").is_none()),
        "{}",
        v["context"]
    );
    assert!(ipc(
        &st,
        "set_book_primary_skill",
        json!({"bookId": b, "taskKind": "bogus", "skillId": "bp"})
    )
    .await
    .is_err());
    assert!(ipc(
        &st,
        "set_book_support_skills",
        json!({"bookId": b, "taskKind": "body", "skillIds": ["ghost"]})
    )
    .await
    .is_err());
}

#[tokio::test]
async fn artifact_ipc_lifecycle_and_views() {
    let (_d, st, b) = state();
    let id = molan_core::artifact::create(
        &st.db,
        &molan_core::artifact::NewArtifact {
            book_id: b.clone(),
            session_id: "S".into(),
            kind: "outline_draft".into(),
            task: "outline".into(),
            title: "第1章细纲".into(),
            target: json!({"ch": 1}),
            content: "# 第1章细纲\n冲突与钩子".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let got = ipc(&st, "artifact_get", json!({"bookId": b, "artifactId": id}))
        .await
        .unwrap();
    assert_eq!(got["state"], "generated");
    fixture("artifact_generated", &got);
    let saved = ipc(&st, "artifact_deliver", json!({"bookId": b, "artifactId": id, "action": "save", "group": "细纲", "name": "细纲_第1章.md", "op": "create", "idempotencyKey": "s1"})).await.unwrap();
    assert_eq!(saved["artifact"]["state"], "saved");
    let confirmed = ipc(
        &st,
        "artifact_deliver",
        json!({"bookId": b, "artifactId": id, "action": "confirm_outline", "idempotencyKey": "c1"}),
    )
    .await
    .unwrap();
    assert_eq!(confirmed["artifact"]["state"], "confirmed");
    fixture("artifact_deliver_confirmed", &confirmed);
    let revised = ipc(
        &st,
        "artifact_revise",
        json!({"bookId": b, "artifactId": id, "baseRev": 1, "content": "# 改过"}),
    )
    .await
    .unwrap();
    assert_eq!(revised["rev"], 2);
    assert_eq!(revised["state"], "generated", "新修订尚未保存");
    let other = molan_core::books::create_book(&st.db, "别的书", "都市", "第一人称")["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        ipc(
            &st,
            "artifact_get",
            json!({"bookId": other, "artifactId": id})
        )
        .await
        .is_err(),
        "跨作品读取必须拒绝"
    );
    let list = ipc(&st, "artifact_list", json!({"bookId": b, "sessionId": "S"}))
        .await
        .unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn legacy_commands_keep_shape_and_safety_fixes_hold() {
    let (_d, st, b) = state();
    // apply_asset_update：缺参报错、不写正式正文、尊重锁定
    assert!(ipc(
        &st,
        "apply_asset_update",
        json!({"bookId": b, "targetName": "x.md"})
    )
    .await
    .is_err());
    molan_core::files::write_file(&st.db, &b, "正文", "第1章.md", "正式稿").unwrap();
    let e = ipc(
        &st,
        "apply_asset_update",
        json!({"bookId": b, "targetName": "第1章.md", "content": "AI改写"}),
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("正式正文"));
    assert_eq!(
        molan_core::files::read_file(&st.db, &b, "正文", "第1章.md").as_deref(),
        Some("正式稿")
    );
    let ok = ipc(
        &st,
        "apply_asset_update",
        json!({"bookId": b, "targetName": "人物表.md", "content": "人物"}),
    )
    .await
    .unwrap();
    assert!(
        ok.as_str().unwrap().contains("\"applied\":true"),
        "旧返回形状（JSON 字符串）保持"
    );
    // approve_chapter：可选 expectedHash 绑定；旧形状字段保留
    molan_core::chapter_commit::submit_pending(&st.db, &b, 2, "第二章待审", "", || Ok(())).unwrap();
    assert!(ipc(
        &st,
        "approve_chapter",
        json!({"bookId": b, "ch": 2, "expectedHash": "wrong"})
    )
    .await
    .is_err());
    let r = ipc(&st, "approve_chapter", json!({"bookId": b, "ch": 2}))
        .await
        .unwrap();
    assert_eq!(r["ok"], true);
    assert_eq!(r["finalName"], "第2章.md");
    assert_eq!(r["postProcess"], "queued");
    let again = ipc(
        &st,
        "approve_chapter",
        json!({"bookId": b, "name": "第2章.md"}),
    )
    .await
    .unwrap();
    assert_eq!(again["alreadyApproved"], true);
    // list_books 形状不变
    let books = ipc(&st, "list_books", json!({})).await.unwrap();
    for k in ["id", "title", "genre", "wordCount", "chapterCount"] {
        assert!(books[0].get(k).is_some(), "list_books 缺字段 {}", k);
    }
}

#[tokio::test]
async fn run_status_reports_state_machine() {
    let (_d, st, b) = state();
    let sid = molan_core::books::create_session(&st.db, &b, "会话").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        ipc(&st, "run_status", json!({"bookId": b, "sessionId": sid}))
            .await
            .unwrap(),
        Value::Null
    );
    let r = ipc(&st, "agent_turn", json!({"onEvent": "__CHANNEL__:1", "bookId": b, "sessionId": sid, "message": "你好", "requestId": "rq", "task": "chat"})).await.unwrap();
    assert_eq!(r["status"], "done");
    let s = ipc(
        &st,
        "run_status",
        json!({"bookId": b, "sessionId": sid, "requestId": "rq"}),
    )
    .await
    .unwrap();
    assert_eq!(s["state"], "completed");
    assert_eq!(s["live"], false);
    fixture("run_state", &s);
    let legacy = ipc(
        &st,
        "agent_session_state",
        json!({"bookId": b, "sessionId": sid}),
    )
    .await
    .unwrap();
    assert_eq!(legacy["run"]["status"], "done", "旧字段保留");
    assert_eq!(legacy["state"]["state"], "completed");
}

/// 发起前预览的计划 = 实际执行冻结的计划（同 planHash、同模板）；运行后改技能只影响之后的预览。
#[tokio::test]
async fn preview_plan_matches_executed_plan() {
    let (_d, st, b) = state();
    st.db.exec("INSERT INTO skills(id,name,prompt_template,kind,enabled,usage_mode,origin,targets_json) VALUES('x','本次细纲法','模板X','user',1,'standalone','user','[\"outline\"]'),('s','辅助法','模板S','user',1,'support','user','[\"outline\"]'),('bp','作品细纲主卡','模板BP','user',1,'primary','user','[\"outline\"]')", &[]).unwrap();
    ipc(
        &st,
        "set_book_primary_skill",
        json!({"bookId": b, "taskKind": "outline", "skillId": "bp"}),
    )
    .await
    .unwrap();
    let sid = molan_core::books::create_session(&st.db, &b, "会话").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let sel = json!({"primarySkillId": "x", "supportSkillIds": ["s"], "humanize": "none"});
    let target = json!({"ch": 1});
    let pv = ipc(
        &st,
        "task_preview",
        json!({"bookId": b, "task": "outline", "skillSelection": sel, "target": target}),
    )
    .await
    .unwrap();
    let preview_hash = pv["plan"]["planHash"].as_str().unwrap().to_string();
    let r = ipc(&st, "agent_turn", json!({"onEvent": "__CHANNEL__:1", "bookId": b, "sessionId": sid, "message": "起草第1章细纲", "requestId": "rq-plan", "task": "outline", "target": target, "skillSelection": sel})).await.unwrap();
    assert_eq!(r["status"], "done", "{}", r);
    let run = ipc(
        &st,
        "run_status",
        json!({"bookId": b, "sessionId": sid, "requestId": "rq-plan"}),
    )
    .await
    .unwrap();
    assert_eq!(
        run["planHash"].as_str(),
        Some(preview_hash.as_str()),
        "预览与执行必须是同一计划"
    );
    let frozen = molan_core::skill_resolver::load_frozen(&st.db, &preview_hash).unwrap();
    let tpl: Vec<&str> = frozen["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["promptTemplate"].as_str().unwrap())
        .collect();
    assert_eq!(
        tpl,
        vec!["模板X", "模板S"],
        "本次主技能替代作品主技能，辅助叠加"
    );
    // 运行后改技能：之后的预览换新计划，已冻结的快照不变
    ipc(
        &st,
        "update_skill",
        json!({"id": "x", "promptTemplate": "模板X2"}),
    )
    .await
    .unwrap();
    let pv2 = ipc(
        &st,
        "task_preview",
        json!({"bookId": b, "task": "outline", "skillSelection": sel, "target": target}),
    )
    .await
    .unwrap();
    assert_ne!(
        pv2["plan"]["planHash"].as_str(),
        Some(preview_hash.as_str())
    );
    let again = molan_core::skill_resolver::load_frozen(&st.db, &preview_hash).unwrap();
    assert_eq!(again["skills"][0]["promptTemplate"], "模板X");
}

/// 技能草稿产物：起草 → 编辑为新修订 → 保存为技能（幂等、回执）→ 技能被改后卡片失效；旧 draft_skill 形状不变。
#[tokio::test]
async fn skill_draft_artifact_lifecycle() {
    let (_d, st, _b) = state();
    let legacy = ipc(
        &st,
        "draft_skill",
        json!({"name": "对白法", "task": "body"}),
    )
    .await
    .unwrap();
    assert!(legacy["draft"].is_string(), "旧命令仍返回 {{draft}}");
    assert!(
        ipc(&st, "skill_draft", json!({"task": "body"}))
            .await
            .is_err(),
        "缺技能名必须拒绝"
    );
    let a = ipc(
        &st,
        "skill_draft",
        json!({"name": "对白法", "description": "写好对白", "task": "body", "usage": "正文"}),
    )
    .await
    .unwrap();
    let id = a["id"].as_str().unwrap().to_string();
    assert_eq!(
        (
            a["kind"].as_str(),
            a["state"].as_str(),
            a["bookId"].as_str()
        ),
        (Some("skill_draft"), Some("generated"), Some(""))
    );
    assert_eq!(a["actions"][0]["id"], "save_skill");
    assert!(
        a["actions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["id"] != "save_as"),
        "技能草稿不能另存为书稿文档"
    );
    let a = ipc(
        &st,
        "skill_draft_revise",
        json!({"artifactId": id, "baseRev": 1, "content": "对白要有潜台词。"}),
    )
    .await
    .unwrap();
    assert_eq!(a["rev"], 2);
    assert!(
        ipc(
            &st,
            "skill_draft_revise",
            json!({"artifactId": id, "baseRev": 1, "content": "旧基线"})
        )
        .await
        .is_err(),
        "旧修订基线必须冲突"
    );
    let r = ipc(
        &st,
        "skill_draft_save",
        json!({"artifactId": id, "idempotencyKey": "k1"}),
    )
    .await
    .unwrap();
    let sid = r["delivery"]["result"]["skillId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(r["artifact"]["state"], "saved");
    assert!(r["artifact"]["summary"]
        .as_str()
        .unwrap()
        .contains("已保存为技能「对白法」"));
    let again = ipc(
        &st,
        "skill_draft_save",
        json!({"artifactId": id, "idempotencyKey": "k1"}),
    )
    .await
    .unwrap();
    assert_eq!(again["delivery"]["replayed"], true);
    let skills = ipc(&st, "list_skills", json!({})).await.unwrap();
    let made: Vec<&Value> = skills
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["name"] == "对白法")
        .collect();
    assert_eq!(made.len(), 1, "重放不得建第二个技能");
    assert_eq!(
        made[0]["promptTemplate"], "对白要有潜台词。",
        "保存的是当前修订"
    );
    assert_eq!(made[0]["targets"], json!(["body"]));
    ipc(
        &st,
        "update_skill",
        json!({"id": sid, "promptTemplate": "作者改过"}),
    )
    .await
    .unwrap();
    let list = ipc(&st, "skill_drafts", json!({})).await.unwrap();
    assert_eq!(list[0]["state"], "stale", "技能被改后草稿卡显示已失效");
    let b = ipc(&st, "skill_draft", json!({"name": "弃稿", "task": "chat"}))
        .await
        .unwrap();
    ipc(&st, "skill_draft_discard", json!({"artifactId": b["id"]}))
        .await
        .unwrap();
    assert_eq!(
        ipc(&st, "skill_drafts", json!({}))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

/// 待审区「审稿当前版本」：结论绑定当前稿 hash；改稿后变为已失效；没有待审稿 / 稿件过短如实报错。
#[tokio::test]
async fn review_pending_binds_to_current_version() {
    let (_d, st, b) = state();
    assert!(ipc(&st, "review_pending", json!({"bookId": b, "ch": 7}))
        .await
        .is_err());
    ipc(
        &st,
        "write_file",
        json!({"bookId": b, "group": "正文待审", "name": "第7章.md", "content": "短稿"}),
    )
    .await
    .unwrap();
    let e = ipc(&st, "review_pending", json!({"bookId": b, "ch": 7}))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("过短"), "{}", e);
    let body = format!(
        "# 第7章 夜考\n{}",
        "林岚在夜色里走上石阶，考核开始了。".repeat(30)
    );
    ipc(
        &st,
        "write_file",
        json!({"bookId": b, "group": "正文待审", "name": "第7章.md", "content": body}),
    )
    .await
    .unwrap();
    let r = ipc(&st, "review_pending", json!({"bookId": b, "ch": 7}))
        .await
        .unwrap();
    assert_eq!(r["review"]["state"], "current", "{}", r);
    assert_eq!(r["review"]["source"], "review");
    let edited = format!("{}\n作者补了一句。", body);
    ipc(
        &st,
        "write_file",
        json!({"bookId": b, "group": "正文待审", "name": "第7章.md", "content": edited}),
    )
    .await
    .unwrap();
    let list = ipc(&st, "list_pending_chapters", json!({"bookId": b}))
        .await
        .unwrap();
    let item = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["ch"] == 7)
        .unwrap()
        .clone();
    assert_eq!(item["review"]["state"], "stale", "改稿后旧审稿结论失效");
}

/// 旧入口写「正文待审」：返回形状不变，且写盘与登记同锁完成（队列里一定有对应行）。
#[tokio::test]
async fn legacy_review_writes_keep_shape_and_always_register() {
    let (_d, st, b) = state();
    let queued = |ch: i64| {
        st.db
            .q_json(
                "SELECT status FROM pending_chapter WHERE book_id=?1 AND ch=?2",
                &[&b as &dyn rusqlite::ToSql, &ch],
            )
            .unwrap()
    };
    // 作者手动写入待审（可覆盖），pending 字段为登记结果
    let r = ipc(
        &st,
        "write_file",
        json!({"bookId": b, "group": "正文待审", "name": "第3章.md", "content": "手稿"}),
    )
    .await
    .unwrap();
    assert_eq!(
        (r["ok"].as_bool(), r["pending"]["status"].as_str()),
        (Some(true), Some("pending"))
    );
    ipc(
        &st,
        "write_file",
        json!({"bookId": b, "group": "正文待审", "name": "第3章.md", "content": "改稿"}),
    )
    .await
    .unwrap();
    assert_eq!(queued(3).len(), 1);
    let list = ipc(&st, "list_pending_chapters", json!({"bookId": b}))
        .await
        .unwrap();
    assert_eq!(
        list[0]["review"]["state"], "none",
        "未审过的待审稿不得显示审稿结论"
    );
    // 中断残稿存入待审
    let body = format!("# 第5章 残稿\n{}", "字".repeat(220));
    let r = ipc(
        &st,
        "save_partial_as_review",
        json!({"bookId": b, "content": body}),
    )
    .await
    .unwrap();
    assert_eq!(
        (r["ch"].as_i64(), r["name"].as_str()),
        (Some(5), Some("第5章.md"))
    );
    assert_eq!(queued(5)[0]["status"], "pending");
    // 聊天气泡保存到待审：AI 规则（已有待审拒绝，原稿不变）
    let r = ipc(
        &st,
        "save_chat_output",
        json!({"bookId": b, "group": "正文待审", "name": "第6章.md", "content": "AI 稿"}),
    )
    .await
    .unwrap();
    assert_eq!(r["pending"]["status"], "pending");
    assert!(ipc(
        &st,
        "save_chat_output",
        json!({"bookId": b, "group": "正文待审", "name": "第6章.md", "content": "另一版"})
    )
    .await
    .is_err());
    assert_eq!(
        molan_core::files::read_file(&st.db, &b, "正文待审", "第6章.md").as_deref(),
        Some("AI 稿")
    );
}
