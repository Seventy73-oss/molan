//! Agent 工具传输层：tool_calls 分片聚合、完成性判定与请求体装配。
//!
//! 该模块只被显式开启 StreamOpts::accept_tool_calls 的调用方使用。
//! 既有正文写作路径（accept_tool_calls=false）语义完全不变：
//! finish_reason=tool_calls 仍然判为不完整，SSE 里的 tool_calls 分片被静默丢弃。
//!
//! 设计原则（对齐 HANDOFF 附录 A2）：
//! - 不能把 tool_calls 加入通用成功白名单，否则「正文写作时上游意外返回工具调用」
//!   会被当成完整书稿；
//! - 工具调用分片按 index 合并，id / function.name 只许首个非空值，后续冲突即报错；
//!   function.arguments 按序拼接，流结束时必须能解析为合法 JSON；
//! - 非流式 message.tool_calls 做同样的校验；
//! - mock:// 渠道提供确定性工具协议，便于离线端到端测试。

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

use crate::{is_incomplete_err, CompletionState, LlmEvent, StreamOpts, INCOMPLETE_MSG};

/// 完成性判定（带工具调用开关）。语义与 crate::classify_completion 完全一致，
/// 唯一的额外分支：accept_tool_calls=true 且 finish_reason == "tool_calls" 时
/// 返回 CompletionState::ToolCalls。
///
/// 注意：即便开启工具调用，length / content_filter / 裸 EOF / 白名单外未知原因
/// 仍然一律报不完整——工具参数不完整时绝不允许调用工具。
pub fn classify_completion_opts(
    saw_done: bool,
    finish_reason: Option<&str>,
    allow_missing: bool,
    accept_tool_calls: bool,
) -> Result<CompletionState> {
    let fr = finish_reason.map(str::trim).filter(|s| !s.is_empty());
    if accept_tool_calls && fr == Some("tool_calls") {
        return Ok(CompletionState::ToolCalls);
    }
    match fr {
        Some("length") => Err(anyhow!(
            "{} 模型输出达到长度上限被截断（finish_reason=length），内容不完整",
            INCOMPLETE_MSG
        )),
        Some("content_filter") => Err(anyhow!(
            "{} 上游内容被安全策略过滤（finish_reason=content_filter）",
            INCOMPLETE_MSG
        )),
        Some(fr) if crate::ACCEPTED_FINISH_REASONS.contains(&fr) => {
            Ok(if saw_done {
                CompletionState::Done
            } else {
                CompletionState::FinishStop
            })
        }
        Some(fr) => Err(anyhow!(
            "{} 上游以 finish_reason={} 结束（非正常收尾，可能是工具调用/未知状态），不可作为完整正文",
            INCOMPLETE_MSG,
            fr
        )),
        None if saw_done => Ok(CompletionState::Done),
        None if allow_missing => Ok(CompletionState::Unverified),
        None => Err(anyhow!(
            "{} 上游在给出 [DONE]/finish_reason 之前就结束了，无法确认内容完整",
            INCOMPLETE_MSG
        )),
    }
}

/// 请求体装配：仅当调用方显式给出 tools 时才注入 OpenAI 兼容的 tools / tool_choice。
/// 未开启工具模式的旧路径（tools=None）请求体一个字节都不变。
pub fn apply_tools(body: &mut Value, opts: &StreamOpts) {
    if let Some(tools) = &opts.tools {
        body["tools"] = tools.clone();
        if let Some(tool_choice) = &opts.tool_choice {
            body["tool_choice"] = tool_choice.clone();
        }
    }
}

/// 单个工具调用分片的累积状态。
#[derive(Debug, Default, Clone)]
struct PartialToolCall {
    index: u64,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// 按 index 聚合流式 tool_calls 分片。
///
/// 不变量：
/// - id / function.name 取首个非空值；后续帧给出不同非空值即报错（协议冲突）；
/// - function.arguments 按帧到达顺序拼接，不做任何 trim；
/// - 只有 finalize 传入 accept=true 且确有分片时才产出结果。
#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    calls: Vec<PartialToolCall>,
}

impl ToolCallAccumulator {
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// 吞掉 delta.tool_calls 数组里的每一个分片（accept=false 时静默忽略，维持旧行为）。
    pub fn collect_delta(&mut self, delta: &Value, accept_tool_calls: bool) -> Result<()> {
        if !accept_tool_calls {
            return Ok(());
        }
        if let Some(arr) = delta["tool_calls"].as_array() {
            for call in arr {
                self.push(call)?;
            }
        }
        Ok(())
    }

    /// 合并单个工具调用分片。
    pub fn push(&mut self, call: &Value) -> Result<()> {
        let index = call["index"].as_u64().unwrap_or(0);
        if !self.calls.iter().any(|c| c.index == index) {
            self.calls.push(PartialToolCall {
                index,
                ..Default::default()
            });
        }
        let entry = self
            .calls
            .iter_mut()
            .find(|c| c.index == index)
            .expect("just inserted");

        if let Some(id) = call["id"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
            match &entry.id {
                Some(prev) if prev != id => {
                    return Err(anyhow!(
                        "{} 上游工具调用 id 分片冲突（先 {} 后 {}），协议已损坏",
                        INCOMPLETE_MSG,
                        prev,
                        id
                    ));
                }
                Some(_) => {}
                None => entry.id = Some(id.to_string()),
            }
        }
        if let Some(name) = call["function"]["name"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match &entry.name {
                Some(prev) if prev != name => {
                    return Err(anyhow!(
                        "{} 上游工具调用函数名分片冲突（先 {} 后 {}），协议已损坏",
                        INCOMPLETE_MSG,
                        prev,
                        name
                    ));
                }
                Some(_) => {}
                None => entry.name = Some(name.to_string()),
            }
        }
        if let Some(args) = call["function"]["arguments"].as_str() {
            entry.arguments.push_str(args);
        }
        Ok(())
    }

    /// 流结束时收尾：
    /// - accept=false 或没有任何分片 -> Ok(None)（旧正文路径不受影响）；
    /// - 每个调用的 arguments 必须能解析为合法 JSON，否则 Err(INCOMPLETE)；
    /// - 返回按 index 升序排列的 OpenAI 兼容 tool_calls 数组。
    pub fn finalize(&self, accept_tool_calls: bool) -> Result<Option<Value>> {
        if !accept_tool_calls || self.calls.is_empty() {
            return Ok(None);
        }
        let mut ordered: Vec<&PartialToolCall> = self.calls.iter().collect();
        ordered.sort_by_key(|c| c.index);
        let mut out = Vec::with_capacity(ordered.len());
        for c in ordered {
            let raw = c.arguments.as_str();
            let args = if raw.trim().is_empty() { "{}" } else { raw };
            serde_json::from_str::<Value>(args).map_err(|e| {
                anyhow!(
                    "{} 工具调用 {} 的 arguments 不是合法 JSON（{}），拒绝当作完整工具调用",
                    INCOMPLETE_MSG,
                    c.name.as_deref().unwrap_or("?"),
                    e
                )
            })?;
            out.push(json!({
                "id": c.id.clone().unwrap_or_default(),
                "type": "function",
                "function": {
                    "name": c.name.clone().unwrap_or_default(),
                    "arguments": args,
                }
            }));
        }
        Ok(Some(Value::Array(out)))
    }
}

/// 把聚合好的工具调用作为单个 LlmEvent::ToolCalls 事件发出（不逐 delta 发）。
/// finish_reason=tool_calls 却没有任何分片时 fail-closed 报不完整。
pub fn emit_tool_calls(acc: &ToolCallAccumulator, tx: &UnboundedSender<LlmEvent>) -> Result<()> {
    match acc.finalize(true)? {
        Some(calls) => {
            let _ = tx.send(LlmEvent::ToolCalls(calls));
            Ok(())
        }
        None => Err(anyhow!(
            "{} 上游以 finish_reason=tool_calls 结束，但没有给出任何工具调用分片",
            INCOMPLETE_MSG
        )),
    }
}

/// 流结束收尾：
/// - state==ToolCalls -> 把聚合好的数组作为单个事件发出（空分片/非法参数报不完整）；
/// - accept=true 且确有分片但 finish_reason 不是 tool_calls -> 不调用工具，但参数
///   仍必须是合法 JSON（否则说明响应被截断/协议损坏，fail-closed 报不完整）。
pub fn finish_tool_calls(
    acc: &ToolCallAccumulator,
    state: CompletionState,
    accept_tool_calls: bool,
    tx: &UnboundedSender<LlmEvent>,
) -> Result<()> {
    if state == CompletionState::ToolCalls {
        return emit_tool_calls(acc, tx);
    }
    if accept_tool_calls && !acc.is_empty() {
        acc.finalize(true)?;
    }
    Ok(())
}

/// 从非流式 message 里解析 tool_calls：数组非空、每个调用有可解析的 arguments 才算数。
pub fn parse_message_tool_calls(message: &Value) -> Option<Value> {
    let arr = message["tool_calls"].as_array()?;
    if arr.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(arr.len());
    for c in arr {
        let name = c["function"]["name"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            return None;
        }
        let raw = c["function"]["arguments"].as_str().unwrap_or("");
        let args = if raw.trim().is_empty() { "{}" } else { raw };
        serde_json::from_str::<Value>(args).ok()?;
        out.push(json!({
            "id": c["id"].as_str().unwrap_or(""),
            "type": "function",
            "function": { "name": name, "arguments": args }
        }));
    }
    Some(Value::Array(out))
}

/// 非流式完成性判定 + tool_calls 提取。
/// 返回 (状态, 可选工具调用数组)；只有 accept=true 且 finish_reason=tool_calls
/// 且 message.tool_calls 校验通过时才返回 ToolCalls。
pub fn classify_nonstream_message(
    message: &Value,
    saw_done: bool,
    finish_reason: Option<&str>,
    allow_missing: bool,
    accept_tool_calls: bool,
) -> Result<(CompletionState, Option<Value>)> {
    let state =
        classify_completion_opts(saw_done, finish_reason, allow_missing, accept_tool_calls)?;
    if state != CompletionState::ToolCalls {
        return Ok((state, None));
    }
    match parse_message_tool_calls(message) {
        Some(calls) => Ok((CompletionState::ToolCalls, Some(calls))),
        None => Err(anyhow!(
            "{} 上游以 finish_reason=tool_calls 结束，但 message.tool_calls 缺失或 arguments 非法",
            INCOMPLETE_MSG
        )),
    }
}

/// 取最后一条 role=="user" 的消息文本；没有 role 标记时退回最后一条消息。
pub fn last_user_text(messages: &[Value]) -> &str {
    messages
        .iter()
        .rev()
        .find(|m| m["role"].as_str() == Some("user"))
        .or_else(|| messages.last())
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
}

/// mock:// 工具协议标记：[call:<工具名>]（工具名不含 ']'，取首个标记）。
pub fn mock_tool_call_name(text: &str) -> Option<String> {
    let start = text.find("[call:")?;
    let rest = &text[start + "[call:".len()..];
    let end = rest.find(']')?;
    let name = rest[..end].trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// mock:// 确定性工具协议：仅当调用方给了 tools 且最后一条 user 文本含
/// [call:<工具名>] 时，返回该工具的一次 tool_call
/// （id="mock-call-1"、arguments="{}"、finish_reason=tool_calls）。
/// 无标记则返回 None，走原有 mock 正文/JSON 分支。
pub fn mock_tool_calls(opts: &StreamOpts, last_user_text: &str) -> Option<Value> {
    opts.tools.as_ref()?;
    let name = mock_tool_call_name(last_user_text)?;
    Some(json!([{
        "id": "mock-call-1",
        "type": "function",
        "function": { "name": name, "arguments": "{}" }
    }]))
}

/// 判断错误是否为不完整（给测试与调用方一个稳定入口）。
pub fn is_incomplete(e: &anyhow::Error) -> bool {
    is_incomplete_err(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- classify_completion_opts 矩阵 ----

    #[test]
    fn opts_accept_false_tool_calls_is_incomplete() {
        // 回归锁：既有正文路径必须把 tool_calls 当不完整
        let e = classify_completion_opts(true, Some("tool_calls"), false, false).unwrap_err();
        assert!(crate::is_incomplete_err(&e));
    }

    #[test]
    fn opts_accept_true_tool_calls_is_ok() {
        assert_eq!(
            classify_completion_opts(true, Some("tool_calls"), false, true).unwrap(),
            CompletionState::ToolCalls
        );
        assert_eq!(
            classify_completion_opts(false, Some("tool_calls"), false, true).unwrap(),
            CompletionState::ToolCalls
        );
    }

    #[test]
    fn opts_accept_true_stop_is_text_completion() {
        assert_eq!(
            classify_completion_opts(true, Some("stop"), false, true).unwrap(),
            CompletionState::Done
        );
        assert_eq!(
            classify_completion_opts(false, Some("stop"), false, true).unwrap(),
            CompletionState::FinishStop
        );
    }

    #[test]
    fn opts_accept_true_length_and_filter_still_incomplete() {
        assert!(crate::is_incomplete_err(
            &classify_completion_opts(true, Some("length"), true, true).unwrap_err()
        ));
        assert!(crate::is_incomplete_err(
            &classify_completion_opts(false, Some("content_filter"), true, true).unwrap_err()
        ));
        assert!(crate::is_incomplete_err(
            &classify_completion_opts(false, None, false, true).unwrap_err()
        ));
        assert_eq!(
            classify_completion_opts(false, None, true, true).unwrap(),
            CompletionState::Unverified
        );
    }

    // ---- SSE 分片重组 ----

    #[test]
    fn sse_arguments_split_across_three_frames_reassemble() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(&json!({"index": 0, "id": "call_1", "function": {"name": "lookup", "arguments": "{\"city\":\"北"}}))
            .unwrap();
        acc.push(&json!({"index": 0, "function": {"arguments": "京\",\"days\":"}}))
            .unwrap();
        acc.push(&json!({"index": 0, "function": {"arguments": "3}"}}))
            .unwrap();
        let out = acc.finalize(true).unwrap().expect("aggregated");
        assert_eq!(out[0]["id"], json!("call_1"));
        assert_eq!(out[0]["function"]["name"], json!("lookup"));
        let parsed: Value =
            serde_json::from_str(out[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(parsed, json!({"city": "北京", "days": 3}));
    }

    #[test]
    fn sse_multiple_tool_calls_grouped_by_index() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(
            &json!({"index": 0, "id": "call_a", "function": {"name": "a", "arguments": "{\"x\":"}}),
        )
        .unwrap();
        acc.push(
            &json!({"index": 1, "id": "call_b", "function": {"name": "b", "arguments": "{\"y\":"}}),
        )
        .unwrap();
        acc.push(&json!({"index": 0, "function": {"arguments": "1}"}}))
            .unwrap();
        acc.push(&json!({"index": 1, "function": {"arguments": "2}"}}))
            .unwrap();
        let out = acc.finalize(true).unwrap().expect("aggregated");
        assert_eq!(out.as_array().unwrap().len(), 2);
        assert_eq!(out[0]["function"]["name"], json!("a"));
        assert_eq!(out[1]["function"]["name"], json!("b"));
        assert_eq!(out[0]["function"]["arguments"], json!("{\"x\":1}"));
        assert_eq!(out[1]["function"]["arguments"], json!("{\"y\":2}"));
    }

    #[test]
    fn sse_invalid_json_arguments_is_incomplete() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(
            &json!({"index": 0, "id": "c", "function": {"name": "f", "arguments": "{\"a\":"}}),
        )
        .unwrap();
        let e = acc.finalize(true).unwrap_err();
        assert!(crate::is_incomplete_err(&e));
    }

    #[test]
    fn sse_id_conflict_is_error() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(
            &json!({"index": 0, "id": "call_a", "function": {"name": "f", "arguments": "{}"}}),
        )
        .unwrap();
        let e = acc
            .push(&json!({"index": 0, "id": "call_b", "function": {"arguments": ""}}))
            .unwrap_err();
        assert!(crate::is_incomplete_err(&e));
    }

    #[test]
    fn sse_name_conflict_is_error() {
        let mut acc = ToolCallAccumulator::default();
        acc.push(&json!({"index": 0, "id": "c", "function": {"name": "a", "arguments": "{}"}}))
            .unwrap();
        let e = acc
            .push(&json!({"index": 0, "function": {"name": "b", "arguments": ""}}))
            .unwrap_err();
        assert!(crate::is_incomplete_err(&e));
    }

    #[test]
    fn non_accept_mode_drops_tool_calls() {
        let mut acc = ToolCallAccumulator::default();
        acc.collect_delta(
            &json!({"tool_calls": [{"index": 0, "id": "c", "function": {"name": "f", "arguments": "{}"}}]}),
            false,
        )
        .unwrap();
        assert!(acc.is_empty());
        assert!(acc.finalize(false).unwrap().is_none());
        // 即使误传 accept=true，只要 collect 阶段被忽略，也拿不到工具调用
        assert!(acc.finalize(true).unwrap().is_none());
    }

    #[test]
    fn emit_tool_calls_sends_single_event() {
        let mut acc = ToolCallAccumulator::default();
        acc.collect_delta(
            &json!({"tool_calls": [{"index": 0, "id": "c", "function": {"name": "f", "arguments": "{}"}}]}),
            true,
        )
        .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        emit_tool_calls(&acc, &tx).unwrap();
        drop(tx);
        match rx.try_recv().unwrap() {
            LlmEvent::ToolCalls(v) => assert_eq!(v[0]["id"], json!("c")),
            other => panic!("expected ToolCalls, got {:?}", other),
        }
        // 无分片时 fail-closed
        let empty = ToolCallAccumulator::default();
        let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel();
        assert!(crate::is_incomplete_err(
            &emit_tool_calls(&empty, &tx2).unwrap_err()
        ));
    }

    // ---- 非流式 message.tool_calls ----

    #[test]
    fn nonstream_tool_calls_accept_on_and_off() {
        let message = json!({"content": null, "tool_calls": [
            {"id": "call_1", "type": "function", "function": {"name": "f", "arguments": "{\"a\":1}"}}
        ]});
        let (state, calls) =
            classify_nonstream_message(&message, false, Some("tool_calls"), false, true).unwrap();
        assert_eq!(state, CompletionState::ToolCalls);
        assert_eq!(calls.unwrap()[0]["function"]["name"], json!("f"));

        let e = classify_nonstream_message(&message, false, Some("tool_calls"), false, false)
            .unwrap_err();
        assert!(crate::is_incomplete_err(&e));

        // finish_reason=tool_calls 但缺 tool_calls 数组：accept 开也必须不完整
        let empty = json!({"content": null});
        let e =
            classify_nonstream_message(&empty, false, Some("tool_calls"), false, true).unwrap_err();
        assert!(crate::is_incomplete_err(&e));

        // arguments 非法同样 fail-closed
        let bad =
            json!({"tool_calls": [{"id": "c", "function": {"name": "f", "arguments": "{oops"}}]});
        let e =
            classify_nonstream_message(&bad, false, Some("tool_calls"), false, true).unwrap_err();
        assert!(crate::is_incomplete_err(&e));

        // 普通文本完成不受影响
        let (state, calls) =
            classify_nonstream_message(&json!({"content": "hi"}), true, Some("stop"), false, true)
                .unwrap();
        assert_eq!(state, CompletionState::Done);
        assert!(calls.is_none());
    }

    #[test]
    fn parse_message_tool_calls_rejects_non_array() {
        assert!(parse_message_tool_calls(&json!({"content": "x"})).is_none());
        assert!(parse_message_tool_calls(&json!({"tool_calls": []})).is_none());
        assert!(parse_message_tool_calls(&json!({"tool_calls": "nope"})).is_none());
        assert!(parse_message_tool_calls(
            &json!({"tool_calls": [{"id": "c", "function": {"name": "f", "arguments": "{}"}}]})
        )
        .is_some());
    }

    // ---- 请求体装配 ----

    #[test]
    fn apply_tools_only_when_present() {
        let mut body = json!({"model": "m"});
        apply_tools(&mut body, &StreamOpts::default());
        assert!(body.get("tools").is_none());

        let opts = StreamOpts {
            tools: Some(json!([{"type": "function"}])),
            tool_choice: Some(json!("auto")),
            accept_tool_calls: true,
            ..Default::default()
        };
        apply_tools(&mut body, &opts);
        assert_eq!(body["tools"][0]["type"], json!("function"));
        assert_eq!(body["tool_choice"], json!("auto"));
    }

    // ---- mock:// 确定性工具协议 ----

    #[test]
    fn mock_tool_protocol_is_deterministic() {
        let opts = StreamOpts {
            tools: Some(json!([])),
            accept_tool_calls: true,
            ..Default::default()
        };
        assert!(mock_tool_calls(&opts, "请调用 [call:lookup] 查询").is_some());
        let calls = mock_tool_calls(&opts, "请调用 [call:lookup] 查询").unwrap();
        assert_eq!(calls[0]["id"], json!("mock-call-1"));
        assert_eq!(calls[0]["function"]["name"], json!("lookup"));
        assert_eq!(calls[0]["function"]["arguments"], json!("{}"));
        // 无标记 -> None，走原有 mock 分支
        assert!(mock_tool_calls(&opts, "写一章正文").is_none());
        // 未提供 tools -> 即便有标记也不启用工具协议
        assert!(mock_tool_calls(&StreamOpts::default(), "[call:lookup]").is_none());
        // 空工具名不认
        assert!(mock_tool_calls(&opts, "[call:]").is_none());
    }

    #[test]
    fn finish_tool_calls_validates_fragments_without_tool_calls_finish() {
        // finish_reason=stop 却带非法工具分片：不发事件，但必须 fail-closed
        let mut acc = ToolCallAccumulator::default();
        acc.push(&json!({"index": 0, "id": "c", "function": {"name": "f", "arguments": "{bad"}}))
            .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let e = finish_tool_calls(&acc, CompletionState::Done, true, &tx).unwrap_err();
        assert!(crate::is_incomplete_err(&e));
        assert!(rx.try_recv().is_err());

        // 合法参数则静默通过（不把工具调用当正文，也不发 ToolCalls 事件）
        let mut ok = ToolCallAccumulator::default();
        ok.push(&json!({"index": 0, "id": "c", "function": {"name": "f", "arguments": "{}"}}))
            .unwrap();
        finish_tool_calls(&ok, CompletionState::Done, true, &tx).unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn last_user_text_prefers_latest_user_role() {
        let msgs = vec![
            json!({"role": "user", "content": "旧的 [call:a]"}),
            json!({"role": "assistant", "content": "回复"}),
            json!({"role": "user", "content": "新的 [call:b]"}),
        ];
        assert_eq!(last_user_text(&msgs), "新的 [call:b]");
        // 无 role 字段时退回最后一条
        let raw = vec![json!({"content": "无 role"})];
        assert_eq!(last_user_text(&raw), "无 role");
        assert_eq!(last_user_text(&[]), "");
    }

    // ---- 端到端：mock:// 流式工具协议 ----

    #[tokio::test]
    async fn mock_stream_emits_tool_calls_event_and_state() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let params = crate::ChatParams {
            base_url: "mock://".to_string(),
            messages: vec![
                json!({"role": "system", "content": "你是助手"}),
                json!({"role": "user", "content": "帮我 [call:read_file] 看看"}),
            ],
            ..Default::default()
        };
        let opts = StreamOpts {
            tools: Some(json!([{"type": "function", "function": {"name": "read_file"}}])),
            accept_tool_calls: true,
            ..Default::default()
        };
        let state = crate::chat_completion_stream_opts(params, &tx, opts.clone())
            .await
            .unwrap();
        assert_eq!(state, CompletionState::ToolCalls);
        drop(tx);
        let mut got: Option<Value> = None;
        while let Ok(ev) = rx.try_recv() {
            if let LlmEvent::ToolCalls(v) = ev {
                got = Some(v);
            }
        }
        let calls = got.expect("ToolCalls event");
        assert_eq!(calls[0]["id"], json!("mock-call-1"));
        assert_eq!(calls[0]["function"]["name"], json!("read_file"));

        // 无标记 -> 仍走原有 mock 正文分支，Done 且不发 ToolCalls
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        let params2 = crate::ChatParams {
            base_url: "mock://".to_string(),
            messages: vec![json!({"role": "user", "content": "写一章正文"})],
            ..Default::default()
        };
        let state2 = crate::chat_completion_stream_opts(params2, &tx2, opts)
            .await
            .unwrap();
        assert_eq!(state2, CompletionState::Done);
        drop(tx2);
        let mut text = String::new();
        let mut saw_tool = false;
        while let Ok(ev) = rx2.try_recv() {
            match ev {
                LlmEvent::Delta(d) => text.push_str(&d),
                LlmEvent::ToolCalls(_) => saw_tool = true,
                _ => {}
            }
        }
        assert!(!text.is_empty());
        assert!(!saw_tool);
    }
}

/// 真实 SSE 解析路径的集成测试：直接驱动 lib.rs 的 handle_sse_line。
#[cfg(test)]
mod sse_integration_tests {
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
}
