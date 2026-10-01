//! 静态托管：新前端（Paper Studio）与旧编译包共用同一端口、同一鉴权。
//!
//! - Web 目录：`MOLAN_WEB_DIR`（绝对路径，或相对 `MOLAN_ROOT`）优先，否则 `$MOLAN_ROOT/web`；
//! - 入口 HTML 带 `<meta name="molan-app" content="paper-studio">` → 原样返回，**不注入**旧脚本；
//!   其他 HTML（旧静态树回退）按原行为注入 glue/pipeline_ui/agent_ui；
//! - `/assets/` 下带内容 hash 的资源长缓存 immutable（字体短缓存）；HTML 与其余资源 no-store，
//!   保证新 HTML 总是引用与之匹配的新资源名。
use crate::{plain, redirect_to_login, SharedState};
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode, Uri},
    response::Response,
};
use std::path::{Path, PathBuf};

/// 新前端入口标记：出现即视为 Paper Studio，不再注入旧注入脚本。
pub const NEW_UI_MARKER: &str = r#"<meta name="molan-app" content="paper-studio">"#;

pub fn web_dir(root: &Path) -> PathBuf {
    match std::env::var("MOLAN_WEB_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        Some(v) => {
            let p = PathBuf::from(v.trim());
            if p.is_absolute() {
                p
            } else {
                root.join(p)
            }
        }
        None => root.join("web"),
    }
}

/// 旧静态树才注入旧脚本；新入口原样返回。返回 (html, 是否注入)。
pub fn prepare_html(html: String, build_ts: i64) -> (String, bool) {
    if html.contains(NEW_UI_MARKER) || html.contains("__WRITERX_GLUE__") {
        return (html, false);
    }
    let inject = format!(
        "<script>window.__WX_BUILD__=\"{}\";{}</script><script>{}</script><script>{}</script>",
        build_ts,
        include_str!("glue.js"),
        include_str!("pipeline_ui.js"),
        include_str!("agent_ui.js")
    );
    (
        html.replacen("<head>", &format!("<head>{}", inject), 1),
        true,
    )
}

pub async fn static_handler(
    State(state): State<SharedState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let path = percent_decode(uri.path());
    let path = if path == "/" || path.is_empty() {
        "/index.html".to_string()
    } else {
        path
    };
    // 启用鉴权时，静态资源（含主页）一律需要凭据；登录页/health 走各自路由，不在此处。
    if state.auth_required() && !state.authorized(&headers) {
        return redirect_to_login();
    }
    let fp = state.web_dir.join(path.trim_start_matches('/'));
    // 统一 canonical：Windows 下 PathBuf 带 \\?\ 前缀、大小写不一致，需双方同源比较
    let canonical = fp.canonicalize().unwrap_or(fp.clone());
    if !canonical.starts_with(&state.web_dir_canon) {
        return plain(StatusCode::FORBIDDEN, "forbidden");
    }
    if !canonical.is_file() {
        return plain(StatusCode::NOT_FOUND, "not found");
    }
    let content = match std::fs::read(&canonical) {
        Ok(c) => c,
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "read failed"),
    };
    let ext = canonical
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "html" {
        // 绝不注入服务器 token：匿名可访问的 HTML 一旦内联 token 就等于公开凭据（历史缺陷 F02）。
        let build_ts = std::fs::metadata(&canonical)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let (html, _) = prepare_html(String::from_utf8_lossy(&content).to_string(), build_ts);
        return Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(
                header::CACHE_CONTROL,
                "no-store, no-cache, must-revalidate, max-age=0",
            )
            .header("Pragma", "no-cache")
            .body(Body::from(html))
            .unwrap();
    }
    let mime = match ext.as_str() {
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" | "map" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "txt" | "md" => "text/plain",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    };
    // 缓存分级：/assets/ 下的字体短缓存（便于替换），其余 /assets/ 静态资源长缓存 immutable
    let cache_control = if path.starts_with("/assets/") {
        if matches!(ext.as_str(), "woff" | "woff2" | "ttf") {
            "public, max-age=86400"
        } else {
            "public, max-age=31536000, immutable"
        }
    } else {
        "no-store"
    };
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, cache_control)
        .header("X-Content-Type-Options", "nosniff")
        .body(Body::from(content))
        .unwrap()
}

/// 单字节 hex 值。
fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// 百分号解码。必须**按字节**解析：旧实现用字符串切片，'%' 后跟多字节字符时会 panic。
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut bytes = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(hi), Some(lo)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                bytes.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        bytes.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&bytes).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("/a%20b"), "/a b");
        assert_eq!(percent_decode("/%E4%B8%AD"), "/中");
        assert_eq!(percent_decode("/plain"), "/plain");
        assert_eq!(percent_decode(""), "");
    }

    #[test]
    fn percent_decode_multibyte_after_percent_does_not_panic() {
        assert_eq!(percent_decode("/%A中"), "/%A中");
        assert_eq!(percent_decode("/%中"), "/%中");
        assert_eq!(percent_decode("/%"), "/%");
        assert_eq!(percent_decode("/%A"), "/%A");
        assert_eq!(percent_decode("/%2"), "/%2");
        assert_eq!(percent_decode("/%zz"), "/%zz");
        assert_eq!(percent_decode("/%2G"), "/%2G");
    }

    #[test]
    fn percent_decode_boundary_and_truncation() {
        assert_eq!(percent_decode("abc%"), "abc%");
        assert_eq!(percent_decode("/%2F中"), "//中");
        assert_eq!(percent_decode("/%FF"), "/�");
    }

    #[test]
    fn new_ui_is_served_untouched_legacy_gets_injection() {
        let new_html = format!(
            "<html><head>{}<title>墨澜</title></head></html>",
            NEW_UI_MARKER
        );
        let (out, injected) = prepare_html(new_html.clone(), 1);
        assert_eq!(out, new_html);
        assert!(!injected);
        let (legacy, injected) =
            prepare_html("<html><head><title>旧</title></head></html>".into(), 7);
        assert!(injected);
        assert!(legacy.contains("window.__WX_BUILD__=\"7\""));
        assert!(!legacy.contains("MOLAN_AUTH_TOKEN"));
    }

    #[test]
    fn web_dir_env_resolution() {
        let root = Path::new("/srv/molan");
        assert_eq!(web_dir(root), root.join("web"));
    }
}
