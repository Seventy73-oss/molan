//! P2 阶段上下文契约（AGENT-PIPELINE-PLAN §C）。
//! 既有 auto_book_context 已注入：总纲/建书档案/人物/伏笔/信息差/已定稿章节记忆/最近正文结尾。
//! 本模块补上唯一缺口：**目标章细纲**（body 任务的核心依据），并修复 contextFiles
//! 纯字符串条目被静默丢弃的问题（runNextChapter 等既有调用方传的就是字符串名）。
use molan_core::files;
use serde_json::{json, Value};

/// 按文件名在目录树中解析所属组（contextFiles 字符串条目用）。
pub(crate) fn resolve_file_group(
    db: &molan_core::db::Db,
    book_id: &str,
    name: &str,
) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let tree = files::scan_tree(db, book_id);
    for g in tree.as_array()? {
        let dir = g["dir"].as_str()?;
        if g["files"]
            .as_array()?
            .iter()
            .any(|f| f["name"].as_str() == Some(name))
        {
            return Some(dir.to_string());
        }
    }
    None
}

/// 目标章的细纲文件名（细纲组内章号匹配，兼容 细纲_第N章.md / 第N章细纲.md）。
pub(crate) fn target_outline_name(
    db: &molan_core::db::Db,
    book_id: &str,
    ch: i64,
) -> Option<String> {
    let tree = files::scan_tree(db, book_id);
    let g = tree
        .as_array()?
        .iter()
        .find(|g| g["dir"].as_str() == Some("细纲"))?;
    g["files"]
        .as_array()?
        .iter()
        .filter_map(|f| f["name"].as_str())
        .find(|n| crate::handlers::chapter_num_from_name(n) == Some(ch))
        .map(String::from)
}

/// body/章节任务：把目标章细纲追加进上下文（已显式引用则去重；aiOff 尊重；空稿跳过）。
/// 返回实际注入的细纲文件名（供 meta 回显审计）。
pub(crate) fn append_target_outline(
    db: &molan_core::db::Db,
    book_id: &str,
    target_ch: i64,
    context: &mut String,
) -> Option<String> {
    if target_ch <= 0 {
        return None;
    }
    let name = target_outline_name(db, book_id, target_ch)?;
    if context.contains(&name) || files::file_flag(db, book_id, "细纲", &name, "aiOff") {
        return None;
    }
    let content = files::read_file(db, book_id, "细纲", &name)?;
    if content.trim().is_empty() {
        return None;
    }
    if !context.is_empty() {
        context.push_str("\n\n");
    }
    context.push_str(&format!(
        "【本章细纲·第{}章·{}（正文必须按此展开；要偏离先问作者）】\n{}",
        target_ch,
        name,
        content.chars().take(3000).collect::<String>()
    ));
    Some(name)
}

/// 面板预览：下一步动作将自动携带的上下文清单（只列真实存在/必然注入的项）。
/// memory_blocked=true（pipeline blockers 非空）时不得宣称「已定稿章节记忆」（F4 收尾）。
pub(crate) fn preview(
    db: &molan_core::db::Db,
    book_id: &str,
    next: &Value,
    memory_blocked: bool,
) -> Value {
    let tree = files::scan_tree(db, book_id);
    let has = |dir: &str, pred: &dyn Fn(&str) -> bool| -> Option<String> {
        tree.as_array()?
            .iter()
            .find(|g| g["dir"].as_str() == Some(dir))?["files"]
            .as_array()?
            .iter()
            .filter_map(|f| f["name"].as_str())
            .find(|n| pred(n))
            .map(String::from)
    };
    let mut out: Vec<Value> = Vec::new();
    if let Some(n) = has("设定", &|n| n.contains("大纲")) {
        out.push(json!({"label": "作者总纲", "file": n}));
    }
    if let Some(n) = has("设定", &|n| n == "建书档案.md" || n.contains("世界观")) {
        out.push(json!({"label": "全局设定", "file": n}));
    }
    if let Some(n) = has("设定", &|n| n.contains("人物")) {
        out.push(json!({"label": "人物资产", "file": n}));
    }
    if let Some(n) = has("设定", &|n| n.contains("伏笔")) {
        out.push(json!({"label": "伏笔台账", "file": n}));
    }
    let ch = next["chapter"].as_i64().unwrap_or(0);
    if ch > 1 {
        if memory_blocked {
            // 记忆未同步/失败：如实标注降级来源，不谎称注入已定稿记忆
            out.push(json!({"label": "原文结尾（降级：章节记忆未同步）", "file": null}));
        } else {
            out.push(json!({"label": "已定稿章节记忆（截至第N-1章）", "file": null}));
        }
        out.push(json!({"label": "最近正文结尾（衔接锚点）", "file": null}));
    }
    let stage = next["stage"].as_str().unwrap_or("");
    if (stage == "chapter_body" || stage == "outline_confirm") && ch > 0 {
        if let Some(n) = target_outline_name(db, book_id, ch) {
            // 手动线：outline_confirm 阶段明示「待确认」的正是这份细纲
            let label = if stage == "outline_confirm" {
                format!("本章细纲（待你确认）·第{}章", ch)
            } else {
                format!("本章细纲·第{}章", ch)
            };
            out.push(json!({"label": label, "file": n}));
        }
    }
    json!(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use molan_core::db::Db;

    fn setup() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let book = molan_core::books::create_book(&db, "测试", "悬疑", "第三人称");
        let bid = book["id"].as_str().unwrap().to_string();
        files::write_file(
            &db,
            &bid,
            "细纲",
            "第1章细纲.md",
            "第一章从调查现场开始。MARKER_OUTLINE",
        )
        .unwrap();
        files::write_file(&db, &bid, "设定", "建书档案.md", "测试世界观MARKER_SETUP").unwrap();
        (dir, db, bid)
    }

    #[test]
    fn string_context_files_resolve_group_and_inject() {
        let (_d, db, bid) = setup();
        // 既有缺陷回归：runNextChapter 传字符串文件名，曾经被静默丢弃
        let text = super::super::context_text(&db, &bid, &json!(["第1章细纲.md"]));
        assert!(
            text.contains("MARKER_OUTLINE"),
            "string entry must inject: {}",
            text
        );
    }

    #[test]
    fn target_outline_injected_once_and_deduped() {
        let (_d, db, bid) = setup();
        let mut ctx = String::new();
        assert_eq!(
            append_target_outline(&db, &bid, 1, &mut ctx).as_deref(),
            Some("第1章细纲.md")
        );
        assert!(ctx.contains("本章细纲·第1章"));
        // 已含该文件名（用户显式引用）→ 去重不再注入
        let mut ctx2 = ctx.clone();
        assert_eq!(append_target_outline(&db, &bid, 1, &mut ctx2), None);
        assert_eq!(ctx2, ctx);
    }

    #[test]
    fn no_chapter_or_missing_outline_injects_nothing() {
        let (_d, db, bid) = setup();
        let mut ctx = String::new();
        assert_eq!(append_target_outline(&db, &bid, 0, &mut ctx), None);
        assert_eq!(append_target_outline(&db, &bid, 7, &mut ctx), None);
        assert!(ctx.is_empty());
    }

    #[test]
    fn preview_lists_real_files_only() {
        let (_d, db, bid) = setup();
        let p = preview(
            &db,
            &bid,
            &json!({"stage": "chapter_body", "chapter": 1}),
            false,
        );
        let labels: Vec<String> = p
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["label"].as_str().unwrap().to_string())
            .collect();
        assert!(labels.iter().any(|l| l.contains("本章细纲")));
        assert!(labels.iter().any(|l| l == "全局设定"));
        assert!(
            !labels.iter().any(|l| l.contains("已定稿")),
            "第1章无前章记忆"
        );
    }

    #[test]
    fn preview_memory_blocked_never_claims_memory_label() {
        let (_d, db, bid) = setup();
        // 记忆被阻塞（failed/stale/pending）：不得宣称「已定稿章节记忆」，降级标注原文
        let p = preview(
            &db,
            &bid,
            &json!({"stage": "memory_fix", "chapter": 3}),
            true,
        );
        let labels: Vec<String> = p
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["label"].as_str().unwrap().to_string())
            .collect();
        assert!(
            !labels.iter().any(|l| l.contains("已定稿章节记忆")),
            "记忆阻塞时不得谎称注入记忆: {:?}",
            labels
        );
        assert!(labels.iter().any(|l| l.contains("降级")));
    }

    #[test]
    fn preview_memory_ok_keeps_memory_label() {
        let (_d, db, bid) = setup();
        let p = preview(
            &db,
            &bid,
            &json!({"stage": "chapter_outline", "chapter": 3}),
            false,
        );
        let labels: Vec<String> = p
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["label"].as_str().unwrap().to_string())
            .collect();
        assert!(labels.iter().any(|l| l.contains("已定稿章节记忆")));
    }
}
