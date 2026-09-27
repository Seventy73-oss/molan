//! P3b 显式角色回退（AGENT-PIPELINE-PLAN §D3）：
//! 主模型上游限流/暂不可用且**零输出**时，若用户显式配置了 agent_fallback__<role>，
//! 用同渠道回退模型重试一次；全程可见（fallback 事件），绝不静默换模型。
//! 默认关闭：未配置/配置为 off/与当前模型相同 = 不启用，行为与 P0 完全一致。
use super::chat::chat_contract;
use super::chat::{drain_chat_completion, Ctx};
use molan_llm::ChatParams;
use serde_json::{json, Value};

/// 读取显式配置的回退模型：空 / off / 与当前模型相同都视为未启用。
pub(crate) fn fallback_model(db: &molan_core::db::Db, role: &str, current: &str) -> Option<String> {
    if role.is_empty() {
        return None;
    }
    let v = molan_llm::get_setting(db, &format!("agent_fallback__{}", role));
    let t = v.trim();
    if t.is_empty() || t == "off" || t == current {
        return None;
    }
    Some(t.to_string())
}

/// 零输出瞬态故障处理。返回 true=已发错误事件收口（调用方 return Ok(None)）；
/// false=回退重试成功，cx.full 已有内容，主流程照常继续（落库/done）。
pub(crate) async fn limit(
    db: &molan_core::db::Db,
    role: &str,
    chn: &Value,
    params: &ChatParams,
    cx: &mut Ctx<'_>,
    err: &anyhow::Error,
) -> anyhow::Result<bool> {
    let current = chn["model"].as_str().unwrap_or("");
    if let Some(fb) = fallback_model(db, role, current) {
        cx.ev(json!({"type": "fallback", "from": current, "to": fb}))
            .await;
        let mut p2 = params.clone();
        p2.model = fb.clone();
        cx.full.clear();
        cx.chars = 0;
        match drain_chat_completion(p2, cx).await {
            Ok(()) if !cx.full.trim().is_empty() => return Ok(false),
            Ok(()) => {
                cx.ev(chat_contract::upstream_error_event(
                    &format!(
                        "回退模型 {} 返回空内容，未写入任何内容。主模型错误：{}",
                        fb, err
                    ),
                    chn,
                    role,
                    false,
                ))
                .await;
            }
            Err(e2) => {
                cx.ev(chat_contract::upstream_error_event(
                    &format!("回退模型 {} 也失败：{}。主模型错误：{}", fb, e2, err),
                    chn,
                    role,
                    false,
                ))
                .await;
            }
        }
        return Ok(true);
    }
    let msg = format!("{}", err);
    cx.ev(chat_contract::upstream_error_event(&msg, chn, role, false))
        .await;
    Ok(true)
}

/// 中断收尾事件（自 chat.rs 原样迁出以省行预算；行为不变）：
/// 残稿原因（含 UPSTREAM_LIMIT 分类）+ interrupted 标记。
pub(crate) async fn emit_interrupted(
    cx: &mut Ctx<'_>,
    reason: &Option<String>,
    chn: &Value,
    role: &str,
) {
    if let Some(r) = reason {
        cx.ev(chat_contract::upstream_error_event(r, chn, role, true))
            .await;
    }
    cx.ev(json!({"type": "interrupted", "partial": true})).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fallback_is_explicit_opt_in_only() {
        let dir = tempfile::tempdir().unwrap();
        let db = molan_core::db::Db::open(dir.path(), None).unwrap();
        assert_eq!(fallback_model(&db, "chapter", "deepseek-v4-pro"), None);
        db.exec(
            "INSERT INTO settings(key,value) VALUES('agent_fallback__chapter','off')",
            &[],
        )
        .unwrap();
        assert_eq!(fallback_model(&db, "chapter", "deepseek-v4-pro"), None);
        db.exec(
            "UPDATE settings SET value='deepseek-v4-pro' WHERE key='agent_fallback__chapter'",
            &[],
        )
        .unwrap();
        assert_eq!(fallback_model(&db, "chapter", "deepseek-v4-pro"), None);
        db.exec(
            "UPDATE settings SET value='glm-5.3-flash' WHERE key='agent_fallback__chapter'",
            &[],
        )
        .unwrap();
        assert_eq!(
            fallback_model(&db, "chapter", "deepseek-v4-pro").as_deref(),
            Some("glm-5.3-flash")
        );
        assert_eq!(fallback_model(&db, "outline", "x"), None);
        assert_eq!(fallback_model(&db, "", "x"), None);
    }
}
