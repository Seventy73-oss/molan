//! 任务标识单一来源：稳定内部 ID、兼容别名、中文展示名、模型角色与产物类型。
//!
//! 旧代码把九种任务复制在 skill_plan / chat_contract / agent_tools 三处，别名
//! `chapter→body` 只在聊天入口生效。这里收敛为一张表，其余模块只引用本模块。

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
    Chat,
    Plot,
    Outline,
    Body,
    Revise,
    Review,
    Humanize,
    Summary,
    Distill,
}

pub const ALL: [TaskKind; 9] = [
    TaskKind::Chat,
    TaskKind::Plot,
    TaskKind::Outline,
    TaskKind::Body,
    TaskKind::Revise,
    TaskKind::Review,
    TaskKind::Humanize,
    TaskKind::Summary,
    TaskKind::Distill,
];

/// 任务 ID 列表（技能 targets 校验等处使用）。
pub const IDS: [&str; 9] = [
    "chat", "plot", "outline", "body", "revise", "review", "humanize", "summary", "distill",
];

impl TaskKind {
    pub fn id(self) -> &'static str {
        match self {
            TaskKind::Chat => "chat",
            TaskKind::Plot => "plot",
            TaskKind::Outline => "outline",
            TaskKind::Body => "body",
            TaskKind::Revise => "revise",
            TaskKind::Review => "review",
            TaskKind::Humanize => "humanize",
            TaskKind::Summary => "summary",
            TaskKind::Distill => "distill",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TaskKind::Chat => "聊天",
            TaskKind::Plot => "剧情推演",
            TaskKind::Outline => "细纲",
            TaskKind::Body => "正文",
            TaskKind::Revise => "修改",
            TaskKind::Review => "审稿",
            TaskKind::Humanize => "去AI味",
            TaskKind::Summary => "总结",
            TaskKind::Distill => "蒸馏",
        }
    }

    /// 解析任务 ID 或兼容别名（去首尾空白；大小写不敏感）。未知返回 None，调用方必须拒绝而非回落。
    pub fn parse(raw: &str) -> Option<TaskKind> {
        let r = raw.trim();
        let lower = r.to_ascii_lowercase();
        let kind = match lower.as_str() {
            "chat" | "聊天" | "对话" => TaskKind::Chat,
            "plot" | "剧情" | "剧情推演" => TaskKind::Plot,
            "outline" | "细纲" | "大纲" | "chapter_outline" => TaskKind::Outline,
            "body" | "chapter" | "正文" | "写正文" | "draft" => TaskKind::Body,
            "revise" | "rewrite" | "修改" | "改写" | "润色" => TaskKind::Revise,
            "review" | "审稿" | "审核" | "审读" => TaskKind::Review,
            "humanize" | "deai" | "去ai味" | "去味" => TaskKind::Humanize,
            "summary" | "总结" | "摘要" => TaskKind::Summary,
            "distill" | "蒸馏" | "拆书" => TaskKind::Distill,
            _ => return None,
        };
        Some(kind)
    }

    /// 生成该任务的角色模型槽位（settings `agent_profile__<role>`）。空串 = 活跃渠道。
    pub fn role(self) -> &'static str {
        match self {
            TaskKind::Body => "chapter",
            TaskKind::Plot | TaskKind::Outline => "outline",
            TaskKind::Revise | TaskKind::Review | TaskKind::Humanize => "review",
            TaskKind::Summary => "summary",
            TaskKind::Distill => "distill",
            TaskKind::Chat => "",
        }
    }

    /// 任务结束时最终文本对应的产物类型；None = 普通对话，不形成可交付产物。
    pub fn artifact_kind(self) -> Option<&'static str> {
        match self {
            TaskKind::Chat => None,
            TaskKind::Plot => Some("plot_note"),
            TaskKind::Outline => Some("outline_draft"),
            TaskKind::Body => Some("body_draft"),
            TaskKind::Revise => Some("revision"),
            TaskKind::Review => Some("review_report"),
            TaskKind::Humanize => Some("humanize_rewrite"),
            TaskKind::Summary => Some("summary"),
            TaskKind::Distill => Some("distill_note"),
        }
    }

    /// 该任务的产物能否以「改写目标文本」的方式交付（全文替换/选区替换）。
    /// 审稿报告、剧情推演、总结只能另存为文档，绝不允许覆盖被审文本。
    pub fn rewrites_target(self) -> bool {
        matches!(
            self,
            TaskKind::Revise | TaskKind::Humanize | TaskKind::Body | TaskKind::Outline
        )
    }
}

/// 兼容解析：空串 → chat；未知 → Err（附可选值），不静默回落。
pub fn parse_or_chat(raw: &str) -> Result<TaskKind, String> {
    if raw.trim().is_empty() {
        return Ok(TaskKind::Chat);
    }
    TaskKind::parse(raw)
        .ok_or_else(|| format!("未知任务类型「{}」，可选：{}", raw.trim(), IDS.join(" / ")))
}

/// 供前端展示的任务目录。
pub fn catalog() -> Value {
    json!(ALL
        .iter()
        .map(|t| json!({
            "id": t.id(),
            "label": t.label(),
            "role": t.role(),
            "artifactKind": t.artifact_kind(),
            "rewritesTarget": t.rewrites_target(),
        }))
        .collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip_and_aliases() {
        for t in ALL {
            assert_eq!(TaskKind::parse(t.id()), Some(t));
        }
        assert_eq!(IDS.len(), ALL.len());
        assert_eq!(TaskKind::parse(" chapter "), Some(TaskKind::Body));
        assert_eq!(TaskKind::parse("正文"), Some(TaskKind::Body));
        assert_eq!(TaskKind::parse("CHAT"), Some(TaskKind::Chat));
        assert_eq!(TaskKind::parse("去AI味"), Some(TaskKind::Humanize));
        assert_eq!(TaskKind::parse("write"), None);
    }

    #[test]
    fn empty_is_chat_unknown_is_error() {
        assert_eq!(parse_or_chat(""), Ok(TaskKind::Chat));
        assert!(parse_or_chat("nonsense")
            .unwrap_err()
            .contains("未知任务类型"));
    }

    #[test]
    fn roles_match_legacy_chat_contract() {
        assert_eq!(TaskKind::Body.role(), "chapter");
        assert_eq!(TaskKind::Outline.role(), "outline");
        assert_eq!(TaskKind::Plot.role(), "outline");
        assert_eq!(TaskKind::Humanize.role(), "review");
        assert_eq!(TaskKind::Chat.role(), "");
    }

    #[test]
    fn review_reports_never_rewrite_target() {
        assert!(!TaskKind::Review.rewrites_target());
        assert!(!TaskKind::Summary.rewrites_target());
        assert!(TaskKind::Revise.rewrites_target());
        assert_eq!(TaskKind::Chat.artifact_kind(), None);
    }
}
