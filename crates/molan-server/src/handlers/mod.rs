// handler 分发：对齐 lib/registry.js 全部命令。
// 返回 Ok(Some(v)) = 一次性结果；Ok(None) = handler 已写完 NDJSON（流式）。
use crate::AppState;
use anyhow::{anyhow, Result};
use molan_core::files;

pub mod exports;
mod saved_output;
use molan_core::stats;
use serde_json::{json, Value};
use std::sync::Arc;

pub async fn dispatch(
    st: &Arc<AppState>,
    cmd: &str,
    args: &Value,
    tx: &tokio::sync::mpsc::Sender<String>,
) -> Result<Option<Value>> {
    let db = &st.db;
    let a = |k: &str| args.get(k).cloned().unwrap_or(Value::Null);
    let s = |k: &str| a(k).as_str().unwrap_or("").to_string();
    let i64v = |k: &str| a(k).as_i64().unwrap_or(0);
    // 布尔标志解析：前端可能传 true/false 也可能传 1/0。
    // 旧实现 v.as_i64().unwrap_or(1) 会把 false 当"缺省=1"，导致"停用"实际仍是启用。
    let flag = |k: &str, default: i64| match a(k) {
        Value::Bool(b) => {
            if b {
                1
            } else {
                0
            }
        }
        Value::Number(n) => n.as_i64().unwrap_or(default),
        Value::String(x) => match x.trim().to_ascii_lowercase().as_str() {
            "false" | "0" | "off" | "no" => 0,
            "true" | "1" | "on" | "yes" => 1,
            _ => default,
        },
        _ => default,
    };
    // bookId 非空守卫（原 14 处重复的 4 行样板合并；消息按调用点定制）
    let need_book = |msg: &str| -> Result<String> {
        let b = s("bookId");
        if b.is_empty() {
            return Err(anyhow!("{}", msg));
        }
        Ok(b)
    };

    // 可选 bookId（空=全书范围，用于导出）；避免各臂重复 6 行样板
    let opt_book = |k: &str| Some(s(k)).filter(|b| !b.is_empty());

    // ---------- 技能 ----------
    match cmd {
        "list_skills" => {
            let rows = db
                .q_json(
                    "SELECT * FROM skills ORDER BY origin='official' DESC, name",
                    &[],
                )
                .unwrap_or_default();
            Ok(Some(json!(rows.iter().map(skill_row).collect::<Vec<_>>())))
        }
        "create_skill" | "update_skill" => {
            // 技能配置是"本书输入"（effective_skills 依赖）：局部持锁，作用域结束自动释放
            let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
            let name = s("name");
            let kind = s("kind");
            let _usage = s("usageMode");
            let _origin = s("origin");
            let enabled = flag("enabled", 1);
            let targets = a("targets").as_array().cloned().unwrap_or_default();
            if cmd == "create_skill" {
                let id = uuid::Uuid::new_v4().to_string();
                let targets_s = serde_json::to_string(&targets).unwrap_or_default();
                let desc = s("description");
                let tpl = s("promptTemplate");
                // 前端可能传 usageMode:null——兜底 support，否则触发 CHECK 约束失败
                let usage = match a("usageMode") {
                    Value::Null => "support".to_string(),
                    v => v.as_str().unwrap_or("support").to_string(),
                };
                let origin = if s("origin").is_empty() {
                    "user".to_string()
                } else {
                    s("origin")
                };
                // builtin_key 为空必须插 NULL：唯一索引只豁免 NULL，空串会让第二个自建技能撞唯一约束
                let builtin_key: Option<String> = {
                    let k = s("builtinKey");
                    if k.is_empty() {
                        None
                    } else {
                        Some(k)
                    }
                };
                db.exec(
                    "INSERT INTO skills(id,name,description,prompt_template,kind,source,enabled,builtin_key,usage_mode,origin,targets_json) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
                    &[
                        &id as &dyn rusqlite::ToSql, &name, &desc, &tpl, &kind, &"", &enabled,
                        &builtin_key, &usage, &origin, &targets_s,
                    ],
                )?;
                Ok(Some(skill_mapped(db, &id)))
            } else {
                let id = s("id");
                // 动态 UPDATE
                let mut sets = Vec::new();
                let mut vals: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
                let map = [
                    ("name", "name"),
                    ("description", "description"),
                    ("promptTemplate", "prompt_template"),
                    ("kind", "kind"),
                    ("source", "source"),
                    ("enabled", "enabled"),
                    ("usageMode", "usage_mode"),
                    ("origin", "origin"),
                    ("builtinKey", "builtin_key"),
                ];
                for (k, col) in map {
                    if let Some(v) = args.get(k) {
                        sets.push(format!("{}=?", col));
                        if col == "enabled" {
                            // 支持 false：布尔/数字/字符串统一归一
                            let n = match v {
                                Value::Bool(b) => {
                                    if *b {
                                        1i64
                                    } else {
                                        0
                                    }
                                }
                                Value::Number(x) => x.as_i64().unwrap_or(1),
                                Value::String(x) => match x.trim().to_ascii_lowercase().as_str() {
                                    "false" | "0" | "off" | "no" => 0,
                                    _ => 1,
                                },
                                _ => 1,
                            };
                            vals.push(Box::new(n));
                        } else {
                            vals.push(Box::new(v.as_str().unwrap_or("").to_string()));
                        }
                    }
                }
                if let Some(t) = args.get("targets") {
                    if t.is_array() {
                        sets.push("targets_json=?".into());
                        vals.push(Box::new(serde_json::to_string(t).unwrap_or_default()));
                    }
                }
                if !sets.is_empty() {
                    vals.push(Box::new(id.clone()));
                    let sql = format!("UPDATE skills SET {} WHERE id=?", sets.join(","));
                    let refs: Vec<&dyn rusqlite::ToSql> = vals.iter().map(|b| b.as_ref()).collect();
                    db.exec(&sql, &refs)?;
                }
                Ok(Some(skill_mapped(db, &id)))
            }
        }
        "delete_skill" => {
            let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
            let id = s("id");
            db.exec(
                "DELETE FROM dw_book_skill WHERE skill_id=?1",
                &[&id as &dyn rusqlite::ToSql],
            )?;
            db.exec(
                "DELETE FROM skills WHERE id=?1",
                &[&id as &dyn rusqlite::ToSql],
            )?;
            Ok(Some(json!({"ok": true})))
        }
        "read_skill_ref" => Ok(Some(skill_mapped(db, &s("id")))),
        "read_skill_script" => {
            let rows = db.q_json(
                "SELECT prompt_template FROM skills WHERE id=?",
                &[&s("id") as &dyn rusqlite::ToSql],
            )?;
            Ok(Some(json!(rows
                .first()
                .and_then(|r| r["promptTemplate"].as_str())
                .unwrap_or(""))))
        }
        "write_skill_ref" => {
            let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
            // ref: {name/description/promptTemplate/...} 更新（复用 update_skill 动态字段）
            let id = s("id");
            let mut sets = Vec::new();
            let mut vals: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            for (k, col) in [
                ("name", "name"),
                ("description", "description"),
                ("promptTemplate", "prompt_template"),
                ("kind", "kind"),
                ("usageMode", "usage_mode"),
                ("origin", "origin"),
            ] {
                if let Some(v) = a("ref").get(k).or_else(|| args.get(k)) {
                    sets.push(format!("{}=?", col));
                    vals.push(Box::new(v.as_str().unwrap_or("").to_string()));
                }
            }
            if !sets.is_empty() {
                vals.push(Box::new(id.clone()));
                let sql = format!("UPDATE skills SET {} WHERE id=?", sets.join(","));
                let refs: Vec<&dyn rusqlite::ToSql> = vals.iter().map(|b| b.as_ref()).collect();
                db.exec(&sql, &refs)?;
            }
            Ok(Some(skill_mapped(db, &id)))
        }
        "create_skill_from_draft" => {
            let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
            let id = uuid::Uuid::new_v4().to_string();
            let name = {
                let n = s("name");
                if n.is_empty() {
                    "新技能".to_string()
                } else {
                    n
                }
            };
            db.exec(
                "INSERT INTO skills(id,name,description,prompt_template,kind,source,enabled,builtin_key,usage_mode,origin) VALUES(?1,?2,?3,?4,'user','',1,NULL,'standalone','user')",
                &[
                    &id as &dyn rusqlite::ToSql,
                    &name,
                    &s("description").chars().take(200).collect::<String>(),
                    &s("draft"),
                ],
            )?;
            Ok(Some(skill_mapped(db, &id)))
        }
        "draft_skill" => {
            // 技能工坊「帮我起草」：真实 LLM 生成技能模板（对齐 node draftSkill）
            let chn =
                molan_llm::active_channel(db).ok_or_else(|| anyhow!("未配置可用的模型渠道"))?;
            let p = read_prompts(&st.root);
            let user = format!(
                "{}\n用法场景：{}\n任务：{}\n技能名：{}\n描述：{}",
                p["skill_draft"].as_str().unwrap_or(""),
                s("usage"),
                s("task"),
                s("name"),
                s("description"),
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
            Ok(Some(json!({"draft": out.trim().to_string()})))
        }
        "set_skill_script_authorization2" => Ok(Some(json!({"ok": true, "text": ""}))),
        "test_run_skill_script" | "test_run_draft_script" => {
            // 技能工坊「试运行」：真实调一次 LLM 验证模板（draft 或技能库模板二选一）
            let tpl = {
                let mut t = s("draft");
                if t.is_empty() {
                    t = s("source");
                }
                let by_id = s("skillId");
                let by_name = s("skillName");
                let tpl_of = |r: &Value| {
                    r["promptTemplate"]
                        .as_str()
                        .map(|x| x.to_string())
                        .unwrap_or_default()
                };
                if t.is_empty() && !by_id.is_empty() {
                    t = skill_by_id(db, &by_id)
                        .map(|r| tpl_of(&r))
                        .unwrap_or_default();
                }
                if t.is_empty() && !by_name.is_empty() {
                    t = db
                        .q_json(
                            "SELECT * FROM skills WHERE name=?1",
                            &[&by_name as &dyn rusqlite::ToSql],
                        )
                        .ok()
                        .and_then(|r| r.first().map(tpl_of))
                        .unwrap_or_default();
                }
                t.chars().take(8000).collect::<String>()
            };
            let chn =
                molan_llm::active_channel(db).ok_or_else(|| anyhow!("未配置可用的模型渠道"))?;
            let out = molan_llm::chat_once(molan_llm::ChatParams {
                base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
                api_key: chn["key"].as_str().unwrap_or("").to_string(),
                model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
                messages: vec![
                    json!({"role": "system", "content": tpl}),
                    json!({"role": "user", "content": s("input")}),
                ],
                max_tokens: 4096,
                ..Default::default()
            })
            .await?;
            Ok(Some(json!({"ok": true, "text": out})))
        }
        "app_os" => {
            // 前端 Jy() 启动时取系统标识（网页版仅展示用途，对齐 node 'win32'）
            Ok(Some(json!("win32")))
        }
        "export_skill_md" => {
            let Some(row) = skill_by_id(db, &s("id")) else {
                return Err(anyhow!("技能不存在"));
            };
            let name = row["name"].as_str().unwrap_or("技能");
            let desc = row["description"].as_str().unwrap_or("");
            let tpl = row["promptTemplate"].as_str().unwrap_or("");
            let fp = st
                .root
                .join("data")
                .join("exports")
                .join(format!("{}.md", safe(name)));
            std::fs::create_dir_all(fp.parent().unwrap()).ok();
            let md = format!(
                "# {}\n\n{}\n\n## 模板\n\n```text\n{}\n```\n",
                name, desc, tpl
            );
            std::fs::write(&fp, md).ok();
            Ok(Some(json!({"path": fp.to_string_lossy()})))
        }
        "export_skill_bundle" => {
            let Some(row) = skill_by_id(db, &s("id")) else {
                return Err(anyhow!("技能不存在"));
            };
            let fp = st.root.join("data").join("exports").join(format!(
                "skill_{}.json",
                s("id").chars().take(8).collect::<String>()
            ));
            std::fs::create_dir_all(fp.parent().unwrap()).ok();
            let bundle = json!({"kind": "writerx-skill-bundle", "version": 1, "skills": [row]});
            std::fs::write(&fp, bundle.to_string()).ok();
            Ok(Some(json!({"path": fp.to_string_lossy()})))
        }
        "inspect_skill_md" => Ok(Some(
            json!({"name": "", "description": "", "instructions": ""}),
        )),
        "inspect_skill_bundle" => {
            let j = load_skill_bundle(st, &s("path"))?;
            if j.is_null() {
                return Err(anyhow!("不是有效的技能包 JSON"));
            }
            let skills = j["skills"]
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![j.clone()]);
            Ok(Some(
                json!({"valid": true, "skills": skills, "kind": j["kind"].as_str().unwrap_or("writerx-skill-bundle")}),
            ))
        }
        "install_skill_bundle" => {
            let j = load_skill_bundle(st, &s("path"))?;
            let skills = j["skills"]
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![j.clone()]);
            let mut added = 0;
            for sk in skills {
                let name = sk["name"].as_str().unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                let id = uuid::Uuid::new_v4().to_string();
                let _ = db.exec(
                    "INSERT INTO skills(id,name,description,prompt_template,kind,source,enabled,builtin_key,usage_mode,origin) VALUES(?1,?2,?3,?4,?5,'',1,NULL,'standalone','imported')",
                    &[
                        &id as &dyn rusqlite::ToSql,
                        &name,
                        &sk["description"].as_str().unwrap_or(""),
                        &sk["promptTemplate"].as_str().or_else(|| sk["prompt_template"].as_str()).unwrap_or(""),
                        &sk["kind"].as_str().unwrap_or("imported"),
                    ],
                );
                added += 1;
            }
            Ok(Some(json!({"ok": true, "added": added})))
        }
        "set_skill_enabled" => {
            let on = flag("on", 1);
            let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
            db.exec(
                "UPDATE skills SET enabled=? WHERE id=?",
                &[&on as &dyn rusqlite::ToSql, &s("id")],
            )?;
            drop(_g);
            Ok(Some(json!({"ok": true})))
        }

        // ---------- 设置 ----------
        "get_settings" => {
            let rows = db
                .q_json("SELECT key,value FROM settings", &[])
                .unwrap_or_default();
            let mut obj = serde_json::Map::new();
            for r in rows {
                if let (Some(k), Some(v)) = (r["key"].as_str(), r["value"].as_str()) {
                    // 渠道密钥脱敏：channel_key__* 非空值一律返回占位符，避免明文外带
                    if k.starts_with("channel_key__") && !v.is_empty() {
                        obj.insert(k.to_string(), json!("***"));
                    } else if k == "channels" {
                        // 旧版/脚本写入的渠道条目可能缺 models 数组，预打包前端直接
                        // 读 T.models.length 会整页崩溃——读侧补齐，不改库存数据。
                        obj.insert(k.to_string(), json!(normalize_channels_value(v)));
                    } else {
                        obj.insert(k.to_string(), json!(v));
                    }
                }
            }
            Ok(Some(Value::Object(obj)))
        }
        "direct_model_profiles" => {
            // 官方 Provider 预设（前端 Dl() 期望的结构）；与 Node 版同源（profiles.json）
            let p: Value = serde_json::from_str(include_str!("../profiles.json")).unwrap_or_else(
                |_| json!({"catalogVersion": "replica-1", "updatedAt": 0, "profiles": []}),
            );
            Ok(Some(p))
        }
        "sync_app_config" => Ok(Some(json!({"ok": true}))),
        "trial_signup_enabled" => Ok(Some(json!(false))),
        "set_setting" | "set_settings" | "set_settings_batch" => {
            // 设置是"本书输入"：必须在 fs_lock 局部范围内提交，
            // 否则 B 在依赖检查后作者仍可改设置，绕过指纹校验。
            // 注意：只包住纯 db 写，不跨 await、不调用会自行取锁的 core 函数（fs_lock 非可重入）。
            let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
            if cmd == "set_setting" {
                db.exec(
                    "INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    &[&s("key") as &dyn rusqlite::ToSql, &s("value")],
                )?;
            } else {
                // set_settings 用 entries，set_settings_batch 用 updates——两者都是 KV 对象
                let updates = a("entries")
                    .as_object()
                    .cloned()
                    .or_else(|| a("updates").as_object().cloned());
                if let Some(updates) = updates {
                    for (k, v) in updates {
                        // channels 这类数组值前端已 JSON.stringify；对象值序列化存 JSON 文本
                        let vs = match &v {
                            Value::String(x) => x.clone(),
                            other => other.to_string(),
                        };
                        db.exec(
                            "INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                            &[&k as &dyn rusqlite::ToSql, &vs],
                        )?;
                    }
                }
            }
            drop(_g);
            Ok(Some(json!({"ok": true})))
        }

        // ---------- 渠道 / LLM 目录 ----------
        "has_channel_key" => {
            let id = s("id");
            Ok(Some(json!(!molan_llm::channel_key(db, &id).is_empty())))
        }
        "has_api_key" => Ok(Some(json!(molan_llm::has_any_key(db)))),
        "set_channel_key" => {
            let id = s("id");
            let key = s("key");
            db.exec(
                "INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                &[&format!("channel_key__{}", id) as &dyn rusqlite::ToSql, &key],
            )?;
            Ok(Some(json!({"ok": true})))
        }
        "delete_channel_key" => {
            let id = s("id");
            db.exec(
                "DELETE FROM settings WHERE key=?",
                &[&format!("channel_key__{}", id) as &dyn rusqlite::ToSql],
            )?;
            Ok(Some(json!({"ok": true})))
        }
        "llm_catalog" => Ok(Some(llm_catalog(db, &st.root))),
        "llm_balance" => Ok(Some(
            json!({"enabled": false, "totalRemaining": 0, "packs": []}),
        )),
        "llm_usage" => {
            // 真实用量账本：按模型/用途与按天聚合 llm_call_log（默认近 7 天）
            let since = a("sinceMs")
                .as_i64()
                .unwrap_or_else(|| stats::now_ms() - 7 * 86_400_000);
            let by_model = db.q_json(
                "SELECT model, tag, COUNT(*) AS calls, SUM(prompt_tokens) AS promptTokens, SUM(completion_tokens) AS completionTokens, SUM(total_tokens) AS totalTokens FROM llm_call_log WHERE ts >= ?1 GROUP BY model, tag ORDER BY totalTokens DESC",
                &[&since as &dyn rusqlite::ToSql],
            )?;
            let by_day = db.q_json(
                "SELECT (ts / 86400000) AS day, COUNT(*) AS calls, SUM(total_tokens) AS totalTokens FROM llm_call_log WHERE ts >= ?1 GROUP BY day ORDER BY day",
                &[&since as &dyn rusqlite::ToSql],
            )?;
            Ok(Some(
                json!({"since": since, "byModel": by_model, "byDay": by_day}),
            ))
        }
        "llm_orders" => Ok(Some(json!({"orders": [], "items": []}))),
        "list_models_ex" | "list_models_channel" => {
            let (mu, key) = if cmd == "list_models_ex" {
                // 用户自填 URL + Key：仅做 scheme 校验，禁止 file:// 等非 http(s) 目标
                let u = s("modelsUrl");
                if !(u.starts_with("http://") || u.starts_with("https://")) {
                    return Err(anyhow!("modelsUrl 必须以 http:// 或 https:// 开头"));
                }
                (u, s("apiKey"))
            } else {
                // 渠道凭据：忽略调用方 modelsUrl，一律用渠道自身配置的地址，防凭据被外带到任意 URL
                let id = s("id");
                let (channels, _) = molan_llm::all_settings(db);
                let ch = channels
                    .iter()
                    .find(|c| c["id"].as_str() == Some(id.as_str()))
                    .ok_or_else(|| anyhow!("渠道不存在：{}", id))?;
                let mu = ch["modelsUrl"].as_str().unwrap_or("").to_string();
                if mu.trim().is_empty() {
                    return Err(anyhow!("该渠道未配置 modelsUrl，无法获取模型列表"));
                }
                (mu, molan_llm::channel_key(db, &id))
            };
            let list = molan_llm::fetch_model_list(&mu, &key)
                .await
                .unwrap_or_default();
            Ok(Some(json!(list)))
        }
        "test_chat_ex" => {
            // 渠道测试：一次小请求（调用方自填 baseUrl，仅允许 http(s)）
            let base_url = s("baseUrl");
            if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
                return Err(anyhow!("baseUrl 必须以 http:// 或 https:// 开头"));
            }
            let ch = molan_llm::ChatParams {
                base_url,
                api_key: s("apiKey"),
                model: s("model"),
                messages: vec![json!({"role": "user", "content": "回复\"OK\"两个字母即可"})],
                max_tokens: 16,
                // 渠道连通测试：不思考
                reasoning_effort: String::new(),
                ..Default::default()
            };
            let t0 = std::time::Instant::now();
            match molan_llm::chat_once_retry(ch).await {
                Ok(out) => Ok(Some(json!({
                    "ok": true, "caseId": format!("direct-{}", stats::now_ms()),
                    "resolvedMode": "off", "ttftMs": 120,
                    "totalMs": t0.elapsed().as_millis() as i64,
                    "reasoningChars": 0, "outputChars": out.chars().count(),
                    "output": out,
                }))),
                Err(e) => Ok(Some(
                    json!({"ok": false, "output": e.to_string().chars().take(200).collect::<String>()}),
                )),
            }
        }

        // ---------- 书 ----------
        "list_books" => Ok(Some(molan_core::books::list_books(db))),
        "begin_book_setup" => Ok(Some(molan_core::setup_workspace::begin(db, args)?)),
        "get_book_setup_context" => Ok(Some(molan_core::setup_workspace::context(db, args)?)),
        "create_book" => {
            let title = s("title").trim().to_string();
            if title.is_empty() {
                return Err(anyhow!("书名不能为空"));
            }
            Ok(Some(molan_core::books::create_book(
                db,
                &title,
                &s("genre"),
                &s("pov"),
            )))
        }
        "rename_book" => Ok(Some(molan_core::books::rename_book(
            db,
            &s("bookId"),
            &s("title"),
            &s("genre"),
            &s("status"),
        ))),
        "delete_book" => Ok(Some(molan_core::books::delete_book(db, &s("bookId")))),
        "import_book" => {
            let files: Vec<Value> = a("files").as_array().cloned().unwrap_or_default();
            Ok(Some(files::import_book(
                db,
                &s("title"),
                &s("genre"),
                &files,
            )?))
        }
        "scan_tree" => Ok(Some(files::scan_tree(db, &s("bookId")))),
        "read_file" => {
            let content =
                files::read_file(db, &s("bookId"), &s("group"), &s("name")).unwrap_or_default();
            Ok(Some(json!(content)))
        }
        // ============ 生产线状态（P1：文件即真相推导，只读聚合，不写任何状态） ============
        "get_pipeline_state" => {
            let book_id = s("bookId");
            if book_id.is_empty() {
                return Err(anyhow!("缺少 bookId"));
            }
            let tree = files::scan_tree(db, &book_id);
            let queue = Value::Array(
                db.q_json(
                    "SELECT ch, review_file, status FROM pending_chapter WHERE book_id=?1",
                    &[&book_id as &dyn rusqlite::ToSql],
                )
                .unwrap_or_default(),
            );
            let mut state = molan_core::pipeline::derive_state(&tree, &queue);
            state["bookId"] = json!(book_id);
            let next = state["next"].clone();
            state["nextContext"] = stream::stage_context::preview(db, &book_id, &next);
            Ok(Some(state))
        }
        // ============ 审批流：AI 产出待审章节的人工接受/拒绝 ============
        "list_pending_chapters" => {
            let book_id = s("bookId");
            let rows = db
                .q_json(
                    "SELECT ch, review_file, status, created_at FROM pending_chapter WHERE book_id=?1 AND status='pending' ORDER BY ch",
                    &[&book_id as &dyn rusqlite::ToSql],
                )
                .unwrap_or_default();
            let mut out: Vec<Value> = Vec::new();
            for r in rows {
                let name = r["reviewFile"].as_str().unwrap_or("").to_string();
                let ch = r["ch"].as_i64().unwrap_or(0);
                let content = files::read_file(db, &book_id, molan_core::db::REVIEW_GROUP, &name)
                    .unwrap_or_default();
                let mut outline = String::new();
                if let Some(o) =
                    files::read_file(db, &book_id, "细纲", &format!("细纲_第{}章.md", ch))
                {
                    outline = o.chars().take(200).collect();
                }
                out.push(json!({
                    "ch": ch, "name": name,
                    "words": content.chars().count(),
                    "preview": content.chars().take(160).collect::<String>(),
                    "outline": outline,
                    "deai": molan_core::deai::score_text(&content)["score"],
                }));
            }
            Ok(Some(json!(out)))
        }
        "read_pending_chapter" => {
            let content =
                files::read_file(db, &s("bookId"), molan_core::db::REVIEW_GROUP, &s("name"))
                    .unwrap_or_default();
            Ok(Some(json!(content)))
        }
        "approve_chapter" => {
            // 审批唯一入口：core::approve_pending_chapter（依赖检查 + 非覆盖 + 先落盘后删待审）
            let book_id = s("bookId");
            let name = {
                let n = s("name");
                if !n.is_empty() {
                    n
                } else {
                    // 兼容只传 ch 的调用方：从队列反查文件名
                    let ch = a("ch").as_i64().unwrap_or(0);
                    db.q_json(
                        "SELECT review_file FROM pending_chapter WHERE book_id=?1 AND ch=?2",
                        &[
                            &book_id as &dyn rusqlite::ToSql,
                            &ch as &dyn rusqlite::ToSql,
                        ],
                    )
                    .ok()
                    .and_then(|v| {
                        v.first()
                            .and_then(|r| r["reviewFile"].as_str().map(|x| x.to_string()))
                    })
                    .unwrap_or_default()
                }
            };
            if book_id.is_empty() || name.is_empty() {
                return Err(anyhow!("缺少 bookId/name"));
            }
            // 重入判定：不能只看"文件在不在"。
            // 需要 pending_chapter 为 approved **且** file_revision 记录的正式稿 hash
            // 与当前磁盘正式稿 hash 一致，才算真正幂等已批准。
            // 旧库没有 hash 记录时明确提示"需人工确认"，绝不假装相等。
            let row = db
                .q_json(
                    "SELECT status FROM pending_chapter WHERE book_id=?1 AND review_file=?2",
                    &[
                        &book_id as &dyn rusqlite::ToSql,
                        &name as &dyn rusqlite::ToSql,
                    ],
                )
                .unwrap_or_default();
            let approved_status =
                row.first().and_then(|r| r["status"].as_str()) == Some("approved");
            if approved_status {
                let formal = files::book_path(db, &book_id, "正文", &name);
                let disk = std::fs::read_to_string(&formal).ok();
                // 批准回执取自 immutable 的 chapter_approved 事件（父 continuity）。
                // 不能用 file_revision：它记录的是"最近一次作者改写"，永远等于最新稿，无法证明当初批的是哪一版。
                let receipt =
                    molan_core::continuity::approved_hash(db, &book_id, &name).unwrap_or(None);
                match (disk.as_deref(), receipt.as_deref()) {
                    (Some(d), Some(rec)) if molan_core::continuity::content_hash(d) == rec => {
                        return Ok(Some(json!({
                            "ok": true, "finalName": name, "alreadyApproved": true,
                            "verified": "approved-receipt-hash-match",
                        })));
                    }
                    (Some(_), Some(_)) => {
                        return Err(anyhow!(
                            "该章已批准，但当前正式稿与批准回执的指纹不一致（批准后被改动过）。请人工确认后再处理，系统不会静默重复接受。"
                        ));
                    }
                    (Some(_), None) => {
                        // 旧库无批准回执：不假称已验
                        return Ok(Some(json!({
                            "ok": true, "finalName": name, "alreadyApproved": true,
                            "verified": "unverified",
                            "warning": "该章已标记批准，但缺少批准回执（历史数据），无法核对批准后是否被改动，请人工确认。",
                        })));
                    }
                    (None, _) => {
                        return Err(anyhow!(
                            "队列显示该章已批准，但正式稿文件不存在：{}。请检查磁盘或重新生成。",
                            name
                        ));
                    }
                }
            }
            let final_name = files::approve_pending_chapter(db, &book_id, &name)?;
            stats::refresh_words(db, &book_id);
            tracing::info!("审批通过：{} → 正文/{}", name, final_name);
            // 章后处理：以"刚批准的章"为对象（不再取全书最大章）。失败不抹掉已提交成果。
            let ch = chapter_num_from_name(&final_name).unwrap_or(0);
            // 章节状态机（P0-2）：人工批准入账（只观测不阻断）
            if ch > 0 {
                let _ = molan_core::chapter_state::record_approved(
                    db,
                    &book_id,
                    ch,
                    json!({"finalName": final_name, "by": "manual_approve"}),
                );
            }
            let post = if ch > 0 {
                spawn_post_approved(Arc::clone(st), &book_id, ch);
                "queued"
            } else {
                "skipped（无法从文件名解析章号）"
            };
            Ok(Some(json!({
                "ok": true, "finalName": final_name, "postProcess": post,
                "postProcessNote": "章后处理异步执行；若失败，定稿仍保留，失败原因可通过 memory_status 查询并重跑。",
            })))
        }
        "reject_chapter" => {
            let book_id = s("bookId");
            let ch = a("ch").as_i64().unwrap_or(0);
            let name = s("name");
            // 拒绝 = 待审稿安全进回收站（可恢复）；回收失败必须报错，不能假装拒绝成功
            files::delete_file(db, &book_id, molan_core::db::REVIEW_GROUP, &name)?;
            let updated = db
                .exec(
                    "UPDATE pending_chapter SET status='rejected', updated_at=?2 WHERE book_id=?1 AND (ch=?3 OR review_file=?4)",
                    &[
                        &book_id as &dyn rusqlite::ToSql,
                        &stats::now_ms(),
                        &ch,
                        &name as &dyn rusqlite::ToSql,
                    ],
                )
                .unwrap_or(0);
            if updated == 0 {
                tracing::warn!(
                    "reject_chapter：未找到待审记录（book={} ch={} name={}）",
                    &book_id,
                    ch,
                    &name
                );
            }
            Ok(Some(json!({"ok": true})))
        }
        "approve_all_pending" => {
            let book_id = s("bookId");
            let rows = db
                .q_json(
                    "SELECT ch, review_file FROM pending_chapter WHERE book_id=?1 AND status='pending' ORDER BY ch",
                    &[&book_id as &dyn rusqlite::ToSql],
                )
                .unwrap_or_default();
            let mut approved: Vec<i64> = Vec::new();
            let mut failures: Vec<Value> = Vec::new();
            for r in rows {
                let ch = r["ch"].as_i64().unwrap_or(0);
                let name = r["reviewFile"].as_str().unwrap_or("").to_string();
                if name.is_empty() {
                    continue;
                }
                match files::approve_pending_chapter(db, &book_id, &name) {
                    Ok(final_name) => {
                        approved.push(ch);
                        let real_ch = chapter_num_from_name(&final_name).unwrap_or(ch);
                        if real_ch > 0 {
                            spawn_post_approved(Arc::clone(st), &book_id, real_ch);
                        }
                    }
                    // 单章失败不中断其余：逐条返回原因（冲突/依赖失效/IO 错误）
                    Err(e) => {
                        failures.push(json!({"ch": ch, "name": name, "error": e.to_string()}))
                    }
                }
            }
            if !approved.is_empty() {
                stats::refresh_words(db, &book_id);
            }
            Ok(Some(json!({
                "ok": failures.is_empty(),
                "approved": approved.len(),
                "approvedChapters": approved,
                "failures": failures,
            })))
        }
        "save_chat_output" => {
            // 作者在回复气泡中主动选择去向后才能调用。AI 文本绝不写正式正文或覆盖已有文件。
            let book_id = s("bookId");
            let group = files::normalize_group(&s("group"));
            let name = s("name");
            let content = s("content");
            if !safe_chat_save_group(&group) {
                return Err(anyhow!(
                    "请选择设定、细纲、参考或正文待审；正式正文必须走审批"
                ));
            }
            if content.trim().is_empty()
                || name.trim().is_empty()
                || files::safe_name(&name) != name
            {
                return Err(anyhow!("内容或文件名无效，未写入"));
            }
            if group == molan_core::db::REVIEW_GROUP {
                let ch = chapter_num_from_name(&name)
                    .ok_or_else(|| anyhow!("正文待审文件名必须包含第N章，才能登记审批队列"))?;
                let rows = db.q_json(
                    "SELECT review_file FROM pending_chapter WHERE book_id=?1 AND ch=?2 AND status='pending'",
                    &[&book_id as &dyn rusqlite::ToSql, &ch],
                )?;
                if !rows.is_empty() {
                    return Err(anyhow!("该章已有待审稿，请先处理审批队列"));
                }
                files::write_ai_file(db, &book_id, &group, &name, &content)?;
                let queued = register_review_queue(db, &book_id, &name, &content)?;
                return Ok(Some(
                    json!({"ok": true, "group": group, "name": name, "pending": queued}),
                ));
            }
            files::write_ai_file(db, &book_id, &group, &name, &content)?;
            Ok(Some(json!({"ok": true, "group": group, "name": name})))
        }
        "write_file" => {
            let mut group = s("group");
            let name = s("name");
            // 方向纠偏：前端「存入资料库」弹窗可能带错分组（bundle 不可改，后端兜底）
            if name.starts_with("细纲") && group != "细纲" {
                group = "细纲".to_string();
            }
            if name.starts_with("聊天摘录") && group == "正文" {
                group = "参考".to_string();
            }
            let book_id = s("bookId");
            let content = s("content");
            files::write_file(db, &book_id, &group, &name, &content)?;
            // 作者手动把稿子放进「正文待审」时，必须同步登记审批队列，
            // 否则 approve 会因查不到队列记录而失败（正文已落盘却报错，用户卡在双稿状态）。
            // 规则（按章 upsert，且**内容感知**）：
            // - 保持 approved 仅当"队列已批准"且"本次写入内容 == 批准回执 hash"（同一份稿重复保存）；
            // - 否则一律回到 pending —— 作者改了稿、或这是同章的新草稿，都必须重新走审批，
            //   绝不能因为"该章历史上批准过"就让新内容继承 approved（否则新稿永远批不了）。
            let queued = if files::normalize_group(&group) == molan_core::db::REVIEW_GROUP {
                register_review_queue(db, &book_id, &name, &content)?
            } else {
                Value::Null
            };
            Ok(Some(json!({"ok": true, "group": group, "pending": queued})))
        }
        // 中断产出恢复通道：人工明确点击后，把已生成部分存为正文待审章（留版本快照 + 走审批队列），
        // 解决「中断后已生成部分无法写入」的痛点。只接受带章节标题的产出；短文本/无标题一律拒绝，绝不静默落盘。
        "save_partial_as_review" => {
            let book_id = s("bookId");
            let content = s("content");
            if book_id.is_empty() {
                return Err(anyhow!("缺少 bookId"));
            }
            if content.chars().count() < 200 {
                return Err(anyhow!("内容过短（<200字），不存入待审"));
            }
            let ch = content
                .lines()
                .find_map(|l| {
                    let t = l.trim();
                    if !t.starts_with('#') {
                        return None;
                    }
                    chapter_num_from_name(t.trim_start_matches('#').trim())
                })
                .or_else(|| {
                    // 无标题残稿：用消息里点名的章号定章（快捷写作流常见）
                    let t = s("ch");
                    let t = t.trim().to_string();
                    if t.is_empty() {
                        None
                    } else {
                        chapter_num_from_name(&format!("第{}章", t))
                    }
                })
                .ok_or_else(|| anyhow!("产出无「# 第N章」标题且未指明章号，无法定章存入待审"))?;
            let name = format!("第{}章.md", ch);
            files::write_file(db, &book_id, molan_core::db::REVIEW_GROUP, &name, &content)?;
            let queued = register_review_queue(db, &book_id, &name, &content)?;
            Ok(Some(
                json!({"ok": true, "ch": ch, "name": name, "pending": queued}),
            ))
        }
        "apply_asset_update" => {
            // 设定提取「应用」按键：同名文件在哪个组就回写哪个组（正文>细纲>设定>参考），找不到归设定
            let book_id = s("bookId");
            let t = s("targetName");
            let content = s("content");
            if !book_id.is_empty() && !t.is_empty() && !content.is_empty() {
                let tree = files::scan_tree(db, &book_id);
                let order = |d: &str| match d {
                    "正文" => 0,
                    "细纲" => 1,
                    "设定" => 2,
                    "参考" => 3,
                    _ => 9,
                };
                let mut best: Option<String> = None;
                if let Some(arr) = tree.as_array() {
                    for g in arr {
                        let dir = g["dir"]
                            .as_str()
                            .or_else(|| g["groupDir"].as_str())
                            .unwrap_or("");
                        let has = g["files"]
                            .as_array()
                            .map(|fs| fs.iter().any(|f| f["name"].as_str() == Some(t.as_str())))
                            .unwrap_or(false);
                        if has {
                            let better = match &best {
                                None => true,
                                Some(b) => order(dir) < order(b),
                            };
                            if better {
                                best = Some(dir.to_string());
                            }
                        }
                    }
                }
                let group = best.unwrap_or_else(|| "设定".to_string());
                files::write_file(db, &book_id, &group, &t, &content)?;
            }
            let out = json!({
                "ok": true, "applied": true,
                "proposalId": if s("proposalId").is_empty() { Value::Null } else { json!(s("proposalId")) },
                "targetName": t,
                "messageId": if s("messageId").is_empty() { Value::Null } else { json!(s("messageId")) },
                "appliedAt": stats::now_ms(),
            });
            // Node 版返回 JSON 字符串，前端直接渲染
            Ok(Some(json!(out.to_string())))
        }
        "save_decompose_report" => {
            // 前端 {bookId, name, content|report}：写入参考目录，返回文件名
            let book_id = s("bookId");
            let mut content = s("content");
            if content.is_empty() {
                content = s("report");
            }
            if book_id.is_empty() || content.is_empty() {
                return Ok(Some(json!("")));
            }
            let fname = format!(
                "{}.md",
                s("name").trim_end_matches(".md").trim_end_matches(".txt")
            );
            files::write_file(db, &book_id, "参考", &fname, &content)?;
            // 拆解报告同时存为文风卡（kind=style）：「本书写作配置→文风→我的文风卡」可选用
            let skill_name = fname.trim_end_matches(".md").to_string();
            let first_line = content
                .lines()
                .map(|l| l.trim())
                .find(|l| !l.is_empty())
                .unwrap_or("");
            let desc = format!(
                "拆解报告文风卡：{}",
                first_line.chars().take(60).collect::<String>()
            );
            let existing = db
                .q_json(
                    "SELECT id FROM skills WHERE name=?1 AND kind='style'",
                    &[&skill_name as &dyn rusqlite::ToSql],
                )
                .unwrap_or_default();
            if let Some(row) = existing.first() {
                let id = row["id"].as_str().unwrap_or("").to_string();
                let _ = db.exec(
                    "UPDATE skills SET description=?1, prompt_template=?2, enabled=1, origin='user', source='' WHERE id=?3",
                    &[&desc as &dyn rusqlite::ToSql, &content, &id],
                );
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                let _ = db.exec(
                    "INSERT INTO skills(id,name,description,prompt_template,kind,source,enabled,builtin_key) VALUES(?1,?2,?3,?4,'style','',1,NULL)",
                    &[&id as &dyn rusqlite::ToSql, &skill_name, &desc, &content],
                );
            }
            Ok(Some(json!(fname)))
        }
        "rename_file" => {
            files::rename_file(db, &s("bookId"), &s("group"), &s("name"), &s("newName"))?;
            Ok(Some(json!({"ok": true})))
        }
        "delete_file" => {
            let trash_id = files::delete_file(db, &s("bookId"), &s("group"), &s("name"))?;
            Ok(Some(json!({"ok": true, "trashId": trash_id})))
        }
        "create_file" => {
            // 新建空文件：走 core 的锁内 check+write（write_file_new），
            // 不再手拼路径 + std::fs::write 绕开锁与审计；已存在即冲突报错。
            let book_id = s("bookId");
            let group = s("group");
            let name = s("name");
            if book_id.is_empty() || name.is_empty() {
                return Err(anyhow!("缺少 bookId/name"));
            }
            files::write_file_new(db, &book_id, &group, &name, "")?;
            Ok(Some(
                json!({"ok": true, "group": files::normalize_group(&group)}),
            ))
        }
        "create_folder" => Ok(Some(files::create_folder(db, &s("bookId"), &s("name")))),
        "rename_folder" => Ok(Some(files::rename_folder(
            db,
            &s("bookId"),
            &s("from"),
            &s("to"),
        ))),
        "delete_folder" => Ok(Some(json!(files::delete_folder(
            db,
            &s("bookId"),
            &s("name")
        )?))),
        "list_versions" => Ok(Some(files::list_versions(
            db,
            &s("bookId"),
            &s("group"),
            &s("name"),
        ))),
        "restore_version" => Ok(Some(files::restore_version(
            db,
            &s("bookId"),
            &s("group"),
            &s("name"),
            i64v("ts"),
        ))),
        "set_file_ai_flag" => {
            // 文件锁定/AI隐藏：交给 core 的统一 helper（内部已做规范组名归一 + fs_lock + 校验）。
            // 不再手拼 file_flags JSON，避免重复取锁与 shadow。
            let book_id = s("bookId");
            let group = s("group");
            let name = s("name");
            let flag_name = s("flag"); // "locked" | "aiOff"
            if book_id.is_empty() || name.is_empty() {
                return Err(anyhow!("缺少 bookId/name"));
            }
            // value 同时接受布尔与 0/1/字符串，避免 as_bool 失败被当成 false
            let value = flag("value", 0) != 0;
            files::set_file_flag(db, &book_id, &group, &name, &flag_name, value)?;
            // 章节状态机：locked 标志联动 LOCKED 状态（只观测不阻断）
            if flag_name == "locked" && group == "正文" {
                if let Some(ch) = chapter_num_from_name(&name) {
                    let _ = molan_core::chapter_state::record_locked(db, &book_id, ch, value);
                }
            }
            Ok(Some(json!({"ok": true, "flag": flag_name, "value": value})))
        }
        "chapter_asset_footprint" => Ok(Some(json!([]))),
        "rollback_chapter_assets" => Ok(Some(json!({"ok": true}))),

        // ---------- 回收站（bookId 分流：文件级 / 书本级） ----------
        "list_trash" => {
            let bid = s("bookId");
            Ok(Some(if bid.is_empty() {
                molan_core::books::list_trash(db)
            } else {
                files::list_file_trash(db, &bid)
            }))
        }
        "restore_trash" => {
            let bid = s("bookId");
            Ok(Some(if bid.is_empty() {
                molan_core::books::restore_trash(db, &s("id"))
            } else {
                files::restore_file_trash(db, &bid, &s("id"))
            }))
        }
        "purge_book" => {
            let bid = s("bookId");
            if bid.is_empty() {
                return Err(anyhow!("缺少 bookId"));
            }
            // 运行中禁止破坏性操作：任务还在写盘时清书会造成半删状态
            if running_auto_books(db).iter().any(|b| b == &bid) {
                return Err(anyhow!("该书自动写作任务正在运行，请先停止任务再永久删除"));
            }
            Ok(Some(molan_core::books::purge_book(db, &bid)))
        }
        "clear_trash" => {
            let bid = s("bookId");
            if bid.is_empty() {
                return Ok(Some(molan_core::books::clear_trash(db)));
            }
            files::clear_file_trash(db, &bid)?;
            Ok(Some(json!({"ok": true})))
        }

        // ---------- 写作引擎（对齐 Node runSmart / saveDoc / scene 消息等） ----------
        "get_next_chapter_state" => {
            // 前端 du() 期望对象 {targetChapter, outlineName}——返回裸数字会让「写第N章/连写」显示 undefined
            let tree = files::scan_tree(db, &s("bookId"));
            let target = max_chapter_num(&tree, "正文") + 1;
            let outline_name = tree
                .as_array()
                .and_then(|arr| {
                    arr.iter()
                        .find(|g| g["dir"] == json!("细纲") || g["groupDir"] == json!("细纲"))
                        .and_then(|g| g["files"].as_array().map(|fs| fs.to_vec()))
                })
                .unwrap_or_default()
                .iter()
                .find_map(|f| {
                    let name = f["name"].as_str().unwrap_or("");
                    (chapter_num_from_name(name) == Some(target)).then(|| name.to_string())
                });
            Ok(Some(
                json!({"targetChapter": target, "outlineName": outline_name}),
            ))
        }
        "get_next_section_state" => {
            // 短篇写作状态（对齐前端 zu() 期望：sections/hasLead/hasBlueprint/words/blueprintSections）
            let book_id = s("bookId");
            let tree = files::scan_tree(db, &book_id);
            let files_in = |group: &str| -> Vec<Value> {
                tree.as_array()
                    .and_then(|arr| {
                        arr.iter()
                            .find(|g| g["dir"] == json!(group) || g["groupDir"] == json!(group))
                            .and_then(|g| g["files"].as_array().map(|fs| fs.to_vec()))
                    })
                    .unwrap_or_default()
            };
            let chapters = files_in("正文");
            let has_lead = chapters.iter().any(|f| f["name"] == json!("全文.md"));
            let words: i64 = chapters
                .iter()
                .filter_map(|f| f["name"].as_str())
                .filter_map(|n| files::read_file(db, &book_id, "正文", n))
                .map(|c| c.chars().count() as i64)
                .sum();
            let blueprint = files_in("设定");
            let has_blueprint = blueprint.iter().any(|f| f["name"] == json!("节奏蓝图.md"));
            let blueprint_name = "节奏蓝图.md";
            let blueprint_sections = if has_blueprint {
                files::read_file(db, &book_id, "设定", blueprint_name)
                    .map(|c| c.matches("##").count() as i64)
                    .unwrap_or(0)
            } else {
                0
            };
            Ok(Some(json!({
                "sections": chapters.len() as i64,
                "hasLead": has_lead,
                "hasBlueprint": has_blueprint,
                "words": words,
                "blueprintSections": blueprint_sections,
            })))
        }
        "create_scene_message" => {
            let id = uuid::Uuid::new_v4().to_string();
            let session_id = s("sessionId");
            // C10 归属校验：会话必须属于 args.bookId，否则拒绝往任意会话插消息
            sessions_owned(db, &session_id, &s("bookId"))?;
            let doc_name = s("docName");
            let plan: Value =
                serde_json::from_str::<Value>(&s("scenePlanJson")).unwrap_or(Value::Null);
            let content = plan
                .as_object()
                .and_then(|o| o.get("title").and_then(|t| t.as_str()))
                .unwrap_or(&doc_name)
                .to_string();
            if !session_id.is_empty() {
                db.exec(
                    "INSERT INTO messages(id,session_id,role,content,context_json,steps_json,result_json,created_at) VALUES(?,?,?,?,?,NULL,NULL,?)",
                    &[
                        &id as &dyn rusqlite::ToSql,
                        &session_id,
                        &"scene",
                        &content,
                        &json!({"docName": doc_name, "plan": plan}).to_string(),
                        &stats::now_ms(),
                    ],
                )?;
                db.exec(
                    "UPDATE sessions SET updated_at=? WHERE id=?",
                    &[&stats::now_ms() as &dyn rusqlite::ToSql, &session_id],
                )?;
            }
            let rows = db.q_json(
                "SELECT * FROM messages WHERE id=?",
                &[&id as &dyn rusqlite::ToSql],
            )?;
            Ok(Some(
                rows.first()
                    .map(molan_core::books::msg_row)
                    .unwrap_or(json!({})),
            ))
        }
        "refresh_empty_scene_messages" => {
            // C10 归属校验：会话必须属于 args.bookId，否则拒绝读取他人会话消息
            let session_id = s("sessionId");
            sessions_owned(db, &session_id, &s("bookId"))?;
            let rows = db
                .q_json(
                    "SELECT * FROM messages WHERE session_id=? AND role='scene' ORDER BY created_at ASC",
                    &[&session_id as &dyn rusqlite::ToSql],
                )
                .unwrap_or_default();
            Ok(Some(msg_rows_json(&rows)))
        }
        // ---------- 计划持久化（arc/scene）----------
        // 真实落库：story_plan(book_id,kind) 由父代理 continuity 维护，含 revision。
        // 未实现的能力明确报错，绝不返回"空成功"。
        "save_arc_plan" => {
            let book_id = s("bookId");
            if book_id.is_empty() {
                return Err(anyhow!("缺少 bookId：计划必须绑定到具体书籍"));
            }
            // 兼容两种载荷：{arcPlan:...} 与 {chaptersJson:[...]}
            let payload = if let Some(p) = args.get("arcPlan") {
                p.clone()
            } else if let Some(c) = args.get("chaptersJson") {
                json!({ "chapters": c })
            } else if let Some(p) = args.get("plan") {
                p.clone()
            } else {
                return Err(anyhow!("缺少 arcPlan/chaptersJson 载荷"));
            };
            let saved = molan_core::continuity::save_plan(db, &book_id, "arc", &payload)?;
            Ok(Some(
                json!({"ok": true, "arcPlan": saved["plan"], "revision": saved["revision"]}),
            ))
        }
        "update_arc_plan" => {
            // 兼容旧行为：仍写消息 result_json；同时真实持久化到 story_plan
            let book_id = s("bookId");
            let payload = json!({"chapters": a("chaptersJson")});
            let _ = db.exec(
                "UPDATE messages SET result_json=?, updated_at=? WHERE id=?",
                &[
                    &json!({"arcPlan": a("chaptersJson")}).to_string() as &dyn rusqlite::ToSql,
                    &stats::now_ms(),
                    &s("messageId"),
                ],
            );
            if !book_id.is_empty() {
                let saved = molan_core::continuity::save_plan(db, &book_id, "arc", &payload)?;
                return Ok(Some(
                    json!({"ok": true, "arcPlan": saved["plan"], "revision": saved["revision"]}),
                ));
            }
            Ok(Some(json!({"ok": true, "persisted": false})))
        }
        "get_arc_plan" => {
            let book_id = need_book("缺少 bookId")?;
            let got = molan_core::continuity::get_plan(db, &book_id, "arc")?;
            Ok(Some(
                json!({"ok": true, "arcPlan": got["plan"], "revision": got["revision"], "updatedAt": got["updatedAt"]}),
            ))
        }
        "update_scene_plan" => {
            let book_id = s("bookId");
            if book_id.is_empty() {
                return Err(anyhow!("缺少 bookId：计划必须绑定到具体书籍"));
            }
            let payload = if let Some(p) = args.get("scenePlan") {
                p.clone()
            } else if let Some(p) = args.get("plan") {
                p.clone()
            } else if let Some(c) = args.get("scenesJson") {
                json!({ "scenes": c })
            } else {
                return Err(anyhow!("缺少 scenePlan 载荷"));
            };
            let saved = molan_core::continuity::save_plan(db, &book_id, "scene", &payload)?;
            Ok(Some(
                json!({"ok": true, "scenePlan": saved["plan"], "revision": saved["revision"]}),
            ))
        }
        "get_scene_plan" => {
            let book_id = need_book("缺少 bookId")?;
            let got = molan_core::continuity::get_plan(db, &book_id, "scene")?;
            Ok(Some(
                json!({"ok": true, "scenePlan": got["plan"], "revision": got["revision"], "updatedAt": got["updatedAt"]}),
            ))
        }
        // ---------- 当前生效配置（只读，供 E 展示"实际加载了什么"）----------
        // 只输出 id/name/usage/source 等元信息，**不返回完整 prompt 正文**（避免密钥/长文本外带）。
        "resolve_effective_skills" => {
            let book_id = s("bookId");
            let task = {
                let t = s("task");
                if t.is_empty() {
                    "body".to_string()
                } else {
                    t
                }
            };
            let explicit: Vec<Value> = a("skills").as_array().cloned().unwrap_or_default();
            let list = stream::effective_skills(db, &book_id, &task, &explicit);
            let out: Vec<Value> = list
                .iter()
                .map(|sk| {
                    let tpl = sk["promptTemplate"].as_str().unwrap_or("");
                    json!({
                        "id": sk["id"],
                        "name": sk["name"].as_str().unwrap_or(""),
                        "kind": sk["kind"].as_str().unwrap_or(""),
                        "origin": sk["origin"].as_str().unwrap_or(""),
                        "usageMode": sk["usageMode"].as_str().unwrap_or(""),
                        "builtinKey": sk["builtinKey"].as_str().unwrap_or(""),
                        "enabled": sk["enabled"],
                        // 有效性提示：空模板/极短模板不能执行（N04）
                        "templateChars": tpl.chars().count(),
                        "usable": !tpl.trim().is_empty(),
                        "validity": if tpl.trim().is_empty() { "空模板·不可执行" }
                                    else if tpl.chars().count() < 10 { "模板过短·疑似未完成" }
                                    else { "ok" },
                    })
                })
                .collect();
            Ok(Some(
                json!({"ok": true, "bookId": book_id, "task": task, "skills": out, "count": out.len()}),
            ))
        }
        "effective_book_config" => {
            let book_id = need_book("缺少 bookId")?;
            let (text, effective) = stream::book_config_block(db, &st.root, &book_id);
            Ok(Some(json!({
                "ok": true,
                "bookId": book_id,
                "effective": effective,
                // 是否真的注入了文本（前端可显示"当前配置是否生效"）
                "injectedChars": text.chars().count(),
            })))
        }
        // ---------- 章节记忆（父代理 continuity）----------
        "memory_status" => {
            let book_id = need_book("缺少 bookId")?;
            Ok(Some(molan_core::continuity::memory_status(db, &book_id)?))
        }
        "rebuild_memory" => {
            // 逐章重建：真实调用 B 的 post_approved_chapter 执行记忆抽取并落库。
            // 不清理任何作者资产（作者手写设定/Skill 卡永不作为"重建"对象）；
            // 每章独立、失败不中断其余，失败原因写 memory_job 可由 memory_status 查、可重跑。
            let book_id = need_book("缺少 bookId")?;
            let only_ch = a("ch").as_i64();
            let tree = files::scan_tree(db, &book_id);
            let mut targets: Vec<(i64, String)> = Vec::new();
            if let Some(arr) = tree.as_array() {
                for g in arr {
                    if g["dir"].as_str() != Some("正文") {
                        continue;
                    }
                    for f in g["files"].as_array().into_iter().flatten() {
                        let name = f["name"].as_str().unwrap_or("").to_string();
                        let Some(n) = chapter_num_from_name(&name) else {
                            continue;
                        };
                        if let Some(only) = only_ch {
                            if n != only {
                                continue;
                            }
                        }
                        targets.push((n, name));
                    }
                }
            }
            targets.sort_by_key(|(n, _)| *n);
            if targets.is_empty() {
                return Ok(Some(
                    json!({"ok": true, "rebuilt": [], "failed": [], "note": "没有匹配的正式正文章节"}),
                ));
            }
            let mut rebuilt: Vec<i64> = Vec::new();
            let mut failed: Vec<Value> = Vec::new();
            for (n, name) in targets {
                match stream::post_approved_chapter(st, &book_id, n).await {
                    Ok(()) => rebuilt.push(n),
                    Err(e) => {
                        mark_post_process_failed(db, &book_id, n, &name, &e.to_string());
                        failed.push(json!({"ch": n, "name": name, "error": e.to_string()}));
                    }
                }
            }
            Ok(Some(json!({
                "ok": failed.is_empty(),
                "rebuilt": rebuilt,
                "failed": failed,
                "note": "逐章执行；失败章已记入 memory_job，可再次调用本命令重跑（同 hash 幂等）",
            })))
        }
        // ---------- 长篇稳定性（P0）：事实账本 / 章节状态机 / 上下文清单 ----------
        "list_story_facts" => {
            let book_id = need_book("缺少 bookId")?;
            let limit = a("limit").as_i64().unwrap_or(500);
            Ok(Some(molan_core::facts::list_facts(
                db,
                &book_id,
                &s("subjectId"),
                &s("state"),
                limit,
            )?))
        }
        "get_thread_alerts" => {
            // 伏笔滞留告警（A1）：埋了 staleAfter 章（默认8）仍未推进的未回收伏笔
            let book_id = need_book("缺少 bookId")?;
            let stale_after = a("staleAfter").as_i64().unwrap_or(0);
            Ok(Some(molan_core::facts::thread_alerts(
                db,
                &book_id,
                stale_after,
            )?))
        }
        "rebuild_story_facts" => {
            // 从已 valid 的章后记忆回填事实账本（不调 LLM，幂等）；
            // 用于老书升级后一次性建仓，或事实表异常后的自愈合。
            let book_id = need_book("缺少 bookId")?;
            Ok(Some(molan_core::facts::rebuild_from_memories(
                db, &book_id,
            )?))
        }
        "get_fact_conflicts" => {
            let book_id = need_book("缺少 bookId")?;
            Ok(Some(molan_core::facts::conflicts(db, &book_id)?))
        }
        "set_fact_state" => {
            // 人工裁决事实：confirm（升格 confirmed）/ dispute / retire
            let id = s("id");
            let action = s("action");
            if id.is_empty() || action.is_empty() {
                return Err(anyhow!("缺少 id/action"));
            }
            Ok(Some(molan_core::facts::set_state(db, &id, &action)?))
        }
        "get_chapter_states" => {
            let book_id = need_book("缺少 bookId")?;
            Ok(Some(molan_core::chapter_state::states_view(db, &book_id)?))
        }
        "get_chapter_state_history" => {
            let book_id = s("bookId");
            let ch = a("ch").as_i64().unwrap_or(0);
            if book_id.is_empty() || ch <= 0 {
                return Err(anyhow!("缺少 bookId/ch"));
            }
            let limit = a("limit").as_i64().unwrap_or(100);
            Ok(Some(molan_core::chapter_state::history(
                db, &book_id, ch, limit,
            )?))
        }
        "get_chapter_commit" => {
            let book_id = s("bookId");
            let ch = a("ch").as_i64().unwrap_or(0);
            if book_id.is_empty() || ch <= 0 {
                return Err(anyhow!("缺少 bookId/ch"));
            }
            Ok(Some(molan_core::chapter_state::commit_info(
                db, &book_id, ch,
            )?))
        }
        "get_context_manifest" => {
            // id 给单条详情；否则按 bookId/sessionId 列最近清单
            let id = s("id");
            if !id.is_empty() {
                return Ok(Some(molan_core::ctx_manifest::get(db, &id)?));
            }
            let book_id = s("bookId");
            let session_id = s("sessionId");
            if book_id.is_empty() && session_id.is_empty() {
                return Err(anyhow!("缺少 bookId/sessionId/id"));
            }
            let limit = a("limit").as_i64().unwrap_or(50);
            Ok(Some(molan_core::ctx_manifest::list(
                db,
                &book_id,
                &session_id,
                limit,
            )?))
        }

        // ---------- 网页导出（F）：txt | zip | all ----------
        "web_export" => {
            let book_id = opt_book("bookId");
            let format = {
                let f = s("format");
                if f.is_empty() {
                    "all".to_string()
                } else {
                    f
                }
            };
            let out = exports::web_export(db, &st.root.join("data"), book_id.as_deref(), &format)?;
            Ok(Some(out))
        }
        // 旧导出命令：不再返回空桩，统一走真实导出（旧桌面 dialog 链已由 E 接替）
        "export_book_zip" | "export_txt" => {
            let book_id = opt_book("bookId");
            let format = if cmd == "export_txt" { "txt" } else { "zip" };
            let out = exports::web_export(db, &st.root.join("data"), book_id.as_deref(), format)?;
            Ok(Some(out))
        }
        "export_all_data" => {
            let out = exports::web_export(db, &st.root.join("data"), None, "all")?;
            Ok(Some(out))
        }
        "regen_arc_chapter" => {
            // 真 LLM 单章重推演 + CAS 写回：
            // 1) 读当前计划与 revision；2) 调模型产出严格 JSON；3) 校验结构；
            // 4) save_plan_cas 提交——若用户期间改过计划，revision 不符则报冲突，绝不覆盖用户编辑。
            let book_id = s("bookId");
            if book_id.is_empty() {
                return Err(anyhow!("缺少 bookId：重推演必须绑定到具体书籍"));
            }
            let n: i64 = a("chapterN").as_i64().unwrap_or(0);
            if n <= 0 {
                return Err(anyhow!("缺少有效 chapterN（要重推演的章号）"));
            }
            let instruction = s("instruction");
            // 1) 当前计划快照（无计划时 revision=0，等价"首次创建"）
            let current = molan_core::continuity::get_plan(db, &book_id, "arc")?;
            let expected_rev = current["revision"].as_i64().unwrap_or(0);
            let current_plan = current["plan"].clone();
            let chn = molan_llm::resolve_agent_channel(db, "outline")
                .ok_or_else(|| anyhow!("未配置可用的模型渠道"))?;
            let p = read_prompts(&st.root);
            let mut sys = prompt_prefix(&p);
            if let Some(t) = p["arc_plan"].as_str() {
                sys.push_str(t);
                sys.push_str("\n\n");
            }
            sys.push_str(
                "只输出一个 JSON 对象，形如 {\"chapters\":[{\"n\":正整数,\"title\":\"一句话标题\",\"summary\":\"走向级梗概 2~4 句\"}]}。\n\
                 要求：n 为正整数且在同一数组内唯一；只重推演被点名的那一章，其余章原样保留；不要输出 JSON 以外的任何文字。",
            );
            let user = format!(
                "【本书现有卷计划（revision {}）】\n{}\n\n【要重推演的章号】第{}章\n【作者的新指示】{}\n\n请只改这一章，其余章保持原样，输出完整 chapters 数组的 JSON。",
                expected_rev,
                if current_plan.is_null() { "（暂无计划，请新建该章）".to_string() } else { current_plan.to_string() },
                n,
                if instruction.is_empty() { "（无特别指示：给出更强的冲突与钩子，并埋一处新伏笔）" } else { instruction.as_str() },
            );
            let out = chat_once_params(
                &chn,
                vec![
                    json!({"role": "system", "content": sys}),
                    json!({"role": "user", "content": user}),
                ],
                4096,
            )
            .await?;
            let parsed = molan_llm::extract_json(&out).unwrap_or(Value::Null);
            let chapters = parsed
                .get("chapters")
                .and_then(|c| c.as_array())
                .cloned()
                .ok_or_else(|| anyhow!("重推演结果不是有效计划 JSON（缺少 chapters 数组）"))?;
            if chapters.is_empty() {
                return Err(anyhow!("重推演结果 chapters 为空，拒绝写回"));
            }
            // 结构校验（与 core 同口径）：章号为正整数且唯一
            {
                let mut seen = std::collections::BTreeSet::new();
                for c in &chapters {
                    let cn = c["n"]
                        .as_i64()
                        .or_else(|| c["chapter"].as_i64())
                        .ok_or_else(|| anyhow!("重推演结果存在缺少有效章号的章节"))?;
                    if cn <= 0 {
                        return Err(anyhow!("重推演结果章号必须为正整数：{}", cn));
                    }
                    if !seen.insert(cn) {
                        return Err(anyhow!("重推演结果章号重复：{}", cn));
                    }
                }
                if !seen.contains(&n) {
                    return Err(anyhow!("重推演结果未包含目标第{}章，拒绝写回", n));
                }
            }
            // 4) CAS 写回：用户若在此期间编辑过计划，这里会冲突而不是覆盖
            let payload = json!({ "chapters": chapters });
            let saved =
                molan_core::continuity::save_plan_cas(db, &book_id, "arc", expected_rev, &payload)
                    .map_err(|e| {
                        anyhow!(
                            "计划写回冲突：{}（计划可能已被其他编辑更新，请刷新后重试）",
                            e
                        )
                    })?;
            tracing::info!(
                "第{}章卷计划已重推演并 CAS 写回（revision {} → {}）",
                n,
                expected_rev,
                saved["revision"]
            );
            Ok(Some(json!({
                "ok": true,
                "arcPlan": saved["plan"],
                "revision": saved["revision"],
                "regenChapter": n,
            })))
        }
        "gen_chapter_outline" => {
            let book_id = s("bookId");
            if book_id.is_empty() {
                return Err(anyhow!("缺少 bookId：细纲必须绑定到具体书籍"));
            }
            let tree = files::scan_tree(db, &book_id);
            let next = max_chapter_num(&tree, "正文") + 1;
            // 目标章号：优先结构化参数 targetCh（契约），兼容旧前端 chapterNum；都没有才回落下一章
            let explicit = a("targetCh").as_i64().or_else(|| a("chapterNum").as_i64());
            let num = match explicit {
                Some(n) if n > 0 => n,
                _ => next,
            };
            let chn = molan_llm::resolve_agent_channel(db, "outline")
                .ok_or_else(|| anyhow!("未配置可用的模型渠道"))?;
            // 提示词组装（供 outline_json 使用）
            let p = read_prompts(&st.root);
            let mut sys = prompt_prefix(&p);
            if let Some(t) = p["anti_ai_tone"].as_str() {
                sys.push_str(t);
                sys.push_str("\n\n");
            }
            if let Some(t) = p["outline_json"].as_str() {
                sys.push_str(t);
            }
            // 题要检索必须限定本书：JOIN sessions 过滤 book_id，杜绝 A 书细纲混入 B 书对话
            let brief_rows = db
                .q_json(
                    "SELECT m.content FROM messages m JOIN sessions s ON s.id=m.session_id \
                     WHERE s.book_id=?1 AND m.content LIKE ?2 ORDER BY m.created_at DESC LIMIT 5",
                    &[
                        &book_id as &dyn rusqlite::ToSql,
                        &format!("%第{}%", num) as &dyn rusqlite::ToSql,
                    ],
                )
                .unwrap_or_default();
            let brief = brief_rows
                .iter()
                .filter_map(|r| r["content"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
                .chars()
                .take(2000)
                .collect::<String>();
            // 注入四层记忆（建书档案/人物表/前情摘要/最近正文），细纲必须贴合前文不得另起炉灶
            // C 已落地：结构化目标章号，不做自然语言解析
            let memory = stream::auto_book_context_for_chapter(db, &book_id, num, false);
            let user = format!(
                "为本书生成【第 {} 章】细纲。\n方向：{}\n已有题要：{}\n\n【资料库上下文（细纲必须贴合这些设定与前文，不得另起炉灶）】\n{}\n\n请按大纲 JSON 格式输出。",
                num,
                s("direction"),
                brief,
                memory
            );
            let out = chat_once_params(
                &chn,
                vec![
                    json!({"role": "system", "content": sys}),
                    json!({"role": "user", "content": user}),
                ],
                8192,
            )
            .await?;
            let parsed = molan_llm::extract_json(&out);
            // AI 细纲只能新建：已有细纲或作者锁定时报错，绝不覆盖作者手稿。
            let outline_name = format!("细纲_第{}章.md", num);
            files::write_ai_file_checked(db, &book_id, "细纲", &outline_name, out.trim(), || {
                Ok(())
            })
            .map_err(|e| anyhow!("{} 未写入（不覆盖已有细纲）：{}", outline_name, e))?;
            Ok(Some(json!({"ok": true, "text": out, "parsed": parsed})))
        }
        "assemble_scene_doc" => {
            let book_id = s("bookId");
            let message_id = s("messageId");
            // 归属校验 + 驼峰键：q_json 已 snake->camel（db.rs:269），旧读 m["session_id"] 恒为 Null。
            let rows = db.q_json(
                "SELECT m.session_id FROM messages m JOIN sessions s ON s.id=m.session_id WHERE m.id=?1 AND s.book_id=?2",
                &[&message_id as &dyn rusqlite::ToSql, &book_id as &dyn rusqlite::ToSql],
            )?;
            let session_id = rows
                .first()
                .and_then(|m| m["sessionId"].as_str())
                .unwrap_or("")
                .to_string();
            let scenes = if session_id.is_empty() {
                Vec::new()
            } else {
                db.q_json(
                    "SELECT * FROM messages WHERE session_id=? AND role='scene' ORDER BY created_at ASC",
                    &[&session_id as &dyn rusqlite::ToSql],
                )
                .unwrap_or_default()
            };
            let body = scenes
                .iter()
                .map(|s| format!("\n\n--- 场景 ---\n{}", s["content"].as_str().unwrap_or("")))
                .collect::<String>();
            if body.trim().is_empty() {
                return Err(anyhow!("该会话没有可拼稿的场景内容，未写入"));
            }
            let fname = format!(
                "场景_{}.md",
                uuid::Uuid::new_v4()
                    .to_string()
                    .chars()
                    .take(8)
                    .collect::<String>()
            );
            if !book_id.is_empty() {
                // 场景拼稿是 AI 会话产出，只能新建进正文待审，绝不直写正式正文。
                files::write_ai_file(
                    db,
                    &book_id,
                    molan_core::db::REVIEW_GROUP,
                    &fname,
                    body.trim(),
                )?;
                let _ = register_review_queue(db, &book_id, &fname, body.trim());
                let _ = db.exec(
                    "UPDATE messages SET content=?, updated_at=? WHERE id=?",
                    &[
                        &fname as &dyn rusqlite::ToSql,
                        &stats::now_ms(),
                        &message_id,
                    ],
                );
            }
            Ok(Some(
                json!({"ok": true, "content": body.trim(), "fileName": fname}),
            ))
        }
        "save_doc" | "save_section" => Ok(Some(saved_output::save(db, args)?)),
        "save_book_setup_selection" => Ok(Some(molan_core::setup_destination::save_selection(
            db, args,
        )?)),
        "apply_book_setup" => Err(anyhow!(
            "旧版建书保存已停用，请刷新页面并明确选择目标作品后保存"
        )),

        "update_book_meta" => {
            let mut sets = Vec::new();
            let mut vals: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            for (k, col) in [
                ("title", "title"),
                ("genre", "genre"),
                ("pov", "pov"),
                ("status", "status"),
                ("coverChar", "cover_char"),
            ] {
                if let Some(v) = args.get(k) {
                    sets.push(format!("{}=?", col));
                    vals.push(Box::new(v.as_str().unwrap_or("").to_string()));
                }
            }
            if !sets.is_empty() {
                sets.push("updated_at=?".into());
                vals.push(Box::new(stats::now_ms()));
                vals.push(Box::new(s("bookId")));
                let sql = format!("UPDATE books SET {} WHERE id=?", sets.join(","));
                let refs: Vec<&dyn rusqlite::ToSql> = vals.iter().map(|b| b.as_ref()).collect();
                // 书名/题材/pov/状态是本书输入（genre/pov 直接影响生成），局部持锁提交
                let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
                db.exec(&sql, &refs)?;
                drop(_g);
            }
            let rows = db.q_json(
                "SELECT * FROM books WHERE id=?",
                &[&s("bookId") as &dyn rusqlite::ToSql],
            )?;
            Ok(Some(rows.first().cloned().unwrap_or(json!({}))))
        }
        "set_book_cover" => {
            let cover = s("coverChar").chars().take(1).collect::<String>();
            let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
            db.exec(
                "UPDATE books SET cover_char=? WHERE id=?",
                &[&cover as &dyn rusqlite::ToSql, &s("bookId")],
            )?;
            drop(_g);
            Ok(Some(json!({"ok": true})))
        }
        "set_book_support_skills" => {
            let ids: Vec<String> = a("skillIds")
                .as_array()
                .map(|v| {
                    v.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            // 主/辅助技能是核心输入（effective_skills 依赖）。core 这两个函数内部不取 fs_lock，
            // 父授权由 handler 局部包裹（纯同步、无 await，不会与 core 自身加锁的函数互相递归）。
            let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
            let out =
                molan_core::books::set_book_support_skills(db, &s("bookId"), &s("taskKind"), &ids);
            drop(_g);
            Ok(Some(out))
        }
        "save_decompose_as_skill" => {
            let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
            let name = {
                let n = s("skillName");
                if n.is_empty() {
                    s("name")
                } else {
                    n
                }
            };
            let name = if name.is_empty() {
                "拆解手法".to_string()
            } else {
                name
            };
            let desc = {
                let d = s("skillDescription");
                if d.is_empty() {
                    s("description")
                } else {
                    d
                }
            };
            let desc = if desc.is_empty() {
                "源自拆解可借鉴手法".to_string()
            } else {
                desc
            };
            let tpl = format!(
                "【任务】基于以下拆解报告，把手法用运用指导写成可执行的写作技能。\n\n【拆解报告】\n{}",
                s("report").chars().take(8000).collect::<String>()
            );
            let id = uuid::Uuid::new_v4().to_string();
            db.exec(
                "INSERT INTO skills(id,name,description,prompt_template,kind,source,enabled,builtin_key,usage_mode,origin) VALUES(?1,?2,?3,?4,'craft','',1,NULL,'standalone','decompose')",
                &[&id as &dyn rusqlite::ToSql, &name, &desc, &tpl],
            )?;
            Ok(Some(skill_mapped(db, &id)))
        }
        "save_capability_doc" => {
            let book_id = s("bookId");
            let content = s("content");
            if book_id.is_empty() || content.trim().is_empty() {
                return Err(anyhow!("save_capability_doc 需要非空 bookId 与 content"));
            }
            if !files::valid_book_id(db, &book_id) {
                return Err(anyhow!("非法或不存在的书籍：{}", book_id));
            }
            // 能力文档只允许落 设定/细纲/参考：正文类必须走审批队列命令，杜绝旁路
            let d = s("dir");
            let dir = files::normalize_group(if d.is_empty() { "设定" } else { d.as_str() });
            if !matches!(dir.as_str(), "设定" | "细纲" | "参考") {
                return Err(anyhow!("能力文档只允许写入 设定/细纲/参考，收到：{}", dir));
            }
            let n = s("name");
            let name = if n.is_empty() {
                "能力清单.md".to_string()
            } else {
                n
            };
            if files::safe_name(&name) != name {
                return Err(anyhow!("非法文件名：{}", name));
            }
            files::write_ai_file(db, &book_id, &dir, &name, &content)?;
            Ok(Some(json!({"ok": true, "group": dir, "name": name})))
        }
        "read_sensitive_words" => {
            let fp = st.root.join("data").join("sensitive_words.txt");
            let text = std::fs::read_to_string(&fp).unwrap_or_default();
            Ok(Some(json!(text)))
        }
        "write_sensitive_words" => {
            let fp = st.root.join("data").join("sensitive_words.txt");
            let _ = std::fs::create_dir_all(fp.parent().unwrap());
            std::fs::write(&fp, s("text")).ok();
            Ok(Some(json!({"ok": true})))
        }
        "check_sensitive" => {
            let words = s("text");
            let fp = st.root.join("data").join("sensitive_words.txt");
            let list: Vec<String> = std::fs::read_to_string(&fp)
                .unwrap_or_default()
                .split(|c: char| {
                    c == '\n' || c == ',' || c == '，' || c == ';' || c == '；' || c.is_whitespace()
                })
                .filter(|w| !w.is_empty())
                .map(|w| w.to_string())
                .collect();
            let matched: Vec<String> = list
                .iter()
                .filter(|w| words.contains(w.as_str()))
                .cloned()
                .collect();
            Ok(Some(json!({"matched": matched, "ok": matched.is_empty()})))
        }
        // ---------- 通用桩 ----------
        "abort_chat" => {
            // 取消长任务（拆解等）：requestId 放进取消集合，任务在下个分段边界消费
            stream::request_abort(&s("requestId"));
            Ok(Some(json!({"ok": true})))
        }
        "set_skill_script_authorization" => Ok(Some(json!({"ok": true}))),
        "open_url"
        | "reveal_path"
        | "open_data_dir"
        | "log_frontend_error"
        | "report_online_skill_download"
        | "evaluate_direct_model" => Ok(Some(Value::Null)),
        "change_data_dir" => {
            let _ = db.exec(
                "INSERT INTO settings(key,value) VALUES('replica_data_dir',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                &[&s("newDir") as &dyn rusqlite::ToSql],
            );
            Ok(Some(
                json!({"ok": true, "current": s("newDir"), "note": "重启后生效"}),
            ))
        }
        "import_book_zip" => {
            let files: Vec<Value> = a("files").as_array().cloned().unwrap_or_default();
            let book = files::import_book(db, "导入书籍", "其他", &files)?;
            Ok(Some(
                json!({"bookId": book["bookId"], "imported": files.len()}),
            ))
        }
        "save_decompose_report_stub" => Ok(Some(json!(""))),

        // ---------- 在线服务：统一离线错误 ----------
        c if [
            "account_login",
            "account_register",
            "account_send_code",
            "account_reset_password",
            "account_set_nickname",
            "payment_create_order",
            "payment_cancel_order_web",
            "plaza_comments",
            "plaza_comment",
            "plaza_comment_reply",
            "plaza_comment_helpful",
            "plaza_comment_report",
            "plaza_submit",
            "plaza_set_nickname",
            "plaza_download_bundle",
            "sync_status",
            "sync_upload",
            "sync_download",
            "sync_cloud_books",
            "sync_adopt_cloud_book",
            "cloud_download_book",
            "check_thread_update",
        ]
        .contains(&c) =>
        {
            Err(anyhow!("离线复刻版不支持该在线功能，请使用本地功能"))
        }

        // ---------- 会话 / 消息 ----------
        "list_sessions" => Ok(Some(molan_core::books::list_sessions(db, &s("bookId")))),
        "create_session" => Ok(Some(molan_core::books::create_session(
            db,
            &s("bookId"),
            &s("title"),
        )?)),
        "rename_session" => {
            // C10 归属校验：会话必须属于 args.bookId，否则拒绝改他人会话
            let session_id = s("sessionId");
            sessions_owned(db, &session_id, &s("bookId"))?;
            Ok(Some(molan_core::books::rename_session(
                db,
                &session_id,
                &s("title"),
            )))
        }
        "delete_session" => {
            let session_id = s("sessionId");
            sessions_owned(db, &session_id, &s("bookId"))?;
            if s("confirm") != session_id {
                return Err(anyhow!("删除会话须明确确认"));
            }
            Ok(Some(molan_core::books::delete_session(db, &session_id)?))
        }
        "list_messages" => {
            // C10 归属校验：会话必须属于 args.bookId，否则拒绝读他人会话
            let session_id = s("sessionId");
            sessions_owned(db, &session_id, &s("bookId"))?;
            Ok(Some(molan_core::books::list_messages(db, &session_id)))
        }
        "search_messages" => {
            let q = format!("%{}%", s("query"));
            let rows = if s("sessionId").is_empty() {
                db.q_json(
                    "SELECT * FROM messages WHERE content LIKE ?1 ORDER BY created_at DESC LIMIT 200",
                    &[&q as &dyn rusqlite::ToSql],
                )?
            } else {
                db.q_json(
                    "SELECT * FROM messages WHERE session_id=?1 AND content LIKE ?2 ORDER BY created_at ASC",
                    &[&s("sessionId") as &dyn rusqlite::ToSql, &q],
                )?
            };
            Ok(Some(msg_rows_json(&rows)))
        }
        "search_book" => {
            // 按书搜会话消息（对齐 core.searchBook：JOIN sessions 过滤 book_id）
            let like = format!("%{}%", s("query"));
            let rows = db.q_json(
                "SELECT m.* FROM messages m JOIN sessions s ON s.id=m.session_id WHERE s.book_id=?1 AND m.content LIKE ?2 ORDER BY m.created_at DESC LIMIT 200",
                &[&s("bookId") as &dyn rusqlite::ToSql, &like],
            )?;
            Ok(Some(msg_rows_json(&rows)))
        }
        "get_book_style" => {
            let book_id = s("bookId");
            let key = {
                let k = molan_llm::get_setting(db, &format!("book_style__{}", book_id));
                if k.trim().is_empty() {
                    "auto".to_string()
                } else {
                    k
                }
            };
            let genre = db
                .q_json(
                    "SELECT genre FROM books WHERE id=?1",
                    &[&book_id as &dyn rusqlite::ToSql],
                )
                .ok()
                .and_then(|v| {
                    v.first()
                        .and_then(|r| r["genre"].as_str().map(|x| x.to_string()))
                })
                .unwrap_or_default();
            Ok(Some(book_style_resolved(db, &st.root, &key, &genre)))
        }
        "set_book_style" => {
            // 前端传的是 key：auto / off / distill / style:<技能id> / 题材预设key
            let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
            let out = molan_core::books::set_book_style(db, &s("bookId"), key);
            drop(_g);
            Ok(Some(out))
        }
        "set_book_humanize" => {
            let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
            // 前端传的是方法字符串：official:standard / official:deep / skill:<id> / none
            let raw = a("value");
            let method = match &raw {
                Value::Bool(true) => "official:standard".to_string(),
                Value::Bool(false) => "none".to_string(),
                Value::String(v) => v.clone(),
                _ => "official:standard".to_string(),
            };
            Ok(Some(molan_core::books::set_book_humanize(
                db,
                &s("bookId"),
                &method,
            )))
        }
        "set_book_primary_skill" => {
            let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
            let out = molan_core::books::set_book_primary_skill(
                db,
                &s("bookId"),
                &s("taskKind"),
                &s("skillId"),
            );
            drop(_g);
            Ok(Some(out))
        }
        "save_lead" => {
            let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
            let out = molan_core::books::save_lead(db, &s("bookId"), &s("text"));
            drop(_g);
            Ok(Some(out))
        }
        "list_genre_styles" => Ok(Some(list_genre_styles(&st.root))),

        // ---------- 统计 ----------
        "get_writing_stats" => Ok(Some(stats::get_writing_stats(db))),
        // LLM 用量只读查询：总量 / 按 tag 汇总 / 最近 7 天日报
        "llm_usage_stats" => {
            let total_calls: i64 = db
                .q_json("SELECT COUNT(*) AS n FROM llm_call_log", &[])
                .ok()
                .and_then(|v| v.first().and_then(|r| r["n"].as_i64()))
                .unwrap_or(0);
            let total_tokens: i64 = db
                .q_json(
                    "SELECT COALESCE(SUM(total_tokens),0) AS n FROM llm_call_log",
                    &[],
                )
                .ok()
                .and_then(|v| v.first().and_then(|r| r["n"].as_i64()))
                .unwrap_or(0);
            let by_tag = db
                .q_json(
                    "SELECT tag, COUNT(*) AS calls, COALESCE(SUM(total_tokens),0) AS total_tokens FROM llm_call_log GROUP BY tag ORDER BY total_tokens DESC",
                    &[],
                )
                .unwrap_or_default();
            let daily = db
                .q_json(
                    "SELECT date(datetime(ts/1000,'unixepoch','localtime')) AS date, COUNT(*) AS calls, COALESCE(SUM(total_tokens),0) AS total_tokens FROM llm_call_log GROUP BY date ORDER BY date DESC LIMIT 7",
                    &[],
                )
                .unwrap_or_default();
            Ok(Some(json!({
                "totalCalls": total_calls,
                "totalTokens": total_tokens,
                "byTag": by_tag,
                "daily": daily,
            })))
        }

        // ---------- 书源 ----------
        "list_book_sources" => Ok(Some(molan_sources::list_sources(&st.root.join("data")))),
        "sync_book_sources" => {
            let srcs = molan_sources::list_sources(&st.root.join("data"));
            Ok(Some(
                json!({"ok": true, "sources": srcs, "fromRemote": false}),
            ))
        }
        "search_books" => {
            let r =
                molan_sources::search_books(&st.root.join("data"), &s("sourceId"), &s("keyword"))
                    .await?;
            Ok(Some(r))
        }
        "fetch_book_catalog" => {
            let r = molan_sources::fetch_book_catalog(
                &st.root.join("data"),
                &s("sourceId"),
                &s("bookUrl"),
            )
            .await?;
            Ok(Some(r))
        }
        "fetch_chapter_texts" => {
            // 逐章抓取 + step 进度事件（对齐官方 Gj/Aj 的 {index,title} 步骤流）+ 返回拼接文本
            let source_id = s("sourceId");
            let chapters: Vec<Value> = a("chapters").as_array().cloned().unwrap_or_default();
            let data_dir = st.root.join("data");
            let channel = channel_id(&a("onEvent"));
            let mut texts = Vec::new();
            let total = chapters.len();
            for (idx, c) in chapters.iter().enumerate() {
                let url = c["url"].as_str().unwrap_or("");
                let name = c["name"]
                    .as_str()
                    .or_else(|| c["title"].as_str())
                    .unwrap_or("")
                    .to_string();
                let _ = tx
                    .send(format!("{}\n", json!({"ch": channel, "e": {"type": "step", "index": idx + 1, "title": format!("正在抓取第 {}/{} 章 · {}", idx + 1, total, name)}})))
                    .await;
                let text = molan_sources::fetch_chapter_text(&data_dir, &source_id, url)
                    .await
                    .unwrap_or_default();
                texts.push(json!({"name": name, "url": url, "text": text}));
            }
            let _ = tx
                .send(format!(
                    "{}\n",
                    json!({"ch": channel, "e": {"type": "done", "texts": texts}})
                ))
                .await;
            let joined = texts
                .iter()
                .map(|t| t["text"].as_str().unwrap_or("").to_string())
                .collect::<Vec<_>>()
                .join("\n\n");
            Ok(Some(json!(joined)))
        }

        // ---------- 导入 / 回收 / 导出 ----------
        "pending_import" => {
            let bid = s("bookId");
            if bid.is_empty() {
                return Ok(Some(json!(false)));
            }
            let mark = molan_llm::get_setting(db, &format!("import_analyzed__{}", bid));
            if mark == "1" {
                return Ok(Some(json!(false)));
            }
            let tree = files::scan_tree(db, &bid);
            let n: usize = tree
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter(|g| g["dir"] == json!("正文") || g["dir"] == json!("参考"))
                        .map(|g| g["files"].as_array().map(|f| f.len()).unwrap_or(0))
                        .sum()
                })
                .unwrap_or(0);
            Ok(Some(json!(n > 0)))
        }
        "import_analyzed_mark" => {
            db.exec(
                "INSERT INTO settings(key,value) VALUES(?,'1') ON CONFLICT(key) DO UPDATE SET value='1'",
                &[&format!("import_analyzed__{}", s("bookId")) as &dyn rusqlite::ToSql],
            )?;
            Ok(Some(json!({"ok": true})))
        }
        "pending_decompose" => {
            // 官方拆解弹窗打开时查询：返回 {name,text} 让「从断点继续」可用（现在后端逐段落库，天然支持续拆）
            let bid = s("bookId");
            let mark = db
                .q_json(
                    "SELECT value FROM settings WHERE key=?1",
                    &[&format!("decompose_pending__{}", bid) as &dyn rusqlite::ToSql],
                )
                .unwrap_or_default();
            if let Some(row) = mark.first() {
                if let Ok(v) = serde_json::from_str::<Value>(row["value"].as_str().unwrap_or("")) {
                    let text = v["text"].as_str().unwrap_or("");
                    if !text.is_empty() {
                        return Ok(Some(
                            json!({"name": v["name"].as_str().unwrap_or(""), "text": text}),
                        ));
                    }
                }
            }
            Ok(Some(Value::Null))
        }
        "read_import_folder" => Ok(Some(json!([]))),
        "export_diagnostics" => {
            let books = json!(molan_core::books::list_books(db))
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
            let skills = db
                .q_json("SELECT COUNT(*) c FROM skills", &[])
                .unwrap_or_default();
            Ok(Some(json!({
                "ok": true, "file": "diagnostics.txt",
                "content": json!({"app": "WriterX replica (rust)", "os": std::env::consts::OS, "books": books, "skills": skills[0]["c"]}).to_string(),
            })))
        }
        "reset_all_data" => {
            // 破坏性操作：必须显式确认，避免误触发清库
            if s("confirm") != "RESET_ALL" {
                return Err(anyhow!("危险操作：需传 confirm=\"RESET_ALL\""));
            }
            // 运行中禁止重置：任务仍在写盘时清库会留下磁盘/DB 孤儿
            let running = running_auto_books(db);
            if !running.is_empty() {
                return Err(anyhow!(
                    "仍有自动写作任务在运行（{}），请先停止全部任务再重置",
                    running.join(", ")
                ));
            }
            // 真实全量清理：业务表 + 磁盘 books/versions/trash（A 的 purge_all_rows，单事务 + 锁）
            molan_core::books::purge_all_rows(db)?;
            // 清理后统计与磁盘对齐（原实现遗留 word_stats 导致"重置后仍有字数"）
            stats::rebuild_word_stats(db);
            Ok(Some(json!({"ok": true})))
        }

        // ---------- DeepWrite 服务端移植层 ----------
        "dw_list_agents" => Ok(Some(molan_core::deepwrite::list_agents(db, &s("bookId"))?)),
        "dw_save_agent" => Ok(Some(molan_core::deepwrite::save_agent(
            db,
            &s("bookId"),
            &s("role"),
            &s("name"),
            &s("systemPrompt"),
            &s("model"),
            flag("enabled", 1) != 0,
        )?)),
        "dw_list_skill_bindings" => Ok(Some(molan_core::deepwrite::list_skill_bindings(
            db,
            &s("bookId"),
        )?)),
        "dw_bind_skill" => Ok(Some(molan_core::deepwrite::bind_skill(
            db,
            &s("bookId"),
            &s("skillId"),
            flag("enabled", 1) != 0,
        )?)),
        "dw_unbind_skill" => Ok(Some(molan_core::deepwrite::unbind_skill(
            db,
            &s("bookId"),
            &s("skillId"),
        )?)),
        "dw_context_bundle" => {
            let max_chars = a("maxChars").as_u64().unwrap_or(120_000) as usize;
            Ok(Some(molan_core::deepwrite::context_bundle(
                db,
                &s("bookId"),
                max_chars,
            )?))
        }
        "dw_create_proposal" => Ok(Some(molan_core::deepwrite::create_proposal(
            db,
            &s("bookId"),
            &s("role"),
            &s("group"),
            &s("name"),
            &s("summary"),
            &s("proposedContent"),
        )?)),
        "dw_get_proposal" => Ok(Some(molan_core::deepwrite::get_proposal_for_book(
            db,
            &s("bookId"),
            &s("id"),
        )?)),
        "dw_list_proposals" => {
            let status = s("status");
            Ok(Some(molan_core::deepwrite::list_proposals(
                db,
                &s("bookId"),
                if status.is_empty() {
                    None
                } else {
                    Some(status.as_str())
                },
            )?))
        }
        "dw_accept_proposal" => Ok(Some(molan_core::deepwrite::accept_proposal(
            db,
            &s("bookId"),
            &s("id"),
        )?)),
        "dw_reject_proposal" => Ok(Some(molan_core::deepwrite::reject_proposal(
            db,
            &s("bookId"),
            &s("id"),
            &s("reason"),
        )?)),

        // ---------- 在线服务降级 ----------
        "account_status" => Ok(Some(
            json!({"loggedIn": true, "profile": {"nickname": "本地版", "email": "local@writerx.local", "avatarUrl": ""}, "local": true}),
        )),
        "account_logout" => Ok(Some(json!({"ok": true}))),
        "account_sync" => Ok(Some(json!({"ok": true, "synced": 0, "changes": 0}))),
        "payment_plans" => Ok(Some(
            json!({"salesEnabled": false, "channels": [], "items": []}),
        )),
        "payment_cancel_order" => Ok(Some(json!({"ok": true}))),
        "payment_pending_order" => Ok(Some(Value::Null)),
        "payment_qr_data_url" => Ok(Some(json!(s("url")))),
        "plaza_home" => Ok(Some(json!({"featured": [], "hot": [], "fresh": []}))),
        "plaza_me" | "plaza_mine" => Ok(Some(json!([]))),
        "plaza_list" | "plaza_list2" => Ok(Some(json!({"items": [], "isEnd": true}))),
        "plaza_detail" => Ok(Some(json!({"item": null}))),
        "license_status" => Ok(Some(
            json!({"state": "active", "trialDaysLeft": 0, "licensee": "", "expiresAt": 0, "sku": "", "betaUntil": 0, "purchaseUrl": "", "codeRevokedNote": ""}),
        )),
        "check_feature" => {
            let f = s("feature");
            if [
                "sync",
                "cloud",
                "account",
                "payment",
                "plaza",
                "updater",
                "llm_balance",
                "llm_usage",
                "llm_orders",
                "workshop_online",
            ]
            .contains(&f.as_str())
            {
                return Err(anyhow!("该功能在离线复刻版中不可用"));
            }
            Ok(Some(Value::Null))
        }
        "data_dir" => {
            // 网页/Docker 部署：优先显示宿主真实工作区路径（DATA_HOST_PATH 由容器启动参数注入）
            let host = std::env::var("DATA_HOST_PATH").unwrap_or_default();
            if host.is_empty() {
                return Ok(Some(json!(st.root.join("data").to_string_lossy())));
            }
            Ok(Some(json!(host)))
        }
        "data_dir_info" => {
            let host = std::env::var("DATA_HOST_PATH").unwrap_or_default();
            let cur = if host.is_empty() {
                st.root.join("data").to_string_lossy().to_string()
            } else {
                host
            };
            Ok(Some(json!({"current": cur, "pending": null})))
        }

        // ---------- 插件桩 ----------
        "plugin:dialog|open" | "plugin:dialog|save" => {
            // 桌面版目录/文件选择弹窗：网页版数据在服务器，选本地目录无意义——明确报错而非静默取消
            let is_dir = args.to_string().contains("directory");
            Err(anyhow!(if is_dir {
                "网页版不支持目录选择弹窗。工作区由服务器挂载配置（当前 /vol2/1000/molan-books），如需变更请在宿主机调整 docker 挂载或联系管理员迁移。".to_string()
            } else {
                "网页版不支持文件选择弹窗，请直接上传文件。".to_string()
            }))
        }
        c if c.starts_with("plugin:") => Ok(Some(plugin_stub(c, args))),

        // ---------- 流式（chat/蒸馏等，写在 handlers.rs 流式区） ----------
        _ => {
            return stream::dispatch_stream(st, cmd, args, tx).await;
        }
    }
}

/// 破坏性操作前的运行任务护栏：返回正在运行的自动写作任务所属书籍。
/// 优先用 B 暴露的内存态（若存在），否则回落到持久化的 auto_task 表。
/// 任一在跑都不允许 purge/reset，避免"清库时任务仍在写盘"。
fn running_auto_books(db: &molan_core::db::Db) -> Vec<String> {
    db.q_json(
        "SELECT DISTINCT book_id FROM auto_task WHERE status='running'",
        &[],
    )
    .map(|rows| {
        rows.iter()
            .filter_map(|r| r["bookId"].as_str().map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// 章后处理失败必须**可见可重试**，不能只写日志：
/// 把失败写进 memory_job（status='failed' + error）并追加 continuity_event，
/// 前端/运维可通过 memory_status IPC 查到该章"定稿已保存、记忆未同步"。
fn mark_post_process_failed(
    db: &molan_core::db::Db,
    book_id: &str,
    ch: i64,
    name: &str,
    err: &str,
) {
    let now = stats::now_ms();
    let _ = db.exec(
        "INSERT INTO memory_job(book_id,ch,name,source_hash,status,error,updated_at) VALUES(?1,?2,?3,'','failed',?4,?5)
         ON CONFLICT(book_id,ch) DO UPDATE SET status='failed', error=excluded.error, updated_at=excluded.updated_at",
        &[&book_id as &dyn rusqlite::ToSql, &ch, &name, &err, &now],
    );
    let _ = db.exec(
        "INSERT INTO continuity_event(book_id,kind,detail_json,created_at) VALUES(?1,'post_process_failed',?2,?3)",
        &[
            &book_id as &dyn rusqlite::ToSql,
            &json!({"chapter": ch, "name": name, "error": err, "note": "定稿已保存，记忆未同步，可重跑"}).to_string(),
            &now,
        ],
    );
    tracing::warn!(
        "章后处理失败（第{}章已提交，成果保留，可重试）：{}",
        ch,
        err
    );
}

/// 审批成功后的章后处理（人物表/伏笔/摘要/记忆）。
/// 关键语义：提交已成功 → 后处理失败**只标记、可重试**，绝不回滚或抹掉已提交的正式稿。
/// 调用 B 的 post_approved_chapter：只处理传入的"刚批准章"，不再用全书最大章替代。
fn spawn_post_approved(st: Arc<AppState>, book_id: &str, ch: i64) {
    let book = book_id.to_string();
    let name = format!("第{}章.md", ch);
    tokio::spawn(async move {
        if let Err(e) = stream::post_approved_chapter(&st, &book, ch).await {
            mark_post_process_failed(&st.db, &book, ch, &name, &e.to_string());
        }
    });
}

/// 解析技能包载荷：内联 JSON 或 data/exports 内的文件（inspect/install 共用）。
fn load_skill_bundle(st: &Arc<AppState>, raw: &str) -> Result<Value> {
    if raw.trim().starts_with('{') {
        return Ok(serde_json::from_str(raw).unwrap_or(Value::Null));
    }
    Ok(std::fs::read_to_string(check_skill_bundle_path(st, raw)?)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null))
}

/// 消息行批量序列化（q_json 原始行 -> msg_row 形状数组）
fn msg_rows_json(rows: &[Value]) -> Value {
    json!(rows
        .iter()
        .map(molan_core::books::msg_row)
        .collect::<Vec<_>>())
}

/// 按 id 取技能原始行（合并多处重复的 SELECT * FROM skills WHERE id=? 样板）
fn skill_by_id(db: &molan_core::db::Db, id: &str) -> Option<Value> {
    db.q_json(
        "SELECT * FROM skills WHERE id=?",
        &[&id as &dyn rusqlite::ToSql],
    )
    .ok()
    .and_then(|r| r.into_iter().next())
}

/// 按 id 取技能并映射为 skill_row 形状（create/update/read_skill_ref 的返回体）
fn skill_mapped(db: &molan_core::db::Db, id: &str) -> Value {
    skill_by_id(db, id)
        .map(|r| skill_row(&r))
        .unwrap_or(Value::Null)
}

/// 会话归属校验（C10）：按 sessionId 读写会话前须证明会话属于 args.bookId（同 chat.rs/auto_write.rs；q_json 键为 camelCase）
fn sessions_owned(db: &molan_core::db::Db, session_id: &str, book_id: &str) -> Result<()> {
    let rows = db.q_json(
        "SELECT book_id FROM sessions WHERE id=?1",
        &[&session_id as &dyn rusqlite::ToSql],
    )?;
    if rows.first().and_then(|r| r["bookId"].as_str()) != Some(book_id) {
        return Err(anyhow!("会话不存在或不属于当前书"));
    }
    Ok(())
}

pub fn skill_row(r: &Value) -> Value {
    // q_json 已把列名转 camel：builtin_key->builtinKey 等
    json!({
        "id": r["id"], "name": r["name"],
        "description": r["description"].as_str().unwrap_or(""),
        "kind": r["kind"].as_str().unwrap_or("user"),
        "source": r["source"].as_str().unwrap_or(""),
        "enabled": r["enabled"].as_i64().unwrap_or(1) != 0,
        "builtinKey": r["builtinKey"].as_str().unwrap_or(""),
        "usageMode": r["usageMode"].as_str().unwrap_or("support"),
        "origin": r["origin"].as_str().unwrap_or("user"),
        "promptTemplate": r["promptTemplate"].as_str().unwrap_or(""),
        "targets": serde_json::from_str::<Value>(r["targetsJson"].as_str().unwrap_or("[]")).unwrap_or(json!([])),
        "capabilities": r["capabilitiesJson"].as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()),
    })
}

pub fn channel_id(ch: &Value) -> String {
    if ch.is_null() {
        return String::new();
    }
    if let Some(s) = ch.as_str() {
        return s.trim_start_matches("__CHANNEL__:").to_string();
    }
    ch["id"]
        .as_i64()
        .map(|v| v.to_string())
        .or_else(|| ch["id"].as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

fn safe(n: &str) -> String {
    n.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect()
}

/// 技能包导入路径限制：canonicalize 后必须落在 data/exports 之内，阻断任意文件读
fn check_skill_bundle_path(st: &Arc<AppState>, raw: &str) -> Result<std::path::PathBuf> {
    let base = st.root.join("data").join("exports");
    let base_canon = base
        .canonicalize()
        .map_err(|_| anyhow!("仅允许导入 data/exports 目录内的技能包"))?;
    let p = std::path::Path::new(raw)
        .canonicalize()
        .map_err(|_| anyhow!("仅允许导入 data/exports 目录内的技能包"))?;
    if !p.starts_with(&base_canon) {
        return Err(anyhow!("仅允许导入 data/exports 目录内的技能包"));
    }
    Ok(p)
}

// 题材卡（对齐 registry.listGenreStyles）
/// 题材风格预设表：(key, 名称, 说明) —— 与前端预设保持一致
const GENRE_STYLE_PRESETS: &[(&str, &str, &str)] = &[
    ("xuanhuan", "玄幻·仙侠·修真", "境界灵气法宝，文白相间飘逸"),
    ("dushi", "都市·现实·异能", "接地气现代质感，人情世故"),
    ("moshi", "末世·废土·求生", "冷峻紧绷，资源稀缺感"),
    ("gaowu", "高武·超凡", "现代力量体系，硬朗快节奏"),
    ("kehuan", "科幻·星际", "冷静精确，技术与宏大尺度"),
    ("lishi", "历史·架空", "古风称谓得体，权谋人心"),
    ("youxi", "游戏·无限流", "副本数值规则感，信息清晰"),
    ("xuanyi", "悬疑·灵异", "阴冷克制留白，氛围铺恐惧"),
    ("yanqing", "现代言情·都市情感", "甜虐拉扯，情绪细腻氛围感"),
    ("guyan", "古代言情·宅斗宫斗", "古雅细腻，落子有声"),
    ("kuaichuan", "快穿·穿书", "一世界一副本，轻快反套路"),
    ("danvzhu", "女频玄幻·大女主", "她自己赢，升级翻盘两开花"),
    ("niandai", "年代·种田·经营", "生活流经营，烟火气里稳步翻身"),
    ("nvshengcun", "女频悬疑·末世求生", "女性群像与危机抉择并重"),
    ("tongyong", "通用·原味", "不强加题材腔，按设定自然写"),
];

fn list_genre_styles(root: &std::path::Path) -> Value {
    let p = root.join("data").join("prompts.defaults.json");
    let prompts = std::fs::read_to_string(&p)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(json!({}));
    let has = |k: &str| {
        prompts
            .as_object()
            .map(|o| o.contains_key(&format!("genre__{}", k)))
            .unwrap_or(false)
    };
    let mut out = Vec::new();
    for (key, name, desc) in GENRE_STYLE_PRESETS {
        // 预设文件缺失该题材时也保留条目（前端下拉不能空），有正文则带上
        let text = prompts[format!("genre__{}", key)].as_str().unwrap_or("");
        out.push(json!({
            "key": key, "name": name, "desc": desc, "label": name,
            "text": if has(key) { text } else { "" }
        }));
    }
    json!(out)
}

/// 解析本书文风卡状态，返回前端期望的结构：
/// {key, isAuto, resolvedKey, resolvedName, hasDistill, needsRedistill, styleSkillId}
fn book_style_resolved(
    db: &molan_core::db::Db,
    root: &std::path::Path,
    key: &str,
    genre: &str,
) -> Value {
    let list = list_genre_styles(root);
    let arr = list.as_array().cloned().unwrap_or_default();
    let preset_name = |k: &str| -> Option<String> {
        arr.iter()
            .find(|e| e["key"].as_str() == Some(k))
            .and_then(|e| e["name"].as_str().map(|s| s.to_string()))
    };
    let mut out = json!({
        "key": key,
        "isAuto": key == "auto",
        "resolvedKey": key,
        "resolvedName": key,
        "hasDistill": false,
        "needsRedistill": false,
        "styleSkillId": Value::Null,
    });
    if let Some(sid) = key.strip_prefix("style:") {
        let name = db
            .q_json(
                "SELECT name FROM skills WHERE id=?1",
                &[&sid as &dyn rusqlite::ToSql],
            )
            .ok()
            .and_then(|v| {
                v.first()
                    .and_then(|r| r["name"].as_str().map(|x| x.to_string()))
            })
            .unwrap_or_else(|| "我的风格".to_string());
        out["resolvedName"] = json!(name);
        out["styleSkillId"] = json!(sid);
        return out;
    }
    if key == "distill" {
        out["resolvedName"] = json!("原作风格");
        return out;
    }
    if key == "off" {
        out["resolvedName"] = json!("关闭");
        return out;
    }
    let rk = if key == "auto" {
        stream::genre_key(genre)
    } else {
        key.to_string()
    };
    out["resolvedKey"] = json!(rk);
    out["resolvedName"] = json!(preset_name(&rk).unwrap_or_else(|| rk.clone()));
    out
}

// 内置模型目录反代（对齐 handlers-misc.llmCatalog）
fn llm_catalog(db: &molan_core::db::Db, root: &std::path::Path) -> Value {
    // 缓存优先
    let cache = molan_llm::get_setting(db, "llm_catalog_cache");
    if let Ok(j) = serde_json::from_str::<Value>(&cache) {
        if let Some(models) = j["models"].as_array() {
            if !models.is_empty() {
                return json!({"models": models, "premium": [], "enabled": true, "rolloutMode": "public"});
            }
        }
    }
    let mut models = std::collections::BTreeSet::new();
    // 收集一个渠道条目的 models[]（缺省回落到 model 单值）；两处来源共用
    let collect = |models: &mut std::collections::BTreeSet<String>, c: &Value| {
        if let Some(ms) = c["models"].as_array() {
            for m in ms.iter().filter_map(|m| m.as_str()) {
                if !m.is_empty() {
                    models.insert(m.to_string());
                }
            }
        } else if let Some(m) = c["model"].as_str() {
            if !m.is_empty() {
                models.insert(m.to_string());
            }
        }
    };
    let (channels, _) = molan_llm::all_settings(db);
    for c in &channels {
        collect(&mut models, c);
    }
    if let Ok(cfg) = serde_json::from_str::<Value>(
        &std::fs::read_to_string(root.join("config.json")).unwrap_or_default(),
    ) {
        if let Some(chs) = cfg["channels"].as_array() {
            for c in chs {
                collect(&mut models, c);
            }
        }
    }
    let list: Vec<&str> = models.iter().map(|s| s.as_str()).collect();
    json!({"models": list, "premium": [], "enabled": !list.is_empty(), "rolloutMode": "public"})
}

// 插件桩（对齐 handlers-misc.pluginStub 常用分支）
fn plugin_stub(name: &str, _args: &Value) -> Value {
    match name {
        "plugin:app|app_os" => json!("win32"),
        "plugin:app|name" => json!("WriterX"),
        "plugin:app|version" => json!("1.0.3"),
        "plugin:app|tauri_version" => json!("2.11.3"),
        "plugin:app|identifier" => json!("com.mochang.mzai"),
        "plugin:window|theme" => json!("light"),
        "plugin:window|title" => json!("WriterX"),
        "plugin:window|inner_size" | "plugin:window|outer_size" => {
            json!({"width": 1280, "height": 800})
        }
        "plugin:window|scale_factor" => json!(1),
        "plugin:updater|check" => Value::Null,
        _ => Value::Null,
    }
}
// 写作引擎辅助：中文/阿拉伯章节数字解析（对齐 Node zhNum）
fn zh_num(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        return s.parse::<i64>().ok();
    }
    let map = [
        ('零', 0),
        ('一', 1),
        ('二', 2),
        ('两', 2),
        ('三', 3),
        ('四', 4),
        ('五', 5),
        ('六', 6),
        ('七', 7),
        ('八', 8),
        ('九', 9),
        ('十', 10),
        ('百', 100),
        ('千', 1000),
    ];
    let mut r = 0i64;
    let mut cur = 0i64;
    for ch in s.chars() {
        let v = map.iter().find(|(c, _)| *c == ch).map(|(_, v)| *v)?;
        if v >= 10 {
            r += (if cur == 0 { 1 } else { cur }) * v;
            cur = 0;
        } else {
            cur = v;
        }
    }
    Some(r + cur)
}

// 「第X章」文件名 → X（不依赖正则）
pub fn chapter_num_from_name(name: &str) -> Option<i64> {
    let chars: Vec<char> = name.chars().collect();
    let mut i = 0;
    while i < chars.len() && chars[i] != '第' {
        i += 1;
    }
    if i >= chars.len() {
        return None;
    }
    i += 1;
    let mut num_txt = String::new();
    while i < chars.len() {
        let ch = chars[i];
        if ch == '章' {
            break;
        }
        if ch.is_whitespace() {
            if num_txt.is_empty() {
                i += 1;
                continue;
            }
            break;
        }
        num_txt.push(ch);
        i += 1;
    }
    if num_txt.is_empty() {
        return None;
    }
    zh_num(&num_txt)
}

/// 写入「正文待审」后登记审批队列（write_file 与 save_partial_as_review 共用）：
/// 保持 approved 仅当「队列已批准 且 本次内容 == 批准回执 hash」（同一份稿重复保存）；
/// 否则一律回到 pending——作者改了稿、或这是同章的新草稿，都必须重新走审批，
/// 绝不能因为「该章历史上批准过」就让新内容继承 approved（否则新稿永远批不了）。
pub(crate) fn register_review_queue(
    db: &molan_core::db::Db,
    book_id: &str,
    name: &str,
    content: &str,
) -> anyhow::Result<Value> {
    let now = stats::now_ms();
    let _g = db.fs_lock.lock().unwrap_or_else(|e| e.into_inner());
    // Non-chapter prose keeps its filename; negative queue IDs never collide with chapters.
    let ch = match chapter_num_from_name(name) {
        Some(ch) if ch > 0 => ch,
        _ => {
            let existing = db.q_json(
                "SELECT ch FROM pending_chapter WHERE book_id=?1 AND review_file=?2",
                &[&book_id as &dyn rusqlite::ToSql, &name],
            )?;
            if let Some(ch) = existing.first().and_then(|r| r["ch"].as_i64()) {
                ch
            } else {
                let rows = db.q_json(
                    "SELECT MIN(ch) AS low FROM pending_chapter WHERE book_id=?1",
                    &[&book_id],
                )?;
                rows.first()
                    .and_then(|r| r["low"].as_i64())
                    .unwrap_or(0)
                    .min(0)
                    .checked_sub(1)
                    .ok_or_else(|| anyhow!("待审编号已耗尽"))?
            }
        }
    };
    let prev = db
        .q_json(
            "SELECT status FROM pending_chapter WHERE book_id=?1 AND ch=?2",
            &[&book_id as &dyn rusqlite::ToSql, &ch],
        )
        .unwrap_or_default();
    let prev_approved = prev.first().and_then(|r| r["status"].as_str()) == Some("approved");
    let receipt = molan_core::continuity::approved_hash(db, book_id, name).unwrap_or(None);
    let same_as_approved = match receipt.as_deref() {
        Some(rec) => molan_core::continuity::content_hash(content) == rec,
        None => false,
    };
    let keep_approved = prev_approved && same_as_approved;
    let status = if keep_approved { "approved" } else { "pending" };
    db.exec(
        "INSERT INTO pending_chapter(book_id,ch,review_file,status,created_at,updated_at)
         VALUES(?1,?2,?3,?4,?5,?5)
         ON CONFLICT(book_id,ch) DO UPDATE SET
             review_file=excluded.review_file,
             status=excluded.status,
             updated_at=excluded.updated_at",
        &[&book_id as &dyn rusqlite::ToSql, &ch, &name, &status, &now],
    )?;
    drop(_g);
    Ok(json!({ "ch": ch, "status": status }))
}

fn max_chapter_num(tree: &Value, group_dir: &str) -> i64 {
    let Some(arr) = tree.as_array() else { return 0 };
    let mut max = 0i64;
    for g in arr {
        if g["dir"].as_str() != Some(group_dir) {
            continue;
        }
        if let Some(fs) = g["files"].as_array() {
            for f in fs {
                if let Some(n) = chapter_num_from_name(f["name"].as_str().unwrap_or("")) {
                    max = max.max(n);
                }
            }
        }
    }
    max
}

/// 组装系统提示词前缀：prompts.system 去空白 + 两个换行（无则空串）。
fn prompt_prefix(p: &Value) -> String {
    match p["system"].as_str() {
        Some(x) => format!("{}\n\n", x.trim()),
        None => String::new(),
    }
}

fn read_prompts(root: &std::path::Path) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(root.join("data").join("prompts.defaults.json"))
            .unwrap_or_default(),
    )
    .unwrap_or(json!({}))
}

async fn chat_once_params(
    chn: &Value,
    messages: Vec<Value>,
    max_tokens: i64,
) -> anyhow::Result<String> {
    molan_llm::chat_once(molan_llm::ChatParams {
        base_url: chn["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: chn["key"].as_str().unwrap_or("").to_string(),
        model: chn["model"].as_str().unwrap_or("deepseek-chat").to_string(),
        messages,
        max_tokens,
        // 轻量辅助（会话标题等）：不思考，保持秒回
        reasoning_effort: String::new(),
        ..Default::default()
    })
    .await
}

pub mod stream;

fn safe_chat_save_group(group: &str) -> bool {
    ["设定", "细纲", "参考", molan_core::db::REVIEW_GROUP].contains(&group)
}

/// get_settings 读侧归一化：channels 是 JSON 数组字符串时，为缺 models 数组的
/// 条目补 []（预打包前端读 T.models.length，缺字段会整页白屏）。
/// 解析失败或不是数组时原样返回，绝不改库存数据。
pub fn normalize_channels_value(raw: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    let Some(arr) = v.as_array_mut() else {
        return raw.to_string();
    };
    let mut changed = false;
    for e in arr.iter_mut() {
        if let Some(o) = e.as_object_mut() {
            let has = o.get("models").map(|m| m.is_array()).unwrap_or(false);
            if !has {
                let fallback: Vec<Value> = match o.get("model").and_then(|m| m.as_str()) {
                    Some(m) if !m.is_empty() => vec![Value::String(m.to_string())],
                    _ => vec![],
                };
                o.insert("models".to_string(), Value::Array(fallback));
                changed = true;
            }
        }
    }
    if !changed {
        return raw.to_string();
    }
    serde_json::to_string(&v).unwrap_or_else(|_| raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::{normalize_channels_value, safe_chat_save_group};

    #[test]
    fn chat_reply_cannot_write_formal_body_or_unknown_group() {
        assert!(safe_chat_save_group("正文待审"));
        assert!(safe_chat_save_group("细纲"));
        assert!(!safe_chat_save_group("正文"));
        assert!(!safe_chat_save_group("../正文"));
    }

    #[test]
    fn missing_models_gets_model_fallback_array() {
        let raw =
            r#"[{"id":"mock1","model":"mock-model","label":"mock"},{"id":"a","models":["m1"]}]"#;
        let out = normalize_channels_value(raw);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v[0]["models"].as_array().unwrap().len(), 1);
        assert_eq!(v[0]["models"][0], serde_json::json!("mock-model"));
        assert_eq!(v[1]["models"][0], serde_json::json!("m1"));
    }

    #[test]
    fn entry_without_model_gets_empty_models() {
        let raw = r#"[{"id":"x"}]"#;
        let v: serde_json::Value = serde_json::from_str(&normalize_channels_value(raw)).unwrap();
        assert!(v[0]["models"].is_array());
        assert_eq!(v[0]["models"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn non_array_and_garbage_pass_through_unchanged() {
        assert_eq!(normalize_channels_value(""), "");
        assert_eq!(normalize_channels_value("not json"), "not json");
        assert_eq!(normalize_channels_value(r#"{"id":"o"}"#), r#"{"id":"o"}"#);
        // 已是数组且条目齐全：逐字节原样返回（幂等，不重排不格式化）
        let ok = r#"[{"id":"a","models":[]}]"#;
        assert_eq!(normalize_channels_value(ok), ok);
    }

    #[test]
    fn null_models_counts_as_missing() {
        let raw = r#"[{"id":"a","models":null}]"#;
        let v: serde_json::Value = serde_json::from_str(&normalize_channels_value(raw)).unwrap();
        assert!(v[0]["models"].is_array());
    }
}
