//! 聊天产出落盘（从 stream/mod.rs 原样搬出，为手动线腾出 stream/mod.rs 行预算）。
//! 语义、路径、行为与原实现逐字一致；仅把 `super::register_review_queue` 改为绝对路径，
//! 并把 is_chapter_head / parse_chapter_title 通过 `use super::` 引入。
use super::{is_chapter_head, parse_chapter_title};
use anyhow::Result;
use molan_core::files;

/// 聊天产出落盘（F07 / 契约 C）：正文一律进入「正文待审」审批队列，
/// 绝不覆盖已有正式稿或已有待审稿；细纲只新建不覆盖。
/// 只有真正写盘成功才计入 saved；任一写入失败返回 Err，绝不假成功。
/// 带「锁内前置校验」的落盘：`check` 在每次 AI 写盘进入 fs_lock 之后、写盘之前执行
/// （取消/依赖等），因此不存在 check/write 竞态，也不会与外部加锁形成死锁。
/// check 每个文件都会被调用一次，返回 Err 即拒绝写入。
pub(crate) fn auto_save_chat_output_checked<F>(
    db: &molan_core::db::Db,
    book_id: &str,
    full: &str,
    _explicit_ch: Option<i64>,
    suppress_body: bool,
    check: F,
) -> Result<(Vec<String>, Vec<String>)>
where
    F: Fn() -> Result<()>,
{
    let mut saved = Vec::new();
    let mut skipped = Vec::new();
    if book_id.is_empty() || full.chars().count() < 120 {
        return Ok((saved, skipped));
    }
    let lines: Vec<&str> = full.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let head = lines[i].trim_start();
        if !head.starts_with('#') {
            i += 1;
            continue;
        }
        let Some((num, is_outline)) = parse_chapter_title(head) else {
            i += 1;
            continue;
        };
        let mut j = i + 1;
        let mut body = String::new();
        while j < lines.len() && !is_chapter_head(lines[j]) {
            body.push_str(lines[j]);
            body.push('\n');
            j += 1;
        }
        let body = body.trim().to_string();
        i = j;
        if body.chars().count() < 150 {
            continue;
        }
        if is_outline {
            let fname = format!("细纲_第{}章.md", num);
            if files::read_file(db, book_id, "细纲", &fname).is_some() {
                skipped.push(format!("细纲 / {} 已存在，未覆盖", fname));
                continue;
            }
            // 细纲也是 AI 产稿：走 checked（只新建、锁内校验、locked 拒绝）
            if let Err(e) = files::write_ai_file_checked(db, book_id, "细纲", &fname, &body, &check)
            {
                skipped.push(format!("细纲 / {} 未保存：{}", fname, e));
                continue;
            }
            // 章节状态机（P0-2）：只观测不阻断，记账失败不影响已落盘文件
            let _ = molan_core::chapter_state::record_outline(
                db,
                book_id,
                num,
                &molan_core::continuity::content_hash(&body),
            );
            saved.push(format!("细纲 / {}", fname));
        } else if suppress_body {
            // 细纲/大纲指令的产出绝不进正文组：正文段整段跳过（全链路 S2 回归锁）
            tracing::info!("细纲指令产出按 suppress_body 跳过正文落盘（第{}章）", num);
            skipped.push(format!(
                "第{}章正文段按细纲/大纲指令跳过（不进正文组）；如需保留请点击「保存到书籍目录」",
                num
            ));
            continue;
        } else {
            // 正文只能生成待审稿：新章也需作者明确接受后才进入正式正文。
            // 待审已存在则拒绝，绝不覆盖正式稿或已有待审稿。
            let fname = format!("第{}章.md", num);
            if files::read_file(db, book_id, molan_core::db::REVIEW_GROUP, &fname).is_some() {
                skipped.push(format!("第{}章已有待审稿，未覆盖；请先处理审批队列", num));
                continue;
            }
            // 锁内 check（取消/依赖）→ 只新建写盘 → 待审登记 → 章节状态，全部在同一把锁内（chapter_commit）
            if let Err(e) = molan_core::chapter_commit::submit_pending(
                db,
                book_id,
                num,
                &body,
                "chat_save",
                &check,
            ) {
                skipped.push(format!("第{}章待审：{}", num, e));
                continue;
            }
            saved.push(format!("{} / {}", molan_core::db::REVIEW_GROUP, fname));
        }
    }
    Ok((saved, skipped))
}
