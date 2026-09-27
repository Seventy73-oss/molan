//! 认证与会话：GET /login、POST /auth/login、会话 Cookie、限速、常量时间比较。
//! 归属 G；main.rs 负责把这里接到路由上。不接触 glue（E 的文件）。
//!
//! 设计要点（对齐 V1 契约 F02/G 条）：
//! - 匿名 HTML 绝不分发服务器 token；浏览器改用 HttpOnly + SameSite=Strict 会话 Cookie。
//! - 脚本兼容：仍接受 X-Molan-Token（常量时间比较）。
//! - 无 token 且绑定非 loopback 则拒绝启动，除非显式 MOLAN_ALLOW_INSECURE=1。
//! - /health 公开且不含隐私。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;

/// 会话 Cookie 名。
pub const COOKIE_NAME: &str = "molan_session";
/// 登录请求体里 token 的最大长度（防注入/防超长）。
pub const MAX_TOKEN_LEN: usize = 512;
/// 默认会话空闲 TTL（滑动续期）。
pub const DEFAULT_TTL_SECS: u64 = 12 * 3600;
/// 会话绝对上限，防止无限续期。
pub const ABSOLUTE_CAP_SECS: u64 = 7 * 24 * 3600;
/// 登录失败限速窗口。
pub const LOGIN_WINDOW_SECS: u64 = 300;
/// 窗口内允许的最大失败次数。
pub const LOGIN_MAX_FAILURES: usize = 10;

// ---------------------------------------------------------------------------
// 常量时间比较 / 指纹
// ---------------------------------------------------------------------------

/// 常量时间字节比较：长度不同直接 false（长度本身不是秘密）。
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// 常量时间字符串比较。
pub fn ct_eq_str(a: &str, b: &str) -> bool {
    ct_eq(a.as_bytes(), b.as_bytes())
}

/// FNV-1a 64 位指纹：只用于 token 变更后让旧会话失效的等值标记，不作密码学哈希。
pub fn token_fingerprint(token: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in token.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 生成不透明会话 id（uuid v4 两次，约 244 bit 熵，不暴露 token 内容）。
pub fn new_session_id() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// 登录 token 长度校验。
pub fn token_len_ok(token: &str) -> bool {
    let n = token.len();
    n > 0 && n <= MAX_TOKEN_LEN
}

// ---------------------------------------------------------------------------
// 会话存储
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Session {
    created: Instant,
    expires_at: Instant,
    token_fp: u64,
}

/// 进程内会话表。单实例服务够用；重启即登出（可接受）。
pub struct SessionStore {
    inner: RwLock<HashMap<String, Session>>,
    ttl: Duration,
    absolute_cap: Duration,
}

impl SessionStore {
    pub fn new(ttl: Duration, absolute_cap: Duration) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            ttl,
            absolute_cap,
        }
    }

    /// 用默认 TTL 构造。
    pub fn with_defaults() -> Self {
        Self::new(
            Duration::from_secs(DEFAULT_TTL_SECS),
            Duration::from_secs(ABSOLUTE_CAP_SECS),
        )
    }

    pub fn ttl_secs(&self) -> u64 {
        self.ttl.as_secs()
    }

    /// 签发新会话，返回 id。
    pub fn issue(&self, token_fp: u64) -> String {
        self.issue_at(token_fp, Instant::now())
    }

    pub(crate) fn issue_at(&self, token_fp: u64, now: Instant) -> String {
        let id = new_session_id();
        let expires_at = now + self.ttl;
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if let Some(exp) = now.checked_sub(self.absolute_cap) {
            g.retain(|_, s| s.created > exp);
        }
        g.insert(
            id.clone(),
            Session {
                created: now,
                expires_at,
                token_fp,
            },
        );
        id
    }

    /// 校验会话；命中则滑动续期（不超过绝对上限）。token 指纹不符视为无效。
    pub fn validate(&self, id: &str, token_fp: u64) -> bool {
        self.validate_at(id, token_fp, Instant::now())
    }

    pub(crate) fn validate_at(&self, id: &str, token_fp: u64, now: Instant) -> bool {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let ok = match g.get(id) {
            Some(s) => {
                if now >= s.expires_at || s.token_fp != token_fp {
                    None
                } else {
                    let cap = s.created + self.absolute_cap;
                    let slid = now + self.ttl;
                    Some(if slid < cap { slid } else { cap })
                }
            }
            None => None,
        };
        match ok {
            Some(expires) => {
                if let Some(s) = g.get_mut(id) {
                    s.expires_at = expires;
                }
                true
            }
            None => {
                g.remove(id);
                false
            }
        }
    }

    /// 撤销单个会话。
    pub fn revoke(&self, id: &str) {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        g.remove(id);
    }

    /// 清空全部会话（token 轮换时用）。
    pub fn clear(&self) {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        g.clear();
    }

    /// 清理过期会话，返回清理条数。
    pub fn gc(&self) -> usize {
        self.gc_at(Instant::now())
    }

    pub(crate) fn gc_at(&self, now: Instant) -> usize {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let before = g.len();
        g.retain(|_, s| now < s.expires_at);
        before - g.len()
    }

    pub fn len(&self) -> usize {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// 登录限速
// ---------------------------------------------------------------------------

/// 按来源键（IP）滑动窗口限速。只记失败，成功即清。
pub struct LoginLimiter {
    inner: RwLock<HashMap<String, Vec<Instant>>>,
    window: Duration,
    max_failures: usize,
}

impl LoginLimiter {
    pub fn new(window: Duration, max_failures: usize) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            window,
            max_failures,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(Duration::from_secs(LOGIN_WINDOW_SECS), LOGIN_MAX_FAILURES)
    }

    /// 是否允许尝试登录。被限速时返回 Err(建议的 Retry-After 秒数)。
    pub fn check(&self, key: &str) -> Result<(), u64> {
        self.check_at(key, Instant::now())
    }

    pub(crate) fn check_at(&self, key: &str, now: Instant) -> Result<(), u64> {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let v = g.entry(key.to_string()).or_default();
        v.retain(|t| now.duration_since(*t) < self.window);
        if v.len() >= self.max_failures {
            let oldest = *v.first().unwrap_or(&now);
            let retry = self.window.saturating_sub(now.duration_since(oldest));
            return Err(retry.as_secs().max(1));
        }
        Ok(())
    }

    /// 记录一次失败。
    pub fn record_failure(&self, key: &str) {
        self.record_failure_at(key, Instant::now());
    }

    pub(crate) fn record_failure_at(&self, key: &str, now: Instant) {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let v = g.entry(key.to_string()).or_default();
        v.retain(|t| now.duration_since(*t) < self.window);
        v.push(now);
    }

    /// 登录成功后清空该键的失败记录。
    pub fn clear(&self, key: &str) {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        g.remove(key);
    }
}

// ---------------------------------------------------------------------------
// Cookie
// ---------------------------------------------------------------------------

/// 构造会话 Cookie。
/// 局域网纯 HTTP 环境**不能**加 Secure（加了浏览器会直接丢弃），
/// 因此是否带 Secure 由可信部署 scheme 决定（MOLAN_TRUSTED_SCHEME=https 时才加）。
pub fn session_cookie(id: &str, max_age_secs: u64, secure: bool) -> String {
    if secure {
        format!(
            "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}; Secure",
            COOKIE_NAME, id, max_age_secs
        )
    } else {
        format!(
            "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
            COOKIE_NAME, id, max_age_secs
        )
    }
}

/// 使会话 Cookie 立即失效。
///
/// The current LAN/insecure deployment does not expose a logout handler, but this
/// helper remains part of the cookie-auth contract for authenticated deployments.
#[allow(dead_code)]
pub fn clear_cookie() -> String {
    format!(
        "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
        COOKIE_NAME
    )
}

/// 从请求头解析指定 Cookie（可能有多条 Cookie 头）。
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for hv in headers.get_all(axum::http::header::COOKIE).iter() {
        let raw = match hv.to_str() {
            Ok(s) => s,
            Err(_) => continue,
        };
        for part in raw.split(';') {
            let part = part.trim();
            if let Some((k, v)) = part.split_once('=') {
                if k.trim() == name {
                    let v = v.trim();
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

/// 读取 X-Molan-Token 头。
pub fn script_token(headers: &HeaderMap) -> Option<&str> {
    headers.get("x-molan-token").and_then(|v| v.to_str().ok())
}

/// 会话 Cookie 是否存在。
///
/// Retained for the cookie-auth contract and future logout/session diagnostics.
#[allow(dead_code)]
pub fn has_session_cookie(headers: &HeaderMap) -> bool {
    cookie_value(headers, COOKIE_NAME).is_some()
}

// ---------------------------------------------------------------------------
// Origin / Referer 同源校验（cookie 通道的纵深防御）
// ---------------------------------------------------------------------------

fn split_hostport(s: &str) -> (String, Option<u16>) {
    if let Some(rest) = s.strip_prefix('[') {
        if let Some(idx) = rest.find(']') {
            let host = &rest[..idx];
            let port = rest[idx + 1..]
                .strip_prefix(':')
                .and_then(|p| p.parse::<u16>().ok());
            return (host.to_ascii_lowercase(), port);
        }
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            (h.to_ascii_lowercase(), p.parse::<u16>().ok())
        }
        _ => (s.to_ascii_lowercase(), None),
    }
}

fn default_port(scheme: &str) -> u16 {
    if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    }
}

/// 解析 scheme://authority/path 的 (scheme, host, port)。
fn url_origin(u: &str) -> Option<(String, String, Option<u16>)> {
    let u = u.trim();
    let (scheme, rest) = match u.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => return None,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    let (host, port) = split_hostport(authority);
    Some((scheme, host, port))
}

/// Host 头归一化成 (host, port)。
pub fn host_header_parts(host: Option<&str>) -> Option<(String, Option<u16>)> {
    let h = host?.trim();
    if h.is_empty() {
        return None;
    }
    Some(split_hostport(h))
}

/// 本服务对外实际使用的 scheme。
///
/// 反代终止 TLS 时，Host 头常常不带端口，而 Origin 是 https 且默认 443。
/// 若像旧实现那样把 Host 无端口一律当成 80，会把合法的 https 同源请求误判为跨站。
/// 这里改为读取**可信配置**（MOLAN_TRUSTED_SCHEME），而不是盲信 X-Forwarded-Proto：
/// 代理头可被客户端伪造，只有部署方显式声明才可信。
pub fn trusted_scheme_from_env() -> String {
    match std::env::var("MOLAN_TRUSTED_SCHEME")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("https") => "https".to_string(),
        _ => "http".to_string(),
    }
}

/// 可信 scheme 为 https 时，会话 Cookie 应带 Secure。
pub fn secure_cookies_for(trusted_scheme: &str) -> bool {
    trusted_scheme.eq_ignore_ascii_case("https")
}

/// 同源判断。
/// - Origin 与 Referer 都缺失则 true（非浏览器客户端 / 脚本）。
/// - 提供 Origin 时用 Origin；否则用 Referer。
/// - 任一存在时：scheme 必须等于可信 scheme，主机必须等于 Host 头，
///   端口按各自 scheme 的默认值补全后比较（https 默认 443，http 默认 80）。
/// - 可信 scheme 用于 Host 头缺省端口的补全；候选方用自己的 scheme 补全，
///   两者 scheme 必须一致，因此 http/https 混淆或降级都会被拒绝。
pub fn same_origin(
    host: Option<&str>,
    origin: Option<&str>,
    referer: Option<&str>,
    trusted_scheme: &str,
) -> bool {
    let candidate = match origin.filter(|s| !s.trim().is_empty()) {
        Some(o) => o,
        None => match referer.filter(|s| !s.trim().is_empty()) {
            Some(r) => r,
            None => return true,
        },
    };
    // 跨站导航可能给出字面量 "null"，一律拒绝。
    if candidate.trim().eq_ignore_ascii_case("null") {
        return false;
    }
    let (scheme, chost, cport) = match url_origin(candidate) {
        Some(v) => v,
        None => return false,
    };
    // scheme 必须与可信配置一致（防降级 / 防混淆）。
    if !scheme.eq_ignore_ascii_case(trusted_scheme) {
        return false;
    }
    let (hhost, hport) = match host_header_parts(host) {
        Some(v) => v,
        None => return false,
    };
    if chost != hhost {
        return false;
    }
    let cport = cport.unwrap_or_else(|| default_port(&scheme));
    let hport = hport.unwrap_or_else(|| default_port(trusted_scheme));
    cport == hport
}

// ---------------------------------------------------------------------------
// 启动安全门
// ---------------------------------------------------------------------------

/// 绑定地址是否 loopback。
pub fn is_loopback_bind(bind: &str) -> bool {
    match bind.trim().parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => bind.trim().eq_ignore_ascii_case("localhost"),
    }
}

/// 启动安全决策。
#[derive(Debug, PartialEq, Eq)]
pub enum StartupGate {
    /// 直接启动（loopback 且无 token，或已配置 token）。
    Allowed,
    /// 非 loopback 且无 token，但显式 MOLAN_ALLOW_INSECURE=1。
    AllowedInsecure,
    /// 非 loopback 且无 token，拒绝启动。
    Refused,
}

pub fn startup_gate(bind: &str, has_token: bool, allow_insecure: bool) -> StartupGate {
    if has_token || is_loopback_bind(bind) {
        StartupGate::Allowed
    } else if allow_insecure {
        StartupGate::AllowedInsecure
    } else {
        StartupGate::Refused
    }
}

// ---------------------------------------------------------------------------
// 登录页
// ---------------------------------------------------------------------------

/// 登录页：宣纸底 + 朱砂印章品牌，纯内联，无外部资源、无 token、无书籍信息。
pub const LOGIN_HTML: &str = concat!(
    "<!DOCTYPE html>\n",
    "<html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\n",
    "<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n",
    "<title>墨澜工坊 · 登录</title>\n",
    "<style>\n",
    ":root{color-scheme:light dark}\n",
    "*{box-sizing:border-box}\n",
    "body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;padding:24px;\n",
    "background:radial-gradient(1100px 560px at 85% -10%,rgba(199,67,60,.08),transparent 60%),\n",
    "radial-gradient(900px 520px at -10% 110%,rgba(38,34,27,.06),transparent 55%),#F7F5F0;\n",
    "color:#26221B;font:15px/1.7 \"Noto Sans SC\",system-ui,-apple-system,\"Segoe UI\",sans-serif;\n",
    "-webkit-font-smoothing:antialiased}\n",
    ".card{width:min(92vw,380px);background:#FFFDF9;border:1px solid rgba(38,34,27,.08);border-radius:18px;\n",
    "padding:34px 30px 26px;box-shadow:0 1px 2px rgba(38,34,27,.05),0 14px 36px rgba(38,34,27,.09);\n",
    "animation:rise .45s cubic-bezier(.2,.8,.2,1) both}\n",
    "@keyframes rise{from{opacity:0;transform:translateY(10px)}to{opacity:1;transform:none}}\n",
    ".brand{display:flex;align-items:center;gap:13px;margin-bottom:8px}\n",
    ".seal{width:42px;height:42px;flex:none;border-radius:11px;\n",
    "background:linear-gradient(145deg,#D04A42,#B03A34);color:#FFF;\n",
    "display:flex;align-items:center;justify-content:center;\n",
    "font:700 22px/1 \"Noto Serif SC\",\"Songti SC\",\"STSong\",serif;\n",
    "box-shadow:0 4px 12px rgba(199,67,60,.32),inset 0 1px 0 rgba(255,255,255,.28)}\n",
    "h1{margin:0;font:700 21px/1.3 \"Noto Serif SC\",\"Songti SC\",\"STSong\",serif;letter-spacing:3px}\n",
    "p.sub{margin:0 0 24px;color:#8A8378;font-size:13px;letter-spacing:.5px}\n",
    "label{display:block;font-size:13px;margin-bottom:7px;color:#5C554A;font-weight:500}\n",
    "input[type=password]{width:100%;box-sizing:border-box;padding:11px 13px;font-size:15px;\n",
    "border:1px solid #DDD5C7;border-radius:10px;background:#FFF;color:inherit;outline:none;\n",
    "transition:border-color .15s,box-shadow .15s}\n",
    "input[type=password]:focus{border-color:#C7433C;box-shadow:0 0 0 3px rgba(199,67,60,.14)}\n",
    "button{width:100%;margin-top:16px;padding:11px 12px;font-size:15px;font-weight:600;letter-spacing:4px;\n",
    "border:0;border-radius:10px;background:#C7433C;color:#FFF;cursor:pointer;\n",
    "box-shadow:0 3px 10px rgba(199,67,60,.28);transition:background .15s,transform .1s,box-shadow .15s}\n",
    "button:hover:not(:disabled){background:#B03A34;box-shadow:0 5px 14px rgba(199,67,60,.34)}\n",
    "button:active:not(:disabled){transform:translateY(1px)}\n",
    "button:disabled{opacity:.6;cursor:default}\n",
    ".msg{margin-top:13px;font-size:13px;min-height:18px;color:#B03A34}\n",
    "@media (prefers-color-scheme:dark){\n",
    "body{background:radial-gradient(1100px 560px at 85% -10%,rgba(212,90,83,.10),transparent 60%),\n",
    "radial-gradient(900px 520px at -10% 110%,rgba(0,0,0,.35),transparent 55%),#12141A;color:#EDEFF2}\n",
    ".card{background:#1C1F27;border-color:rgba(255,255,255,.09);\n",
    "box-shadow:0 1px 2px rgba(0,0,0,.4),0 14px 36px rgba(0,0,0,.45)}\n",
    "input[type=password]{background:#14171E;border-color:#363D4D;color:#EDEFF2}\n",
    "input[type=password]:focus{border-color:#D45A53;box-shadow:0 0 0 3px rgba(212,90,83,.2)}\n",
    "p.sub{color:#8E96A5}label{color:#B6BECB}\n",
    "button{background:#D45A53}button:hover:not(:disabled){background:#C7433C}\n",
    ".msg{color:#E88D87}\n",
    "}\n",
    "@media (prefers-reduced-motion:reduce){.card{animation:none}}\n",
    "</style></head>\n",
    "<body>\n",
    "<div class=\"card\">\n",
    "  <div class=\"brand\"><div class=\"seal\">墨</div><h1>墨澜工坊</h1></div>\n",
    "  <p class=\"sub\">请输入访问令牌以继续</p>\n",
    "  <form id=\"f\">\n",
    "    <label for=\"t\">访问令牌</label>\n",
    "    <input id=\"t\" name=\"token\" type=\"password\" autocomplete=\"current-password\" autofocus>\n",
    "    <button id=\"b\" type=\"submit\">登录</button>\n",
    "  </form>\n",
    "  <div class=\"msg\" id=\"m\"></div>\n",
    "</div>\n",
    "<script>\n",
    "(function(){\n",
    "  var f=document.getElementById('f'),b=document.getElementById('b'),\n",
    "      m=document.getElementById('m'),t=document.getElementById('t');\n",
    "  f.addEventListener('submit',function(e){\n",
    "    e.preventDefault();\n",
    "    if(!t.value){m.textContent='请输入令牌';return;}\n",
    "    b.disabled=true;m.textContent='';\n",
    "    fetch('/auth/login',{method:'POST',headers:{'Content-Type':'application/json'},\n",
    "      body:JSON.stringify({token:t.value})}).then(function(r){\n",
    "        if(r.ok){location.replace('/');return null;}\n",
    "        return r.json().catch(function(){return{};}).then(function(j){\n",
    "          m.textContent=(j&&j.error)||'登录失败';b.disabled=false;\n",
    "        });\n",
    "      }).catch(function(){m.textContent='网络错误，请重试';b.disabled=false;});\n",
    "  });\n",
    "})();\n",
    "</script>\n",
    "</body></html>",
);

/// /health 最小响应体（不含书数/路径/token/渠道等任何隐私）。
/// db_ready 表示数据库可查询；DB 不可用时仍返回 200 但 ok=false，
/// 便于部署脚本判定“进程活着但存储不可用”。
pub fn health_body(db_ready: bool) -> serde_json::Value {
    serde_json::json!({
        "ok": db_ready,
        "service": "molan-server",
        "version": env!("CARGO_PKG_VERSION"),
        "dbReady": db_ready,
    })
}

// ---------------------------------------------------------------------------
// 单测
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn ct_eq_basic() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(ct_eq(b"", b""));
        assert!(ct_eq_str("token-123", "token-123"));
        assert!(!ct_eq_str("token-123", "token-124"));
    }

    #[test]
    fn token_len_bounds() {
        assert!(!token_len_ok(""));
        assert!(token_len_ok("a"));
        assert!(token_len_ok(&"x".repeat(MAX_TOKEN_LEN)));
        assert!(!token_len_ok(&"x".repeat(MAX_TOKEN_LEN + 1)));
    }

    #[test]
    fn fingerprint_changes_with_token() {
        assert_eq!(token_fingerprint("a"), token_fingerprint("a"));
        assert_ne!(token_fingerprint("a"), token_fingerprint("b"));
    }

    #[test]
    fn session_idle_expiry_without_renewal() {
        // 不续期：签发后超过 TTL 即失效，且过期条目被移除。
        let s = SessionStore::new(Duration::from_secs(100), Duration::from_secs(1000));
        let t0 = Instant::now();
        let id = s.issue_at(7, t0);
        assert!(s.validate_at(&id, 7, t0 + Duration::from_secs(10)));
        // 注意：上面这次命中会滑动续期，所以用独立会话验证空闲过期。
        let s2 = SessionStore::new(Duration::from_secs(100), Duration::from_secs(1000));
        let id2 = s2.issue_at(7, t0);
        assert!(!s2.validate_at(&id2, 7, t0 + Duration::from_secs(101)));
        assert_eq!(s2.len(), 0);
    }

    #[test]
    fn session_validate_renews_sliding_window() {
        // 每次命中都滑动续期，因此连续访问不会掉线。
        let s = SessionStore::new(Duration::from_secs(100), Duration::from_secs(1000));
        let t0 = Instant::now();
        let id = s.issue_at(7, t0);
        assert!(s.validate_at(&id, 7, t0 + Duration::from_secs(90)));
        // 原本 t0+100 就到期，但 t0+90 的访问把它续到 t0+190
        assert!(s.validate_at(&id, 7, t0 + Duration::from_secs(150)));
    }

    #[test]
    fn session_sliding_renewal_capped_by_absolute() {
        let s = SessionStore::new(Duration::from_secs(100), Duration::from_secs(150));
        let t0 = Instant::now();
        let id = s.issue_at(1, t0);
        assert!(s.validate_at(&id, 1, t0 + Duration::from_secs(90)));
        assert!(s.validate_at(&id, 1, t0 + Duration::from_secs(145)));
        assert!(!s.validate_at(&id, 1, t0 + Duration::from_secs(151)));
    }

    #[test]
    fn session_invalidated_when_token_changes() {
        let s = SessionStore::new(Duration::from_secs(100), Duration::from_secs(1000));
        let t0 = Instant::now();
        let fp_old = token_fingerprint("old-token");
        let fp_new = token_fingerprint("new-token");
        let id = s.issue_at(fp_old, t0);
        assert!(s.validate_at(&id, fp_old, t0 + Duration::from_secs(1)));
        assert!(!s.validate_at(&id, fp_new, t0 + Duration::from_secs(2)));
    }

    #[test]
    fn session_revoke_clear_gc() {
        let s = SessionStore::new(Duration::from_secs(10), Duration::from_secs(100));
        let t0 = Instant::now();
        let a = s.issue_at(1, t0);
        let _b = s.issue_at(1, t0);
        assert_eq!(s.len(), 2);
        s.revoke(&a);
        assert_eq!(s.len(), 1);
        assert_eq!(s.gc_at(t0 + Duration::from_secs(11)), 1);
        assert!(s.is_empty());
        s.issue_at(1, t0);
        s.clear();
        assert!(s.is_empty());
    }

    #[test]
    fn unknown_session_rejected() {
        let s = SessionStore::new(Duration::from_secs(100), Duration::from_secs(1000));
        assert!(!s.validate("does-not-exist", 0));
    }

    #[test]
    fn login_limiter_blocks_after_max() {
        let l = LoginLimiter::new(Duration::from_secs(300), 10);
        let t0 = Instant::now();
        for i in 0..10 {
            assert!(l.check_at("1.2.3.4", t0 + Duration::from_secs(i)).is_ok());
            l.record_failure_at("1.2.3.4", t0 + Duration::from_secs(i));
        }
        let err = l.check_at("1.2.3.4", t0 + Duration::from_secs(11));
        assert!(err.is_err());
        assert!(err.unwrap_err() >= 1);
        assert!(l.check_at("1.2.3.4", t0 + Duration::from_secs(320)).is_ok());
        assert!(l.check_at("5.6.7.8", t0 + Duration::from_secs(11)).is_ok());
    }

    #[test]
    fn login_limiter_clear_on_success() {
        let l = LoginLimiter::new(Duration::from_secs(300), 3);
        let t0 = Instant::now();
        for i in 0..3 {
            l.record_failure_at("k", t0 + Duration::from_secs(i));
        }
        assert!(l.check_at("k", t0 + Duration::from_secs(3)).is_err());
        l.clear("k");
        assert!(l.check_at("k", t0 + Duration::from_secs(4)).is_ok());
    }

    #[test]
    fn cookie_has_required_attributes() {
        let c = session_cookie("abc123", 3600, false);
        assert!(c.contains("molan_session=abc123"));
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Strict"));
        assert!(c.contains("Path=/"));
        assert!(c.contains("Max-Age=3600"));
        // 纯 HTTP 局域网部署绝不能带 Secure，否则浏览器直接丢弃 cookie
        assert!(!c.contains("Secure"));
        assert!(clear_cookie().contains("Max-Age=0"));
    }

    #[test]
    fn cookie_secure_only_on_trusted_https() {
        let c = session_cookie("abc123", 3600, true);
        assert!(c.contains("Secure"));
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Strict"));
        assert!(secure_cookies_for("https"));
        assert!(!secure_cookies_for("http"));
        assert!(!secure_cookies_for("HTTPS ")); // 调用方负责归一化，这里只验精确匹配
        assert!(secure_cookies_for("HTTPS"));
    }

    #[test]
    fn trusted_scheme_defaults_to_http() {
        // 未设置 MOLAN_TRUSTED_SCHEME 时必须退回 http，而不是猜 https
        let saved = std::env::var("MOLAN_TRUSTED_SCHEME").ok();
        std::env::remove_var("MOLAN_TRUSTED_SCHEME");
        assert_eq!(trusted_scheme_from_env(), "http");
        std::env::set_var("MOLAN_TRUSTED_SCHEME", "https");
        assert_eq!(trusted_scheme_from_env(), "https");
        std::env::set_var("MOLAN_TRUSTED_SCHEME", "  HTTPS  ");
        assert_eq!(trusted_scheme_from_env(), "https");
        match saved {
            Some(v) => std::env::set_var("MOLAN_TRUSTED_SCHEME", v),
            None => std::env::remove_var("MOLAN_TRUSTED_SCHEME"),
        }
    }

    #[test]
    fn cookie_parse_and_script_token() {
        let h = hm(&[("cookie", "a=1; molan_session=xyz; b=2")]);
        assert_eq!(cookie_value(&h, COOKIE_NAME).as_deref(), Some("xyz"));
        assert!(has_session_cookie(&h));
        assert!(cookie_value(&h, "missing").is_none());

        let two = hm(&[("cookie", "x=1"), ("cookie", "molan_session=second")]);
        assert_eq!(cookie_value(&two, COOKIE_NAME).as_deref(), Some("second"));

        let t = hm(&[("x-molan-token", "secret")]);
        assert_eq!(script_token(&t), Some("secret"));
        assert!(script_token(&hm(&[])).is_none());
    }

    #[test]
    fn origin_same_host_allowed() {
        assert!(same_origin(Some("127.0.0.1:17381"), None, None, "http"));
        assert!(same_origin(
            Some("127.0.0.1:17381"),
            Some("http://127.0.0.1:17381"),
            None,
            "http"
        ));
        assert!(same_origin(
            Some("192.168.1.100:17381"),
            Some("http://192.168.1.100:17381"),
            None,
            "http"
        ));
        assert!(same_origin(
            Some("nas.example.com:17381"),
            None,
            Some("http://nas.example.com:17381/settings"),
            "http"
        ));
        // Host 无端口时按可信 scheme 的默认端口补全（http -> 80）
        assert!(same_origin(
            Some("nas.example.com"),
            Some("http://nas.example.com"),
            None,
            "http"
        ));
    }

    /// 父代理复核发现：HTTPS 反代下 Host 常不带端口，
    /// 而 Origin 是 https 默认 443；旧实现把 Host 无端口当 80，会误拒合法登录。
    #[test]
    fn origin_https_behind_reverse_proxy_without_port() {
        // 典型反代：外部 https://site，Host: site（无端口）
        assert!(same_origin(
            Some("site"),
            Some("https://site"),
            None,
            "https"
        ));
        assert!(same_origin(
            Some("site"),
            Some("https://site:443"),
            None,
            "https"
        ));
        assert!(same_origin(
            Some("site"),
            None,
            Some("https://site/login"),
            "https"
        ));
        // 带非默认端口时仍按显式端口比较
        assert!(same_origin(
            Some("site:8443"),
            Some("https://site:8443"),
            None,
            "https"
        ));
        assert!(!same_origin(
            Some("site:8443"),
            Some("https://site"),
            None,
            "https"
        ));
    }

    /// 可信 scheme 为 https 时，http 来源（降级/混淆）必须拒绝。
    #[test]
    fn origin_scheme_downgrade_rejected() {
        assert!(!same_origin(
            Some("site"),
            Some("http://site"),
            None,
            "https"
        ));
        assert!(!same_origin(
            Some("site"),
            Some("https://site"),
            None,
            "http"
        ));
    }

    #[test]
    fn origin_cross_site_rejected() {
        assert!(!same_origin(
            Some("127.0.0.1:17381"),
            Some("http://evil.example"),
            None,
            "http"
        ));
        assert!(!same_origin(
            Some("127.0.0.1:17381"),
            Some("http://127.0.0.1:9999"),
            None,
            "http"
        ));
        assert!(!same_origin(
            Some("127.0.0.1:17381"),
            Some("null"),
            None,
            "http"
        ));
        assert!(!same_origin(
            Some("127.0.0.1:17381"),
            None,
            Some("http://evil.example/x"),
            "http"
        ));
        assert!(!same_origin(
            None,
            Some("http://127.0.0.1:17381"),
            None,
            "http"
        ));
        // 反代场景下的跨站
        assert!(!same_origin(
            Some("site"),
            Some("https://evil.example"),
            None,
            "https"
        ));
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback_bind("127.0.0.1"));
        assert!(is_loopback_bind("127.0.0.5"));
        assert!(is_loopback_bind("::1"));
        assert!(is_loopback_bind("localhost"));
        assert!(!is_loopback_bind("0.0.0.0"));
        assert!(!is_loopback_bind("192.168.1.100"));
        assert!(!is_loopback_bind("::"));
    }

    #[test]
    fn startup_gate_matrix() {
        assert_eq!(
            startup_gate("127.0.0.1", false, false),
            StartupGate::Allowed
        );
        assert_eq!(startup_gate("0.0.0.0", true, false), StartupGate::Allowed);
        assert_eq!(startup_gate("0.0.0.0", false, false), StartupGate::Refused);
        assert_eq!(
            startup_gate("0.0.0.0", false, true),
            StartupGate::AllowedInsecure
        );
        assert_eq!(startup_gate("127.0.0.1", false, true), StartupGate::Allowed);
    }

    #[test]
    fn login_page_has_no_secret_surface() {
        assert!(LOGIN_HTML.contains("/auth/login"));
        assert!(!LOGIN_HTML.contains("__MOLAN_TOKEN__"));
        assert!(!LOGIN_HTML.contains("MOLAN_AUTH_TOKEN"));
        // 登录页不得内联任何会话标识或 cookie 名（Cookie 由服务端响应头下发）。
        assert!(!LOGIN_HTML.contains("molan_session"));
        assert!(!LOGIN_HTML.contains("document.cookie"));
    }

    #[test]
    fn health_body_minimal() {
        let v = health_body(true);
        assert_eq!(v["ok"], true);
        assert_eq!(v["service"], "molan-server");
        assert_eq!(v["dbReady"], true);
        let s = v.to_string().to_ascii_lowercase();
        for forbidden in ["books", "root", "path", "token", "channel", "session"] {
            assert!(
                !s.contains(forbidden),
                "health should not contain {forbidden}"
            );
        }
    }

    #[test]
    fn health_body_reports_db_not_ready() {
        let v = health_body(false);
        assert_eq!(v["ok"], false);
        assert_eq!(v["dbReady"], false);
        // 仍不得泄露隐私字段
        let s = v.to_string().to_ascii_lowercase();
        for forbidden in ["books", "root", "path", "token", "channel", "session"] {
            assert!(
                !s.contains(forbidden),
                "health should not contain {forbidden}"
            );
        }
    }
}
