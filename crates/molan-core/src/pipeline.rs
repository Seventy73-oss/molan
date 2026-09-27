//! 生产线状态推导（AGENT-PIPELINE-PLAN §A）：文件即真相。
//! 不建独立状态表——阶段状态全部从既有产物（目录树+审批队列）推导，
//! 避免「状态说完成了但文件没有」的脱节事故。纯函数，便于契约测试。
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// 宽松章号解析：文件名任意位置的「第N章」（阿拉伯数字或常见汉字数字）。
/// 与 continuity::chapter_number（要求「第」开头）不同——细纲_第1章.md 这类前缀名也要识别，
/// 语义对齐服务端 chapter_num_from_name。
fn chapter_of(name: &str) -> Option<i64> {
    let chars: Vec<char> = name.chars().collect();
    let start = chars.iter().position(|c| *c == '第')?;
    let zh = |c: char| -> Option<i64> {
        match c {
            '一' => Some(1),
            '二' => Some(2),
            '三' => Some(3),
            '四' => Some(4),
            '五' => Some(5),
            '六' => Some(6),
            '七' => Some(7),
            '八' => Some(8),
            '九' => Some(9),
            '十' => Some(10),
            _ => None,
        }
    };
    let mut i = start + 1;
    let mut num: i64 = 0;
    while i < chars.len() {
        if let Some(d) = chars[i].to_digit(10) {
            num = num.saturating_mul(10).saturating_add(d as i64);
            i += 1;
        } else if num == 0 {
            let v = zh(chars[i])?;
            // 十X = 10+X；X十Y 组合超出常见章名范围，从简。
            num = if v == 10 {
                i += 1;
                match i < chars.len() {
                    true => zh(chars[i]).map(|x| 10 + x).unwrap_or(10),
                    false => 10,
                }
            } else {
                v
            };
            if num > 10 && chars[start + 1] == '十' {
                i += 1;
            } else if num <= 10 && chars[start + 1] == '十' && num == 10 {
                // 已在上面消费
            } else {
                i += 1;
            }
        } else {
            break;
        }
    }
    if num == 0 {
        return None;
    }
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    (chars.get(i) == Some(&'章')).then_some(num)
}

/// 从目录树某组取文件名列表。
fn group_files(tree: &Value, dir: &str) -> Vec<String> {
    tree.as_array()
        .into_iter()
        .flatten()
        .find(|g| g["dir"].as_str() == Some(dir))
        .and_then(|g| g["files"].as_array())
        .map(|fs| {
            fs.iter()
                .filter_map(|f| f["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// 推导整本书的生产线状态（纯 tree+queue 契约入口；不做空文件/记忆检测）。
/// tree: scan_tree 输出；queue: pending_chapter 行 [{ch,review_file,status}]。
pub fn derive_state(tree: &Value, queue: &Value) -> Value {
    derive_state_opts(tree, queue, &BTreeSet::new())
}

/// 章节列表上限：病态文件名（如「第200000章」）不得把 1..=max_ch 撑爆（G5）。
const MAX_LISTED_CH: i64 = 20_000;

/// empty_bodies：「正文文件存在但内容全为空白」的章号集合——空正式稿不得冒充 approved。
pub fn derive_state_opts(tree: &Value, queue: &Value, empty_bodies: &BTreeSet<i64>) -> Value {
    let settings = group_files(tree, "设定");
    let outlines = group_files(tree, "细纲");
    let bodies = group_files(tree, "正文");
    let outline_file = settings.iter().find(|n| n.contains("大纲")).cloned();
    let has_setup = settings.iter().any(|n| n == "建书档案.md");

    // 预建索引消除 1..=max_ch × find 的 O(n²)（面板每次审批都刷新，必须线性）。
    // 同章多文件：按文件名排序取首个为准，重复数以 bodyDupFiles/outlineDupFiles 暴露。
    let mapped = |names: &Vec<String>| -> BTreeMap<i64, (String, usize)> {
        let mut sorted = names.clone();
        sorted.sort();
        let mut m: BTreeMap<i64, (String, usize)> = BTreeMap::new();
        for n in &sorted {
            if let Some(c) = chapter_of(n) {
                let e = m.entry(c).or_insert_with(|| (n.clone(), 0));
                e.1 += 1;
            }
        }
        m
    };
    let outline_map = mapped(&outlines);
    let body_map = mapped(&bodies);
    // 章节摘要（参考组，名字含「摘要」+章号）：P3a 承接环产物
    let summaries: BTreeSet<i64> = group_files(tree, "参考")
        .iter()
        .filter(|n| n.contains("摘要"))
        .filter_map(|n| chapter_of(n))
        .collect();
    let mut max_ch = 0i64;
    for c in outline_map.keys().chain(body_map.keys()) {
        max_ch = max_ch.max(*c);
    }
    let queue_rows: Vec<(i64, String)> = queue
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["status"].as_str() == Some("pending"))
        .filter_map(|r| {
            // 服务端 q_json 会把 review_file 驼峰化为 reviewFile；两种键都接受
            let name = r["review_file"]
                .as_str()
                .or_else(|| r["reviewFile"].as_str())
                .unwrap_or("");
            r["ch"].as_i64().map(|c| (c, name.to_string()))
        })
        .collect();
    let mut queue_map: BTreeMap<i64, String> = BTreeMap::new();
    for (c, f) in &queue_rows {
        max_ch = max_ch.max(*c);
        queue_map.entry(*c).or_insert_with(|| f.clone());
    }
    let truncated = max_ch > MAX_LISTED_CH;
    let listed_to = max_ch.min(MAX_LISTED_CH);

    let mut chapters = Vec::new();
    for n in 1..=listed_to {
        let outline = outline_map.get(&n).map(|(f, _)| f.clone());
        let body_file = body_map.get(&n).map(|(f, _)| f.clone());
        let empty_formal = body_file.is_some() && empty_bodies.contains(&n);
        let approved = body_file.clone().filter(|_| !empty_formal);
        let pending = queue_map.get(&n).cloned();
        let body = if approved.is_some() {
            "approved"
        } else if pending.is_some() {
            "pending"
        } else {
            "none"
        };
        let mut c = json!({
            "n": n, "outline": outline, "body": body,
            "bodyFile": approved.or(pending),
            "summary": summaries.contains(&n),
        });
        if empty_formal {
            c["emptyFormal"] = json!(true);
        }
        if let Some((_, k)) = body_map.get(&n) {
            if *k > 1 {
                c["bodyDupFiles"] = json!(*k);
            }
        }
        if let Some((_, k)) = outline_map.get(&n) {
            if *k > 1 {
                c["outlineDupFiles"] = json!(*k);
            }
        }
        chapters.push(c);
    }

    // 下一步建议：待审优先（批准后才进入下一章），其次补细纲/正文，最后续写新章。
    let next = if outline_file.is_none() {
        json!({"stage": "outline", "chapter": null})
    } else if !has_setup {
        json!({"stage": "setup", "chapter": null})
    } else if let Some(c) = chapters
        .iter()
        .find(|c| c["body"].as_str() == Some("pending"))
    {
        json!({"stage": "review", "chapter": c["n"]})
    } else if let Some(c) = chapters
        .iter()
        .find(|c| c["outline"].is_null() && c["body"].as_str() == Some("none"))
    {
        // 细纲补洞只针对尚未开写正文的章；已定稿章不回溯卡流程
        json!({"stage": "chapter_outline", "chapter": c["n"]})
    } else if let Some(c) = chapters.iter().find(|c| c["body"].as_str() == Some("none")) {
        json!({"stage": "chapter_body", "chapter": c["n"]})
    } else {
        json!({"stage": "chapter_outline", "chapter": listed_to + 1})
    };

    // 承接建议：最近的已定稿但尚无摘要的章（非阻塞，面板显示为副操作）
    let summary_due = chapters
        .iter()
        .rev()
        .find(|c| {
            c["body"].as_str() == Some("approved") && !c["summary"].as_bool().unwrap_or(false)
        })
        .map(|c| c["n"].clone());
    let total = chapters.len();
    let approved_count = chapters
        .iter()
        .filter(|c| c["body"].as_str() == Some("approved"))
        .count();
    let pending_count = chapters
        .iter()
        .filter(|c| c["body"].as_str() == Some("pending"))
        .count();
    let mut out = json!({
        "summaryDue": summary_due,
        "hasOutline": outline_file.is_some(),
        "outlineFile": outline_file,
        "hasSetup": has_setup,
        "chapters": chapters,
        "next": next,
        "counts": {
            "total": total,
            "approved": approved_count,
            "pending": pending_count,
        },
    });
    if truncated {
        out["truncated"] = json!(true);
        out["maxChapterSeen"] = json!(max_ch);
    }
    out
}

/// 记忆状态合成：失败语义优先，绝不掩盖问题（failed > stale > pending > valid > missing）。
fn memory_status_of(job: Option<&str>, mem: Option<&str>) -> &'static str {
    if job == Some("failed") {
        "failed"
    } else if mem == Some("stale") {
        "stale"
    } else if job == Some("pending") {
        "pending"
    } else if mem == Some("valid") {
        "valid"
    } else if job == Some("missing") {
        "missing"
    } else {
        "absent"
    }
}

fn is_effectively_empty(fp: &std::path::Path) -> bool {
    match std::fs::metadata(fp) {
        Ok(md) if md.len() == 0 => true,
        Ok(_) => std::fs::read_to_string(fp)
            .map(|s| s.trim().is_empty())
            .unwrap_or(false),
        // 读不出：不猜"空"，维持存在性语义（问题留给健康视图暴露）
        Err(_) => false,
    }
}

/// 全量推导（get_pipeline_state 唯一入口）：空文件检测 + 记忆维度 + chapter_memory
/// 参与承接判定。G3/G4/G5 修复：批准后正文被改（stale）、记忆失败（failed）在
/// 面板可见；valid 记忆即承接产物，summaryDue 不再因缺 参考/摘要 文件长期卡住。
pub fn derive_state_with_memory(
    db: &crate::db::Db,
    book: &str,
    tree: &Value,
    queue: &Value,
) -> Value {
    let mut empty_bodies = BTreeSet::new();
    for name in group_files(tree, "正文") {
        if let Some(ch) = chapter_of(&name) {
            if is_effectively_empty(&crate::files::book_path(db, book, "正文", &name)) {
                empty_bodies.insert(ch);
            }
        }
    }
    let mut state = derive_state_opts(tree, queue, &empty_bodies);

    let to_map = |rows: Vec<Value>| -> BTreeMap<i64, String> {
        rows.iter()
            .filter_map(|r| {
                Some((
                    r["ch"].as_i64()?,
                    r["status"].as_str().unwrap_or("").to_string(),
                ))
            })
            .collect()
    };
    let jobs = to_map(
        db.q_json(
            "SELECT ch,status FROM memory_job WHERE book_id=?1",
            &[&book],
        )
        .unwrap_or_default(),
    );
    let mems = to_map(
        db.q_json(
            "SELECT ch,status FROM chapter_memory WHERE book_id=?1",
            &[&book],
        )
        .unwrap_or_default(),
    );
    let (mut stale, mut failed, mut pending) = (0i64, 0i64, 0i64);
    let mut keys: BTreeSet<i64> = jobs.keys().chain(mems.keys()).copied().collect();
    for ch in std::mem::take(&mut keys) {
        match memory_status_of(
            jobs.get(&ch).map(String::as_str),
            mems.get(&ch).map(String::as_str),
        ) {
            "stale" => stale += 1,
            "failed" => failed += 1,
            "pending" => pending += 1,
            _ => {}
        }
    }
    if let Some(arr) = state["chapters"].as_array_mut() {
        for c in arr.iter_mut() {
            let n = c["n"].as_i64().unwrap_or(0);
            let s = memory_status_of(
                jobs.get(&n).map(String::as_str),
                mems.get(&n).map(String::as_str),
            );
            c["memory"] = json!(s);
            if s == "valid" && !c["summary"].as_bool().unwrap_or(false) {
                c["summary"] = json!(true);
                c["summarySource"] = json!("memory");
            } else if c["summary"].as_bool().unwrap_or(false) {
                c["summarySource"] = json!("file");
            }
        }
    }
    // summary 语义升级后重算承接建议
    let summary_due = state["chapters"].as_array().and_then(|arr| {
        arr.iter()
            .rev()
            .find(|c| {
                c["body"].as_str() == Some("approved") && !c["summary"].as_bool().unwrap_or(false)
            })
            .map(|c| c["n"].clone())
    });
    state["summaryDue"] = summary_due.unwrap_or(Value::Null);
    state["memory"] = json!({"stale": stale, "failed": failed, "pending": pending});
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tree(setup: &[&str], outline: &[&str], body: &[&str]) -> Value {
        let mk = |dir: &str, names: &[&str]| json!({"dir": dir, "files": names.iter().map(|n| json!({"name": n})).collect::<Vec<_>>()});
        json!([mk("设定", setup), mk("细纲", outline), mk("正文", body)])
    }

    #[test]
    fn lenient_chapter_parser_handles_prefixed_names() {
        assert_eq!(chapter_of("细纲_第1章.md"), Some(1));
        assert_eq!(chapter_of("第12章.md"), Some(12));
        assert_eq!(chapter_of("细纲_第3章_结果保存.md"), Some(3));
        assert_eq!(chapter_of("第十章.md"), Some(10));
        assert_eq!(chapter_of("建书档案.md"), None);
        assert_eq!(chapter_of("全文.md"), None);
    }

    #[test]
    fn empty_book_starts_at_outline() {
        let s = derive_state(&tree(&[], &[], &[]), &json!([]));
        assert_eq!(s["next"]["stage"], "outline");
        assert!(!s["hasOutline"].as_bool().unwrap());
    }

    #[test]
    fn setup_missing_blocks_chapters() {
        let s = derive_state(&tree(&["剧情大纲.md"], &[], &[]), &json!([]));
        assert!(s["hasOutline"].as_bool().unwrap());
        assert_eq!(s["next"]["stage"], "setup");
    }

    #[test]
    fn pending_review_takes_priority_over_next_chapter() {
        let q = json!([{"ch": 1, "review_file": "第1章.md", "status": "pending"}]);
        let s = derive_state(
            &tree(&["建书档案.md", "剧情大纲.md"], &["细纲_第1章.md"], &[]),
            &q,
        );
        assert_eq!(s["next"]["stage"], "review");
        assert_eq!(s["next"]["chapter"], 1);
        assert_eq!(s["chapters"][0]["body"], "pending");
    }

    #[test]
    fn approved_chapter_advances_to_next_outline() {
        let q = json!([{"ch": 1, "review_file": "第1章.md", "status": "approved"}]);
        let s = derive_state(
            &tree(
                &["建书档案.md", "剧情大纲.md"],
                &["细纲_第1章.md"],
                &["第1章.md"],
            ),
            &q,
        );
        assert_eq!(s["chapters"][0]["body"], "approved");
        assert_eq!(s["next"]["stage"], "chapter_outline");
        assert_eq!(s["next"]["chapter"], 2);
        assert_eq!(s["counts"]["approved"], 1);
    }

    #[test]
    fn outline_gap_fills_before_body() {
        let s = derive_state(
            &tree(
                &["建书档案.md", "剧情大纲.md"],
                &["细纲_第2章.md"],
                &["第1章.md"],
            ),
            &json!([]),
        );
        // 第1章缺细纲但正文已定稿；第2章有细纲无正文 → 先补第2章正文
        assert_eq!(s["next"]["stage"], "chapter_body");
        assert_eq!(s["next"]["chapter"], 2);
    }

    #[test]
    fn summary_due_tracks_latest_approved_without_summary() {
        let q = json!([{"ch": 1, "review_file": "第1章.md", "status": "approved"}]);
        let t = |refs: &[&str]| {
            let mk = |dir: &str, names: &[&str]| json!({"dir": dir, "files": names.iter().map(|n| json!({"name": n})).collect::<Vec<_>>()});
            json!([
                mk("设定", &["建书档案.md", "剧情大纲.md"]),
                mk("细纲", &["细纲_第1章.md"]),
                mk("正文", &["第1章.md"]),
                mk("参考", refs)
            ])
        };
        let s = derive_state(&t(&[]), &q);
        assert_eq!(s["summaryDue"], 1);
        assert!(!s["chapters"][0]["summary"].as_bool().unwrap());
        let s2 = derive_state(&t(&["摘要_第1章.md"]), &q);
        assert!(s2["summaryDue"].is_null());
        assert!(s2["chapters"][0]["summary"].as_bool().unwrap());
    }

    #[test]
    fn rejected_queue_rows_do_not_count_as_pending() {
        let q = json!([{"ch": 1, "review_file": "第1章.md", "status": "rejected"}]);
        let s = derive_state(
            &tree(&["建书档案.md", "剧情大纲.md"], &["细纲_第1章.md"], &[]),
            &q,
        );
        assert_eq!(s["chapters"][0]["body"], "none");
        assert_eq!(s["next"]["stage"], "chapter_body");
    }

    fn db_fixture() -> (tempfile::TempDir, crate::db::Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(dir.path(), None).unwrap();
        let book = crate::books::create_book(&db, "pipe-test", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        (dir, db, book)
    }

    #[test]
    fn empty_formal_body_is_not_approved() {
        // G5：空白正式稿不得冒充 approved（旧实现文件存在即 approved）
        let (_d, db, book) = db_fixture();
        crate::files::write_file(&db, &book, "设定", "建书档案.md", "档案").unwrap();
        crate::files::write_file(&db, &book, "设定", "剧情大纲.md", "大纲").unwrap();
        crate::files::write_file(&db, &book, "细纲", "细纲_第1章.md", "细纲").unwrap();
        crate::files::write_file(&db, &book, "正文", "第1章.md", "   \n\t ").unwrap();
        let tree = crate::files::scan_tree(&db, &book);
        let s = derive_state_with_memory(&db, &book, &tree, &json!([]));
        assert_eq!(s["chapters"][0]["body"], "none");
        assert_eq!(s["chapters"][0]["emptyFormal"], true);
        assert_eq!(s["next"]["stage"], "chapter_body");
        assert_eq!(s["next"]["chapter"], 1);
    }

    #[test]
    fn memory_dimension_and_summary_via_chapter_memory() {
        // G3/G4：记忆维度可见；valid 记忆即承接产物，summaryDue 不再长期卡住
        let (_d, db, book) = db_fixture();
        crate::files::write_file(&db, &book, "设定", "建书档案.md", "档案").unwrap();
        crate::files::write_file(&db, &book, "设定", "剧情大纲.md", "大纲").unwrap();
        crate::files::write_file(&db, &book, "正文", "第1章.md", "第一章内容").unwrap();
        crate::files::write_file(&db, &book, "正文", "第2章.md", "第二章内容").unwrap();
        db.exec(
            "INSERT INTO chapter_memory(book_id,ch,name,source_hash,payload_json,status,updated_at) VALUES(?1,1,'第1章.md','h1','{}','valid',0)",
            &[&book as &dyn rusqlite::ToSql],
        )
        .unwrap();
        // write_file(正文) 已由 note_file_change 预置 memory_job(pending)：只 UPDATE
        db.exec(
            "UPDATE memory_job SET status='done' WHERE book_id=?1 AND ch=1",
            &[&book as &dyn rusqlite::ToSql],
        )
        .unwrap();
        db.exec(
            "UPDATE memory_job SET status='failed', error='上游错误' WHERE book_id=?1 AND ch=2",
            &[&book as &dyn rusqlite::ToSql],
        )
        .unwrap();
        let tree = crate::files::scan_tree(&db, &book);
        let s = derive_state_with_memory(&db, &book, &tree, &json!([]));
        assert_eq!(s["chapters"][0]["memory"], "valid");
        assert_eq!(s["chapters"][0]["summary"], true);
        assert_eq!(s["chapters"][0]["summarySource"], "memory");
        assert_eq!(s["chapters"][1]["memory"], "failed");
        assert_eq!(s["memory"]["failed"].as_i64().unwrap(), 1);
        assert_eq!(s["summaryDue"], 2, "第2章已定稿但无承接产物 → due");
    }

    #[test]
    fn editing_formal_marks_memory_stale_in_state() {
        let (_d, db, book) = db_fixture();
        crate::files::write_file(&db, &book, "正文", "第1章.md", "旧内容").unwrap();
        db.exec(
            "INSERT INTO chapter_memory(book_id,ch,name,source_hash,payload_json,status,updated_at) VALUES(?1,1,'第1章.md','h1','{}','valid',0)",
            &[&book as &dyn rusqlite::ToSql],
        )
        .unwrap();
        db.exec(
            "UPDATE memory_job SET status='done' WHERE book_id=?1 AND ch=1",
            &[&book as &dyn rusqlite::ToSql],
        )
        .unwrap();
        // 作者改正文 → note_file_change：memory stale + job pending
        crate::files::write_file(&db, &book, "正文", "第1章.md", "作者改后的新内容").unwrap();
        let tree = crate::files::scan_tree(&db, &book);
        let s = derive_state_with_memory(&db, &book, &tree, &json!([]));
        assert_eq!(
            s["chapters"][0]["memory"], "stale",
            "批准后正文被改必须可见"
        );
        assert_eq!(s["memory"]["stale"].as_i64().unwrap(), 1);
        assert_eq!(s["chapters"][0]["summary"], false, "stale 记忆不算承接产物");
        assert_eq!(s["summaryDue"], 1);
    }

    #[test]
    fn huge_chapter_number_is_capped_not_exploded() {
        let t = |names: &[&str]| json!([{"dir": "正文", "files": names.iter().map(|n| json!({"name": n})).collect::<Vec<_>>()}]);
        let s = derive_state(&t(&["第200000章.md"]), &json!([]));
        assert_eq!(s["truncated"], true);
        assert_eq!(s["chapters"].as_array().unwrap().len() as i64, 20_000);
        assert_eq!(s["maxChapterSeen"].as_i64().unwrap(), 200000);
    }

    #[test]
    fn many_chapters_derive_is_linear() {
        let names: Vec<String> = (1..=500).map(|i| format!("第{}章.md", i)).collect();
        let files: Vec<Value> = names.iter().map(|n| json!({"name": n})).collect();
        let t = json!([
            {"dir": "设定", "files": [{"name": "建书档案.md"}, {"name": "剧情大纲.md"}]},
            {"dir": "细纲", "files": []},
            {"dir": "正文", "files": files}
        ]);
        let start = std::time::Instant::now();
        let s = derive_state(&t, &json!([]));
        let elapsed = start.elapsed().as_secs_f64();
        assert!(elapsed < 2.0, "500章推导必须线性：{}s", elapsed);
        assert_eq!(s["counts"]["approved"].as_i64().unwrap(), 500);
    }
}
