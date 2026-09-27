use super::*;
#[test]
fn five_role_profiles_preserve_legacy_settings_and_distill_fallback() {
    let dir = std::env::temp_dir().join(format!("molan-agent-roles-{}", std::process::id()));
    let db = Db::open(&dir, None).unwrap();
    let channels = json!([
        {"id":"primary", "label":"主模型", "model":"general", "models":["general", "writing"]},
        {"id":"secondary", "label":"分工模型", "model":"analysis", "models":["analysis", "distill"]}
    ]);
    db.exec(
        "INSERT INTO settings(key,value) VALUES(?1,?2)",
        &[&"channels" as &dyn rusqlite::ToSql, &channels.to_string()],
    )
    .unwrap();
    db.exec(
        "INSERT INTO settings(key,value) VALUES(?1,?2)",
        &[&"active_channel" as &dyn rusqlite::ToSql, &"primary"],
    )
    .unwrap();
    let old_chapter = json!({"channelId":"primary", "model":"writing"}).to_string();
    db.exec(
        "INSERT INTO settings(key,value) VALUES(?1,?2)",
        &[
            &"agent_profile__chapter" as &dyn rusqlite::ToSql,
            &old_chapter,
        ],
    )
    .unwrap();
    let profiles = all_agent_profiles(&db);
    let roles = profiles.as_object().unwrap();
    assert_eq!(roles.len(), 5);
    for role in ["distill", "outline", "chapter", "review", "summary"] {
        assert!(roles.contains_key(role), "missing role {role}");
    }
    assert_eq!(profiles["chapter"]["model"], "writing");
    assert_eq!(profiles["distill"]["model"], "");
    assert_eq!(
        resolve_agent_channel(&db, "chapter").unwrap()["model"],
        "writing"
    );
    assert_eq!(
        resolve_agent_channel(&db, "distill").unwrap()["model"],
        "general"
    );
    let distill = json!({"channelId":"secondary", "model":"distill"}).to_string();
    db.exec(
        "INSERT INTO settings(key,value) VALUES(?1,?2)",
        &[&"agent_profile__distill" as &dyn rusqlite::ToSql, &distill],
    )
    .unwrap();
    assert_eq!(
        resolve_agent_channel(&db, "distill").unwrap()["id"],
        "secondary"
    );
    assert_eq!(
        resolve_agent_channel(&db, "distill").unwrap()["model"],
        "distill"
    );
    let no_catalog = json!([{"id":"primary","model":"general","models":[]}]).to_string();
    db.exec(
        "UPDATE settings SET value=?1 WHERE key='channels'",
        &[&no_catalog],
    )
    .unwrap();
    assert_eq!(
        resolve_agent_channel(&db, "distill").unwrap()["model"],
        "general"
    );
    assert_eq!(
        resolve_agent_channel(&db, "chapter").unwrap()["model"],
        "writing"
    );
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}
