//! ContextBuilder：按任务与目标组装本次上下文，所有入口共用。
//!
//! 规则：
//! - 聊天只带最小必要上下文（作品概况 + 作者显式附上的资料），不自动塞全书；
//! - 章节类任务（剧情/细纲/正文）沿用 `auto_book_context_for_chapter`（已定稿记忆、前章结尾、
//!   目标细纲）+ 目标细纲补注（已修复两种标题格式导致的重复注入）；
//! - 修改/去味/审稿/总结类任务以「目标文本」为核心：选区必须与基线一致（服务端按 UTF-16 偏移
//!   重新切片，不信任客户端文本），原文已变即阻塞，绝不按旧选区生成；
//! - 截断与遗漏逐块记录（chars/omitted/hash），必要块放不下时阻塞并要求缩小范围；
//! - aiOff 文件一律不读。
use molan_core::continuity::content_hash;
use molan_core::db::Db;
use molan_core::doc_write;
use molan_core::files;
use molan_core::task_kind::TaskKind;
use serde_json::{json, Value};

/// 单个显式资料上限 / 显式资料总上限 / 目标文本上限（字符）。
const FILE_CAP: usize = 6000;
const FILES_TOTAL: usize = 16000;
const TARGET_CAP: usize = 30000;

pub(crate) struct Built {
    pub text: String,
    pub blocks: Vec<Value>,
    pub blockers: Vec<String>,
    pub ch: i64,
}

pub(crate) fn target_ch(target: &Value) -> i64 {
    target["ch"]
        .as_i64()
        .or_else(|| {
            target["name"]
                .as_str()
                .and_then(crate::handlers::chapter_num_from_name)
        })
        .filter(|c| (1..=1_000_000).contains(c))
        .unwrap_or(0)
}

fn block(label: &str, source: &str, text: &str, omitted: usize, required: bool) -> Value {
    json!({"label": label, "source": source, "chars": text.chars().count(), "hash": content_hash(text),
           "truncated": omitted > 0, "omitted": omitted, "required": required})
}

fn clip(text: &str, cap: usize) -> (String, usize) {
    let total = text.chars().count();
    if total <= cap {
        (text.to_string(), 0)
    } else {
        (text.chars().take(cap).collect(), total - cap)
    }
}

/// 作者显式附带的资料（{group,name} 或纯文件名）。截断与跳过逐项记录。
fn explicit_files(db: &Db, book: &str, files_v: &Value, out: &mut Built) {
    let mut used = 0usize;
    for f in files_v.as_array().into_iter().flatten() {
        let name = f["name"]
            .as_str()
            .or_else(|| f.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            continue;
        }
        let group = f["group"]
            .as_str()
            .map(str::to_string)
            .or_else(|| super::stage_context::resolve_file_group(db, book, &name))
            .unwrap_or_else(|| "设定".into());
        let label = format!("资料：{}/{}", group, name);
        if files::file_flag(db, book, &group, &name, "aiOff") {
            out.blocks.push(json!({"label": label, "source": "file", "skipped": "aiOff（作者设为 AI 不可见）", "chars": 0}));
            continue;
        }
        let Some(content) = files::read_file(db, book, &group, &name) else {
            out.blocks.push(
                json!({"label": label, "source": "file", "skipped": "文件不存在", "chars": 0}),
            );
            continue;
        };
        let room = FILES_TOTAL.saturating_sub(used).min(FILE_CAP);
        if room == 0 {
            out.blocks.push(json!({"label": label, "source": "file", "skipped": "超出资料总预算，未注入", "chars": 0}));
            continue;
        }
        let (taken, omitted) = clip(&content, room);
        used += taken.chars().count();
        let note = if omitted > 0 {
            format!(
                "（已截断：注入{}字，省略{}字）",
                taken.chars().count(),
                omitted
            )
        } else {
            String::new()
        };
        out.text
            .push_str(&format!("\n\n【{}{}】\n{}", label, note, taken));
        out.blocks.push(block(
            &label,
            &format!("file:{}/{}", group, name),
            &taken,
            omitted,
            false,
        ));
    }
}

/// 目标文本（文档或选区）。选区按基线 hash 与 UTF-16 偏移在服务端重新切片。
fn target_text(db: &Db, book: &str, task: TaskKind, target: &Value, out: &mut Built) {
    let (group, name) = (
        target["group"].as_str().unwrap_or(""),
        target["name"].as_str().unwrap_or(""),
    );
    if name.is_empty() {
        if matches!(
            task,
            TaskKind::Revise | TaskKind::Humanize | TaskKind::Review | TaskKind::Summary
        ) && target["text"].as_str().is_none()
        {
            out.blockers.push(format!(
                "「{}」任务需要目标文本：请打开文档或选中片段",
                task.label()
            ));
        }
        if let Some(t) = target["text"].as_str().filter(|t| !t.trim().is_empty()) {
            let (taken, omitted) = clip(t, TARGET_CAP);
            out.text
                .push_str(&format!("\n\n【待处理文本（作者粘贴）】\n{}", taken));
            out.blocks
                .push(block("待处理文本", "inline", &taken, omitted, true));
        }
        return;
    }
    if files::file_flag(db, book, group, name, "aiOff") {
        out.blockers
            .push(format!("{}/{} 已设为 AI 不可见，不能作为目标", group, name));
        return;
    }
    let doc = match doc_write::read_doc(db, book, group, name) {
        Ok(d) if d["exists"] == json!(true) => d,
        _ => {
            out.blockers
                .push(format!("目标文档不存在：{}/{}", group, name));
            return;
        }
    };
    let content = doc["content"].as_str().unwrap_or("");
    if let Some(base) = target["baseHash"].as_str().filter(|b| !b.is_empty()) {
        if doc["hash"].as_str() != Some(base) {
            out.blockers
                .push("原文在发起前已被修改（基线不一致），请保存或刷新后重新选择".into());
            return;
        }
    }
    let label_doc = format!("{}/{}", group, name);
    if let (Some(s), Some(e)) = (target["start"].as_u64(), target["end"].as_u64()) {
        let (bs, be) = match (
            doc_write::utf16_to_byte(content, s as usize),
            doc_write::utf16_to_byte(content, e as usize),
        ) {
            (Some(a), Some(b)) if a < b => (a, b),
            _ => {
                out.blockers
                    .push("选区范围无效（越界或落在多字节字符中间）".into());
                return;
            }
        };
        let sel = &content[bs..be];
        if let Some(t) = target["selectionText"].as_str() {
            if t != sel {
                out.blockers
                    .push("选区文本与服务端原文不一致，请重新选择".into());
                return;
            }
        }
        let before: String = content[..bs]
            .chars()
            .rev()
            .take(1500)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let after: String = content[be..].chars().take(800).collect();
        let (sel_t, omitted) = clip(sel, TARGET_CAP);
        if omitted > 0 {
            out.blockers.push(format!(
                "选区过长（{}字），超过单次处理上限 {} 字，请缩小选区",
                sel.chars().count(),
                TARGET_CAP
            ));
            return;
        }
        out.text.push_str(&format!(
            "\n\n【选区所在文档】{}\n【选区前文（仅供承接，不要改写）】\n{}\n【待修改选区（只输出替换这一段的新文本）】\n{}\n【选区后文（仅供承接，不要改写）】\n{}",
            label_doc, before, sel_t, after
        ));
        out.blocks.push(block(
            &format!("选区：{}", label_doc),
            &format!("selection:{}", label_doc),
            &sel_t,
            0,
            true,
        ));
        out.blocks.push(block(
            "选区前后文",
            "selection-context",
            &format!("{}{}", before, after),
            0,
            false,
        ));
        return;
    }
    let (taken, omitted) = clip(content, TARGET_CAP);
    if omitted > 0 && matches!(task, TaskKind::Revise | TaskKind::Humanize) {
        out.blockers.push(format!(
            "目标文档 {} 共 {} 字，超过整篇改写上限 {} 字；请选中片段处理",
            label_doc,
            content.chars().count(),
            TARGET_CAP
        ));
        return;
    }
    let note = if omitted > 0 {
        format!(
            "（超长：注入前{}字，省略{}字；结论只针对已注入部分）",
            taken.chars().count(),
            omitted
        )
    } else {
        String::new()
    };
    out.text.push_str(&format!(
        "\n\n【目标文档：{}{}】\n{}",
        label_doc, note, taken
    ));
    out.blocks.push(block(
        &format!("目标文档：{}", label_doc),
        &format!("file:{}", label_doc),
        &taken,
        omitted,
        true,
    ));
}

/// 组装上下文。返回的 `text` 放进系统提示的【资料库上下文】；`blocks` 进上下文清单与界面。
pub(crate) fn build(db: &Db, book: &str, task: TaskKind, target: &Value, files_v: &Value) -> Built {
    let mut out = Built {
        text: String::new(),
        blocks: Vec::new(),
        blockers: Vec::new(),
        ch: target_ch(target),
    };
    let meta = db
        .q_json(
            "SELECT title, genre, pov FROM books WHERE id=?1",
            &[&book as &dyn rusqlite::ToSql],
        )
        .ok()
        .and_then(|r| r.into_iter().next())
        .unwrap_or(Value::Null);
    let overview = format!(
        "【作品概况】《{}》 题材：{} 视角：{}",
        meta["title"].as_str().unwrap_or(""),
        meta["genre"]
            .as_str()
            .filter(|g| !g.is_empty())
            .unwrap_or("未设定"),
        meta["pov"]
            .as_str()
            .filter(|g| !g.is_empty())
            .unwrap_or("未设定")
    );
    out.blocks
        .push(block("作品概况", "book", &overview, 0, false));
    out.text.push_str(&overview);
    let chapter_task = matches!(task, TaskKind::Plot | TaskKind::Outline | TaskKind::Body);
    if chapter_task && out.ch > 0 {
        let auto = super::auto_book_context_for_chapter(db, book, out.ch, false);
        if !auto.is_empty() {
            for b in molan_core::ctx_manifest::block_inventory(&auto)
                .as_array()
                .into_iter()
                .flatten()
            {
                out.blocks.push(json!({"label": b["label"], "source": "auto", "chars": b["chars"], "required": false}));
            }
            out.text.push_str("\n\n");
            out.text.push_str(&auto);
        }
        let before = out.text.len();
        let injected = super::stage_context::append_target_outline(db, book, out.ch, &mut out.text);
        if let Some(n) = injected {
            let added = out.text[before..].to_string();
            out.blocks.push(block(
                &format!("本章细纲：细纲/{}", n),
                &format!("file:细纲/{}", n),
                &added,
                0,
                task == TaskKind::Body,
            ));
        }
        let has_outline = out.text.contains(&format!("【本章细纲·第{}章", out.ch));
        if task == TaskKind::Body && !has_outline {
            out.blockers.push(format!(
                "第{}章没有可用细纲（不存在、为空或设为 AI 不可见）：请先生成并确认细纲",
                out.ch
            ));
        }
    } else if chapter_task {
        out.blocks.push(json!({"label": "章节记忆与前文", "source": "auto", "skipped": "未指定章号，未自动加载", "chars": 0}));
    }
    if !chapter_task && task != TaskKind::Chat {
        target_text(db, book, task, target, &mut out);
    }
    explicit_files(db, book, files_v, &mut out);
    out
}

/// 旧聊天入口（chat_stream）的附带资料：沿用旧预算（单文件 4000 / 合计 8000 字），
/// 但与新路径一致尊重「对 AI 隐藏」，并逐项报告截断 / 跳过 / 不存在（meta.contextFiles），不再静默。
pub(crate) fn legacy_context_text(db: &Db, book: &str, files_v: &Value) -> (String, Vec<Value>) {
    let mut chunks = Vec::new();
    let mut report = Vec::new();
    for f in files_v.as_array().into_iter().flatten() {
        // 兼容纯字符串条目（runNextChapter 等既有调用方传文件名）：按目录树解析所属组
        let name = f["name"].as_str().or_else(|| f.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let group = f["group"]
            .as_str()
            .or_else(|| f["groupDir"].as_str())
            .map(String::from)
            .or_else(|| super::stage_context::resolve_file_group(db, book, name))
            .unwrap_or_else(|| "设定".to_string());
        let file = format!("{}/{}", group, name);
        if files::file_flag(db, book, &group, name, "aiOff") {
            report.push(
                json!({"file": file, "status": "skipped", "reason": "作者设为 AI 不可见，未注入"}),
            );
            continue;
        }
        let Some(c) = files::read_file(db, book, &group, name) else {
            report.push(json!({"file": file, "status": "missing", "reason": "文件不存在"}));
            continue;
        };
        let n = c.chars().count();
        let status = if n > 4000 { "truncated" } else { "ok" };
        report.push(json!({"file": file, "status": status, "chars": n, "used": n.min(4000)}));
        chunks.push(format!(
            "# 资料：{}\n{}",
            name,
            c.chars().take(4000).collect::<String>()
        ));
    }
    let joined = chunks.join("\n\n");
    let total = joined.chars().count();
    if total > 8000 {
        report.push(
            json!({"file": "（合计）", "status": "truncated", "chars": total, "used": 8000,
                           "reason": "附带资料合计超过 8000 字，超出部分未注入"}),
        );
    }
    (joined.chars().take(8000).collect(), report)
}

/// 发起前预览：列出将要加载的上下文块（不返回全文）。`files` 与 agent_turn 的
/// `contextFiles` 同源，保证预览与执行按同一规则组装。
pub(crate) fn preview(db: &Db, book: &str, task: TaskKind, target: &Value, files: &Value) -> Value {
    let built = build(db, book, task, target, files);
    json!({"blocks": built.blocks, "blockers": built.blockers, "totalChars": built.text.chars().count(), "ch": built.ch})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Db, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), None).unwrap();
        let id = molan_core::books::create_book(&db, "书名", "玄幻", "第三人称")["id"]
            .as_str()
            .unwrap()
            .to_string();
        (dir, db, id)
    }

    #[test]
    fn chat_gets_minimal_context_only() {
        let (_d, db, b) = setup();
        files::write_file(&db, &b, "正文", "第1章.md", "正文内容不应出现").unwrap();
        let built = build(&db, &b, TaskKind::Chat, &json!({}), &json!([]));
        assert!(built.text.contains("《书名》"));
        assert!(!built.text.contains("正文内容不应出现"));
        assert!(built.blockers.is_empty());
    }

    #[test]
    fn body_outline_is_injected_exactly_once() {
        let (_d, db, b) = setup();
        files::write_file(&db, &b, "正文", "第1章.md", "第一章正文").unwrap();
        files::write_file(&db, &b, "细纲", "细纲_第2章.md", "第二章细纲节拍唯一标记").unwrap();
        let built = build(&db, &b, TaskKind::Body, &json!({"ch": 2}), &json!([]));
        assert_eq!(
            built.text.matches("第二章细纲节拍唯一标记").count(),
            1,
            "{}",
            built.text
        );
        assert!(built.blockers.is_empty(), "{:?}", built.blockers);
        let none = build(&db, &b, TaskKind::Body, &json!({"ch": 3}), &json!([]));
        assert!(none.blockers[0].contains("没有可用细纲"));
    }

    #[test]
    fn selection_is_resliced_and_base_checked() {
        let (_d, db, b) = setup();
        let text = "前文😀。选中这句。后文。";
        files::write_file(&db, &b, "设定", "a.md", text).unwrap();
        let s = doc_write::utf16_len("前文😀。");
        let e = s + doc_write::utf16_len("选中这句。");
        let t = json!({"group": "设定", "name": "a.md", "baseHash": content_hash(text), "start": s, "end": e, "selectionText": "选中这句。"});
        let built = build(&db, &b, TaskKind::Revise, &t, &json!([]));
        assert!(built.blockers.is_empty(), "{:?}", built.blockers);
        assert!(built
            .text
            .contains("【待修改选区（只输出替换这一段的新文本）】\n选中这句。"));
        let stale =
            json!({"group": "设定", "name": "a.md", "baseHash": "old", "start": s, "end": e});
        assert!(
            build(&db, &b, TaskKind::Revise, &stale, &json!([])).blockers[0].contains("基线不一致")
        );
        let wrong =
            json!({"group": "设定", "name": "a.md", "start": s, "end": e, "selectionText": "别的"});
        assert!(
            build(&db, &b, TaskKind::Revise, &wrong, &json!([])).blockers[0].contains("不一致")
        );
    }

    #[test]
    fn explicit_files_report_truncation_and_ai_off() {
        let (_d, db, b) = setup();
        files::write_file(&db, &b, "设定", "长.md", &"字".repeat(7000)).unwrap();
        files::write_file(&db, &b, "设定", "藏.md", "秘密").unwrap();
        files::set_file_flag(&db, &b, "设定", "藏.md", "aiOff", true).unwrap();
        let built = build(
            &db,
            &b,
            TaskKind::Chat,
            &json!({}),
            &json!([{"group": "设定", "name": "长.md"}, {"group": "设定", "name": "藏.md"}]),
        );
        let long = built
            .blocks
            .iter()
            .find(|x| x["label"] == "资料：设定/长.md")
            .unwrap();
        assert_eq!(long["omitted"], 1000);
        assert!(built.text.contains("省略1000字"));
        assert!(!built.text.contains("秘密"));
        assert!(built
            .blocks
            .iter()
            .any(|x| x["skipped"].as_str().unwrap_or("").contains("aiOff")));
    }

    #[test]
    fn revise_requires_target() {
        let (_d, db, b) = setup();
        let built = build(&db, &b, TaskKind::Revise, &json!({}), &json!([]));
        assert!(built.blockers[0].contains("需要目标文本"));
    }

    /// 旧聊天附带资料：旧预算不变，但隐藏文件不注入，截断 / 缺失逐项报告。
    #[test]
    fn legacy_context_files_are_reported_and_respect_ai_off() {
        let (_d, db, b) = setup();
        files::write_file(&db, &b, "设定", "长.md", &"甲".repeat(4500)).unwrap();
        files::write_file(&db, &b, "设定", "长2.md", &"乙".repeat(4500)).unwrap();
        files::write_file(&db, &b, "设定", "藏.md", "秘密内容").unwrap();
        files::set_file_flag(&db, &b, "设定", "藏.md", "aiOff", true).unwrap();
        let (text, report) = legacy_context_text(
            &db,
            &b,
            &json!([{"group": "设定", "name": "长.md"}, "长2.md", {"group": "设定", "name": "藏.md"}, {"group": "设定", "name": "无.md"}]),
        );
        assert!(!text.contains("秘密内容"), "对 AI 隐藏的文件不得注入");
        assert_eq!(text.chars().count(), 8000);
        let st: Vec<&str> = report
            .iter()
            .map(|r| r["status"].as_str().unwrap())
            .collect();
        assert_eq!(
            st,
            vec!["truncated", "truncated", "skipped", "missing", "truncated"]
        );
        assert_eq!(report[1]["file"], "设定/长2.md", "字符串条目按目录解析分组");
        assert_eq!(report[4]["file"], "（合计）");
    }
}
