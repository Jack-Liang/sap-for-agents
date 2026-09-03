//! ADT（ABAP Development Tooling）REST 代理：把 ICF 上的 `/sap/bc/adt/**`
//! 服务以统一网关端点 `/api/adt/**` 透传，Basic 认证与 CSRF 由网关统一处理。
//!
//! 背景：dump 正文、类/程序源码读写等开发对象操作在 ADT REST API
//! （Eclipse ADT 用的同一套）上是同步、结构化、零 SAP 对象的，但需要
//! ICF 端口 + Basic 认证 + 写操作的 CSRF token。本模块让调用方只面对网关：
//!
//! - 读操作（GET）：直接透传；
//! - 写操作（POST/PUT/DELETE/PATCH）：自动获取并携带 `X-CSRF-Token` 与
//!   会话 Cookie，遇 403 自动刷新重试一次。token 从目标 URL 自身签发
//!   （GET + Fetch 头，ICF 对 404 资源同样签发，无需依赖特定服务存在）；
//! - `ETag` / `Last-Modified` / `If-Match` / `If-None-Match` 双向透传，
//!   支持调用方做乐观锁写流程（如类源码更新）。
//!
//! 路径转发使用**原始 percent-encoded 路径**（axum 的通配捕获会解码，
//! 解码后的 `/` 与调用方刻意编码的 `%2F` 无法区分，重编码会破坏段结构），
//! 解码仅用于安全校验（拒绝 `..` 段与控制字符）。
//!
//! ADT 自身的 HTTP 状态码与响应体原样透传（XML）；网关侧故障（未启用 /
//! 不可达 / 超时）才走 JSON 错误契约。

use crate::error::RfcError;
use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, Uri};
use axum::response::Response;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// ADT 全局配置（启动期由 [`init`] 写入；未启用时为 None）
struct AdtConfig {
    /// 基地址，如 `http://localhost:50000`（不带尾斜杠）
    base_url: String,
    /// 预组装的 `Basic xxx` Authorization 头
    basic: String,
    client: reqwest::Client,
}

static ADT: OnceLock<AdtConfig> = OnceLock::new();

/// CSRF token 缓存：写操作共用一个会话 token，过期或 403 后刷新
struct CsrfToken {
    token: String,
    /// 会话 Cookie（sap-usercontext 等），随 token 一起使用
    cookie: Option<String>,
    fetched: Instant,
}

static CSRF: OnceLock<Mutex<Option<CsrfToken>>> = OnceLock::new();
const CSRF_TTL: Duration = Duration::from_secs(30 * 60);

/// 启动期初始化 ADT 代理（main 调一次）。`base_url=None` 表示未启用。
pub fn init(base_url: Option<String>, user: &str, passwd: &str, timeout: Duration) {
    if let Some(base) = base_url {
        use base64::Engine;
        let basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", user, passwd))
        );
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_default();
        let base = base.trim_end_matches('/').to_string();
        tracing::info!(base = %base, "ADT 代理已启用（/api/adt/**）");
        let _ = ADT.set(AdtConfig {
            base_url: base,
            basic,
            client,
        });
    } else {
        tracing::info!("ADT 代理未启用（SAP_ADT_BASE_URL 为空）");
    }
}

/// percent-decode（仅用于校验；转发用原始路径）。
fn percent_decode(s: &str) -> Result<Vec<u8>, RfcError> {
    fn hex_val(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let (h, l) = (bytes.get(i + 1), bytes.get(i + 2));
            match (h.and_then(|&b| hex_val(b)), l.and_then(|&b| hex_val(b))) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    return Err(RfcError {
                        code: -1,
                        status: 400,
                        message: format!("ADT 路径含非法百分号编码: {}", s),
                        key: "ADT_PATH_INVALID".into(),
                    })
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// 校验原始（percent-encoded）路径：解码后拒绝 `..` 段与控制字符。
/// 返回原样路径供转发。
fn validate_raw_path(raw: &str) -> Result<&str, RfcError> {
    let bad = |msg: &str| RfcError {
        code: -1,
        status: 400,
        message: format!("ADT 路径非法（{}）: {}", msg, raw),
        key: "ADT_PATH_INVALID".into(),
    };
    let decoded = percent_decode(raw)?;
    let decoded = std::str::from_utf8(&decoded).map_err(|_| bad("非 UTF-8 路径"))?;
    if decoded.chars().any(|c| c.is_control()) {
        return Err(bad("含控制字符"));
    }
    // 防穿越：ADT 路径不存在合法的 ".." 段
    if decoded.split('/').any(|seg| seg == "..") {
        return Err(bad("含 .. 段"));
    }
    Ok(raw)
}

/// 取缓存的 CSRF token（未失效直接用）。token 从 `url` 自身签发：
/// 对该 URL 发 GET + Fetch 头，ICF 对已认证请求都会返回 token（含 404 资源）。
async fn csrf_token(url: &str, force_refresh: bool) -> Result<Option<(String, Option<String>)>, RfcError> {
    let cfg = ADT.get().expect("init 已确保配置存在");
    let cache = CSRF.get_or_init(|| Mutex::new(None));
    if !force_refresh {
        if let Ok(guard) = cache.lock() {
            if let Some(t) = guard.as_ref() {
                if t.fetched.elapsed() < CSRF_TTL {
                    return Ok(Some((t.token.clone(), t.cookie.clone())));
                }
            }
        }
    }

    let resp = cfg
        .client
        .get(url)
        .header("Authorization", &cfg.basic)
        .header("X-CSRF-Token", "Fetch")
        .header("Accept", "*/*")
        .send()
        .await
        .map_err(adt_unreachable)?;

    let token = resp
        .headers()
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let cookie = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|c| c.split(';').next().map(str::to_owned))
        .reduce(|a, b| format!("{}; {}", a, b));

    match token {
        Some(token) if !token.is_empty() && token != "Fetch" => {
            if let Ok(mut guard) = cache.lock() {
                *guard = Some(CsrfToken {
                    token: token.clone(),
                    cookie: cookie.clone(),
                    fetched: Instant::now(),
                });
            }
            tracing::debug!("ADT CSRF token 已获取并缓存");
            Ok(Some((token, cookie)))
        }
        _ => Err(RfcError {
            code: -1,
            status: 502,
            message: "ADT CSRF token 获取失败（响应缺少 X-CSRF-Token 头）".into(),
            key: "ADT_CSRF_FAILED".into(),
        }),
    }
}

fn adt_unreachable(e: reqwest::Error) -> RfcError {
    if e.is_timeout() {
        RfcError {
            code: -1,
            status: 504,
            message: format!("ADT 请求超时: {}", e),
            key: "ADT_TIMEOUT".into(),
        }
    } else {
        RfcError {
            code: -1,
            status: 502,
            message: format!("ADT 服务不可达: {}", e),
            key: "ADT_UNREACHABLE".into(),
        }
    }
}

fn is_write_method(m: &Method) -> bool {
    matches!(*m, Method::POST | Method::PUT | Method::DELETE | Method::PATCH)
}

/// 内部 GET 通道：供网关自身的结构化端点（`/api/dumps/**`）复用 ADT 配置、
/// 认证与错误契约，避免各自直接拼 URL。
///
/// `rel_raw` 是 `/sap/bc/adt/` 之后的相对路径（原始 percent-encoded 形式，
/// 可带 query）。返回 `(HTTP 状态码, Content-Type, body)`，状态与内容由
/// ADT 原样给出，调用方自行解读。
pub(crate) async fn adt_get_raw(
    rel_raw: &str,
    accept: &str,
) -> Result<(u16, Option<String>, Bytes), RfcError> {
    let cfg = ADT.get().ok_or_else(|| RfcError {
        code: -1,
        status: 503,
        message: "ADT 代理未启用（设置 SAP_ADT_BASE_URL 后重启）".into(),
        key: "ADT_DISABLED".into(),
    })?;
    let rel_raw = validate_raw_path(rel_raw)?;
    let url = format!("{}/sap/bc/adt/{}", cfg.base_url, rel_raw);

    let resp = cfg
        .client
        .get(&url)
        .header("Authorization", &cfg.basic)
        .header("Accept", accept)
        .send()
        .await
        .map_err(|e| {
            metrics::counter!("adt_calls_total", "method" => "GET", "result" => "err").increment(1);
            adt_unreachable(e)
        })?;

    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = resp.bytes().await.map_err(|e| {
        metrics::counter!("adt_calls_total", "method" => "GET", "result" => "err").increment(1);
        adt_unreachable(e)
    })?;
    metrics::counter!("adt_calls_total", "method" => "GET", "result" => "ok").increment(1);
    Ok((status, content_type, body))
}

/// `ANY /api/adt/{*path}` —— ADT REST 通用代理。
///
/// 透传规则：
/// - 请求：`Accept` / `Content-Type` / `If-Match` / `If-None-Match` 与 body 原样转发；
///   路径用**原始编码形式**转发（保留调用方的 `%20`/`%2F` 等编码意图）；
/// - 响应：状态码、`Content-Type`、`ETag`、`Last-Modified` 与 body 原样返回。
pub async fn adt_proxy(
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, RfcError> {
    let cfg = ADT.get().ok_or_else(|| RfcError {
        code: -1,
        status: 503,
        message: "ADT 代理未启用（设置 SAP_ADT_BASE_URL 后重启）".into(),
        key: "ADT_DISABLED".into(),
    })?;

    // 用原始（未解码）路径转发：通配捕获的解码形态无法区分字面 / 与编码 %2F
    let raw_path = uri.path();
    let prefix = "/api/adt/";
    let rel_raw = raw_path.strip_prefix(prefix).ok_or_else(|| RfcError {
        code: -1,
        status: 500,
        message: "ADT 代理路由前缀异常".into(),
        key: "ADT_INTERNAL".into(),
    })?;
    let rel_raw = validate_raw_path(rel_raw)?;
    let mut url = format!("{}/sap/bc/adt/{}", cfg.base_url, rel_raw);
    if let Some(q) = uri.query() {
        url.push('?');
        url.push_str(q);
    }

    let send = |csrf: Option<(String, Option<String>)>| {
        let mut req = cfg
            .client
            .request(method.clone(), &url)
            .header("Authorization", &cfg.basic)
            .header(
                "Accept",
                headers
                    .get("accept")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("*/*"),
            );
        if let Some(ct) = headers.get("content-type").and_then(|v| v.to_str().ok()) {
            req = req.header("Content-Type", ct);
        }
        // 乐观锁相关头双向透传（类源码更新流程需要）
        for h in ["if-match", "if-none-match"] {
            if let Some(v) = headers.get(h).and_then(|v| v.to_str().ok()) {
                req = req.header(h, v);
            }
        }
        if !body.is_empty() {
            req = req.body(body.clone());
        }
        if let Some((token, cookie)) = csrf {
            req = req.header("X-CSRF-Token", token);
            if let Some(c) = cookie {
                req = req.header("Cookie", c);
            }
        }
        req
    };

    let metrics_result = |ok: bool| {
        metrics::counter!(
            "adt_calls_total",
            "method" => method.as_str().to_owned(),
            "result" => if ok { "ok" } else { "err" }.to_owned()
        )
        .increment(1);
    };

    // 写方法先取 token（从目标 URL 签发）；读方法直接透传
    let mut csrf = if is_write_method(&method) {
        csrf_token(&url, false).await?
    } else {
        None
    };
    let mut resp = send(csrf.clone()).send().await.map_err(|e| {
        metrics_result(false);
        adt_unreachable(e)
    })?;

    // 403 通常是 token 过期：刷新后重试一次（仅写方法）
    if resp.status() == StatusCode::FORBIDDEN && is_write_method(&method) {
        tracing::debug!("ADT 写请求 403，刷新 CSRF token 后重试");
        if let Ok(Some(new)) = csrf_token(&url, true).await {
            csrf = Some(new);
            resp = send(csrf.clone()).send().await.map_err(|e| {
                metrics_result(false);
                adt_unreachable(e)
            })?;
        }
    }

    let status = resp.status();
    let mut builder = Response::builder().status(status);
    for h in ["content-type", "etag", "last-modified"] {
        if let Some(v) = resp.headers().get(h) {
            // 已知安全的少量透传头，构建失败视为网关内部错误
            builder = builder.header(
                HeaderName::from_static(h),
                v.clone(),
            );
        }
    }
    let body = resp.bytes().await.map_err(|e| {
        metrics_result(false);
        adt_unreachable(e)
    })?;
    metrics_result(status.is_success());

    builder
        .body(axum::body::Body::from(body))
        .map_err(|e| RfcError {
            code: -1,
            status: 500,
            message: format!("ADT 响应组装失败: {}", e),
            key: "ADT_INTERNAL".into(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_allows_normal_paths_verbatim() {
        // 原样透传：合法路径与编码（%20/%2F）不做任何改动
        assert_eq!(validate_raw_path("runtime/dumps").unwrap(), "runtime/dumps");
        assert_eq!(
            validate_raw_path("runtime/dump/abc%20def/formatted").unwrap(),
            "runtime/dump/abc%20def/formatted"
        );
        assert_eq!(
            validate_raw_path("oo/classes/obj%2Fname/source/main").unwrap(),
            "oo/classes/obj%2Fname/source/main"
        );
    }

    #[test]
    fn validate_rejects_traversal_and_control() {
        // %2e%2e 解码后是 .. 段 → 拒绝
        assert!(validate_raw_path("runtime/%2e%2e/etc").is_err());
        assert!(validate_raw_path("runtime/..%2F..%2Fetc").is_err());
        // 字面 .. 同样拒绝
        assert!(validate_raw_path("runtime/../../etc").is_err());
        // 控制字符（%00 解码后）
        assert!(validate_raw_path("a%00b").is_err());
        // 非法百分号编码
        assert!(validate_raw_path("a%zzb").is_err());
    }
}
