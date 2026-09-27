//! Chat request contract helpers shared by the stream chat handler.

use anyhow::{bail, Result};
use serde_json::{json, Value};

const DEFAULT_TASK: &str = "chat";
const ALLOWED_TASKS: [&str; 9] = [
    "chat", "plot", "outline", "body", "revise", "review", "humanize", "summary", "distill",
];

/// Resolve the effective task kind from a chat request payload: non-empty
/// `args.task` first, then `skillSelection.taskKind`, else `chat`. Values
/// are trimmed, `chapter` is aliased to `body`, unknown non-empty is rejected.
pub(super) fn task(args: &Value) -> Result<String> {
    fn trimmed(v: Option<&str>) -> Option<&str> {
        v.map(str::trim).filter(|s| !s.is_empty())
    }
    let raw = trimmed(args.get("task").and_then(Value::as_str))
        .or_else(|| {
            trimmed(
                args.get("skillSelection")
                    .and_then(|sel| sel.get("taskKind"))
                    .and_then(Value::as_str),
            )
        })
        .unwrap_or(DEFAULT_TASK);

    let name = if raw == "chapter" { "body" } else { raw };
    if !ALLOWED_TASKS.contains(&name) {
        bail!("unknown chat task kind: {name}");
    }
    Ok(name.to_string())
}

/// Map a task kind onto the writing role that produces it.
pub(super) fn role(task: &str) -> &str {
    match task {
        "body" => "chapter",
        "plot" | "outline" => "outline",
        "revise" | "review" | "humanize" => "review",
        "summary" => "summary",
        "distill" => "distill",
        _ => "",
    }
}

/// Persistence gate: only an explicit per-task write intent in auto mode for
/// a content-producing task (`body` or `outline`) may save.
pub(super) fn allow_save(args: &Value, task: &str, mode: &str) -> bool {
    args.get("writeIntent").and_then(Value::as_str) == Some("explicit-task")
        && mode == "auto"
        && matches!(task, "body" | "outline")
}

/// Normalize stored chat rows (newest first) into chronological prompt
/// history, skipping only rows whose id equals `current_user_id`. The 4 most
/// recent kept rows are capped at 4000 chars, older kept rows at 1500 chars;
/// rows are never deduplicated by content.
pub(super) fn history(rows: &[Value], current_user_id: &str) -> Vec<Value> {
    let mut kept: Vec<Value> = Vec::new();
    let mut recent = 0usize;
    for row in rows {
        if row.get("id").and_then(Value::as_str).unwrap_or("") == current_user_id {
            continue;
        }
        let cap = if recent < 4 { 4000 } else { 1500 };
        let content: String = row
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(cap)
            .collect();
        kept.push(json!({
            "role": row.get("role").and_then(Value::as_str).unwrap_or(""),
            "content": content,
        }));
        recent += 1;
    }
    kept.reverse();
    kept
}

/// Merge `args.skills`, `skillSelection.primarySkillId` and
/// `skillSelection.supportSkillIds`; keep each non-empty id once, first seen wins.
pub(super) fn selected_skills(args: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    push_skills(&mut out, args.get("skills"));
    let selection = args.get("skillSelection");
    push_skill(
        &mut out,
        selection.and_then(|sel| sel.get("primarySkillId")),
    );
    push_skills(
        &mut out,
        selection.and_then(|sel| sel.get("supportSkillIds")),
    );
    out
}

fn push_skill(out: &mut Vec<String>, value: Option<&Value>) {
    if let Some(s) = value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !out.iter().any(|existing| existing.as_str() == s) {
            out.push(s.to_string());
        }
    }
}

fn push_skills(out: &mut Vec<String>, value: Option<&Value>) {
    if let Some(Value::Array(items)) = value {
        for item in items {
            push_skill(out, Some(item));
        }
    } else {
        push_skill(out, value);
    }
}

/// P0（AGENT-PIPELINE-PLAN §D）：上游瞬态错误的结构化 error 事件。
/// 429/5xx/限流等瞬态错误带 code=UPSTREAM_LIMIT + 同渠道备选模型，前端据此提供
/// 「一键换模型重试」；非瞬态错误保持普通 error 事件（旧行为不变）。
/// partial=false 表示零输出：明确「未写入任何内容」，避免用户误以为书稿丢失。
pub(crate) fn upstream_error_event(reason: &str, chn: &Value, role: &str, partial: bool) -> Value {
    let mut ev = json!({"type": "error", "message": reason});
    if !molan_llm::is_transient_error(reason) {
        return ev;
    }
    let model = chn["model"].as_str().unwrap_or("");
    let alts: Vec<&str> = chn["models"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .filter(|m| *m != model)
                .take(6)
                .collect()
        })
        .unwrap_or_default();
    ev["code"] = json!("UPSTREAM_LIMIT");
    ev["role"] = json!(role);
    ev["model"] = json!(model);
    ev["alternates"] = json!(alts);
    if !partial {
        ev["message"] = json!(format!(
            "上游对「{}」限流或暂时不可用，本次生成没有产出、未写入任何内容（书稿与设置不受影响）。可换模型后重试。{}",
            model,
            reason.chars().take(160).collect::<String>()
        ));
    }
    ev
}

#[cfg(test)]
mod tests {
    #[test]
    fn upstream_error_event_classifies_and_lists_alternates() {
        let chn = serde_json::json!({"model":"flow-body","models":["flow-chat","flow-body","flow-outline","flow-review"]});
        let ev = super::upstream_error_event(
            "请求太频繁或额度不足（429），稍等再发 [429] Upstream rate limit exceeded",
            &chn,
            "chapter",
            false,
        );
        assert_eq!(ev["code"], "UPSTREAM_LIMIT");
        assert_eq!(ev["role"], "chapter");
        assert_eq!(
            ev["alternates"],
            serde_json::json!(["flow-chat", "flow-outline", "flow-review"])
        );
        assert!(ev["message"].as_str().unwrap().contains("未写入任何内容"));
        let partial = super::upstream_error_event(
            "上游中断，已保留未完成文本：x [502] y",
            &chn,
            "outline",
            true,
        );
        assert_eq!(partial["code"], "UPSTREAM_LIMIT");
        assert_eq!(partial["message"], "上游中断，已保留未完成文本：x [502] y");
        let plain = super::upstream_error_event(
            "模型返回了空内容，请重试或更换渠道",
            &chn,
            "chapter",
            false,
        );
        assert!(plain.get("code").is_none());
    }

    use super::*;

    #[test]
    fn task_maps_chapter_alias_and_rejects_unknown() {
        assert_eq!(task(&json!({"task": " chapter "})).unwrap(), "body");
        assert!(task(&json!({"task": "translate"})).is_err());
        assert!(task(&json!({"skillSelection": {"taskKind": "nope"}})).is_err());
    }

    #[test]
    fn task_prefers_args_then_selection_then_chat() {
        let both = json!({"task": "outline", "skillSelection": {"taskKind": "body"}});
        assert_eq!(task(&both).unwrap(), "outline");
        assert_eq!(
            task(&json!({"skillSelection": {"taskKind": "revise"}})).unwrap(),
            "revise"
        );
        assert_eq!(task(&json!({})).unwrap(), "chat");
        assert_eq!(task(&json!({"task": "   "})).unwrap(), "chat");
    }

    #[test]
    fn role_maps_every_task() {
        let cases = [
            ("body", "chapter"),
            ("plot", "outline"),
            ("outline", "outline"),
            ("revise", "review"),
            ("review", "review"),
            ("humanize", "review"),
            ("summary", "summary"),
            ("distill", "distill"),
            ("chat", ""),
        ];
        for (input, want) in cases {
            assert_eq!(role(input), want);
        }
    }

    #[test]
    fn allow_save_gates_intent_mode_and_task() {
        // 缺授权与 preview 拒绝
        assert!(!allow_save(&json!({}), "body", "auto"));
        assert!(!allow_save(&json!({"writeIntent": "auto"}), "body", "auto"));
        assert!(!allow_save(
            &json!({"writeIntent": "explicit-task"}),
            "body",
            "preview"
        ));
        // explicit + auto + body/outline 允许
        assert!(allow_save(
            &json!({"writeIntent": "explicit-task"}),
            "body",
            "auto"
        ));
        assert!(allow_save(
            &json!({"writeIntent": "explicit-task"}),
            "outline",
            "auto"
        ));
        assert!(!allow_save(
            &json!({"writeIntent": "explicit-task"}),
            "chat",
            "auto"
        ));
    }

    #[test]
    fn history_orders_chronologically_and_keeps_same_text_with_other_ids() {
        let rows = vec![
            json!({"id": "u1", "role": "user", "content": "newest ask"}),
            json!({"id": "a2", "role": "assistant", "content": "second reply"}),
            json!({"id": "a1", "role": "assistant", "content": "same text"}),
            json!({"id": "u1", "role": "user", "content": "same text"}),
            json!({"id": "u2", "role": "user", "content": "same text"}),
        ];
        let out = history(&rows, "u1");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["content"], "same text");
        assert_eq!(out[1]["content"], "same text"); // 同文不同 ID 不丢弃
        assert_eq!(out[2]["content"], "second reply");
    }

    #[test]
    fn history_caps_recent_rows_at_4000_and_older_at_1500() {
        let long = "x".repeat(2000);
        let rows: Vec<Value> = (0..5)
            .rev()
            .map(|i| json!({"id": format!("r{i}"), "role": "assistant", "content": long}))
            .collect();
        let out = history(&rows, "");
        assert_eq!(out.len(), 5);
        assert_eq!(out[0]["content"].as_str().unwrap().chars().count(), 1500);
        assert_eq!(out[4]["content"].as_str().unwrap().chars().count(), 2000);
    }

    #[test]
    fn selected_skills_merge_and_dedupe_ids() {
        let args = json!({
            "skills": "style-x",
            "skillSelection": {
                "primarySkillId": "style-x",
                "supportSkillIds": ["pace-y", "style-x", "", "  ", "tone-z"],
            },
        });
        assert_eq!(selected_skills(&args), vec!["style-x", "pace-y", "tone-z"]);
        assert!(selected_skills(&json!({})).is_empty());
    }
}
