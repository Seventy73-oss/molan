//! DocumentWriteService：统一的文档写入计划、执行与回执。
//!
//! 覆盖五种明确操作：新建（create）、全文替换（replace）、追加（append）、
//! 指定位置插入（insert）、选区替换（replace_range）。规则：
//! - 基线用内容 SHA-256（与 `file_revision.content_hash` 同算法）做 CAS；除 create 外必须带基线；
//! - 偏移一律是**浏览器 UTF-16 码元**，服务端换算为字节下标；落在代理对中间直接拒绝；
//! - 选区替换必须携带基线中该范围的原文（锚点），不做任何模糊匹配；
//! - 两阶段账本：先记 `prepared`（含写前/写后 hash），落盘后记 `committed`；
//!   文件系统与 SQLite 之间没有全局事务，崩溃窗口由账本 + 磁盘 hash 对账恢复；
//! - 文件已写而派生索引失败 → `commit=committed, index=failed`（部分成功），绝不回滚稿件；
//! - 同一 (bookId, idempotencyKey) 重放返回原回执；同键异计划拒绝。
//!
//! 正文待审（REVIEW_GROUP）由章节服务管理（写入与待审登记同锁），不经本服务。

use crate::continuity::content_hash;
use crate::db::{Db, REVIEW_GROUP};
use crate::files;
use crate::stats::now_ms;
use anyhow::{anyhow, Result};
use rusqlite::params;
use serde::Serialize;
use serde_json::{json, Value};

/// 单文档内容上限（字符）。超出视为误用（整书粘贴等），拒绝而非截断。
pub const MAX_CONTENT_CHARS: usize = 2_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOp {
    Create,
    Replace,
    Append,
    Insert,
    ReplaceRange,
}

impl WriteOp {
    pub fn parse(s: &str) -> Option<WriteOp> {
        Some(match s.trim() {
            "create" => WriteOp::Create,
            "replace" => WriteOp::Replace,
            "append" => WriteOp::Append,
            "insert" => WriteOp::Insert,
            "replace_range" => WriteOp::ReplaceRange,
            _ => return None,
        })
    }
    pub fn id(self) -> &'static str {
        match self {
            WriteOp::Create => "create",
            WriteOp::Replace => "replace",
            WriteOp::Append => "append",
            WriteOp::Insert => "insert",
            WriteOp::ReplaceRange => "replace_range",
        }
    }
    /// 片段类操作：只改动文档的一部分。
    pub fn is_partial(self) -> bool {
        matches!(
            self,
            WriteOp::Append | WriteOp::Insert | WriteOp::ReplaceRange
        )
    }
}

/// 写入发起方：作者亲手编辑（可改 locked 文件）或 AI 产物交付（受 locked 约束）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    User,
    Ai,
}

impl Actor {
    fn id(self) -> &'static str {
        match self {
            Actor::User => "user",
            Actor::Ai => "ai",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WritePlan {
    pub book_id: String,
    pub group: String,
    pub name: String,
    pub op: WriteOp,
    pub base_hash: Option<String>,
    pub content: String,
    /// UTF-16 码元偏移；insert 用 start，replace_range 用 [start, end)。
    pub start: Option<usize>,
    pub end: Option<usize>,
    /// replace_range：基线中 [start,end) 的原文，逐字比较。
    pub expected: Option<String>,
    pub actor: Actor,
    pub idempotency_key: String,
    /// 来源（产物 id/修订/条目），原样写入回执。
    pub source: Value,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WriteError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_hash: Option<String>,
}

/// 写入回执。`commit`: committed | noop | conflict | failed；`index`: ok | failed | skipped。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WriteReceipt {
    pub write_id: String,
    pub idempotency_key: String,
    pub book_id: String,
    pub group: String,
    pub name: String,
    pub op: String,
    pub actor: String,
    pub commit: String,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    pub revision: Option<i64>,
    pub index: String,
    pub index_error: Option<String>,
    pub error: Option<WriteError>,
    pub replayed: bool,
    pub recovered: bool,
    pub chars: i64,
    pub ts: i64,
    pub source: Value,
}

impl WriteReceipt {
    pub fn ok(&self) -> bool {
        matches!(self.commit.as_str(), "committed" | "noop")
    }
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

pub fn ensure_schema(db: &Db) -> Result<()> {
    let conn = db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS doc_write_log (
            write_id TEXT PRIMARY KEY,
            idem_key TEXT NOT NULL,
            book_id TEXT NOT NULL,
            group_name TEXT NOT NULL,
            name TEXT NOT NULL,
            op TEXT NOT NULL,
            actor TEXT NOT NULL,
            plan_hash TEXT NOT NULL,
            before_hash TEXT,
            after_hash TEXT,
            phase TEXT NOT NULL,
            index_status TEXT NOT NULL DEFAULT 'skipped',
            revision INTEGER,
            receipt_json TEXT NOT NULL DEFAULT '{}',
            source_json TEXT NOT NULL DEFAULT '{}',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            UNIQUE(book_id, idem_key)
        );
        CREATE INDEX IF NOT EXISTS idx_doc_write_target ON doc_write_log(book_id, group_name, name, created_at);",
    )?;
    Ok(())
}

// ---------- UTF-16 偏移换算 ----------

/// UTF-16 码元偏移 → 字节下标。偏移落在代理对中间或越界返回 None。
pub fn utf16_to_byte(s: &str, off: usize) -> Option<usize> {
    let mut u = 0usize;
    for (b, ch) in s.char_indices() {
        if u == off {
            return Some(b);
        }
        u += ch.len_utf16();
        if u > off {
            return None;
        }
    }
    (u == off).then_some(s.len())
}

/// 字节下标 → UTF-16 码元偏移（调用方保证 b 在字符边界）。
pub fn byte_to_utf16(s: &str, b: usize) -> usize {
    s[..b].encode_utf16().count()
}

pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

// ---------- 读取 ----------

fn revision_of(db: &Db, book: &str, group: &str, name: &str) -> Option<i64> {
    db.q_json(
        "SELECT revision FROM file_revision WHERE book_id=?1 AND group_name=?2 AND name=?3",
        &[&book as &dyn rusqlite::ToSql, &group, &name],
    )
    .ok()
    .and_then(|r| r.first().and_then(|x| x["revision"].as_i64()))
}

/// 带版本信息的读取：`exists=false` 与空文件明确区分；hash 用于后续 CAS。
pub fn read_doc(db: &Db, book: &str, group: &str, name: &str) -> Result<Value> {
    let canon = files::normalize_group(group);
    let content = files::read_checked(db, book, &canon, name)?;
    let flags = (
        files::file_flag(db, book, &canon, name, "locked"),
        files::file_flag(db, book, &canon, name, "aiOff"),
    );
    Ok(match content {
        Some(c) => json!({
            "exists": true, "bookId": book, "group": canon, "name": name,
            "content": c, "hash": content_hash(&c), "chars": c.chars().count(),
            "utf16Len": utf16_len(&c), "revision": revision_of(db, book, &canon, name),
            "locked": flags.0, "aiOff": flags.1,
        }),
        None => json!({
            "exists": false, "bookId": book, "group": canon, "name": name,
            "content": "", "hash": Value::Null, "chars": 0, "utf16Len": 0,
            "revision": Value::Null, "locked": flags.0, "aiOff": flags.1,
        }),
    })
}

// ---------- 计划解析 ----------

fn opt_usize(v: &Value) -> Option<usize> {
    v.as_u64().map(|n| n as usize)
}

impl WritePlan {
    /// 从 IPC 参数构造（作者写入，actor=User）。缺少/非法字段返回 Err，信息可直接给作者看。
    pub fn from_args(args: &Value) -> Result<WritePlan> {
        let s = |k: &str| {
            args.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let op = WriteOp::parse(&s("op"))
            .ok_or_else(|| anyhow!("op 必须是 create/replace/append/insert/replace_range"))?;
        let key = s("idempotencyKey");
        if key.trim().is_empty() || key.len() > 200 {
            return Err(anyhow!("缺少或非法的 idempotencyKey"));
        }
        Ok(WritePlan {
            book_id: s("bookId").trim().to_string(),
            group: s("group"),
            name: s("name").trim().to_string(),
            op,
            base_hash: Some(s("baseHash")).filter(|h| !h.trim().is_empty()),
            content: s("content"),
            start: opt_usize(&args["start"]),
            end: opt_usize(&args["end"]),
            expected: args
                .get("expected")
                .and_then(Value::as_str)
                .map(str::to_string),
            actor: Actor::User,
            idempotency_key: key,
            source: args.get("source").cloned().unwrap_or(Value::Null),
        })
    }

    fn plan_hash(&self, canon_group: &str) -> String {
        content_hash(
            &json!([
                self.book_id,
                canon_group,
                self.name,
                self.op.id(),
                self.base_hash,
                content_hash(&self.content),
                self.start,
                self.end,
                self.expected.as_deref().map(content_hash),
                self.actor.id(),
            ])
            .to_string(),
        )
    }
}

// ---------- 故障注入（仅测试） ----------

#[cfg(test)]
thread_local! {
    pub(crate) static FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn fault(point: &'static str) -> Result<()> {
    if FAULT.with(|f| f.get()) == Some(point) {
        FAULT.with(|f| f.set(None));
        return Err(anyhow!("注入故障：{}", point));
    }
    Ok(())
}

#[cfg(not(test))]
#[inline(always)]
fn fault(_point: &'static str) -> Result<()> {
    Ok(())
}

// ---------- 执行 ----------

struct Ctx<'a> {
    plan: &'a WritePlan,
    canon: String,
    write_id: String,
    ts: i64,
}

impl Ctx<'_> {
    fn receipt(&self, commit: &str) -> WriteReceipt {
        WriteReceipt {
            write_id: self.write_id.clone(),
            idempotency_key: self.plan.idempotency_key.clone(),
            book_id: self.plan.book_id.clone(),
            group: self.canon.clone(),
            name: self.plan.name.clone(),
            op: self.plan.op.id().to_string(),
            actor: self.plan.actor.id().to_string(),
            commit: commit.to_string(),
            before_hash: None,
            after_hash: None,
            revision: None,
            index: "skipped".into(),
            index_error: None,
            error: None,
            replayed: false,
            recovered: false,
            chars: 0,
            ts: self.ts,
            source: self.plan.source.clone(),
        }
    }
    fn reject(
        &self,
        commit: &str,
        code: &str,
        message: String,
        current: Option<String>,
    ) -> WriteReceipt {
        let mut r = self.receipt(commit);
        r.error = Some(WriteError {
            code: code.into(),
            message,
            current_hash: current,
        });
        r
    }
}

/// 计算新内容。返回 Err((code, message)) 表示冲突/非法范围。
fn apply(plan: &WritePlan, current: &str) -> std::result::Result<String, (&'static str, String)> {
    let b =
        |off: Option<usize>, what: &str| -> std::result::Result<usize, (&'static str, String)> {
            let off = off.ok_or(("RANGE_INVALID", format!("缺少{}偏移", what)))?;
            utf16_to_byte(current, off).ok_or((
                "RANGE_INVALID",
                format!(
                    "{}偏移 {} 越界或落在多字节字符中间（文档长度 {}）",
                    what,
                    off,
                    utf16_len(current)
                ),
            ))
        };
    Ok(match plan.op {
        WriteOp::Create | WriteOp::Replace => plan.content.clone(),
        WriteOp::Append => format!("{}{}", current, plan.content),
        WriteOp::Insert => {
            let at = b(plan.start, "插入")?;
            format!("{}{}{}", &current[..at], plan.content, &current[at..])
        }
        WriteOp::ReplaceRange => {
            let (s, e) = (b(plan.start, "选区起点")?, b(plan.end, "选区终点")?);
            if s >= e {
                return Err(("RANGE_INVALID", "选区为空或起止颠倒".into()));
            }
            let expected = plan
                .expected
                .as_deref()
                .ok_or(("RANGE_INVALID", "选区替换必须携带原选区文本".to_string()))?;
            if &current[s..e] != expected {
                return Err((
                    "SELECTION_CHANGED",
                    "原选区文本已变化，拒绝按位置覆盖；请重新选择或比较后手动合并".into(),
                ));
            }
            format!("{}{}{}", &current[..s], plan.content, &current[e..])
        }
    })
}

fn load_ledger(db: &Db, book: &str, key: &str) -> Result<Option<Value>> {
    Ok(db
        .q_json(
            "SELECT * FROM doc_write_log WHERE book_id=?1 AND idem_key=?2",
            &[&book as &dyn rusqlite::ToSql, &key],
        )?
        .into_iter()
        .next())
}

fn stored_receipt(row: &Value) -> Option<WriteReceipt> {
    let v: Value = serde_json::from_str(row["receiptJson"].as_str()?).ok()?;
    Some(WriteReceipt {
        write_id: v["writeId"].as_str()?.to_string(),
        idempotency_key: v["idempotencyKey"].as_str()?.to_string(),
        book_id: v["bookId"].as_str()?.to_string(),
        group: v["group"].as_str()?.to_string(),
        name: v["name"].as_str()?.to_string(),
        op: v["op"].as_str()?.to_string(),
        actor: v["actor"].as_str().unwrap_or("user").to_string(),
        commit: v["commit"].as_str()?.to_string(),
        before_hash: v["beforeHash"].as_str().map(str::to_string),
        after_hash: v["afterHash"].as_str().map(str::to_string),
        revision: v["revision"].as_i64(),
        index: v["index"].as_str().unwrap_or("skipped").to_string(),
        index_error: v["indexError"].as_str().map(str::to_string),
        error: None,
        replayed: false,
        recovered: v["recovered"].as_bool().unwrap_or(false),
        chars: v["chars"].as_i64().unwrap_or(0),
        ts: v["ts"].as_i64().unwrap_or(0),
        source: v["source"].clone(),
    })
}

fn finish_ledger(db: &Db, r: &WriteReceipt, phase: &str) -> Result<()> {
    let n = db.exec(
        "UPDATE doc_write_log SET phase=?2, index_status=?3, revision=?4, receipt_json=?5, updated_at=?6 WHERE write_id=?1",
        &[
            &r.write_id as &dyn rusqlite::ToSql,
            &phase,
            &r.index,
            &r.revision,
            &r.to_json().to_string(),
            &now_ms(),
        ],
    )?;
    if n != 1 {
        return Err(anyhow!("写入账本行缺失：{}", r.write_id));
    }
    Ok(())
}

/// 执行写入计划。可预期的冲突/拒绝以回执返回（`commit=conflict|failed`），
/// 只有数据库不可用等意外才返回 Err。
pub fn execute(db: &Db, plan: &WritePlan) -> Result<WriteReceipt> {
    let canon = files::normalize_group(&plan.group);
    let ctx = Ctx {
        plan,
        canon: canon.clone(),
        write_id: uuid::Uuid::new_v4().to_string(),
        ts: now_ms(),
    };
    // ---- 静态校验 ----
    if canon == "_invalid_" {
        return Ok(ctx.reject(
            "failed",
            "GROUP_INVALID",
            format!("非法分组：{:?}", plan.group),
            None,
        ));
    }
    if canon == REVIEW_GROUP {
        return Ok(ctx.reject(
            "failed",
            "GROUP_FORBIDDEN",
            "「正文待审」由章节服务管理，请使用「提交待审」".into(),
            None,
        ));
    }
    if plan.name.is_empty()
        || files::safe_name(&plan.name) != plan.name
        || plan.name.chars().count() > 120
    {
        return Ok(ctx.reject(
            "failed",
            "NAME_INVALID",
            format!("文件名不合法：{:?}", plan.name),
            None,
        ));
    }
    if plan.op == WriteOp::Create && !(plan.name.ends_with(".md") || plan.name.ends_with(".txt")) {
        return Ok(ctx.reject(
            "failed",
            "NAME_INVALID",
            "新建文档须以 .md 或 .txt 结尾".into(),
            None,
        ));
    }
    if plan.content.chars().count() > MAX_CONTENT_CHARS {
        return Ok(ctx.reject(
            "failed",
            "CONTENT_TOO_LARGE",
            "内容超过单文档上限".into(),
            None,
        ));
    }
    if !files::valid_book_id(db, &plan.book_id) {
        return Ok(ctx.reject("failed", "BOOK_INVALID", "作品不存在或已删除".into(), None));
    }
    let plan_hash = plan.plan_hash(&canon);

    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());

    // ---- 幂等与崩溃对账 ----
    if let Some(row) = load_ledger(db, &plan.book_id, &plan.idempotency_key)? {
        if row["planHash"].as_str() != Some(plan_hash.as_str()) {
            return Ok(ctx.reject(
                "failed",
                "IDEMPOTENCY_MISMATCH",
                "同一幂等键对应了不同的写入内容，已拒绝".into(),
                None,
            ));
        }
        let phase = row["phase"].as_str().unwrap_or("");
        if matches!(phase, "committed" | "noop") {
            if let Some(mut r) = stored_receipt(&row) {
                r.replayed = true;
                return Ok(r);
            }
        }
        if phase == "prepared" {
            let cur = files::read_checked(db, &plan.book_id, &canon, &plan.name)?;
            let cur_hash = cur.as_deref().map(content_hash);
            if cur_hash.is_some() && cur_hash.as_deref() == row["afterHash"].as_str() {
                // 落盘已完成、账本未收尾：补记 committed，不重写文件。
                let mut r = ctx.receipt("committed");
                r.write_id = row["writeId"].as_str().unwrap_or_default().to_string();
                r.before_hash = row["beforeHash"].as_str().map(str::to_string);
                r.after_hash = cur_hash;
                r.revision = revision_of(db, &plan.book_id, &canon, &plan.name);
                r.index = "ok".into();
                r.recovered = true;
                r.chars = cur.map(|c| c.chars().count() as i64).unwrap_or(0);
                finish_ledger(db, &r, "committed")?;
                return Ok(r);
            }
            // 未落盘（或已被他人改动）：丢弃 prepared，按当前磁盘重新判定。
            db.exec(
                "DELETE FROM doc_write_log WHERE write_id=?1",
                &[&row["writeId"].as_str().unwrap_or("") as &dyn rusqlite::ToSql],
            )?;
        }
    }

    // ---- 读取当前并计算新内容 ----
    let current = files::read_checked(db, &plan.book_id, &canon, &plan.name)?;
    let cur_hash = current.as_deref().map(content_hash);
    if plan.actor == Actor::Ai && files::file_flag(db, &plan.book_id, &canon, &plan.name, "locked")
    {
        return Ok(ctx.reject(
            "failed",
            "LOCKED",
            "文件已被作者锁定，AI 产物不能写入；请解锁或另存为新文档".into(),
            cur_hash,
        ));
    }
    let new_content = match (plan.op, current.as_deref()) {
        (WriteOp::Create, Some(c)) => {
            if c == plan.content {
                let mut r = ctx.receipt("noop");
                r.after_hash = cur_hash.clone();
                r.before_hash = cur_hash;
                r.chars = c.chars().count() as i64;
                return Ok(r);
            }
            return Ok(ctx.reject(
                "conflict",
                "TARGET_EXISTS",
                format!(
                    "目标已存在且内容不同：{}/{}；请改名或选择替换",
                    canon, plan.name
                ),
                cur_hash,
            ));
        }
        (WriteOp::Create, None) => plan.content.clone(),
        (_, None) => {
            return Ok(ctx.reject(
                "conflict",
                "TARGET_MISSING",
                format!("目标文档不存在：{}/{}", canon, plan.name),
                None,
            ))
        }
        (_, Some(c)) => {
            let base = match plan.base_hash.as_deref() {
                Some(b) => b,
                None => {
                    return Ok(ctx.reject(
                        "failed",
                        "BASE_REQUIRED",
                        "修改已有文档必须携带基线 hash".into(),
                        cur_hash,
                    ))
                }
            };
            if Some(base) != cur_hash.as_deref() {
                return Ok(ctx.reject(
                    "conflict",
                    "BASE_CHANGED",
                    "文档在你编辑/生成期间已被修改，未写入；请比较后合并或另存".into(),
                    cur_hash,
                ));
            }
            match apply(plan, c) {
                Ok(n) => n,
                Err((code, msg)) => {
                    let commit = if code == "SELECTION_CHANGED" {
                        "conflict"
                    } else {
                        "failed"
                    };
                    return Ok(ctx.reject(commit, code, msg, cur_hash));
                }
            }
        }
    };
    if current.as_deref() == Some(new_content.as_str()) {
        let mut r = ctx.receipt("noop");
        r.before_hash = cur_hash.clone();
        r.after_hash = cur_hash;
        r.chars = new_content.chars().count() as i64;
        return Ok(r);
    }
    let after_hash = content_hash(&new_content);

    // ---- 阶段一：账本 prepared ----
    db.exec(
        "INSERT INTO doc_write_log(write_id,idem_key,book_id,group_name,name,op,actor,plan_hash,before_hash,after_hash,phase,source_json,created_at,updated_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'prepared',?11,?12,?12)",
        &[
            &ctx.write_id as &dyn rusqlite::ToSql, &plan.idempotency_key, &plan.book_id, &canon,
            &plan.name, &plan.op.id(), &plan.actor.id(), &plan_hash, &cur_hash, &after_hash,
            &plan.source.to_string(), &ctx.ts,
        ],
    )?;
    fault("after_prepare")?;

    // ---- 落盘（原子写 + 版本快照 + 派生索引） ----
    let written = if current.is_none() {
        files::write_new_locked_outcome(db, &plan.book_id, &canon, &plan.name, &new_content)
    } else {
        files::write_file_locked_outcome(db, &plan.book_id, &canon, &plan.name, &new_content)
    };
    let index_err = match written {
        Ok(idx) => idx,
        Err(e) => {
            // 文件未写入：撤销 prepared，原文保持不变。
            let _ = db.exec(
                "DELETE FROM doc_write_log WHERE write_id=?1",
                &[&ctx.write_id as &dyn rusqlite::ToSql],
            );
            return Ok(ctx.reject(
                "failed",
                "IO",
                format!("写入失败，原文未改动：{}", e),
                cur_hash,
            ));
        }
    };
    fault("after_file_write")?;

    // ---- 阶段二：账本 committed ----
    let mut r = ctx.receipt("committed");
    r.before_hash = cur_hash;
    r.after_hash = Some(after_hash);
    r.chars = new_content.chars().count() as i64;
    r.revision = revision_of(db, &plan.book_id, &canon, &plan.name);
    match index_err {
        None => r.index = "ok".into(),
        Some(e) => {
            r.index = "failed".into();
            r.index_error = Some(e);
        }
    }
    if let Err(e) = finish_ledger(db, &r, "committed") {
        // 文件已保存：只报告账本收尾失败，下次同键重试或启动对账会补记。
        r.index_error = Some(format!(
            "{}；账本收尾失败：{}",
            r.index_error.clone().unwrap_or_default(),
            e
        ));
        r.index = "failed".into();
    }
    Ok(r)
}

/// 启动对账：处理崩溃遗留的 prepared 行。磁盘 hash == afterHash → 补记 committed；
/// == beforeHash（或仍不存在）→ aborted；其他 → aborted 并记录冲突。返回处理行数。
pub fn recover(db: &Db) -> Result<usize> {
    ensure_schema(db)?;
    let rows = db.q_json("SELECT * FROM doc_write_log WHERE phase='prepared'", &[])?;
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut n = 0;
    for row in rows {
        let (book, group, name) = (
            row["bookId"].as_str().unwrap_or(""),
            row["groupName"].as_str().unwrap_or(""),
            row["name"].as_str().unwrap_or(""),
        );
        let cur = files::read_checked(db, book, group, name).ok().flatten();
        let cur_hash = cur.as_deref().map(content_hash);
        let phase = if cur_hash.is_some() && cur_hash.as_deref() == row["afterHash"].as_str() {
            "committed"
        } else {
            "aborted"
        };
        let receipt = json!({
            "writeId": row["writeId"], "idempotencyKey": row["idemKey"], "bookId": book,
            "group": group, "name": name, "op": row["op"], "actor": row["actor"], "commit": phase,
            "beforeHash": row["beforeHash"], "afterHash": row["afterHash"], "index": "skipped",
            "recovered": true, "ts": now_ms(), "source": serde_json::from_str::<Value>(row["sourceJson"].as_str().unwrap_or("null")).unwrap_or(Value::Null),
        });
        let conn = db.conn.lock().map_err(|_| anyhow!("数据库锁损坏"))?;
        conn.execute(
            "UPDATE doc_write_log SET phase=?2, receipt_json=?3, updated_at=?4 WHERE write_id=?1 AND phase='prepared'",
            params![row["writeId"].as_str().unwrap_or(""), phase, receipt.to_string(), now_ms()],
        )?;
        n += 1;
    }
    Ok(n)
}

/// 某文档最近的写入回执（新→旧）。
pub fn history(db: &Db, book: &str, group: &str, name: &str, limit: i64) -> Result<Value> {
    let canon = files::normalize_group(group);
    let rows = db.q_json(
        "SELECT receipt_json FROM doc_write_log WHERE book_id=?1 AND group_name=?2 AND name=?3 AND phase IN ('committed','aborted') ORDER BY created_at DESC LIMIT ?4",
        &[&book as &dyn rusqlite::ToSql, &canon, &name, &limit.clamp(1, 200)],
    )?;
    Ok(json!(rows
        .iter()
        .filter_map(|r| serde_json::from_str::<Value>(r["receiptJson"].as_str().unwrap_or("")).ok())
        .collect::<Vec<_>>()))
}

#[path = "doc_write_external.rs"]
mod external;
pub use external::{record_ai_create, record_external};

#[cfg(test)]
#[path = "doc_write_tests.rs"]
mod tests;
