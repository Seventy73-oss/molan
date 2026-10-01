use super::*;
use crate::handle_sse_line;
use serde_json::json;

fn frame(index: u64, id: Option<&str>, name: Option<&str>, args: &str) -> String {
    let mut call = json!({"index": index, "function": {"arguments": args}});
    if let Some(id) = id {
        call["id"] = json!(id);
    }
    if let Some(name) = name {
        call["function"]["name"] = json!(name);
    }
    format!(
        "data: {}",
        json!({"choices": [{"delta": {"tool_calls": [call]}, "finish_reason": null}]})
    )
}

// arguments 跨 4 帧拆分（含中文被切开），经 handle_sse_line 聚合后仍是合法 JSON。
#[test]
fn sse_lines_reassemble_arguments_and_finish_reason() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (mut usage, mut saw_done, mut fr) = (None, false, None);
    let mut acc = ToolCallAccumulator::default();
    let lines = [
        frame(0, Some("call_1"), Some("search"), "{\"q\":\"中"),
        frame(0, None, None, "文\",\"page\":"),
        frame(0, None, None, "2,"),
        frame(0, None, None, "\"fuzzy\":true}"),
    ];
    for l in &lines {
        handle_sse_line(l, &tx, &mut usage, &mut saw_done, &mut fr, &mut acc, true).unwrap();
    }
    // 结束帧带 finish_reason=tool_calls
    let end = format!(
        "data: {}",
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})
    );
    handle_sse_line(
        &end,
        &tx,
        &mut usage,
        &mut saw_done,
        &mut fr,
        &mut acc,
        true,
    )
    .unwrap();
    assert_eq!(fr.as_deref(), Some("tool_calls"));
    assert_eq!(
        classify_completion_opts(saw_done, fr.as_deref(), false, true).unwrap(),
        CompletionState::ToolCalls
    );
    let calls = acc.finalize(true).unwrap().expect("aggregated");
    assert_eq!(calls[0]["id"], json!("call_1"));
    assert_eq!(calls[0]["function"]["name"], json!("search"));
    let parsed: Value =
        serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(parsed, json!({"q": "中文", "page": 2, "fuzzy": true}));
}

// accept=false：同样的分片必须被静默丢弃，分类仍为不完整（回归锁）。
#[test]
fn sse_lines_drop_tool_calls_when_not_accepted() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (mut usage, mut saw_done, mut fr) = (None, false, None);
    let mut acc = ToolCallAccumulator::default();
    handle_sse_line(
        &frame(0, Some("call_1"), Some("search"), "{}"),
        &tx,
        &mut usage,
        &mut saw_done,
        &mut fr,
        &mut acc,
        false,
    )
    .unwrap();
    assert!(acc.is_empty());
    assert!(acc.finalize(true).unwrap().is_none());
    let e = classify_completion_opts(true, Some("tool_calls"), false, false).unwrap_err();
    assert!(crate::is_incomplete_err(&e));
}

// 分片冲突经真实解析路径也必须报不完整。
#[test]
fn sse_lines_id_conflict_surfaces_as_incomplete() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (mut usage, mut saw_done, mut fr) = (None, false, None);
    let mut acc = ToolCallAccumulator::default();
    handle_sse_line(
        &frame(0, Some("a"), Some("f"), "{}"),
        &tx,
        &mut usage,
        &mut saw_done,
        &mut fr,
        &mut acc,
        true,
    )
    .unwrap();
    let e = handle_sse_line(
        &frame(0, Some("b"), None, ""),
        &tx,
        &mut usage,
        &mut saw_done,
        &mut fr,
        &mut acc,
        true,
    )
    .unwrap_err();
    assert!(crate::is_incomplete_err(&e));
}
