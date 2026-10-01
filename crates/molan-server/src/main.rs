// molan-server: 离线复刻版 Rust 后端。静态托管 + NDJSON IPC（协议与 Node 版一致）。
mod auth;
mod handlers;
mod web;

use axum::{
    body::Body,
    extract::{ConnectInfo, Path as AxPath, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
    Router,
};
use molan_core::db::Db;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

pub struct AppState {
    pub db: Db,
    pub web_dir: PathBuf,
    pub web_dir_canon: PathBuf,
    pub root: PathBuf,
    // None = 未配置 token（loopback 本地模式，行为与旧版一致）。
    pub auth_token: Option<String>,
    // G 新增（契约 F02）：会话表与登录限速。
    pub sessions: auth::SessionStore,
    pub login_limiter: auth::LoginLimiter,
    // 可信部署 scheme（MOLAN_TRUSTED_SCHEME，默认 http）。
    // 用于同源比较的默认端口与 Secure cookie 决策，不盲信 X-Forwarded-Proto。
    pub trusted_scheme: String,
    pub secure_cookies: bool,
}

pub(crate) type SharedState = Arc<AppState>;

impl AppState {
    /// 是否启用了访问控制（配置了 MOLAN_AUTH_TOKEN）。
    pub fn auth_required(&self) -> bool {
        self.auth_token.is_some()
    }

    fn expected_fp(&self) -> u64 {
        self.auth_token
            .as_deref()
            .map(auth::token_fingerprint)
            .unwrap_or(0)
    }

    /// 会话 Cookie 或 X-Molan-Token 任一有效即通过；未配置 token 时恒通过。
    pub fn authorized(&self, headers: &HeaderMap) -> bool {
        let expected = match self.auth_token.as_deref() {
            Some(t) => t,
            None => return true,
        };
        if let Some(sid) = auth::cookie_value(headers, auth::COOKIE_NAME) {
            if self.sessions.validate(&sid, self.expected_fp()) {
                return true;
            }
        }
        if let Some(provided) = auth::script_token(headers) {
            // 脚本通道：常量时间比较，保留旧版 X-Molan-Token 兼容。
            if auth::ct_eq_str(provided, expected) {
                return true;
            }
        }
        false
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

pub(crate) fn plain(status: StatusCode, msg: &'static str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(msg))
        .unwrap()
}

fn json_response(status: StatusCode, v: serde_json::Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(v.to_string()))
        .unwrap()
}

pub(crate) fn redirect_to_login() -> Response {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, "/login")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(""))
        .unwrap()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .or_else(|| std::env::args().nth(1).and_then(|a| a.parse().ok()))
        .unwrap_or(17381);
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1".into());
    let root = PathBuf::from(std::env::var("MOLAN_ROOT").unwrap_or_else(|_| ".".into()));
    let data_dir = root.join("data");
    let seed = data_dir.join("writerx.seed.db");
    let db = Db::open(&data_dir, Some(&seed)).expect("打开数据库失败");
    let auth_token = std::env::var("MOLAN_AUTH_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());

    // ---- 启动安全门：无 token 且非 loopback 必须显式 override，绝不静默开放 ----
    let allow_insecure = matches!(
        std::env::var("MOLAN_ALLOW_INSECURE")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("on")
    );
    match auth::startup_gate(&bind, auth_token.is_some(), allow_insecure) {
        auth::StartupGate::Refused => {
            eprintln!(
                "[启动失败] BIND={} 不是 loopback 且未设置 MOLAN_AUTH_TOKEN，拒绝无鉴权对外监听。\n\
                 解决其一：\n\
                 1) 设置 MOLAN_AUTH_TOKEN=<强随机令牌> 后重启；\n\
                 2) 绑定 loopback（BIND=127.0.0.1）；\n\
                 3) 确认风险后显式设置 MOLAN_ALLOW_INSECURE=1（仍会打印警告）。",
                bind
            );
            std::process::exit(2);
        }
        auth::StartupGate::AllowedInsecure => {
            tracing::warn!(
                "⚠ 未设置 MOLAN_AUTH_TOKEN 且 BIND={} 对外监听：服务无访问控制，任何能访问该端口的人都可读写书稿。已由 MOLAN_ALLOW_INSECURE=1 显式放行。",
                bind
            );
        }
        auth::StartupGate::Allowed => {}
    }

    if let Some(t) = auth_token.as_deref() {
        if t.len() > auth::MAX_TOKEN_LEN {
            tracing::warn!(
                "MOLAN_AUTH_TOKEN 长度 {} 超过登录上限 {}，浏览器登录会被拒绝（脚本通道不受影响）。",
                t.len(),
                auth::MAX_TOKEN_LEN
            );
        }
    }

    // 可信 scheme：只有部署方显式声明才生效；默认 http（局域网直连）。
    let trusted_scheme = auth::trusted_scheme_from_env();
    if trusted_scheme == "https" {
        tracing::info!("MOLAN_TRUSTED_SCHEME=https：会话 Cookie 将带 Secure，同源比较按 443。");
    }

    // 静态目录：MOLAN_WEB_DIR 优先（新前端 frontend/dist 或旧静态树回退），否则 $MOLAN_ROOT/web
    let web_dir = web::web_dir(&root);
    let web_dir_canon = web_dir.canonicalize().unwrap_or_else(|_| web_dir.clone());
    let state: SharedState = Arc::new(AppState {
        db,
        web_dir,
        web_dir_canon,
        root: root.clone(),
        auth_token,
        sessions: auth::SessionStore::with_defaults(),
        login_limiter: auth::LoginLimiter::with_defaults(),
        trusted_scheme: trusted_scheme.clone(),
        secure_cookies: auth::secure_cookies_for(&trusted_scheme),
    });

    // 重启恢复（交接 §8.6）：已定稿但记忆仍 pending 的章补发抽取（幂等、在飞去重）
    handlers::stream::chapter_service::spawn_boot_memory_sweep(state.clone());
    // 定期清理过期会话，避免长期运行内存增长。
    {
        let st = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
            loop {
                tick.tick().await;
                let n = st.sessions.gc();
                if n > 0 {
                    tracing::debug!("清理过期会话 {} 条", n);
                }
            }
        });
    }

    let app = Router::new()
        .route("/login", get(login_page))
        .route("/auth/login", post(login_submit))
        .route("/health", get(health))
        .route("/ipc/:cmd", post(ipc_handler))
        .fallback(get(web::static_handler))
        .with_state(state.clone());

    let addr = format!("{}:{}", bind, port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("端口绑定失败");
    if state.auth_required() {
        tracing::info!("墨澜工坊 Rust 版已启动: http://{} （已启用登录鉴权）", addr);
    } else {
        tracing::info!("墨澜工坊 Rust 版已启动: http://{} （未启用鉴权）", addr);
    }
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .unwrap();
}

/// 优雅停机：SIGTERM/SIGINT → 停止接单、取消在飞流（残稿由服务端兜底入正文待审）、
/// 等在飞连接收口（30s 宽限后强制退出）。部署重启不再产生「无残稿的中断」。
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let term = async {
        #[cfg(unix)]
        {
            let mut s = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("注册 SIGTERM 失败");
            s.recv().await;
        }
        #[cfg(not(unix))]
        std::future::pending::<()>().await;
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("收到停机信号：停止接单并取消在飞流（残稿自动存正文待审）");
    crate::handlers::stream::chat::shutdown_cancel_all();
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        tracing::warn!("停机宽限 30s 到，强制退出");
        std::process::exit(0);
    });
}

/// GET /login：公开登录页；已认证则回主页。
async fn login_page(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !state.auth_required() || state.authorized(&headers) {
        return Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "/")
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from(""))
            .unwrap();
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(
            header::CACHE_CONTROL,
            "no-store, no-cache, must-revalidate, max-age=0",
        )
        .header("Pragma", "no-cache")
        .body(Body::from(auth::LOGIN_HTML))
        .unwrap()
}

/// GET /health：公开最小存活信息，不含任何隐私（deploy/rollback 也不需秘密）。
/// 额外做一次轻量 DB 探针：进程活着但存储不可用时要能被部署脚本识别。
async fn health(State(state): State<SharedState>) -> Response {
    let db_ready = state.db.q_json("SELECT 1 AS ok", &[]).is_ok();
    json_response(StatusCode::OK, auth::health_body(db_ready))
}

/// POST /auth/login：校验 token，签发 HttpOnly + SameSite=Strict 会话 Cookie。
async fn login_submit(
    State(state): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // 同源防线：浏览器带 Origin/Referer 时必须与 Host 同源。
    if !auth::same_origin(
        header_str(&headers, "host"),
        header_str(&headers, "origin"),
        header_str(&headers, "referer"),
        &state.trusted_scheme,
    ) {
        return json_response(
            StatusCode::FORBIDDEN,
            serde_json::json!({"ok": false, "error": "来源不被信任"}),
        );
    }

    if !state.auth_required() {
        // 本地未启用鉴权：无需登录，明确告知而不是假装成功。
        return json_response(
            StatusCode::OK,
            serde_json::json!({"ok": true, "authRequired": false}),
        );
    }

    let key = peer.ip().to_string();
    if let Err(retry) = state.login_limiter.check(&key) {
        let mut resp = json_response(
            StatusCode::TOO_MANY_REQUESTS,
            serde_json::json!({"ok": false, "error": "尝试过于频繁，请稍后再试"}),
        );
        if let Ok(v) = header::HeaderValue::from_str(&retry.to_string()) {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        return resp;
    }

    let parsed = serde_json::from_slice::<serde_json::Value>(&body).ok();
    let provided = parsed
        .as_ref()
        .and_then(|v| v.get("token"))
        .and_then(|t| t.as_str());

    let generic_fail = || {
        json_response(
            StatusCode::UNAUTHORIZED,
            serde_json::json!({"ok": false, "error": "认证失败"}),
        )
    };

    let provided = match provided {
        Some(p) if auth::token_len_ok(p) => p,
        // 缺失、非字符串或超长一律拒绝；响应不回显输入。
        _ => {
            state.login_limiter.record_failure(&key);
            return generic_fail();
        }
    };

    let expected = state.auth_token.as_deref().unwrap_or("");
    if !auth::ct_eq_str(provided, expected) {
        state.login_limiter.record_failure(&key);
        return generic_fail();
    }

    state.login_limiter.clear(&key);
    let sid = state.sessions.issue(auth::token_fingerprint(expected));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header(
            header::SET_COOKIE,
            auth::session_cookie(&sid, state.sessions.ttl_secs(), state.secure_cookies),
        )
        .body(Body::from(serde_json::json!({"ok": true}).to_string()))
        .unwrap()
}

async fn ipc_handler(
    State(state): State<SharedState>,
    AxPath(cmd): AxPath<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if state.auth_required() {
        // cookie 通道的同源纵深防御；脚本通道（X-Molan-Token）通常不带 Origin。
        if !auth::same_origin(
            header_str(&headers, "host"),
            header_str(&headers, "origin"),
            header_str(&headers, "referer"),
            &state.trusted_scheme,
        ) {
            return plain(StatusCode::FORBIDDEN, "forbidden");
        }
        if !state.authorized(&headers) {
            return plain(StatusCode::UNAUTHORIZED, "unauthorized");
        }
    }
    let args: serde_json::Value = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|b: serde_json::Value| b.get("args").cloned())
        .unwrap_or(serde_json::json!({}));
    // NDJSON 流式响应：handler 通过 channel 写事件，最后写 {"r":..} 或 {"err":..}
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);
    let st = state.clone();
    let cmd_clone = cmd.clone();
    tokio::spawn(async move {
        let result = handlers::dispatch(&st, &cmd_clone, &args, &tx).await;
        let final_line = match result {
            Ok(Some(v)) => format!("{}\n", serde_json::json!({"r": v})),
            Ok(None) => String::new(), // handler 已自行写完（流式）
            Err(e) => format!(
                "{}\n",
                serde_json::json!({"err": {"message": e.to_string()}})
            ),
        };
        if !final_line.is_empty() {
            let _ = tx.send(final_line).await;
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = Body::from_stream(stream.map(Ok::<_, std::convert::Infallible>));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header(header::CACHE_CONTROL, "no-store")
        .header("X-Accel-Buffering", "no")
        .body(body)
        .unwrap()
}

// tokio-stream 依赖
use futures_util::StreamExt as _;
