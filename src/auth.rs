//! Bearer Token 认证（可选）。
//!
//! 设置环境变量 `SAP_API_KEY` 后，所有 `/api/*` 业务端点要求请求携带
//! `Authorization: Bearer <token>`；未设置则免鉴权（本机默认，向后兼容）。
//! 探针与文档端点（`/health` `/ready` `/` `/agents.md`）始终免鉴权。
//!
//! token 在启动期由 [`init`] 写入全局 `OnceLock`，运行期只读。
//! 比对用常量时间算法（[`ct_eq`]），避免按字符提前返回造成时序侧信道。

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::OnceLock;

/// 启动期写入的 API key（`None` = 未配置 = 免鉴权）。
static CONFIGURED_KEY: OnceLock<Option<String>> = OnceLock::new();

/// 启动期初始化（main 调一次）。重复调用忽略后续值（保留首次）。
pub fn init(key: Option<String>) {
    let _ = CONFIGURED_KEY.set(key);
}

/// 当前配置的 key。`None` 表示免鉴权模式（未设置 `SAP_API_KEY`）。
/// 返回 `&'static str`：值借自启动期写入的 `static OnceLock`，存活整个进程。
fn configured_key() -> Option<&'static str> {
    CONFIGURED_KEY.get().and_then(|opt| opt.as_deref())
}

/// 是否启用了 API 认证（设置了 `SAP_API_KEY`）。供首页等处展示认证状态。
pub fn is_enabled() -> bool {
    configured_key().is_some()
}

/// 从 `Authorization` 头值中提取 Bearer token。
/// scheme `Bearer` 大小写不敏感（RFC 7235）；token 前导空白被裁掉。
/// 返回 `None` 表示格式不符合 Bearer 方案（包括空 token）。
fn extract_bearer(header: &str) -> Option<&str> {
    let (scheme, rest) = header.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("Bearer") {
        let token = rest.trim_start();
        (!token.is_empty()).then_some(token)
    } else {
        None
    }
}

/// 常量时间字节比较：遍历到较长一方结尾，避免按字符提前返回造成时序泄露。
/// 长度差异通过额外置位捕获（即使内容前缀相同，长度不同也判定不等）。
fn ct_eq(provided: &[u8], expected: &[u8]) -> bool {
    let mut diff: u8 = 0;
    let max_len = provided.len().max(expected.len());
    for i in 0..max_len {
        let p = provided.get(i).copied().unwrap_or(0);
        let e = expected.get(i).copied().unwrap_or(0);
        diff |= p ^ e;
    }
    diff |= (provided.len() != expected.len()) as u8;
    diff == 0
}

/// 校验请求提供的 token 是否匹配配置的 key。
fn token_matches(provided: Option<&str>, expected: &str) -> bool {
    match provided {
        Some(p) => ct_eq(p.as_bytes(), expected.as_bytes()),
        None => false,
    }
}

/// axum 中间件：未配置 key → 放行；配置了 → 校验 `Authorization`，失败回 401。
pub async fn require_api_key(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // 未配置 key → 免鉴权（本机默认，向后兼容）
    let Some(expected) = configured_key() else {
        return next.run(req).await;
    };

    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(extract_bearer);

    if token_matches(provided, expected) {
        next.run(req).await
    } else {
        unauthorized()
    }
}

/// 构造 401 响应：带 `WWW-Authenticate: Bearer`，body 格式与 `RfcError` 对齐。
fn unauthorized() -> Response {
    let body = axum::Json(serde_json::json!({
        "error": {
            "code": 401,
            "message": "Missing or invalid Authorization header (expected 'Bearer <token>')",
            "key": "AUTH_INVALID"
        }
    }));
    let mut resp = (StatusCode::UNAUTHORIZED, body).into_response();
    resp.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_bearer_valid() {
        assert_eq!(extract_bearer("Bearer abc123"), Some("abc123"));
        // scheme 大小写不敏感（RFC 7235）
        assert_eq!(extract_bearer("bearer abc"), Some("abc"));
        assert_eq!(extract_bearer("BEARER  xyz"), Some("xyz")); // 多空格
    }

    #[test]
    fn extract_bearer_invalid() {
        assert_eq!(extract_bearer("Basic abc"), None, "非 Bearer 方案");
        assert_eq!(extract_bearer("Bearer"), None, "无空格分隔");
        assert_eq!(extract_bearer("Bearer "), None, "空 token");
        assert_eq!(extract_bearer(""), None, "空头");
        assert_eq!(extract_bearer("abc123"), None, "无 scheme");
    }

    #[test]
    fn token_matches_correct() {
        assert!(token_matches(Some("s3cret"), "s3cret"));
    }

    #[test]
    fn token_matches_wrong() {
        assert!(!token_matches(Some("wrong"), "s3cret"));
        assert!(!token_matches(Some(""), "s3cret"));
        assert!(!token_matches(None, "s3cret"), "缺 Authorization 应失配");
    }

    #[test]
    fn token_matches_length_difference() {
        // 长度不同必失配（即使一方是另一方前缀）
        assert!(!token_matches(Some("s3cret"), "s3cret-extra"));
        assert!(!token_matches(Some("s3cret-extra"), "s3cret"));
    }

    #[test]
    fn ct_eq_handles_equal_and_not() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"), "末位不同");
        assert!(!ct_eq(b"abc", b"ab"), "较短");
        assert!(!ct_eq(b"ab", b"abc"), "较长");
        assert!(!ct_eq(b"", b"abc"), "空 vs 非空");
        assert!(ct_eq(b"", b""), "双空相等");
    }
}
