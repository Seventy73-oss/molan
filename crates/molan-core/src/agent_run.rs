//! Agent 运行态账本：对话式 Agent 工具循环的 run/turn 持久化。
//!
//! 定位（交接文档附录 A3.2）：只记录**运行态**（一次 agent_turn 的轮次、工具轨迹、
//! 预算与终态），供断线/重启后恢复与审计；绝不复制"生产真相"——书稿状态仍以
//! 文件 + pending_chapter + continuity 账本为准（pipeline.rs 文件即真相原则）。
//!
//! 幂等键：UNIQUE(session_id, request_id)。调用方缺 requestId 时必须自行生成
//! （对齐 chat.rs 的 auto-uuid 惯例），core 拒绝空键。
use crate::{db::Db, stats::now_ms};
use anyhow::{anyhow, bail, Result};
use serde_json::Value;

pub const MAX_TOOL_ROUNDS_DEFAULT: i64 = 8;
pub const BUDGET_TOKENS_DEFAULT: i64 = 200_000;

pub fn ensure_schema(db: &Db) -> Result<()> {
    db.conn
        .lock()
        .map_err(|_| anyhow!("数据库锁损坏"))?
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_run (
                id TEXT PRIMARY KEY,
                book_id TEXT NOT NULL DEFAULT '',
                session_id TEXT NOT NULL,
                request_id TEXT NOT NULL,
                task TEXT NOT NULL DEFAULT 'agent',
                target_ch INTEGER,
                model TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'running',
                tool_round INTEGER NOT NULL DEFAULT 0,
                max_tool_rounds INTEGER NOT NULL DEFAULT 8,
                budget_tokens INTEGER NOT NULL DEFAULT 0,
                used_prompt_tokens INTEGER NOT NULL DEFAULT 0,
                used_completion_tokens INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                error TEXT NOT NULL DEFAULT '',
                UNIQUE(session_id, request_id));
             CREATE TABLE IF NOT EXISTS agent_turn (
                run_id TEXT NOT NULL,
                turn_index INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL DEFAULT '',
                tool_calls_json TEXT NOT NULL DEFAULT '',
                tool_call_id TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL,
                PRIMARY KEY(run_id, turn_index));
             CREATE INDEX IF NOT EXISTS idx_agent_run_session ON agent_run(session_id, created_at);",
        )?;
    Ok(())
}

/// 一次 agent_turn 的启动参数。target_ch 可空（非章节任务）。
pub struct BeginRun {
    pub book_id: String,
    pub session_id: String,
    pub request_id: String,
    pub task: String,
    pub model: String,
    pub target_ch: Option<i64>,
    pub max_tool_rounds: i64,
    pub budget_tokens: i64,
}

/// 幂等启动/取回 run：同 (session_id, request_id) 重复调用返回既有行，不重复建。
/// 返回 camelCase 行（Db::q_json 口径）。
pub fn begin_run(db: &Db, r: &BeginRun) -> Result<Value> {
    ensure_schema(db)?;
    if r.session_id.trim().is_empty() || r.request_id.trim().is_empty() {
        bail!("agent_run 需要非空 sessionId/requestId");
    }
    let now = now_ms();
    let id = uuid::Uuid::new_v4().to_string();
    db.exec(
        "INSERT OR IGNORE INTO agent_run(id,book_id,session_id,request_id,task,target_ch,model,status,tool_round,max_tool_rounds,budget_tokens,used_prompt_tokens,used_completion_tokens,created_at,updated_at,error)
         VALUES(?1,?2,?3,?4,?5,?6,?7,'running',0,?8,?9,0,0,?10,?10,'')",
        &[
            &id as &dyn rusqlite::ToSql,
            &r.book_id,
            &r.session_id,
            &r.request_id,
            &r.task,
            &r.target_ch,
            &r.model,
            &r.max_tool_rounds,
            &r.budget_tokens,
            &now,
        ],
    )?;
    let rows = db.q_json(
        "SELECT * FROM agent_run WHERE session_id=?1 AND request_id=?2",
        &[&r.session_id, &r.request_id],
    )?;
    rows.into_iter()
        .next()
        .ok_or_else(|| anyhow!("agent_run 写入失败"))
}

pub fn get_run(db: &Db, run_id: &str) -> Result<Option<Value>> {
    ensure_schema(db)?;
    Ok(db
        .q_json("SELECT * FROM agent_run WHERE id=?1", &[&run_id])?
        .into_iter()
        .next())
}

pub fn latest_run_for_session(db: &Db, session_id: &str) -> Result<Option<Value>> {
    ensure_schema(db)?;
    Ok(db
        .q_json(
            "SELECT * FROM agent_run WHERE session_id=?1 ORDER BY created_at DESC, id DESC LIMIT 1",
            &[&session_id],
        )?
        .into_iter()
        .next())
}

/// 追加一条轮次记录（assistant 文本 / assistant 工具调用请求 / tool 结果）。
/// turn_index 自动递增；调用方串行写（同一 run 只有一个循环在跑）。
pub fn record_turn(
    db: &Db,
    run_id: &str,
    role: &str,
    content: &str,
    tool_calls_json: &str,
    tool_call_id: &str,
) -> Result<i64> {
    ensure_schema(db)?;
    let next: i64 = db
        .q_json(
            "SELECT COALESCE(MAX(turn_index),-1)+1 AS n FROM agent_turn WHERE run_id=?1",
            &[&run_id],
        )?
        .first()
        .and_then(|r| r["n"].as_i64())
        .unwrap_or(0);
    db.exec(
        "INSERT INTO agent_turn(run_id,turn_index,role,content,tool_calls_json,tool_call_id,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        &[
            &run_id as &dyn rusqlite::ToSql,
            &next,
            &role,
            &content,
            &tool_calls_json,
            &tool_call_id,
            &now_ms(),
        ],
    )?;
    Ok(next)
}

pub fn list_turns(db: &Db, run_id: &str) -> Result<Vec<Value>> {
    ensure_schema(db)?;
    db.q_json(
        "SELECT * FROM agent_turn WHERE run_id=?1 ORDER BY turn_index",
        &[&run_id],
    )
}

fn usage_i64(v: &Value, key: &str) -> i64 {
    let node = if v.get(key).is_some() { v } else { &v["usage"] };
    match node.get(key) {
        Some(Value::Number(n)) => n
            .as_i64()
            .unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Some(Value::String(s)) => s.parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

/// 累加一轮用量，返回累计总 tokens（prompt+completion）。usage 缺省记 0。
pub fn charge_usage(db: &Db, run_id: &str, usage: Option<&Value>) -> Result<i64> {
    ensure_schema(db)?;
    let (p, c) = match usage {
        Some(u) => (
            usage_i64(u, "prompt_tokens"),
            usage_i64(u, "completion_tokens"),
        ),
        None => (0, 0),
    };
    db.exec(
        "UPDATE agent_run SET used_prompt_tokens=used_prompt_tokens+?2, used_completion_tokens=used_completion_tokens+?3, updated_at=?4 WHERE id=?1",
        &[&run_id as &dyn rusqlite::ToSql, &p, &c, &now_ms()],
    )?;
    let row = get_run(db, run_id)?;
    Ok(row
        .map(|r| {
            r["usedPromptTokens"].as_i64().unwrap_or(0)
                + r["usedCompletionTokens"].as_i64().unwrap_or(0)
        })
        .unwrap_or(0))
}

/// token 预算是否耗尽。budget_tokens<=0 视为不限预算（仍受轮数上限约束）。
pub fn budget_exhausted(db: &Db, run_id: &str) -> Result<bool> {
    let Some(r) = get_run(db, run_id)? else {
        return Ok(false);
    };
    let budget = r["budgetTokens"].as_i64().unwrap_or(0);
    if budget <= 0 {
        return Ok(false);
    }
    let used = r["usedPromptTokens"].as_i64().unwrap_or(0)
        + r["usedCompletionTokens"].as_i64().unwrap_or(0);
    Ok(used >= budget)
}

/// 工具轮数 +1 并返回新值；循环每完成一轮工具调用后调用。
pub fn bump_round(db: &Db, run_id: &str) -> Result<i64> {
    ensure_schema(db)?;
    db.exec(
        "UPDATE agent_run SET tool_round=tool_round+1, updated_at=?2 WHERE id=?1",
        &[&run_id as &dyn rusqlite::ToSql, &now_ms()],
    )?;
    Ok(get_run(db, run_id)?
        .and_then(|r| r["toolRound"].as_i64())
        .unwrap_or(0))
}

pub fn rounds_exhausted(db: &Db, run_id: &str) -> Result<bool> {
    let Some(r) = get_run(db, run_id)? else {
        return Ok(false);
    };
    let max = r["maxToolRounds"]
        .as_i64()
        .unwrap_or(MAX_TOOL_ROUNDS_DEFAULT);
    Ok(max > 0 && r["toolRound"].as_i64().unwrap_or(0) >= max)
}

/// 终态收敛：done|interrupted|budget_exhausted|tools_unsupported|error。
/// 只允许从非终态迁移一次；重复 finish 保持首个终态（幂等，防止覆盖审计结果）。
pub fn finish_run(db: &Db, run_id: &str, status: &str, error: &str) -> Result<()> {
    ensure_schema(db)?;
    if !matches!(
        status,
        "done" | "interrupted" | "budget_exhausted" | "tools_unsupported" | "error"
    ) {
        bail!("非法 agent_run 终态：{}", status);
    }
    db.exec(
        "UPDATE agent_run SET status=?2, error=?3, updated_at=?4 WHERE id=?1 AND status='running'",
        &[&run_id as &dyn rusqlite::ToSql, &status, &error, &now_ms()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        (dir, db)
    }

    fn begin(db: &Db, req: &str, budget: i64) -> Value {
        begin_run(
            db,
            &BeginRun {
                book_id: "b1".into(),
                session_id: "s1".into(),
                request_id: req.into(),
                task: "agent".into(),
                model: "mock".into(),
                target_ch: None,
                max_tool_rounds: MAX_TOOL_ROUNDS_DEFAULT,
                budget_tokens: budget,
            },
        )
        .unwrap()
    }

    #[test]
    fn begin_is_idempotent_per_session_request() {
        let (_d, db) = fixture();
        let a = begin(&db, "r1", 0);
        let b = begin(&db, "r1", 0);
        assert_eq!(a["id"], b["id"], "同 (session,request) 必须返回同一 run");
        let n = db
            .q_json("SELECT COUNT(*) AS c FROM agent_run", &[])
            .unwrap()[0]["c"]
            .as_i64()
            .unwrap();
        assert_eq!(n, 1);
        let c = begin(&db, "r2", 0);
        assert_ne!(a["id"], c["id"], "不同 requestId 是新 run");
    }

    #[test]
    fn rejects_empty_keys() {
        let (_d, db) = fixture();
        assert!(begin_run(
            &db,
            &BeginRun {
                book_id: "b".into(),
                session_id: "".into(),
                request_id: "r".into(),
                task: "agent".into(),
                model: "m".into(),
                target_ch: None,
                max_tool_rounds: 8,
                budget_tokens: 0,
            }
        )
        .is_err());
    }

    #[test]
    fn turns_usage_and_budget() {
        let (_d, db) = fixture();
        let run = begin(&db, "r1", 250);
        let id = run["id"].as_str().unwrap();
        assert_eq!(
            record_turn(&db, id, "assistant", "", "[{\"id\":\"c1\"}]", "").unwrap(),
            0
        );
        assert_eq!(record_turn(&db, id, "tool", "结果", "", "c1").unwrap(), 1);
        let turns = list_turns(&db, id).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1]["role"], "tool");

        let total = charge_usage(
            &db,
            id,
            Some(&serde_json::json!({"prompt_tokens":100,"completion_tokens":50})),
        )
        .unwrap();
        assert_eq!(total, 150);
        assert!(!budget_exhausted(&db, id).unwrap());
        let total = charge_usage(
            &db,
            id,
            Some(&serde_json::json!({"usage":{"prompt_tokens":"100","completion_tokens":50.7}})),
        )
        .unwrap();
        assert_eq!(total, 300, "字符串/浮点用量也要能累加");
        assert!(budget_exhausted(&db, id).unwrap(), "300 >= 250 必须耗尽");

        // budget<=0 不限预算
        let run2 = begin(&db, "r2", 0);
        let id2 = run2["id"].as_str().unwrap();
        charge_usage(&db, id2, Some(&serde_json::json!({"prompt_tokens":999999}))).unwrap();
        assert!(!budget_exhausted(&db, id2).unwrap());
    }

    #[test]
    fn rounds_and_finish_are_guarded() {
        let (_d, db) = fixture();
        let run = begin(&db, "r1", 0);
        let id = run["id"].as_str().unwrap();
        for i in 1..=8 {
            assert_eq!(bump_round(&db, id).unwrap(), i);
        }
        assert!(rounds_exhausted(&db, id).unwrap());
        finish_run(&db, id, "done", "").unwrap();
        // 幂等：二次 finish 不覆盖首个终态
        finish_run(&db, id, "error", "后来的失败").unwrap();
        let r = get_run(&db, id).unwrap().unwrap();
        assert_eq!(r["status"], "done");
        assert!(
            finish_run(&db, id, "bogus", "").is_err(),
            "非法终态必须拒绝"
        );
        assert!(latest_run_for_session(&db, "s1").unwrap().is_some());
    }
}
