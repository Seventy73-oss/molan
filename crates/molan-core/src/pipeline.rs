//! 生产线状态推导（AGENT-PIPELINE-PLAN §A）：文件即真相。
//! 不建独立状态表——阶段状态全部从既有产物（目录树+审批队列）推导，
//! 避免「状态说完成了但文件没有」的脱节事故。纯函数，便于契约测试。
use serde_json::{json, Value};

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
            num = num * 10 + d as i64;
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

/// 推导整本书的生产线状态。
/// tree: scan_tree 输出；queue: pending_chapter 行 [{ch,review_file,status}]。
pub fn derive_state(tree: &Value, queue: &Value) -> Value {
    let settings = group_files(tree, "设定");
    let outlines = group_files(tree, "细纲");
    let bodies = group_files(tree, "正文");
    let outline_file = settings.iter().find(|n| n.contains("大纲")).cloned();
    let has_setup = settings.iter().any(|n| n == "建书档案.md");

    let mapped = |names: &Vec<String>| -> Vec<(i64, String)> {
        names
            .iter()
            .filter_map(|n| chapter_of(n).map(|c| (c, n.clone())))
            .collect()
    };
    let outline_map = mapped(&outlines);
    let body_map = mapped(&bodies);
    // 章节摘要（参考组，名字含「摘要」+章号）：P3a 承接环产物
    let summaries: Vec<i64> = group_files(tree, "参考")
        .iter()
        .filter(|n| n.contains("摘要"))
        .filter_map(|n| chapter_of(n))
        .collect();
    let mut max_ch = 0i64;
    for (c, _) in outline_map.iter().chain(body_map.iter()) {
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
    for (c, _) in &queue_rows {
        max_ch = max_ch.max(*c);
    }

    let mut chapters = Vec::new();
    for n in 1..=max_ch {
        let outline = outline_map
            .iter()
            .find(|(c, _)| *c == n)
            .map(|(_, f)| f.clone());
        let approved = body_map
            .iter()
            .find(|(c, _)| *c == n)
            .map(|(_, f)| f.clone());
        let pending = queue_rows
            .iter()
            .find(|(c, _)| *c == n)
            .map(|(_, f)| f.clone());
        let body = if approved.is_some() {
            "approved"
        } else if pending.is_some() {
            "pending"
        } else {
            "none"
        };
        chapters.push(
            json!({"n": n, "outline": outline, "body": body, "bodyFile": approved.or(pending), "summary": summaries.contains(&n)}),
        );
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
        json!({"stage": "chapter_outline", "chapter": max_ch + 1})
    };

    // 承接建议：最近的已定稿但尚无摘要的章（非阻塞，面板显示为副操作）
    let summary_due = chapters
        .iter()
        .rev()
        .find(|c| {
            c["body"].as_str() == Some("approved") && !c["summary"].as_bool().unwrap_or(false)
        })
        .map(|c| c["n"].clone());
    json!({
        "summaryDue": summary_due,
        "hasOutline": outline_file.is_some(),
        "outlineFile": outline_file,
        "hasSetup": has_setup,
        "chapters": chapters,
        "next": next,
        "counts": {
            "total": chapters.len(),
            "approved": chapters.iter().filter(|c| c["body"].as_str() == Some("approved")).count(),
            "pending": chapters.iter().filter(|c| c["body"].as_str() == Some("pending")).count(),
        },
    })
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
}
