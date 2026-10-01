//! 技能工坊的「技能草稿」产物：AI 起草 → 卡片（可编辑为新修订）→ 保存为技能（回执 + 失效检查）→ 放弃。
//!
//! 草稿属于全局技能库（bookId 为空），与作品产物分开列出。保存走与「从草稿建技能」相同的
//! `skill_plan::create_from_draft`（写技能行 + 初始版本），回执记入 artifact_delivery：
//! 之后技能被修改或删除，卡片显示「已失效」，不会永远停在绿色的「已保存」。
use crate::AppState;
use anyhow::{anyhow, bail, Result};
use molan_core::artifact::{self, Deliver, NewArtifact};
use molan_core::artifact_view;
use molan_core::continuity::content_hash;
use serde_json::{json, Value};
use std::sync::Arc;

fn s(args: &Value, k: &str) -> String {
    args.get(k)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

pub(super) async fn dispatch(st: &Arc<AppState>, cmd: &str, args: &Value) -> Result<Value> {
    let db = &st.db;
    match cmd {
        "skill_draft" => create(st, args).await,
        "skill_drafts" => {
            artifact_view::list_skill_drafts(db, args["limit"].as_i64().unwrap_or(20))
        }
        "skill_draft_revise" => {
            let id = draft_of(st, args)?;
            let content = args["content"].as_str().unwrap_or("").to_string();
            if content.trim().is_empty() {
                bail!("技能模板不能为空");
            }
            let base = args["baseRev"].as_i64().unwrap_or(0);
            artifact::revise(db, &id, "", base, &content, None, &s(args, "note"))?;
            artifact_view::view(db, &id, true)
        }
        "skill_draft_save" => save(st, args),
        "skill_draft_discard" => {
            let id = draft_of(st, args)?;
            let mut d = Deliver::from_args(&json!({"artifactId": id, "action": "discard"}));
            d.book_id = String::new();
            artifact::deliver(db, &d)?;
            artifact_view::view(db, &id, true)
        }
        other => bail!("未知技能草稿命令：{}", other),
    }
}

fn draft_of(st: &AppState, args: &Value) -> Result<String> {
    let id = s(args, "artifactId");
    let a = artifact::get(&st.db, &id)?;
    if a["kind"].as_str() != Some("skill_draft") {
        bail!("该产物不是技能草稿");
    }
    Ok(id)
}

/// 技能工坊「帮我起草」：真实模型生成技能模板（旧命令 draft_skill 与新草稿产物共用）。
pub(crate) async fn generate(st: &AppState, args: &Value) -> Result<String> {
    let chn = molan_llm::active_channel(&st.db).ok_or_else(|| anyhow!("未配置可用的模型渠道"))?;
    let p = super::super::read_prompts(&st.root);
    let user = format!(
        "{}\n用法场景：{}\n任务：{}\n技能名：{}\n描述：{}",
        p["skill_draft"].as_str().unwrap_or(""),
        s(args, "usage"),
        s(args, "task"),
        s(args, "name"),
        s(args, "description"),
    );
    let out = molan_llm::chat_once(molan_llm::ChatParams {
        base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: chn["key"].as_str().unwrap_or("").to_string(),
        model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
        messages: vec![
            json!({"role": "system", "content": "你是写作技能工程师，产出可直接用于 AI 写作技能的 prompt 模板。"}),
            json!({"role": "user", "content": user}),
        ],
        max_tokens: 4000,
        ..Default::default()
    })
    .await?;
    Ok(out.trim().to_string())
}

async fn create(st: &Arc<AppState>, args: &Value) -> Result<Value> {
    let name = s(args, "name");
    if name.is_empty() {
        bail!("请先填写技能名");
    }
    let task = molan_core::task_kind::TaskKind::parse(&s(args, "task"))
        .map(|t| t.id())
        .unwrap_or("chat");
    let text = generate(st, args).await?;
    if text.is_empty() {
        bail!("模型返回了空草稿，未生成卡片");
    }
    let model = molan_llm::active_channel(&st.db)
        .and_then(|c| c["model"].as_str().map(str::to_string))
        .unwrap_or_default();
    let id = artifact::create(
        &st.db,
        &NewArtifact {
            book_id: String::new(),
            session_id: String::new(),
            message_id: String::new(),
            run_id: String::new(),
            kind: "skill_draft".into(),
            task: "chat".into(),
            title: name.clone(),
            scope: "document".into(),
            target: json!({"skillName": name, "description": s(args, "description"),
                           "skillTask": task, "usage": s(args, "usage")}),
            provenance: json!({"model": model}),
            content: text,
            items: Vec::new(),
            origin: "model".into(),
            lifecycle: "generated".into(),
        },
    )?;
    artifact_view::view(&st.db, &id, true)
}

/// 保存为技能：幂等（同 idempotencyKey 返回原回执）；只能保存当前修订。
fn save(st: &AppState, args: &Value) -> Result<Value> {
    let db = &st.db;
    let id = draft_of(st, args)?;
    let idem = s(args, "idempotencyKey");
    if idem.is_empty() {
        bail!("缺少 idempotencyKey");
    }
    if let Some(p) = artifact::prior_delivery(db, &id, &idem) {
        return Ok(
            json!({"delivery": {"ok": true, "replayed": true, "deliveryId": p["id"]},
                         "artifact": artifact_view::view(db, &id, true)?}),
        );
    }
    let a = artifact::get(db, &id)?;
    let head = a["headRev"].as_i64().unwrap_or(1);
    let rev = args["rev"].as_i64().filter(|r| *r > 0).unwrap_or(head);
    if rev != head {
        bail!("REV_STALE：只能保存当前修订（当前 {}，请求 {}）", head, rev);
    }
    let content = artifact::get_rev(db, &id, rev)?["content"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let target: Value =
        serde_json::from_str(a["targetJson"].as_str().unwrap_or("{}")).unwrap_or(json!({}));
    let name = Some(s(args, "name"))
        .filter(|n| !n.is_empty())
        .or_else(|| target["skillName"].as_str().map(str::to_string))
        .unwrap_or_else(|| "新技能".into());
    let targets = args
        .get("targets")
        .filter(|t| t.is_array())
        .cloned()
        .unwrap_or_else(|| json!([target["skillTask"].as_str().unwrap_or("chat")]));
    let (skill_id, defaulted) = {
        let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
        crate::handlers::stream::skill_plan::create_from_draft(
            db,
            &json!({"name": name, "draft": content, "targets": targets,
                    "description": target["description"]}),
        )?
    };
    let detail =
        json!({"skillId": skill_id, "name": name, "targets": targets, "defaultApplied": defaulted});
    let did = artifact::record_skill_delivery(
        db,
        &id,
        rev,
        &idem,
        &name,
        &content_hash(&content),
        "committed",
        &detail,
    )?;
    Ok(
        json!({"delivery": {"ok": true, "action": "save_skill", "status": "committed",
                           "result": detail, "deliveryId": did},
              "artifact": artifact_view::view(db, &id, true)?}),
    )
}
