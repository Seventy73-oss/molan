//! agent_loop 回归测试（mock:// 确定性渠道；协议/流程验证，不代表真实模型质量）。
use super::*;
use tokio_util::sync::CancellationToken;

fn fixture() -> (tempfile::TempDir, Db, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), None).unwrap();
    db.exec(
        "INSERT INTO settings(key,value) VALUES('channels','[{\"id\":\"mock\",\"label\":\"Mock\",\"baseUrl\":\"mock://\",\"model\":\"mock-model\"}]')",
        &[],
    )
    .unwrap();
    db.exec(
        "INSERT INTO settings(key,value) VALUES('active_channel','mock')",
        &[],
    )
    .unwrap();
    let book = molan_core::books::create_book(&db, "agent-test", "玄幻", "第三人称")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let session = molan_core::books::create_session(&db, &book, "工作会话").unwrap();
    let sid = session["id"].as_str().unwrap().to_string();
    (dir, db, book, sid)
}

fn args(book: &str, sid: &str, msg: &str, req: &str) -> Value {
    json!({"onEvent": "__CHANNEL__:1", "sessionId": sid, "bookId": book, "message": msg, "requestId": req})
}

fn with(mut a: Value, extra: Value) -> Value {
    for (k, v) in extra.as_object().unwrap() {
        a[k] = v.clone();
    }
    a
}

async fn collect(db: &Db, args: &Value) -> (Result<Option<Value>>, Vec<Value>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(256);
    let a = args.clone();
    let root = db.data_dir.parent().unwrap().to_path_buf();
    let runner = async {
        let r = run_agent_turn(db, &root, &a, &tx).await;
        drop(tx);
        r
    };
    let collector = async {
        let mut events = Vec::new();
        while let Some(line) = rx.recv().await {
            if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                if v["ch"] != "__hb__" {
                    events.push(v["e"].clone());
                }
            }
        }
        events
    };
    tokio::join!(runner, collector)
}

fn run_row(db: &Db, req: &str) -> Value {
    db.q_json(
        "SELECT * FROM agent_run WHERE request_id=?1",
        &[&req as &dyn rusqlite::ToSql],
    )
    .unwrap()
    .remove(0)
}

fn count(db: &Db, sql: &str, p: &str) -> i64 {
    db.q_json(sql, &[&p as &dyn rusqlite::ToSql]).unwrap()[0]["c"]
        .as_i64()
        .unwrap()
}

#[tokio::test]
async fn tool_round_then_text_completes() {
    let (_d, db, book, sid) = fixture();
    molan_core::files::write_file(&db, &book, "设定", "世界观.md", "修真体系").unwrap();
    let (res, events) = collect(&db, &args(&book, &sid, "[call:scan_book_tree]", "req-1")).await;
    let out = res.unwrap().unwrap();
    assert_eq!(out["status"], "done");
    let tools: Vec<&Value> = events.iter().filter(|e| e["type"] == "tool").collect();
    assert!(tools
        .iter()
        .any(|t| t["status"] == "running" && t["name"] == "scan_book_tree"));
    assert!(tools.iter().any(|t| t["status"] == "ok"));
    assert!(
        events.iter().any(|e| e["type"] == "plan"),
        "计划事件必须下发"
    );
    assert!(
        events.iter().any(|e| e["type"] == "context"),
        "上下文清单事件必须下发"
    );
    let done = events.iter().find(|e| e["type"] == "done").unwrap();
    let row = run_row(&db, "req-1");
    assert_eq!(row["status"], "done");
    assert_eq!(row["toolRound"].as_i64().unwrap(), 1);
    assert!(!row["planHash"].as_str().unwrap().is_empty());
    let turns = agent_run::list_turns(&db, row["id"].as_str().unwrap()).unwrap();
    assert_eq!(turns.len(), 3);
    assert!(!turns[0]["toolCallsJson"].as_str().unwrap().is_empty());
    assert!(turns[1]["content"].as_str().unwrap().contains("设定"));
    // done.full 只含最终文本，与落库一致
    let stored = db
        .q_json(
            "SELECT content FROM messages WHERE id=?1",
            &[&done["messageId"].as_str().unwrap() as &dyn rusqlite::ToSql],
        )
        .unwrap();
    assert_eq!(done["full"], stored[0]["content"]);
    assert_eq!(turns[2]["content"], stored[0]["content"]);
}

#[tokio::test]
async fn plain_text_without_tools_completes_and_usage_is_estimated() {
    let (_d, db, book, sid) = fixture();
    let (res, events) = collect(
        &db,
        &args(&book, &sid, "你好，介绍一下这本书的状态", "req-2"),
    )
    .await;
    assert_eq!(res.unwrap().unwrap()["status"], "done");
    assert!(!events.iter().any(|e| e["type"] == "tool"));
    let row = run_row(&db, "req-2");
    assert_eq!(row["toolRound"].as_i64().unwrap(), 0);
    assert_eq!(
        row["usageEstimated"].as_i64().unwrap(),
        1,
        "mock 不报 usage：必须标记估算"
    );
    assert!(
        row["usedCompletionTokens"].as_i64().unwrap() > 0,
        "缺失用量不能记 0"
    );
}

#[tokio::test]
async fn replay_same_request_returns_receipt_without_rerun() {
    let (_d, db, book, sid) = fixture();
    let a = args(&book, &sid, "[call:scan_book_tree]", "req-3");
    collect(&db, &a).await.0.unwrap().unwrap();
    let (r2, events2) = collect(&db, &a).await;
    assert_eq!(r2.unwrap().unwrap()["replayed"], true);
    assert!(events2.iter().any(|e| e["type"] == "done"));
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) AS c FROM agent_run WHERE request_id=?1",
            "req-3"
        ),
        1
    );
    assert_eq!(run_row(&db, "req-3")["toolRound"].as_i64().unwrap(), 1);
}

#[tokio::test]
async fn foreign_session_bad_book_and_unknown_task_rejected() {
    let (_d, db, book, sid) = fixture();
    let other = molan_core::books::create_book(&db, "别的书", "都市", "第一人称");
    assert!(collect(
        &db,
        &args(other["id"].as_str().unwrap(), &sid, "你好", "req-4")
    )
    .await
    .0
    .is_err());
    assert!(collect(&db, &args("no-such-book", &sid, "你好", "req-5"))
        .await
        .0
        .is_err());
    let bad = with(args(&book, &sid, "你好", "req-5b"), json!({"task": "hack"}));
    assert!(collect(&db, &bad)
        .await
        .0
        .unwrap_err()
        .to_string()
        .contains("未知任务"));
}

#[tokio::test]
async fn pre_cancelled_request_stops_before_model() {
    let (_d, db, book, sid) = fixture();
    chat::request_abort("req-6");
    let (res, _) = collect(&db, &args(&book, &sid, "[call:scan_book_tree]", "req-6")).await;
    assert_eq!(res.unwrap().unwrap()["status"], "interrupted");
    assert_eq!(run_row(&db, "req-6")["toolRound"].as_i64().unwrap(), 0);
    assert!(!chat::take_abort("req-6"), "收尾必须清掉取消标记");
}

#[tokio::test]
async fn rounds_limit_gives_one_final_text_round() {
    let (_d, db, book, sid) = fixture();
    db.exec(
        "INSERT INTO settings(key,value) VALUES('agent_max_tool_rounds','1')",
        &[],
    )
    .unwrap();
    let (res, events) = collect(&db, &args(&book, &sid, "[call:scan_book_tree]", "req-7")).await;
    let out = res.unwrap().unwrap();
    assert_eq!(out["status"], "done", "轮数用尽后模型基于已有结果收尾");
    assert!(events
        .iter()
        .any(|e| e["code"].as_str() == Some("ROUNDS_EXHAUSTED")));
    let row = run_row(&db, "req-7");
    assert_eq!(row["toolRound"].as_i64().unwrap(), 1);
    let turns = agent_run::list_turns(&db, row["id"].as_str().unwrap()).unwrap();
    assert!(!turns.last().unwrap()["content"]
        .as_str()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn failing_tool_is_reported_and_loop_continues() {
    let (_d, db, book, sid) = fixture();
    let (res, events) = collect(&db, &args(&book, &sid, "[call:read_book_file]", "req-8")).await;
    assert_eq!(res.unwrap().unwrap()["status"], "done");
    assert!(events
        .iter()
        .any(|e| e["type"] == "tool" && e["status"] == "error" && e["name"] == "read_book_file"));
}

#[tokio::test]
async fn model_cannot_confirm_or_finalize_by_itself() {
    let (_d, db, book, sid) = fixture();
    let (res, events) = collect(
        &db,
        &args(
            &book,
            &sid,
            "[call:finalize_chapter_draft {\"ch\":1,\"expectedHash\":\"x\"}]",
            "req-fin",
        ),
    )
    .await;
    assert_eq!(res.unwrap().unwrap()["status"], "done");
    let t = events
        .iter()
        .find(|e| e["type"] == "tool" && e["status"] == "error")
        .unwrap();
    assert!(t["summary"].as_str().unwrap().contains("作者"), "{}", t);
    let meta = events.iter().find(|e| e["type"] == "meta").unwrap();
    assert_eq!(meta["tools"], 10, "旧版助手：作者专属动作已从目录移除");
}

#[tokio::test]
async fn chat_task_is_read_only_and_creates_no_artifact() {
    let (_d, db, book, sid) = fixture();
    let a = with(args(&book, &sid, "[call:draft_chapter_outline {\"ch\":1,\"content\":\"细纲内容细纲内容细纲内容细纲内容细纲内容细纲内容\"}]", "req-chat"), json!({"task": "chat"}));
    let (res, events) = collect(&db, &a).await;
    let out = res.unwrap().unwrap();
    assert_eq!(out["status"], "done");
    let t = events
        .iter()
        .find(|e| e["type"] == "tool" && e["status"] == "error")
        .unwrap();
    assert!(t["summary"].as_str().unwrap().contains("不允许"));
    assert!(
        molan_core::files::read_file(&db, &book, "细纲", "细纲_第1章.md").is_none(),
        "聊天无写入副作用"
    );
    assert!(out["artifactIds"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn outline_task_final_text_becomes_outline_artifact() {
    let (_d, db, book, sid) = fixture();
    let a = with(
        args(&book, &sid, "写第2章细纲", "req-ol"),
        json!({"task": "outline", "target": {"ch": 2}}),
    );
    let (res, events) = collect(&db, &a).await;
    let out = res.unwrap().unwrap();
    assert_eq!(out["status"], "done");
    let art = events
        .iter()
        .find(|e| e["type"] == "artifact")
        .expect("必须下发产物事件");
    assert_eq!(art["artifact"]["kind"], "outline_draft");
    assert_eq!(art["artifact"]["state"], "generated");
    assert_eq!(art["artifact"]["target"]["ch"], 2);
    assert_eq!(art["artifact"]["actions"][0]["id"], "save_outline");
    assert!(!art["artifact"]["provenance"]["planHash"]
        .as_str()
        .unwrap()
        .is_empty());
    // 刷新后可从产物列表恢复同一卡片
    let list = molan_core::artifact_view::list_for_session(&db, &book, &sid).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], art["artifact"]["id"]);
}

#[tokio::test]
async fn direct_mode_never_calls_tools() {
    let (_d, db, book, sid) = fixture();
    let a = with(
        args(&book, &sid, "[call:scan_book_tree]", "req-direct"),
        json!({"task": "plot", "mode": "direct"}),
    );
    let (res, events) = collect(&db, &a).await;
    assert_eq!(res.unwrap().unwrap()["status"], "done");
    assert!(!events.iter().any(|e| e["type"] == "tool"));
    assert_eq!(
        events.iter().find(|e| e["type"] == "meta").unwrap()["tools"],
        0
    );
}

#[tokio::test]
async fn revise_without_target_is_blocked_before_model() {
    let (_d, db, book, sid) = fixture();
    let a = with(
        args(&book, &sid, "改一下", "req-rev"),
        json!({"task": "revise"}),
    );
    let (res, events) = collect(&db, &a).await;
    let out = res.unwrap().unwrap();
    assert_eq!(out["code"], "CONTEXT_BLOCKED");
    assert!(
        !events.iter().any(|e| e["type"] == "delta"),
        "阻塞时不调用模型"
    );
    assert_eq!(run_row(&db, "req-rev")["status"], "error");
}

#[tokio::test]
async fn second_request_in_busy_session_is_rejected() {
    let (_d, db, book, sid) = fixture();
    let g = agent_runtime::register_live(&sid, "other-req", "run-x", CancellationToken::new());
    let (res, events) = collect(&db, &args(&book, &sid, "你好", "mine")).await;
    let out = res.unwrap().unwrap();
    assert_eq!(out["status"], "session_busy");
    assert!(events.iter().any(|e| e["code"] == "SESSION_BUSY"));
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) AS c FROM messages WHERE session_id=?1",
            &sid
        ),
        0,
        "被拒请求不落消息"
    );
    drop(g);
    assert_eq!(
        collect(&db, &args(&book, &sid, "你好", "mine2"))
            .await
            .0
            .unwrap()
            .unwrap()["status"],
        "done"
    );
}

#[tokio::test]
async fn abort_inflight_then_next_turn_is_not_poisoned() {
    let (_d, db, book, sid) = fixture();
    let token = CancellationToken::new();
    let g = agent_runtime::register_live(&sid, "inflight", "run-y", token.clone());
    abort_by_args(&json!({"sessionId": sid}));
    assert!(token.is_cancelled(), "在飞运行必须被取消");
    drop(g);
    let (res, events) = collect(&db, &args(&book, &sid, "下一条", "after-abort")).await;
    assert_eq!(
        res.unwrap().unwrap()["status"],
        "done",
        "上一次取消不得毒化下一条"
    );
    assert!(events.iter().any(|e| e["type"] == "delta"));
    // 空闲会话 abort 同样不留痕迹
    abort_by_args(&json!({"sessionId": sid}));
    assert_eq!(
        collect(&db, &args(&book, &sid, "再来", "after-idle"))
            .await
            .0
            .unwrap()
            .unwrap()["status"],
        "done"
    );
}

#[tokio::test]
async fn body_task_drafts_through_chapter_service_into_pending_artifact() {
    let (_d, db, book, sid) = fixture();
    let outline = "第1章细纲：主角登场，夺得入门名额，埋下玉佩伏笔，章末神秘老者出现。";
    molan_core::files::write_file(&db, &book, "细纲", "细纲_第1章.md", outline).unwrap();
    molan_core::outline_confirm::confirm(&db, &book, 1, "细纲_第1章.md", None).unwrap();
    let a = with(
        args(
            &book,
            &sid,
            "[call:draft_chapter_body {\"ch\":1}]",
            "req-body",
        ),
        json!({"task": "body", "target": {"ch": 1}}),
    );
    let (res, events) = collect(&db, &a).await;
    let out = res.unwrap().unwrap();
    assert_eq!(
        out["status"],
        "done",
        "{:?}",
        events
            .iter()
            .filter(|e| e["type"] == "error" || e["type"] == "tool")
            .collect::<Vec<_>>()
    );
    let art = events
        .iter()
        .find(|e| e["type"] == "artifact" && e["artifact"]["kind"] == "body_draft")
        .expect("正文草稿产物");
    assert_eq!(art["artifact"]["state"], "pending_review");
    assert_eq!(art["artifact"]["actions"][0]["id"], "approve");
    // 章节服务内部的模型用量按「提示 + 产出」估算计入本次运行预算，并标记为估算
    let run = db
        .q_json(
            "SELECT used_prompt_tokens, used_completion_tokens, usage_estimated FROM agent_run WHERE request_id='req-body'",
            &[],
        )
        .unwrap();
    let body_chars = art["artifact"]["chars"].as_i64().unwrap();
    let prompt = run[0]["usedPromptTokens"].as_i64().unwrap();
    let completion = run[0]["usedCompletionTokens"].as_i64().unwrap();
    assert!(completion >= body_chars, "{:?} body={}", run, body_chars);
    assert!(prompt > 300, "章节服务的提示字数必须计入：{:?}", run);
    assert_eq!(run[0]["usageEstimated"], 1);
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) AS c FROM pending_chapter WHERE book_id=?1 AND status='pending'",
            &book
        ),
        1
    );
    // 越界章号被服务端拒绝
    let b = with(
        args(
            &book,
            &sid,
            "[call:draft_chapter_body {\"ch\":2}]",
            "req-body2",
        ),
        json!({"task": "body", "target": {"ch": 1}}),
    );
    let (_, ev2) = collect(&db, &b).await;
    assert!(ev2.iter().any(|e| e["type"] == "tool"
        && e["status"] == "error"
        && e["summary"].as_str().unwrap_or("").contains("第1章")));
}

#[tokio::test]
async fn f6_sequential_reentry_same_request_is_rejected_as_running() {
    let (_d, db, book, sid) = fixture();
    let run = agent_run::begin_run(
        &db,
        &BeginRun {
            book_id: book.clone(),
            session_id: sid.clone(),
            request_id: "req-f6".into(),
            task: "agent".into(),
            model: "mock-model".into(),
            target_ch: None,
            max_tool_rounds: MAX_TOOL_ROUNDS_DEFAULT,
            budget_tokens: BUDGET_TOKENS_DEFAULT,
        },
    )
    .unwrap();
    let run_id = run["id"].as_str().unwrap().to_string();
    db.exec(
        "INSERT INTO messages(id,session_id,role,content,context_json,steps_json,result_json,created_at,interrupted) VALUES(?1,?2,'user','前一次在飞的请求',NULL,NULL,NULL,0,0)",
        &[&uuid::Uuid::new_v4().to_string() as &dyn rusqlite::ToSql, &sid],
    )
    .unwrap();
    let (res, events) = collect(&db, &args(&book, &sid, "[call:scan_book_tree]", "req-f6")).await;
    let out = res.unwrap().unwrap();
    assert_eq!(out["status"], "already_running");
    assert_eq!(out["runId"], run_id.as_str());
    assert!(!events
        .iter()
        .any(|e| e["type"] == "meta" || e["type"] == "tool" || e["type"] == "delta"));
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) AS c FROM messages WHERE session_id=?1",
            &sid
        ),
        1
    );
    assert_eq!(run_row(&db, "req-f6")["status"], "running");
    // 真双发：同 requestId 并发只跑一次
    let a1 = args(&book, &sid, "并发一", "req-f6c");
    let a2 = args(&book, &sid, "并发二", "req-f6c");
    let (x, y) = tokio::join!(collect(&db, &a1), collect(&db, &a2));
    let (rx, ry) = (x.0.unwrap().unwrap(), y.0.unwrap().unwrap());
    for st in [&rx["status"], &ry["status"]] {
        assert!(st == "done" || st == "already_running", "{}", st);
    }
    assert_eq!(rx["runId"], ry["runId"]);
    let turns = agent_run::list_turns(&db, rx["runId"].as_str().unwrap()).unwrap();
    assert_eq!(turns.iter().filter(|t| t["role"] == "assistant").count(), 1);
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) AS c FROM messages WHERE session_id=?1 AND role='user'",
            &sid
        ),
        2
    );
}

#[test]
fn tools_unsupported_classifier_is_conservative() {
    assert!(looks_tools_unsupported(&anyhow!(
        "HTTP 400 Bad Request: invalid_request_error unknown parameter tools"
    )));
    assert!(!looks_tools_unsupported(&anyhow!(
        "HTTP 500 internal error"
    )));
    assert!(!looks_tools_unsupported(&anyhow!("400 bad prompt content")));
}

/// 同一轮多个只读调用：并发执行、同轮重复调用只读一次、结果按原顺序回填；运行指标落库并随回执返回。
#[tokio::test]
async fn read_only_round_runs_concurrently_with_cache_and_metrics() {
    let (_d, db, book, sid) = fixture();
    molan_core::files::write_file(&db, &book, "设定", "人物.md", "林照：主角").unwrap();
    let msg = r#"查资料 [call:read_book_file {"group":"设定","name":"人物.md"}] [call:scan_book_tree] [call:read_book_file {"group":"设定","name":"人物.md"}]"#;
    let a = with(args(&book, &sid, msg, "req-par"), json!({"task": "chat"}));
    let (res, events) = collect(&db, &a).await;
    let out = res.unwrap().unwrap();
    assert_eq!(out["status"], "done");
    let done: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "tool" && e["status"] != "running")
        .collect();
    let ids: Vec<&str> = done.iter().map(|e| e["callId"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        vec!["mock-call-1", "mock-call-2", "mock-call-3"],
        "结果按原顺序回填"
    );
    assert!(done.iter().all(|e| e["status"] == "ok"), "{:?}", done);
    let fin = events.iter().find(|e| e["type"] == "done").unwrap();
    let m = &fin["metrics"];
    assert_eq!(
        m["modelCalls"].as_array().unwrap().len(),
        2,
        "工具轮 + 收尾轮"
    );
    assert_eq!(m["tools"].as_array().unwrap().len(), 3);
    assert_eq!(m["cacheHits"], 1, "同轮重复的同参读取只执行一次");
    assert_eq!(m["tools"][0]["parallel"], true);
    assert!(m["firstTokenMs"].is_u64());
    assert_eq!(m["modelCalls"][0]["outcome"], "toolCalls");
    assert_eq!(
        m["modelCalls"][0]["usageSource"], "estimated",
        "mock 不报用量：如实标估算"
    );
    let row = db
        .q_json(
            "SELECT metrics_json FROM agent_run WHERE request_id='req-par'",
            &[],
        )
        .unwrap();
    let stored: Value = serde_json::from_str(row[0]["metricsJson"].as_str().unwrap()).unwrap();
    assert_eq!(stored["cacheHits"], 1, "指标落库，run_status 可读");
}

/// 有副作用的调用顺序执行，并清空只读缓存：其后的同参读取重新执行，不复用副作用之前的结果。
#[tokio::test]
async fn side_effect_call_runs_alone_and_clears_read_cache() {
    let (_d, db, book, sid) = fixture();
    molan_core::files::write_file(&db, &book, "设定", "人物.md", "林照：主角").unwrap();
    let msg = r#"[call:read_book_file {"group":"设定","name":"人物.md"}] [call:create_change_proposal {"group":"设定","name":"人物.md","summary":"补一句","proposedContent":"林照：主角，持玉佩"}] [call:read_book_file {"group":"设定","name":"人物.md"}]"#;
    let a = with(args(&book, &sid, msg, "req-se"), json!({"task": "plot"}));
    let (res, events) = collect(&db, &a).await;
    assert_eq!(res.unwrap().unwrap()["status"], "done");
    let m = &events.iter().find(|e| e["type"] == "done").unwrap()["metrics"];
    let tools = m["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec!["read_book_file", "create_change_proposal", "read_book_file"]
    );
    assert!(tools.iter().all(|t| t["ok"] == true), "{:?}", tools);
    assert_eq!(tools[1]["parallel"], false, "副作用调用单独执行");
    assert_eq!(tools[2]["cached"], false, "副作用之后缓存已清空");
    assert_eq!(m["cacheHits"], 0);
}
