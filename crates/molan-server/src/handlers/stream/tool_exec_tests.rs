use super::*;
use molan_core::files;
use serde_json::json;

fn setup() -> (tempfile::TempDir, Db, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), None).unwrap();
    let id = molan_core::books::create_book(&db, "书", "玄幻", "第三人称")["id"]
        .as_str()
        .unwrap()
        .to_string();
    files::write_file(&db, &id, "设定", "人物.md", "林照").unwrap();
    files::write_file(&db, &id, "设定", "世界.md", "青岚宗").unwrap();
    (dir, db, id)
}

fn read(name: &str) -> (String, Value) {
    (
        "read_book_file".to_string(),
        json!({"group": "设定", "name": name}),
    )
}

fn text(r: &(Result<Value>, u128, bool)) -> String {
    r.0.as_ref().map(|v| v.to_string()).unwrap_or_default()
}

/// 状态戳：文件写入、可见性标志、技能变化、细纲确认都会改变它；无变化时稳定。
#[test]
fn stamp_tracks_book_state() {
    let (_d, db, b) = setup();
    let s0 = state_stamp(&db, &b);
    assert_eq!(s0, state_stamp(&db, &b), "无变化时稳定");
    files::write_file(&db, &b, "设定", "人物.md", "林照（改）").unwrap();
    let s1 = state_stamp(&db, &b);
    assert_ne!(s0, s1, "文件写入");
    files::set_file_flag(&db, &b, "设定", "世界.md", "aiOff", true).unwrap();
    let s2 = state_stamp(&db, &b);
    assert_ne!(s1, s2, "文件可见性");
    db.exec("INSERT INTO skills(id,name,prompt_template,kind,enabled,usage_mode,origin,targets_json) VALUES('k','技','模板','user',1,'support','user','[]')", &[]).unwrap();
    let s3 = state_stamp(&db, &b);
    assert_ne!(s2, s3, "技能变化");
    files::write_file(&db, &b, "细纲", "细纲_第1章.md", "细纲").unwrap();
    let s4 = state_stamp(&db, &b);
    molan_core::outline_confirm::confirm(&db, &b, 1, "细纲_第1章.md", None).unwrap();
    assert_ne!(s4, state_stamp(&db, &b), "细纲确认");
}

/// 只读组：结果按输入顺序；同轮重复调用只执行一次；再次调用命中缓存；状态变化后失效；失败不缓存。
#[test]
fn read_group_caches_dedupes_and_keeps_order() {
    let (_d, db, b) = setup();
    let mut cache = ToolCache::default();
    let items = vec![
        read("人物.md"),
        read("世界.md"),
        read("人物.md"),
        ("scan_book_tree".to_string(), json!({})),
    ];
    let r = run_read_group(&db, &b, &items, &mut cache);
    assert!(
        text(&r[0]).contains("林照") && text(&r[1]).contains("青岚宗"),
        "{:?}",
        text(&r[0])
    );
    assert_eq!(text(&r[2]), text(&r[0]));
    assert_eq!(
        (r[0].2, r[1].2, r[2].2, r[3].2),
        (false, false, true, false),
        "第三个是同轮重复"
    );
    let again = run_read_group(&db, &b, &items, &mut cache);
    assert!(again.iter().all(|x| x.2), "状态未变：全部命中缓存");
    files::write_file(&db, &b, "设定", "人物.md", "林照·新").unwrap();
    let fresh = run_read_group(&db, &b, &items[..1], &mut cache);
    assert!(!fresh[0].2, "文件变化后失效重读");
    assert!(text(&fresh[0]).contains("林照·新"));
    files::set_file_flag(&db, &b, "设定", "世界.md", "aiOff", true).unwrap();
    let hidden = run_read_group(&db, &b, &items[1..2], &mut cache);
    assert!(hidden[0].0.is_err(), "设为 AI 不可见后不得返回缓存的旧内容");
    let again = run_read_group(&db, &b, &items[1..2], &mut cache);
    assert!(!again[0].2, "失败结果不缓存");
    cache.clear();
    let cleared = run_read_group(&db, &b, &items[3..], &mut cache);
    assert!(!cleared[0].2, "清空后重新执行");
}
