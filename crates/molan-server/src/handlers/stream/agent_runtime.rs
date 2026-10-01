//! Agent 运行时：任务作用域与工具权限、计划/上下文冻结、在飞登记、产物落库、运行状态查询。
//!
//! 权限原则：模型能调用什么由「本次任务 + 服务端作用域」决定，不由提示词或自然语言决定。
//! - 确认细纲 / 定稿永远不给模型（只能由作者在产物卡/待审面板上执行）；
//! - 聊天、审稿、总结、修改等只读工具 + 最终文本形成产物，由作者决定落盘；
//! - 正文任务可调用章节服务起草（产物进「正文待审」，不是定稿），且章号绑定本次目标章；
//! - 技能只影响提示，不扩大工具权限。
use super::agent_tools;
use crate::AppState;
use anyhow::{anyhow, bail, Result};
use molan_core::artifact::{self, NewArtifact};
use molan_core::db::Db;
use molan_core::skill_resolver::{self, Selection};
use molan_core::task_kind::TaskKind;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

pub(crate) const READ_TOOLS: [&str; 7] = [
    "scan_book_tree",
    "read_book_file",
    "get_pipeline_state",
    "list_pending_chapters",
    "get_chapter_context",
    "list_skills",
    "get_effective_skills",
];
/// 作者专属动作：任何作用域都不暴露给模型。
const AUTHOR_ONLY: [&str; 2] = ["confirm_chapter_outline", "finalize_chapter_draft"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// 模型自主调用作用域内工具。
    Agent,
    /// 无工具，单次生成（模型不支持 tools 或作者选择直接生成）。
    Direct,
}

#[derive(Debug, Clone)]
pub(crate) struct RunSpec {
    /// None = 旧版创作助手（未传 task）：沿用旧工具集（作者专属动作除外）。
    pub task: Option<TaskKind>,
    pub target: Value,
    pub selection: Selection,
    pub files: Value,
    pub mode: Mode,
}

impl RunSpec {
    pub(crate) fn from_args(args: &Value) -> Result<RunSpec> {
        let raw = args
            .get("task")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let task = if raw.is_empty() || raw == "agent" {
            None
        } else {
            Some(TaskKind::parse(raw).ok_or_else(|| anyhow!("未知任务类型「{}」", raw))?)
        };
        let mode = match args.get("mode").and_then(Value::as_str).unwrap_or("agent") {
            "direct" => Mode::Direct,
            _ => Mode::Agent,
        };
        Ok(RunSpec {
            task,
            target: args
                .get("target")
                .cloned()
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({})),
            selection: Selection::from_args(args),
            files: args.get("contextFiles").cloned().unwrap_or(json!([])),
            mode,
        })
    }

    pub(crate) fn task_id(&self) -> &'static str {
        self.task.map(TaskKind::id).unwrap_or("agent")
    }

    pub(crate) fn ch(&self) -> i64 {
        super::context_builder::target_ch(&self.target)
    }

    /// 本次运行允许的工具名（不含作者专属动作）。
    pub(crate) fn allowed_tools(&self) -> Vec<&'static str> {
        if self.mode == Mode::Direct {
            return Vec::new();
        }
        let mut v: Vec<&'static str> = READ_TOOLS.to_vec();
        match self.task {
            None => v.extend([
                "create_change_proposal",
                "draft_chapter_outline",
                "draft_chapter_body",
            ]),
            Some(TaskKind::Body) => v.push("draft_chapter_body"),
            Some(TaskKind::Plot) | Some(TaskKind::Outline) => v.push("create_change_proposal"),
            _ => {}
        }
        v
    }

    pub(crate) fn catalog(&self) -> Option<Value> {
        let allowed = self.allowed_tools();
        if allowed.is_empty() {
            return None;
        }
        let all = agent_tools::tool_catalog();
        Some(json!(all
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| allowed.contains(&t["function"]["name"].as_str().unwrap_or("")))
            .cloned()
            .collect::<Vec<_>>()))
    }

    /// 服务端作用域复核（模型可能编造不在目录里的工具或越界参数）。
    pub(crate) fn check_call(&self, name: &str, args: &Value) -> Result<()> {
        if AUTHOR_ONLY.contains(&name) {
            bail!(
                "「{}」需要作者在产物卡或待审面板上亲自确认，模型无权执行；请把结果交给作者确认",
                name
            );
        }
        if !self.allowed_tools().contains(&name) {
            bail!(
                "本次任务（{}）不允许调用工具 {}",
                self.task.map(TaskKind::label).unwrap_or("创作助手"),
                name
            );
        }
        if name == "draft_chapter_body" && self.ch() > 0 {
            let want = args["ch"]
                .as_i64()
                .or_else(|| args["ch"].as_str().and_then(|s| s.trim().parse().ok()));
            if want != Some(self.ch()) {
                bail!("本次任务的目标是第{}章，不能起草其他章节", self.ch());
            }
        }
        Ok(())
    }

    /// 任务说明（写给模型的公开流程，不替代服务端校验）。
    fn rules(&self) -> String {
        let ch = self.ch();
        let base = "工具结果是唯一事实来源；没有工具回执，绝不说「已保存/已确认/已定稿」。确认细纲与定稿只能由作者在界面上操作，你只负责产出与说明。";
        let task = match self.task {
            None => "按作者的明确指令推进；建档/设定修改用 create_change_proposal（作者接受才生效）；章细纲可用 draft_chapter_outline 保存草稿（保存≠确认）；作者明确要求写正文时用 draft_chapter_body（进「正文待审」，不是定稿）。".to_string(),
            Some(TaskKind::Chat) => "这是普通交流：直接回答，不要写入任何文件，必要时可读取资料核实。".into(),
            Some(TaskKind::Body) if ch > 0 => format!("本次任务：起草第{0}章正文。可先用只读工具核对状态，然后调用 draft_chapter_body(ch={0}) 由章节服务生成并提交待审；完成后简要说明结果与需要作者确认的事项。不要在回复里粘贴整章正文。", ch),
            Some(TaskKind::Body) => "本次任务：写正文。没有指定章号时，直接输出正文文本（将作为草稿产物交给作者决定去向）。".into(),
            Some(TaskKind::Outline) => format!("本次任务：起草{}细纲。最终回复只输出细纲正文（Markdown），不要加解释；作者会在产物卡上保存并确认。", if ch > 0 { format!("第{}章", ch) } else { String::new() }),
            Some(TaskKind::Revise) | Some(TaskKind::Humanize) => "本次任务：改写目标文本。最终回复只输出改写后的文本本身（若给了选区，只输出替换选区的那一段），不要输出解释、标题或代码围栏。".into(),
            Some(TaskKind::Review) => "本次任务：审稿。输出结构化审稿报告（问题、位置、建议）；不要改写原文。".into(),
            Some(t) => format!("本次任务：{}。最终回复即交付内容。", t.label()),
        };
        format!(
            "你是墨澜工坊的创作 Agent。{}\n{}\n用与作者相同的语言回答，简洁、面向下一步行动。",
            base, task
        )
    }
}

// ---------- 在飞登记（按会话） ----------

struct Live {
    request: String,
    run: String,
    token: CancellationToken,
}

static LIVE: std::sync::LazyLock<Mutex<HashMap<String, Live>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// 同会话已有其他请求在运行 → 返回其 (requestId, runId)。
pub(crate) fn session_busy(session: &str, request: &str) -> Option<(String, String)> {
    let g = LIVE.lock().ok()?;
    g.get(session)
        .filter(|l| l.request != request)
        .map(|l| (l.request.clone(), l.run.clone()))
}

pub(crate) struct LiveGuard {
    session: String,
    request: String,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        if let Ok(mut g) = LIVE.lock() {
            // 只移除自己的登记，绝不误删同会话其他请求的令牌（旧实现的串号问题）
            if g.get(&self.session)
                .is_some_and(|l| l.request == self.request)
            {
                g.remove(&self.session);
            }
        }
    }
}

pub(crate) fn register_live(
    session: &str,
    request: &str,
    run: &str,
    token: CancellationToken,
) -> LiveGuard {
    if let Ok(mut g) = LIVE.lock() {
        g.insert(
            session.to_string(),
            Live {
                request: request.into(),
                run: run.into(),
                token,
            },
        );
    }
    LiveGuard {
        session: session.into(),
        request: request.into(),
    }
}

/// 按会话取消在飞运行；无在飞运行返回 false（不留下任何标记）。
pub(crate) fn abort_session(session: &str) -> bool {
    let token = LIVE
        .lock()
        .ok()
        .and_then(|g| g.get(session).map(|l| l.token.clone()));
    match token {
        Some(t) => {
            t.cancel();
            true
        }
        None => false,
    }
}

pub(crate) fn is_live(run_id: &str) -> bool {
    LIVE.lock()
        .map(|g| g.values().any(|l| l.run == run_id))
        .unwrap_or(false)
}

/// IPC run_status：{bookId, sessionId, requestId?} → 状态机视图（含 live）。
pub(crate) fn run_status(st: &Arc<AppState>, args: &Value) -> Result<Value> {
    let s = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let (book, session, req) = (s("bookId"), s("sessionId"), s("requestId"));
    let owner = st.db.q_json(
        "SELECT book_id FROM sessions WHERE id=?1",
        &[&session as &dyn rusqlite::ToSql],
    )?;
    if owner.first().and_then(|r| r["bookId"].as_str()) != Some(book.as_str()) {
        bail!("会话不存在或不属于当前书");
    }
    let row = if req.is_empty() {
        molan_core::agent_run::latest_run_for_session(&st.db, &session)?
    } else {
        st.db
            .q_json(
                "SELECT * FROM agent_run WHERE session_id=?1 AND request_id=?2",
                &[&session as &dyn rusqlite::ToSql, &req],
            )?
            .into_iter()
            .next()
    };
    Ok(match row {
        Some(r) => {
            let live = is_live(r["id"].as_str().unwrap_or(""));
            molan_core::agent_run::state_of(&r, live)
        }
        None => Value::Null,
    })
}

// ---------- 计划 / 上下文 ----------

pub(crate) struct Prepared {
    pub system: String,
    pub plan: Value,
    pub manifest_id: String,
    pub blocks: Vec<Value>,
    pub blockers: Vec<String>,
}

/// 冻结本次计划与上下文，拼出系统提示，并落上下文清单（含技能版本与 planHash）。
pub(crate) fn prepare(
    db: &Db,
    root: &Path,
    book: &str,
    session: &str,
    spec: &RunSpec,
    model: &str,
) -> Result<Prepared> {
    let task = spec.task.unwrap_or(TaskKind::Chat);
    let plan = super::run_plan::build(db, root, book, task, &spec.selection)?;
    let built = super::context_builder::build(db, book, task, &spec.target, &spec.files);
    let mut system = super::run_plan::system_prompt(db, root, book, &plan, "", &built.text);
    system.push_str("\n\n【本次运行规则】\n");
    system.push_str(&spec.rules());
    let mut manifest_id = String::new();
    if let Ok(mid) = molan_core::ctx_manifest::record(
        db,
        book,
        session,
        &format!("agent:{}", spec.task_id()),
        built.ch,
        model,
        &built.text,
        json!({"blocks": built.blocks}),
    ) {
        let snap = json!(plan["skills"].as_array().into_iter().flatten().map(|s| json!({
            "id": s["id"], "name": s["name"], "rev": s["rev"], "role": s["role"], "source": s["source"],
        })).collect::<Vec<_>>());
        let _ = molan_core::ctx_manifest::attach_skills(
            db,
            &mid,
            &json!({"planHash": plan["planHash"], "skills": snap}),
        );
        manifest_id = mid;
    }
    Ok(Prepared {
        system,
        plan,
        manifest_id,
        blocks: built.blocks,
        blockers: built.blockers,
    })
}

// ---------- 产物 ----------

fn title_for(spec: &RunSpec, task: TaskKind) -> String {
    let ch = spec.ch();
    let target = spec.target["name"].as_str().unwrap_or("");
    let fragment = spec.target["start"].is_u64();
    match task {
        TaskKind::Outline if ch > 0 => format!("第{}章细纲", ch),
        TaskKind::Body if ch > 0 => format!("第{}章正文草稿", ch),
        TaskKind::Revise | TaskKind::Humanize if fragment => {
            format!("{}·选区{}", target, task.label())
        }
        TaskKind::Review if !target.is_empty() => format!("审稿报告·{}", target),
        _ if !target.is_empty() => format!("{}·{}", target, task.label()),
        _ => task.label().to_string(),
    }
}

/// 运行结束时把最终文本落为产物（普通聊天不形成产物）。返回卡片视图。
#[allow(clippy::too_many_arguments)]
pub(crate) fn final_artifact(
    db: &Db,
    book: &str,
    session: &str,
    run: &str,
    message_id: &str,
    spec: &RunSpec,
    plan: &Value,
    text: &str,
    lifecycle: &str,
    model: &str,
    manifest_id: &str,
) -> Option<Value> {
    let task = spec.task?;
    let kind = task.artifact_kind()?;
    if text.trim().is_empty() {
        return None;
    }
    let fragment = spec.target["start"].is_u64() && spec.target["end"].is_u64();
    let mut target = spec.target.clone();
    if task == TaskKind::Outline && spec.ch() > 0 && target.get("ch").is_none() {
        target["ch"] = json!(spec.ch());
    }
    let id = artifact::create(
        db,
        &NewArtifact {
            book_id: book.into(),
            session_id: session.into(),
            message_id: message_id.into(),
            run_id: run.into(),
            kind: kind.into(),
            task: task.id().into(),
            title: title_for(spec, task),
            scope: if fragment {
                "fragment".into()
            } else {
                "document".into()
            },
            target,
            provenance: provenance(plan, model, manifest_id, run),
            content: text.to_string(),
            items: Vec::new(),
            origin: "model".into(),
            lifecycle: lifecycle.into(),
        },
    )
    .ok()?;
    molan_core::artifact_view::view(db, &id, false).ok()
}

fn provenance(plan: &Value, model: &str, manifest_id: &str, run: &str) -> Value {
    json!({
        "planHash": plan["planHash"], "model": model, "manifestId": manifest_id, "runId": run,
        "skills": plan["skills"].as_array().into_iter().flatten().map(|s| json!({
            "id": s["id"], "name": s["name"], "rev": s["rev"], "role": s["role"], "source": s["source"],
        })).collect::<Vec<_>>(),
        "style": {"key": plan["style"]["key"], "label": plan["style"]["label"]},
        "humanize": {"method": plan["humanize"]["method"], "label": plan["humanize"]["label"]},
        "excluded": plan["excluded"],
    })
}

/// 工具内部（章节服务）的模型用量：上游 usage 不经过 Agent 循环，按「提示 + 产出」字数估算计入本次运行预算并标记为估算。
pub(crate) fn charge_tool_usage(db: &Db, run_id: &str, payload: &Value) -> Result<i64> {
    let n =
        |k: &str| molan_core::agent_run::estimate_tokens(payload[k].as_u64().unwrap_or(0) as usize);
    molan_core::agent_run::charge_usage_est(db, run_id, None, n("promptChars"), n("chars"))
}

/// 章节服务起草成功的正文草稿 → 产物（含已提交待审的交付记录）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn adopt_tool_result(
    db: &Db,
    book: &str,
    session: &str,
    run: &str,
    message_id: &str,
    name: &str,
    payload: &Value,
    model: &str,
) -> Option<Value> {
    if name != "draft_chapter_body" || payload["kind"] != "body_draft" {
        return None;
    }
    let ch = payload["ch"].as_i64()?;
    let fname = payload["name"].as_str()?;
    let hash = payload["hash"].as_str()?;
    let content = molan_core::files::read_file(db, book, molan_core::db::REVIEW_GROUP, fname)?;
    let plan = payload["planHash"]
        .as_str()
        .and_then(|h| skill_resolver::load_frozen(db, h))
        .unwrap_or(Value::Null);
    let id = artifact::adopt_pending(
        db,
        &NewArtifact {
            book_id: book.into(),
            session_id: session.into(),
            message_id: message_id.into(),
            run_id: run.into(),
            kind: "body_draft".into(),
            task: "body".into(),
            title: format!("第{}章正文草稿", ch),
            scope: "document".into(),
            target: json!({"ch": ch}),
            provenance: provenance(&plan, model, "", run),
            content,
            items: Vec::new(),
            origin: "tool".into(),
            lifecycle: "generated".into(),
        },
        ch,
        fname,
        hash,
        payload,
    )
    .ok()?;
    molan_core::artifact_view::view(db, &id, false).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(args: Value) -> RunSpec {
        RunSpec::from_args(&args).unwrap()
    }

    #[test]
    fn author_only_actions_are_never_exposed() {
        for a in [
            json!({}),
            json!({"task": "body", "target": {"ch": 2}}),
            json!({"task": "outline"}),
        ] {
            let s = spec(a);
            let names: Vec<String> = s
                .catalog()
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap().to_string())
                .collect();
            assert!(
                !names.iter().any(|n| AUTHOR_ONLY.contains(&n.as_str())),
                "{:?}",
                names
            );
            assert!(s
                .check_call("finalize_chapter_draft", &json!({}))
                .unwrap_err()
                .to_string()
                .contains("作者"));
            assert!(s.check_call("confirm_chapter_outline", &json!({})).is_err());
        }
    }

    #[test]
    fn chat_is_read_only_and_body_is_bound_to_target_chapter() {
        let chat = spec(json!({"task": "chat"}));
        assert!(chat.check_call("read_book_file", &json!({})).is_ok());
        assert!(chat
            .check_call("draft_chapter_body", &json!({"ch": 1}))
            .is_err());
        assert!(chat
            .check_call("create_change_proposal", &json!({}))
            .is_err());
        let body = spec(json!({"task": "body", "target": {"ch": 3}}));
        assert!(body
            .check_call("draft_chapter_body", &json!({"ch": 3}))
            .is_ok());
        assert!(body
            .check_call("draft_chapter_body", &json!({"ch": 4}))
            .unwrap_err()
            .to_string()
            .contains("第3章"));
        let direct = spec(json!({"task": "body", "mode": "direct"}));
        assert!(direct.catalog().is_none());
        assert!(RunSpec::from_args(&json!({"task": "hack"})).is_err());
        assert_eq!(spec(json!({"task": "chapter"})).task, Some(TaskKind::Body));
    }

    #[test]
    fn live_registry_is_per_request_and_guard_only_removes_itself() {
        let t1 = CancellationToken::new();
        let g1 = register_live("S-live", "r1", "run1", t1.clone());
        assert_eq!(
            session_busy("S-live", "r2").map(|x| x.0).as_deref(),
            Some("r1")
        );
        assert!(
            session_busy("S-live", "r1").is_none(),
            "同请求不算忙（走幂等）"
        );
        assert!(is_live("run1"));
        {
            // 另一个请求的 guard 结束不得移除 r1 的登记
            let _g2 = LiveGuard {
                session: "S-live".into(),
                request: "r2".into(),
            };
        }
        assert!(is_live("run1"));
        assert!(abort_session("S-live"));
        assert!(t1.is_cancelled());
        drop(g1);
        assert!(!is_live("run1"));
        assert!(!abort_session("S-live"), "无在飞运行不留标记");
    }
}
