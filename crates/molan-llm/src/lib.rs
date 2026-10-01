// molan-llm: 渠道管理 + OpenAI 兼容流式客户端（对齐 lib/llm.js）
pub mod agent_transport;
mod mock;
pub mod retry;
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};

use molan_core::db::Db;

/// 模块级 HTTP 单例：复用连接池与 TLS 会话。
/// 只限连接与读空闲，不设总超时——LLM 流式响应可能持续很久。
fn http_client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        let b = reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(15));
        // 读空闲 600s：网关不流式转发思考过程时，长思考阶段可能数分钟零字节，
        // 120s 会把「正在想」误判成断流→中断→产出不得落盘（用户痛点）。取消令牌仍可随时掐断。
        b.read_timeout(std::time::Duration::from_secs(600))
            .build()
            .expect("build http client")
    })
}

pub fn all_settings(db: &Db) -> (Vec<Value>, String) {
    let channels: Vec<Value> = {
        let rows = db
            .q_json("SELECT value FROM settings WHERE key='channels'", &[])
            .unwrap_or_default();
        rows.first()
            .and_then(|r| r["value"].as_str())
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
    };
    let active: String = {
        let rows = db
            .q_json("SELECT value FROM settings WHERE key='active_channel'", &[])
            .unwrap_or_default();
        rows.first()
            .and_then(|r| r["value"].as_str())
            .unwrap_or("")
            .to_string()
    };
    (channels, active)
}

pub fn channel_key(db: &Db, id: &str) -> String {
    let rows = db
        .q_json(
            "SELECT value FROM settings WHERE key='channel_key__' || ?1",
            &[&id],
        )
        .unwrap_or_default();
    rows.first()
        .and_then(|r| r["value"].as_str())
        .unwrap_or("")
        .to_string()
}

pub fn get_setting(db: &Db, key: &str) -> String {
    let rows = db
        .q_json("SELECT value FROM settings WHERE key=?1", &[&key])
        .unwrap_or_default();
    rows.first()
        .and_then(|r| r["value"].as_str())
        .unwrap_or("")
        .to_string()
}

pub fn active_channel(db: &Db) -> Option<Value> {
    let (channels, active_id) = all_settings(db);
    if channels.is_empty() {
        return None;
    }
    let mut ch = channels
        .iter()
        .find(|c| c["id"].as_str() == Some(active_id.as_str()))
        .cloned()
        .or_else(|| {
            channels
                .iter()
                .find(|c| c["builtin"] != json!(true))
                .cloned()
        })
        .or_else(|| channels.first().cloned())?;
    if ch["builtin"] == json!(true) {
        // 内置目录已反代：走自配渠道，模型跟随 default_model
        let direct = channels
            .iter()
            .find(|c| c["builtin"] != json!(true))?
            .clone();
        let dm = get_setting(db, "default_model");
        let mut out = direct.clone();
        apply_channel_model(&mut out, &dm);
        out["key"] = json!(channel_key(db, direct["id"].as_str().unwrap_or("")));
        out["builtin"] = json!(false);
        return Some(out);
    }
    apply_channel_model(&mut ch, "");
    let key = channel_key(db, ch["id"].as_str().unwrap_or(""));
    ch["key"] = json!(key);
    Some(ch)
}

pub fn has_any_key(db: &Db) -> bool {
    let (channels, _) = all_settings(db);
    channels.iter().any(|c| {
        c["builtin"] != json!(true) && !channel_key(db, c["id"].as_str().unwrap_or("")).is_empty()
    })
}

/// 从渠道的本地模型目录中选择可用模型。
///
/// 角色配置可能来自旧渠道或手工输入，不能无条件覆盖渠道当前模型；否则
/// 网关会在真正写作时返回 404。空目录表示渠道不提供可验证的模型清单，
/// 此时保留调用方选择，兼容自定义/延迟发现的渠道。
fn select_channel_model(channel: &Value, preferred: &str) -> Option<String> {
    let listed: Vec<&str> = channel["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .collect();
    if listed.is_empty() {
        let candidate = if !preferred.trim().is_empty() {
            preferred.trim()
        } else {
            channel["model"].as_str().unwrap_or("").trim()
        };
        return (!candidate.is_empty()).then(|| candidate.to_string());
    }
    if let Some(model) = listed.iter().find(|model| **model == preferred.trim()) {
        return Some((*model).to_string());
    }
    if let Some(model) = channel["model"]
        .as_str()
        .map(str::trim)
        .filter(|model| listed.iter().any(|known| known == model))
    {
        return Some(model.to_string());
    }
    listed.first().map(|model| (*model).to_string())
}

fn apply_channel_model(channel: &mut Value, preferred: &str) {
    if let Some(model) = select_channel_model(channel, preferred) {
        channel["model"] = json!(model);
    }
}

#[cfg(test)]
mod role_profile_tests;

/// 多智能体分工：distill（蒸馏/拆书）、outline（结构/大纲）、
/// chapter（正文）、review（审核/去味）、summary（总结）各自绑定渠道+模型。
/// settings 表 key = "agent_profile__<task>"，value = {"channelId","model"}。
/// 旧四角色配置键不变；新 distill 未配置时回落当前活跃渠道。
/// 未配置或渠道已删除时回落到当前活跃渠道——零配置也能跑。
pub fn resolve_agent_channel(db: &Db, task: &str) -> Option<Value> {
    let raw = get_setting(db, &format!("agent_profile__{}", task));
    let profile: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let want_id = profile["channelId"].as_str().unwrap_or("").to_string();
    let mut ch = if want_id.is_empty() {
        active_channel(db)?
    } else {
        let (channels, _) = all_settings(db);
        match channels
            .iter()
            .find(|c| c["id"].as_str() == Some(want_id.as_str()))
        {
            Some(c) if c["builtin"] != json!(true) => {
                let mut c = c.clone();
                c["key"] = json!(channel_key(db, &want_id));
                c
            }
            // 渠道不存在 / 是内置目录渠道：一律回落活跃渠道
            _ => active_channel(db)?,
        }
    };
    let preferred = if want_id.is_empty() || ch["id"].as_str() == Some(want_id.as_str()) {
        profile["model"].as_str().unwrap_or("")
    } else {
        "" // A deleted/disabled channel must not lend its model name to another provider.
    };
    apply_channel_model(&mut ch, preferred);
    Some(ch)
}

/// 读全部分工（供设置面板展示）：distill、outline、chapter、review、summary。
/// 旧设置仍使用原键，新增 distill 默认继承活跃渠道而不改动旧四角色。
pub fn all_agent_profiles(db: &Db) -> Value {
    let mut out = serde_json::Map::new();
    for task in ["distill", "outline", "chapter", "review", "summary"] {
        let raw = get_setting(db, &format!("agent_profile__{}", task));
        let profile: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
        out.insert(
            task.to_string(),
            json!({
                "channelId": profile["channelId"].as_str().unwrap_or(""),
                "model": profile["model"].as_str().unwrap_or(""),
            }),
        );
    }
    Value::Object(out)
}

pub fn completions_url(base_url: &str) -> Result<String> {
    let u = base_url.trim();
    if u.is_empty() {
        return Err(anyhow!("渠道缺少 baseUrl，请在 设置→渠道管理 中配置"));
    }
    if u.ends_with("/chat/completions") {
        return Ok(u.to_string());
    }
    Ok(format!("{}/chat/completions", u.trim_end_matches('/')))
}

pub fn models_url(u: &str) -> Result<String> {
    let u = u.trim();
    if u.is_empty() {
        return Err(anyhow!("渠道缺少 modelsUrl，无法获取模型列表"));
    }
    if u.ends_with("/models") {
        return Ok(u.to_string());
    }
    Ok(format!("{}/models", u.trim_end_matches('/')))
}

// 流式事件
#[derive(Debug, Clone)]
pub enum LlmEvent {
    Delta(String),
    Reasoning(String),
    Meta(Option<Value>),
    /// 聚合完成的工具调用数组（OpenAI 兼容 tool_calls）：仅在
    /// `StreamOpts::accept_tool_calls=true` 的 Agent 路径、流结束时一次性发送，
    /// 不逐 delta 发送。旧正文消费方只需忽略该变体。
    ToolCalls(Value),
}

#[derive(Clone)]
pub struct ChatParams {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub messages: Vec<Value>,
    pub temperature: f64,
    pub max_tokens: i64,
    pub stream: bool,
    pub no_thinking: bool,
    /// 思考强度：""=不动 / "high" / "max"（对齐官方 auto/fast/deep/max 的 deep·max 档）
    pub reasoning_effort: String,
    /// 用量落账标签：调用方填（如 auto_outline/auto_chapter…），空串=不落账
    pub log_tag: String,
    /// 用量落账归属书本 id，空串=无归属
    pub log_book: String,
    /// 取消令牌：Some 时，在飞的 HTTP 请求（发请求 / 读流 / 退避等待）会被立即掐断，
    /// future 被 drop -> reqwest 连接断开 -> 上游停止生成（= 停止计费）。
    /// None = 不参与取消（旧行为不变）。
    pub cancel: Option<tokio_util::sync::CancellationToken>,
}

impl Default for ChatParams {
    fn default() -> Self {
        ChatParams {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            messages: vec![],
            temperature: 0.7,
            max_tokens: 4096,
            stream: true,
            no_thinking: false,
            // 全局默认开到最大思考强度（用户要求：思考程度 high/max）
            reasoning_effort: "max".to_string(),
            log_tag: String::new(),
            log_book: String::new(),
            cancel: None,
        }
    }
}

/// 取消哨兵：被取消的调用一律以这条消息结束（绝不与真实上游错误混淆）。
/// 判定统一走 [`is_cancelled_err`]，不要自己写字符串比较。
pub const CANCELLED_MSG: &str = "__molan_cancelled__";

/// 是否是「用户主动取消」造成的错误：沿 anyhow 错误链逐层看文本。
/// 取消不是失败——调用方应据此走「正常停止」语义，而不是记失败/报错/落空洞。
pub fn is_cancelled_err(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.to_string().contains(CANCELLED_MSG))
        || format!("{}", e).contains(CANCELLED_MSG)
}

/// 完成性判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionState {
    /// 收到明确的流终止标记（`[DONE]`）。
    Done,
    /// 收到 finish_reason（部分兼容网关不发 [DONE]）。
    FinishStop,
    /// 兼容模式下的「看不出终止标记」：调用方**不得**据此自动定稿。
    Unverified,
    /// 上游以 finish_reason=tool_calls 正常结束，且工具调用参数已聚合校验。
    /// 仅 `accept_tool_calls=true` 的 Agent 路径可能出现；旧正文路径永不返回。
    ToolCalls,
}

/// 不完整哨兵：响应被截断/缺少终止标记时用它结尾，绝不当成功。
pub const INCOMPLETE_MSG: &str = "__molan_incomplete__";

/// 是否是「上游响应不完整/被截断」造成的错误：调用方应保留为中断草稿，
/// 绝不能当完整正文或自动定稿（与用户取消 [`is_cancelled_err`] 区分）。
pub fn is_incomplete_err(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.to_string().contains(INCOMPLETE_MSG))
        || format!("{}", e).contains(INCOMPLETE_MSG)
}

/// 可续写判定：流中途失败但**已有产出**时，值得「接着写」而不是整轮丢弃：
/// - 瞬态错误（429/5xx/网络抖动）：上游抖动把长章掐断；
/// - 不完整（length 截断 / 裸 EOF / 非正常 finish_reason）。
///
/// 明确排除：用户取消、内容被安全策略过滤（再写只会再被过滤）。
pub fn is_resumable_err(e: &anyhow::Error) -> bool {
    if is_cancelled_err(e) {
        return false;
    }
    let msg = format!("{}", e);
    if msg.contains("content_filter") {
        return false;
    }
    is_transient_error(&msg) || is_incomplete_err(e)
}

/// 显式策略：默认严格（allow_missing_finish_reason=false）。
/// 开启兼容只放宽「终止标记缺失」，length / content_filter 仍一律报错。
/// 工具传输默认全关（tools=None / accept_tool_calls=false），旧正文路径不变。
/// 含 Value 故不再 Copy（旧调用方按值传入一次，不受影响）。
#[derive(Debug, Clone, Default)]
pub struct StreamOpts {
    pub allow_missing_finish_reason: bool,
    /// Some 时在请求体注入 OpenAI 兼容 tools 字段；None = 不发（旧行为）。
    pub tools: Option<serde_json::Value>,
    /// Some 时随 tools 注入 tool_choice；None = 不发。
    pub tool_choice: Option<serde_json::Value>,
    /// 是否接受 finish_reason=tool_calls 作为正常结束（仅 Agent 路径开启）。
    pub accept_tool_calls: bool,
}

/// 明确可接受的「正常收尾」finish_reason 白名单。
/// 不在白名单里的（tool_calls / function_call / unknown / …）一律视为不完整：
/// 绝不能把工具调用或未知原因当成完整小说提交。
pub const ACCEPTED_FINISH_REASONS: &[&str] = &["stop", "end_turn"];

/// 完成性判定（流式与非流式共用；纯函数，便于回归测试）。
/// - finish_reason=length：截断 -> Err（不完整）
/// - finish_reason=content_filter：被过滤 -> Err
/// - 白名单（stop/end_turn）-> Ok
/// - 其它 finish_reason（tool_calls/function_call/unknown…）-> Err（不完整）
/// - 只见 [DONE] -> Ok
/// - 三者皆无 -> 严格 Err；兼容模式下 Ok(Unverified)
pub fn classify_completion(
    saw_done: bool,
    finish_reason: Option<&str>,
    allow_missing: bool,
) -> Result<CompletionState> {
    // 旧正文路径永远不接受工具调用：委托给带开关的新函数并显式传 false，
    // 保证 tool_calls/function_call/unknown 仍然一律判为不完整。
    agent_transport::classify_completion_opts(saw_done, finish_reason, allow_missing, false)
}

fn finish_reason_of(j: &Value) -> Option<&str> {
    j["choices"][0]["finish_reason"].as_str()
}

async fn send_chat_request(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: &Value,
    cancel: &Option<tokio_util::sync::CancellationToken>,
) -> Result<reqwest::Response> {
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .json(body);
    if !api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", api_key));
    }
    // 取消竞速包装：reqwest 的 future 被 drop -> 连接断开 -> 上游停止生成（= 停止计费）。
    match cancel.clone() {
        Some(token) => tokio::select! {
            biased;
            _ = token.cancelled() => Err(anyhow!("{}", CANCELLED_MSG)),
            r = req.send() => Ok(r?),
        },
        None => Ok(req.send().await?),
    }
}

async fn body_text_with_cancel(
    resp: reqwest::Response,
    cancel: &Option<tokio_util::sync::CancellationToken>,
) -> Result<String> {
    match cancel.clone() {
        Some(token) => tokio::select! {
            biased;
            _ = token.cancelled() => Err(anyhow!("{}", CANCELLED_MSG)),
            d = resp.text() => Ok(d.unwrap_or_default()),
        },
        None => Ok(resp.text().await.unwrap_or_default()),
    }
}

/// SSE 字节缓冲：只在拿到完整行之后才做 UTF-8 解码。
/// 跨 chunk 被切断的多字节序列会原样留在缓冲里，绝不会 lossy 成 U+FFFD。
#[derive(Default)]
struct SseBuffer {
    buf: Vec<u8>,
}

impl SseBuffer {
    fn new() -> Self {
        SseBuffer::default()
    }

    /// 单行 SSE 上限：防止上游一直不给换行导致缓冲无界增长（OOM）。
    const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

    /// 追加一块原始字节，返回其中所有完整行（已剥离 CR/LF）。
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>> {
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > Self::MAX_LINE_BYTES {
            return Err(anyhow!(
                "{} 上游 SSE 单行超过 {} 字节仍无换行，疑似协议异常",
                INCOMPLETE_MSG,
                Self::MAX_LINE_BYTES
            ));
        }
        let mut lines = Vec::new();
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop(); // '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            lines.push(decode_utf8_line(&line)?);
        }
        Ok(lines)
    }

    /// 流结束时残留的、没有换行结尾的最后一段字节。
    fn finish(&mut self) -> Result<Option<String>> {
        if self.buf.is_empty() {
            return Ok(None);
        }
        let line = std::mem::take(&mut self.buf);
        Ok(Some(decode_utf8_line(&line)?))
    }
}

/// 严格 UTF-8 解码：只在确实非法/截断时报错，绝不静默 lossy。
fn decode_utf8_line(line: &[u8]) -> Result<String> {
    match std::str::from_utf8(line) {
        Ok(s) => Ok(s.to_string()),
        // error_len()==None 表示序列没收完；按 \n 切行后理论上不该出现，保守报不完整
        Err(e) if e.error_len().is_none() => Err(anyhow!(
            "{} 上游 SSE 行在 UTF-8 序列中间结束",
            INCOMPLETE_MSG
        )),
        Err(_) => Err(anyhow!("上游返回了非法 UTF-8 数据（SSE 协议损坏）")),
    }
}

/// 解析单行 SSE：派发 Delta/Reasoning，并记录 usage、[DONE] 与 finish_reason。
/// accept_tool_calls=false 时 tool_calls 分片被静默丢弃（旧正文路径不变）；
/// =true 时按 index 聚合进 tool_calls（冲突/协议损坏立即报不完整）。
pub(crate) fn handle_sse_line(
    line: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<LlmEvent>,
    usage: &mut Option<Value>,
    saw_done: &mut bool,
    finish_reason: &mut Option<String>,
    tool_calls: &mut agent_transport::ToolCallAccumulator,
    accept_tool_calls: bool,
) -> Result<()> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(()); // event: / id: / 注释行（: ping）忽略
    };
    let data = data.trim();
    if data == "[DONE]" {
        *saw_done = true;
        return Ok(());
    }
    if data.is_empty() {
        return Ok(());
    }
    // 有 data: 前缀就必须能解析；否则协议已损坏，绝不静默跳过（会丢内容还当成功）。
    let j: Value = serde_json::from_str(data).map_err(|e| {
        anyhow!(
            "{} 上游 SSE 的 data 行不是合法 JSON（{}），已中止以免丢失内容",
            INCOMPLETE_MSG,
            e
        )
    })?;
    let delta = &j["choices"][0]["delta"];
    if let Some(r) = delta["reasoning_content"].as_str() {
        let _ = tx.send(LlmEvent::Reasoning(r.to_string()));
    }
    if let Some(c) = delta["content"].as_str() {
        if !c.is_empty() {
            let _ = tx.send(LlmEvent::Delta(c.to_string()));
        }
    }
    // 工具调用分片：非 accept 模式静默忽略（旧行为），accept 模式聚合校验。
    tool_calls.collect_delta(delta, accept_tool_calls)?;
    if let Some(fr) = finish_reason_of(&j) {
        if !fr.trim().is_empty() {
            *finish_reason = Some(fr.to_string());
        }
    }
    if !j["usage"].is_null() {
        *usage = Some(j["usage"].clone());
    }
    Ok(())
}

fn friendly_error(status: u16, detail: &str) -> String {
    let reason = match status {
        401 => "密钥无效或已失效（401），请检查渠道的 API Key",
        403 => "服务拒绝访问（403），检查额度/区域限制",
        404 => "模型或接口地址不存在（404）——检查模型名与 baseUrl",
        429 => "请求太频繁或额度不足（429），稍等再发",
        _ => "",
    };
    let msg = serde_json::from_str::<Value>(detail)
        .ok()
        .and_then(|j| j["error"]["message"].as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| detail.to_string());
    format!(
        "{} [{}] {}",
        reason,
        status,
        msg.chars().take(160).collect::<String>()
    )
}

/// 真流式：SSE 每解析出一块 token 就立即 send 进通道（对齐 lib/llm.js 的逐块消费）；
/// 调用方消费 tx 的 recv 循环即可拿到逐块事件
/// 严格模式（默认）：任何无法确认完整性的响应都返回 Err（见 [`INCOMPLETE_MSG`]）。
/// 需要「上游缺失 finish_reason 也接受」的调用方，显式用 [`chat_completion_stream_opts`]，
/// 并注意 Unverified 结果不得用于自动定稿。
pub async fn chat_completion_stream(
    params: ChatParams,
    tx: &tokio::sync::mpsc::UnboundedSender<LlmEvent>,
) -> Result<()> {
    chat_completion_stream_opts(params, tx, StreamOpts::default())
        .await
        .map(|_| ())
}

/// 带显式策略的流式调用：返回完成性判定（Done/FinishStop/Unverified）。
pub async fn chat_completion_stream_opts(
    params: ChatParams,
    tx: &tokio::sync::mpsc::UnboundedSender<LlmEvent>,
    opts: StreamOpts,
) -> Result<CompletionState> {
    // Mock 渠道（baseUrl = mock://）：不发网络请求，返回确定性假响应。
    // 用于端到端测试/离线演示：细纲、正文、摘要都有最小可用内容。
    if params.base_url.trim() == "mock://" {
        let sys = params
            .messages
            .first()
            .and_then(|m| m["content"].as_str())
            .unwrap_or("");
        // 最后一条 user 消息文本（无 role 时退回最后一条消息）。
        let user = agent_transport::last_user_text(&params.messages);
        // mock 确定性工具协议：仅当调用方给了 tools 且最后一条 user 文本含
        // [call:<工具名>] 时，回一次该工具的 tool_call（id="mock-call-1"、arguments="{}"、
        // finish_reason="tool_calls"）；否则维持原有 mock 正文/JSON 行为。
        // last_is_user 门槛：工具结果回灌后（末条是 tool 消息）回落正文分支，
        // 使 mock 也能驱动「工具轮→文本轮」的多轮 Agent 循环测试。
        let last_is_user = params
            .messages
            .last()
            .is_some_and(|m| m["role"].as_str() == Some("user"));
        if let Some(calls) = agent_transport::mock_tool_calls(&opts, user).filter(|_| last_is_user)
        {
            let _ = tx.send(LlmEvent::ToolCalls(calls));
            let _ = tx.send(LlmEvent::Meta(None));
            return Ok(CompletionState::ToolCalls);
        }
        let body = mock::mock_body(sys, user);
        let _ = tx.send(LlmEvent::Delta(body));
        let _ = tx.send(LlmEvent::Meta(None));
        return Ok(CompletionState::Done);
    }
    let url = completions_url(&params.base_url)?;
    let mut body = json!({
        "model": params.model,
        "messages": params.messages,
        "temperature": params.temperature,
        "max_tokens": params.max_tokens,
        "stream": params.stream,
    });
    if params.no_thinking {
        body["enable_thinking"] = json!(false);
    } else if !params.reasoning_effort.is_empty() {
        // 思考强度开档：qwen/dashscope 认 enable_thinking，OpenAI 系网关认 reasoning_effort
        body["enable_thinking"] = json!(true);
        body["reasoning_effort"] = json!(params.reasoning_effort);
    }
    // 用量：流式路径显式索取 include_usage（默认不发的网关会因此补齐）。
    // 个别网关不认该字段会 400，下面的 400 兜底会自动去掉它重发一次。
    if params.stream {
        body["stream_options"] = json!({"include_usage": true});
    }
    // 仅当调用方显式给出 tools 时注入 OpenAI 兼容 tools / tool_choice；
    // 旧路径（tools=None）请求体完全不变。
    agent_transport::apply_tools(&mut body, &opts);
    let client = http_client();
    let mut resp = send_chat_request(client, &url, &params.api_key, &body, &params.cancel).await?;
    // 5xx 重试一次
    if resp.status().as_u16() >= 500 {
        let backoff = tokio::time::sleep(std::time::Duration::from_millis(1200));
        if let Some(token) = params.cancel.clone() {
            tokio::select! {
                biased;
                _ = token.cancelled() => return Err(anyhow!("{}", CANCELLED_MSG)),
                _ = backoff => {}
            }
        } else {
            backoff.await;
        }
        resp = send_chat_request(client, &url, &params.api_key, &body, &params.cancel).await?;
    }
    let mut status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let detail = body_text_with_cancel(resp, &params.cancel).await?;
        if status == 400
            && params.stream
            && body.get("stream_options").is_some()
            && (detail.contains("stream_options") || detail.contains("include_usage"))
        {
            // 兼容兜底：网关不接受 stream_options.include_usage -> 去掉后重发一次
            if let Some(m) = body.as_object_mut() {
                m.remove("stream_options");
            }
            resp = send_chat_request(client, &url, &params.api_key, &body, &params.cancel).await?;
            status = resp.status().as_u16();
            if !(200..300).contains(&status) {
                let d2 = body_text_with_cancel(resp, &params.cancel).await?;
                return Err(anyhow!("{}", friendly_error(status, &d2)));
            }
        } else {
            return Err(anyhow!("{}", friendly_error(status, &detail)));
        }
    }
    if !params.stream {
        let j: Value = match params.cancel.clone() {
            Some(token) => tokio::select! {
                biased;
                _ = token.cancelled() => return Err(anyhow!("{}", CANCELLED_MSG)),
                r = resp.json() => r?,
            },
            None => resp.json().await?,
        };
        // 非流式同样必须校验完成原因：length / content_filter / 缺失终止都不是成功。
        // accept_tool_calls=true 时支持解析 message.tool_calls（缺失/非法仍报不完整）。
        let message = &j["choices"][0]["message"];
        let (state, calls) = agent_transport::classify_nonstream_message(
            message,
            false,
            finish_reason_of(&j),
            opts.allow_missing_finish_reason,
            opts.accept_tool_calls,
        )?;
        if let Some(calls) = calls {
            let _ = tx.send(LlmEvent::ToolCalls(calls));
        }
        let content = message["content"].as_str().unwrap_or("").to_string();
        let _ = tx.send(LlmEvent::Delta(content));
        let _ = tx.send(LlmEvent::Meta(
            j["usage"].as_object().map(|_| j["usage"].clone()),
        ));
        return Ok(state);
    }
    let mut buf = SseBuffer::new();
    let mut usage: Option<Value> = None;
    let mut saw_done = false;
    let mut finish_reason: Option<String> = None;
    let mut tool_calls = agent_transport::ToolCallAccumulator::default();
    let mut stream = resp.bytes_stream();
    loop {
        // 读流与取消竞速：取消时 drop stream（断开连接），上游立即停算
        let chunk = match params.cancel.clone() {
            Some(token) => tokio::select! {
                biased;
                _ = token.cancelled() => return Err(anyhow!("{}", CANCELLED_MSG)),
                c = stream.next() => c,
            },
            None => stream.next().await,
        };
        let Some(chunk) = chunk else { break };
        let bytes = chunk?;
        // 先攒原始字节、按完整行切分后再 UTF-8 解码：
        // 绝不 per-chunk lossy——否则「中」的 3 字节被网络拆开就会变成 U+FFFD。
        for line in buf.push(&bytes)? {
            handle_sse_line(
                &line,
                tx,
                &mut usage,
                &mut saw_done,
                &mut finish_reason,
                &mut tool_calls,
                opts.accept_tool_calls,
            )?;
        }
    }
    // 最后一段可能没有换行结尾（例如裸 data: [DONE]），仍按一行解析。
    if let Some(line) = buf.finish()? {
        if !line.trim().is_empty() {
            handle_sse_line(
                &line,
                tx,
                &mut usage,
                &mut saw_done,
                &mut finish_reason,
                &mut tool_calls,
                opts.accept_tool_calls,
            )?;
        }
    }
    let state = agent_transport::classify_completion_opts(
        saw_done,
        finish_reason.as_deref(),
        opts.allow_missing_finish_reason,
        opts.accept_tool_calls,
    )?;
    // 工具调用：流结束时把聚合好的数组一次性发送（不逐 delta 发）。
    agent_transport::finish_tool_calls(&tool_calls, state, opts.accept_tool_calls, tx)?;
    let _ = tx.send(LlmEvent::Meta(usage));
    Ok(state)
}

pub async fn chat_completion(params: ChatParams) -> Result<Vec<LlmEvent>> {
    chat_completion_opts(params, StreamOpts::default()).await
}

/// 带显式完成性策略的收集版（兼容模式见 [`StreamOpts`]）。
pub async fn chat_completion_opts(params: ChatParams, opts: StreamOpts) -> Result<Vec<LlmEvent>> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    chat_completion_stream_opts(params, &tx, opts).await?;
    drop(tx);
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    Ok(events)
}

pub async fn chat_once(params: ChatParams) -> Result<String> {
    Ok(chat_once_logged(params).await?.0)
}

/// 非流式一次性调用（带用量）：返回 (全文, usage)。
/// usage 取自 chat_completion 的 Meta 事件（非流式分支发 j["usage"]，mock:// 渠道发 None）。
/// 本身不落账——落账由调用方按 params.log_tag / log_book 决定（此处只负责把用量交出去）。
pub async fn chat_once_logged(params: ChatParams) -> Result<(String, Option<Value>)> {
    chat_once_logged_opts(params, StreamOpts::default()).await
}

/// 带显式完成性策略的一次性调用（非流式）；默认严格见 [`chat_once_logged`]。
pub async fn chat_once_logged_opts(
    params: ChatParams,
    opts: StreamOpts,
) -> Result<(String, Option<Value>)> {
    let mut p = params;
    p.stream = false;
    let mut full = String::new();
    let mut usage: Option<Value> = None;
    for ev in chat_completion_opts(p, opts).await? {
        match ev {
            LlmEvent::Delta(t) => full.push_str(&t),
            LlmEvent::Meta(u) => {
                if u.is_some() {
                    usage = u;
                }
            }
            LlmEvent::Reasoning(_) => {}
            // 一次性文本调用不消费工具调用（Agent 路径自行处理 ToolCalls）。
            LlmEvent::ToolCalls(_) => {}
        }
    }
    Ok((full, usage))
}

/// 瞬态错误判定：只有这类错误值得重试（401/404/参数错误重试也是白烧钱）
pub fn is_transient_error(msg: &str) -> bool {
    // 显式状态码
    for code in ["429", "500", "502", "503", "504"] {
        if msg.contains(&format!("[{}]", code)) || msg.contains(&format!("status: {}", code)) {
            return true;
        }
    }
    // 网关/网络类关键词
    let lower = msg.to_lowercase();
    [
        "overloaded",
        "rate limit",
        "too many requests",
        "connection reset",
        "connection refused",
        "timed out",
        "timeout",
        "temporarily unavailable",
        "upstream",
        "eof while",
        "broken pipe",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// 带重试的 chat_once：瞬态错误指数退避重试（默认 3 次，1s/2s/4s 起步），
/// 非瞬态错误（密钥无效/模型不存在等）立即返回，不浪费时间
pub async fn chat_once_retry(params: ChatParams) -> Result<String> {
    chat_once_retry_n(params, 3).await
}

/// 可指定重试次数的版本（自动写作等长任务用）：丢弃用量，保持原签名
pub async fn chat_once_retry_n(params: ChatParams, retries: u32) -> Result<String> {
    Ok(chat_once_retry_logged_n(params, retries).await?.0)
}

/// 带重试 + 用量的版本：重试判定与退避节奏与 chat_once_retry_n 完全一致，
/// 只在最终成功那一次返回 (全文, usage)；失败仍按原语义返回 Err。
pub async fn chat_once_retry_logged_n(
    params: ChatParams,
    retries: u32,
) -> Result<(String, Option<Value>)> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..=(retries as u64) {
        match chat_once_logged(params.clone()).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                // 用户主动取消不是瞬态错误：立即返回，不进重试、不写重试日志
                if is_cancelled_err(&e) {
                    return Err(e);
                }
                let msg = format!("{}", e);
                let transient = is_transient_error(&msg);
                if !transient || attempt == retries as u64 {
                    return Err(e);
                }
                last_err = Some(e);
                let wait_ms = 1000u64 * (1u64 << attempt); // 1s, 2s, 4s...
                tracing::info!(
                    "LLM 瞬态错误，{}ms 后第 {}/{} 次重试：{}",
                    wait_ms,
                    attempt + 1,
                    retries,
                    &msg.chars().take(120).collect::<String>()
                );
                // 退避睡眠同样与取消竞速：取消时立刻返回，不睡满
                let backoff = tokio::time::sleep(std::time::Duration::from_millis(wait_ms));
                match params.cancel.clone() {
                    Some(token) => tokio::select! {
                        biased;
                        _ = token.cancelled() => return Err(anyhow!("{}", CANCELLED_MSG)),
                        _ = backoff => {}
                    },
                    None => backoff.await,
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("未知上游错误")))
}

pub async fn fetch_model_list(models_url_arg: &str, api_key: &str) -> Result<Vec<String>> {
    let url = models_url(models_url_arg)?;
    let client = http_client();
    let mut req = client.get(&url).header("User-Agent", "writerx-replica");
    if !api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", api_key));
    }
    let resp = req.send().await?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(anyhow!(
            "获取模型列表失败 {} — modelsUrl 换成 {}",
            status,
            url
        ));
    }
    let j: Value = resp.json().await?;
    let list = if j.is_array() {
        j.as_array().unwrap().clone()
    } else {
        j["data"]
            .as_array()
            .or_else(|| j["models"].as_array())
            .cloned()
            .unwrap_or_default()
    };
    Ok(list
        .iter()
        .filter_map(|m| {
            if m.is_string() {
                m.as_str().map(|s| s.to_string())
            } else {
                m["id"]
                    .as_str()
                    .or_else(|| m["name"].as_str())
                    .map(|s| s.to_string())
            }
        })
        .filter(|s| !s.is_empty())
        .collect())
}

// 从响应文本里提取 JSON（剥掉 markdown 围栏/前后杂文）
pub fn extract_json(text: &str) -> Option<Value> {
    let t = text.trim();
    let t = t.strip_prefix("```json").unwrap_or(t);
    let t = t.strip_prefix("```").unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t).trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Some(v);
    }
    for (open, close) in [('{', '}'), ('[', ']')] {
        if let Some(s) = t.find(open) {
            if let Some(e) = t.rfind(close) {
                if e > s {
                    if let Ok(v) = serde_json::from_str::<Value>(&t[s..=e]) {
                        return Some(v);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse_delta(content: &str) -> String {
        format!(
            "data: {}\n\n",
            json!({"choices": [{"delta": {"content": content}, "finish_reason": null}]})
        )
    }

    /// 旧正文路径的单行解析（accept_tool_calls=false：tool_calls 静默丢弃）。
    fn parse_line(
        line: &str,
        tx: &tokio::sync::mpsc::UnboundedSender<LlmEvent>,
        usage: &mut Option<Value>,
        saw_done: &mut bool,
        fr: &mut Option<String>,
    ) -> Result<()> {
        let mut acc = agent_transport::ToolCallAccumulator::default();
        handle_sse_line(line, tx, usage, saw_done, fr, &mut acc, false)
    }

    // F13：SSE 数据在任意字节切点被网络拆开，正文必须逐字符完好，绝不 lossy 成 U+FFFD。
    #[test]
    fn utf8_survives_every_byte_split_in_chinese_and_emoji() {
        let text = "中文测试正文😀🎉";
        let mut buf = SseBuffer::new();
        let mut lines: Vec<String> = Vec::new();
        for b in text.as_bytes() {
            lines.extend(buf.push(std::slice::from_ref(b)).unwrap());
        }
        if let Some(l) = buf.finish().unwrap() {
            lines.push(l);
        }
        assert_eq!(lines, vec![text.to_string()]);
        assert!(!lines[0].contains('\u{FFFD}'));
    }

    // F13：完整 SSE 帧在汉字中间拆包，按行解码后 delta 仍原样还原。
    #[test]
    fn sse_frame_split_inside_multibyte_yields_intact_delta() {
        let frame = sse_delta("中文😀测试");
        let mut buf = SseBuffer::new();
        let mut lines: Vec<String> = Vec::new();
        for chunk in frame.as_bytes().chunks(3) {
            lines.extend(buf.push(chunk).unwrap());
        }
        if let Some(l) = buf.finish().unwrap() {
            lines.push(l);
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut usage, mut saw_done, mut fr) = (None, false, None);
        for l in &lines {
            parse_line(l, &tx, &mut usage, &mut saw_done, &mut fr).unwrap();
        }
        drop(tx);
        let mut got = String::new();
        while let Ok(ev) = rx.try_recv() {
            if let LlmEvent::Delta(d) = ev {
                got.push_str(&d);
            }
        }
        assert_eq!(got, "中文😀测试");
    }

    // F13：真正被切断的 UTF-8 序列（EOF 处没收完）必须显式报不完整，而不是静默替换。
    #[test]
    fn truncated_multibyte_at_eof_is_incomplete_error() {
        let mut buf = SseBuffer::new();
        buf.push(b"\xe4\xb8").unwrap(); // 「中」的前两字节
        let e = buf.finish().unwrap_err();
        assert!(is_incomplete_err(&e));
    }

    // F14：完成性判定矩阵——length/content_filter/无终止一律失败；兼容模式只放宽「缺终止」。
    #[test]
    fn completion_classification_matrix() {
        assert_eq!(
            classify_completion(true, Some("stop"), false).unwrap(),
            CompletionState::Done
        );
        assert_eq!(
            classify_completion(false, Some("stop"), false).unwrap(),
            CompletionState::FinishStop
        );
        assert!(is_incomplete_err(
            &classify_completion(true, Some("length"), true).unwrap_err()
        ));
        assert!(is_incomplete_err(
            &classify_completion(false, Some("content_filter"), true).unwrap_err()
        ));
        // 裸 EOF：无 [DONE] 也无 finish_reason
        assert!(is_incomplete_err(
            &classify_completion(false, None, false).unwrap_err()
        ));
        // 空白 finish_reason 等同缺失
        assert!(classify_completion(false, Some("   "), false).is_err());
        // 显式兼容：仅此处返回 Unverified，调用方不得自动定稿
        assert_eq!(
            classify_completion(false, None, true).unwrap(),
            CompletionState::Unverified
        );
        // 白名单外：工具调用/未知原因必须判不完整，绝不当完整正文
        for fr in ["tool_calls", "function_call", "unknown", "whatever"] {
            assert!(
                is_incomplete_err(&classify_completion(true, Some(fr), false).unwrap_err()),
                "{}",
                fr
            );
        }
        assert_eq!(
            classify_completion(true, Some("end_turn"), false).unwrap(),
            CompletionState::Done
        );
    }

    // 有 data: 前缀但 JSON 非法：必须报错，不能静默跳过丢内容。
    #[test]
    fn invalid_sse_json_is_error_not_silent_skip() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut usage, mut saw_done, mut fr) = (None, false, None);
        let e = parse_line("data: {not json", &tx, &mut usage, &mut saw_done, &mut fr).unwrap_err();
        assert!(is_incomplete_err(&e));
        // 非 data: 行（注释/心跳）仍应忽略
        assert!(parse_line(": ping", &tx, &mut usage, &mut saw_done, &mut fr).is_ok());
    }

    // 上游一直不给换行：超过上限必须报错，避免无界缓冲 OOM。
    #[test]
    fn oversized_sse_line_without_newline_is_rejected() {
        let mut buf = SseBuffer::new();
        let big = vec![b'a'; SseBuffer::MAX_LINE_BYTES + 1];
        let e = buf.push(&big).unwrap_err();
        assert!(is_incomplete_err(&e));
    }

    // F14 + usage：单行解析要能同时收集 [DONE]、finish_reason 与 usage。
    #[test]
    fn sse_line_collects_done_finish_reason_and_usage() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut usage, mut saw_done, mut fr) = (None, false, None);
        let payload = format!(
            "data: {}",
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}], "usage": {"total_tokens": 3}})
        );
        parse_line(&payload, &tx, &mut usage, &mut saw_done, &mut fr).unwrap();
        parse_line("data: [DONE]", &tx, &mut usage, &mut saw_done, &mut fr).unwrap();
        assert!(saw_done);
        assert_eq!(fr.as_deref(), Some("stop"));
        assert_eq!(usage.unwrap()["total_tokens"], json!(3));
        assert!(classify_completion(saw_done, fr.as_deref(), false).is_ok());
        drop(tx);
        // [DONE] 行不产生事件；sender 已 drop，因此必须是 Empty（缓冲已空）或 Disconnected。
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
    }

    // TLS 回归：https 必须能被客户端接受（不得是 "scheme is not http"）。
    // 不打真实渠道、不带 key：只请求 example.com；成功或网络失败都可接受，
    // 但错误链里出现 scheme 判定失败即说明 workspace 少了 rustls-tls。
    #[test]
    fn https_scheme_is_supported_by_client() {
        let client = http_client();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // 10s 上限：公网 DNS/连接不得无限等待（生产 client 走 read_timeout=120s，不受此影响）。
        let res = rt.block_on(async {
            client
                .get("https://example.com/")
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
        });
        if let Err(e) = res {
            let mut chain = format!("{}", e);
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                chain.push_str(&format!(" | {}", s));
                src = s.source();
            }
            assert!(
                !chain.contains("scheme is not http"),
                "reqwest 缺少 TLS 特性，https 被判为非法 scheme：{}",
                chain
            );
        }
    }

    #[test]
    fn unsupported_role_model_falls_back_to_channel_model() {
        let channel = json!({
            "model": "cline-free/deepseek-v4.1-flash",
            "models": ["cline-free/deepseek-v4.1-flash", "moonshotai/kimi-k3"]
        });
        assert_eq!(
            select_channel_model(&channel, "glm-5.3-flash").as_deref(),
            Some("cline-free/deepseek-v4.1-flash")
        );
        assert_eq!(
            select_channel_model(&channel, "moonshotai/kimi-k3").as_deref(),
            Some("moonshotai/kimi-k3")
        );
    }

    #[test]
    fn stale_channel_model_falls_back_to_first_listed_model() {
        let channel = json!({
            "model": "removed-model",
            "models": ["first-model", "second-model"]
        });
        assert_eq!(
            select_channel_model(&channel, "").as_deref(),
            Some("first-model")
        );
    }

    #[test]
    fn empty_model_catalog_preserves_explicit_custom_model() {
        let channel = json!({"model": "configured-model", "models": []});
        assert_eq!(
            select_channel_model(&channel, "custom-model").as_deref(),
            Some("custom-model")
        );
        assert_eq!(
            select_channel_model(&channel, "").as_deref(),
            Some("configured-model")
        );
    }
}
