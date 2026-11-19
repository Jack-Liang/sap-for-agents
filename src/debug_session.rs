//! 调试器专用**有状态** ADT 通道（v0.22，契约见 docs/DEBUGGER-CONTRACT.zh-CN.md）。
//!
//! 与 [`crate::adt`] 的共享代理互不接触（"CSRF 会话各自独立"原则的调试版）：
//! - 独立 cookie jar：`sap-contextid` 从建会话起全程保持，`set-cookie` 回写；
//! - **每请求**（含 GET 与 CSRF fetch）带 `X-sap-adt-sessiontype: stateful`——
//!   没有它 ICF 按无状态处理、请求结束即回收上下文，调试会话活不过下一个请求
//!   （P0 实测：缺它时 attach 后 stack 一律 `noSessionAttached`）；
//! - 会话内 CSRF：同会话签发，403 后同会话刷新重试一次。
//!
//! 调试器与 debuggee 必须分属两个会话（P0 实测同会话时触发请求占住上下文）——
//! 本模块只承载调试器侧；触发走 `/api/abap/run` / `/api/rfc`（各自的新会话）。
//!
//! 复用 [`AdtEndpoint`] 的基地址/鉴权/HTTP 客户端，但只取通道不取缓存：
//! OAuth 的 token 缓存与共享 CSRF 缓存都留在端点里，本会话的凭据语义仅多出
//! "cookie jar 必须续命"一条（OAuth 401 重授权后 contextid 仍有效，凭据头换新即可）。

use crate::adt::{is_write_method, validate_raw_path, AdtEndpoint};
use crate::error::RfcError;
use axum::body::Bytes;
use axum::http::Method;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 每请求必带的 stateful 标记（ICF 无它即按无状态回收上下文）。
const STATEFUL_HEADER: &str = "X-sap-adt-sessiontype";

/// 调试器操作请求的默认超时（send_once 兜底，listen/teardown 显式传值覆盖）。
/// 共享端点的全局超时是 60s（SAP_REQUEST_TIMEOUT_SECS 默认）——对调试这种
/// 交互式操作太长：对死端点（隧道断、ICM 假死）每次调用白等 60s 才报错，
/// agent 侧早超时了，观感即"挂死"（2026-11-19 黑洞实验实证）。调试操作本身
/// 都是毫秒级（attach/栈/变量/步进实测 <1s），15s 已是 10 倍余量。
const DEBUG_REQ_TIMEOUT: Duration = Duration::from_secs(15);

/// 调试器有状态会话：一条到 ICF 的长命 ABAP 上下文。
pub(crate) struct DebugSession {
    ep: Arc<AdtEndpoint>,
    /// 会话 cookie jar（`sap-contextid` / `SAP_SESSIONID` / `sap-usercontext`…）。
    cookies: Mutex<BTreeMap<String, String>>,
    /// 会话内 CSRF token（同会话签发；None = 尚未签发）。
    csrf: Mutex<Option<String>>,
    created: Instant,
    last_used: Mutex<Instant>,
}

/// [`DebugSession::request`] 的响应（ADT 原样透出，调用方自行解读）。
pub(crate) struct DebugResponse {
    pub status: u16,
    pub content_type: Option<String>,
    /// 同响应携带的 CSRF token（CSRF fetch 时取用）
    pub csrf_token: Option<String>,
    pub body: Bytes,
}

/// [`DebugSession::request`] 的请求侧可选项（正文/媒体类型/单请求超时）。
#[derive(Default)]
pub(crate) struct DebugRequest<'a> {
    pub accept: &'a str,
    pub content_type: Option<&'a str>,
    pub body: Option<&'a [u8]>,
    /// 覆盖调试器默认超时（[`DEBUG_REQ_TIMEOUT`] 15s；listen 传 listen_secs+20、
    /// teardown 传 10s）。
    pub timeout: Option<Duration>,
    /// 额外请求头（如 `Accept: application/vnd.sap.as+xml`）。
    pub extra_headers: Vec<(String, String)>,
}

impl DebugSession {
    /// 建立有状态上下文：GET discovery 拿 contextid → 同会话签发 CSRF。
    pub(crate) async fn open(ep: Arc<AdtEndpoint>) -> Result<Self, RfcError> {
        let s = Self {
            ep,
            cookies: Mutex::new(BTreeMap::new()),
            csrf: Mutex::new(None),
            created: Instant::now(),
            last_used: Mutex::new(Instant::now()),
        };
        // ① 建上下文（set-cookie 落 jar）
        s.request(Method::GET, "discovery", &DebugRequest {
            accept: "application/atomsvc+xml",
            ..Default::default()
        })
        .await?
        .expect_success("discovery", 200)?;
        // ② 同会话签发 CSRF（带 stateful 头——无状态 fetch 会把 stateful 上下文挤掉）
        s.refresh_csrf().await?;
        Ok(s)
    }

    /// 会话内签发/刷新 CSRF token（同会话 fetch——带 stateful 头 + jar cookie）。
    async fn refresh_csrf(&self) -> Result<String, RfcError> {
        let url = format!("{}/sap/bc/adt/debugger/breakpoints", self.ep.base_url());
        let cookie_hdr = self.cookie_header().await;
        let resp = self
            .send_once(
                &Method::GET,
                &url,
                &DebugRequest {
                    accept: "*/*",
                    extra_headers: vec![("X-CSRF-Token".into(), "Fetch".into())],
                    ..Default::default()
                },
                None,
                &[],
                None,
                None,
                &cookie_hdr,
            )
            .await?;
        let resp = self.finish(resp).await?;
        resp.expect_success("csrf fetch", 200)?;
        let token = resp
            .csrf_token
            .filter(|t| !t.is_empty() && t != "Fetch")
            .ok_or_else(|| RfcError {
                code: -1,
                status: 502,
                message: "调试会话 CSRF 签发失败（目标 URL 未回 token）".into(),
                key: "ADT_CSRF_FAILED".into(),
            })?;
        *self.csrf.lock().map_err(|_| poisoned())? = Some(token.clone());
        Ok(token)
    }

    /// reqwest::Response → DebugResponse（吸收 set-cookie、读 body、记 last_used）。
    async fn finish(&self, resp: reqwest::Response) -> Result<DebugResponse, RfcError> {
        let status = resp.status().as_u16();
        let csrf_token = resp
            .headers()
            .get("x-csrf-token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        self.absorb_set_cookies(&resp);
        let body = resp.bytes().await.map_err(adt_unreachable)?;
        *self.last_used.lock().map_err(|_| poisoned())? = Instant::now();
        Ok(DebugResponse {
            status,
            content_type,
            csrf_token,
            body,
        })
    }

    /// 当前 CSRF（未过期直接用）；过期返回 None 触发调用方重签。
    async fn cached_csrf(&self) -> Option<String> {
        let guard = self.csrf.lock().ok()?;
        guard.clone()
    }

    /// 通用 ADT 调用：path 为 `/sap/bc/adt/` 之后的相对路径（可带 query）。
    pub(crate) async fn request(
        &self,
        method: Method,
        path_and_query: &str,
        req: &DebugRequest<'_>,
    ) -> Result<DebugResponse, RfcError> {
        let url = format!(
            "{}/sap/bc/adt/{}",
            self.ep.base_url(),
            validate_raw_path(path_and_query)?
        );

        // 请求期快照（锁不跨 await）
        let cookie_hdr = self.cookie_header().await;
        let csrf = if is_write_method(&method) {
            self.cached_csrf().await
        } else {
            None
        };
        let auth = self.ep.auth_header().await?;
        let (plugin_hdrs, plugin_cookie) = self.ep.plugin_extras().await?;
        // 插件可自带 Authorization——它优先于计算出的 auth_header（与 adt.rs 同规）
        let auth = if plugin_hdrs.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization")) {
            None
        } else {
            auth
        };

        let mut resp = self
            .send_once(
                &method,
                &url,
                req,
                auth.as_deref(),
                &plugin_hdrs,
                plugin_cookie.as_deref(),
                csrf.as_deref(),
                &cookie_hdr,
            )
            .await?;

        // 403 = token 过期（或会话旋转）：同会话重签一次重试。仅写方法走这条。
        if resp.status() == axum::http::StatusCode::FORBIDDEN.as_u16()
            && is_write_method(&method)
        {
            tracing::debug!("调试会话写请求 403，同会话重签 CSRF 后重试");
            let fresh = self.refresh_csrf().await?;
            resp = self
                .send_once(
                    &method,
                    &url,
                    req,
                    auth.as_deref(),
                    &plugin_hdrs,
                    plugin_cookie.as_deref(),
                    Some(fresh).as_deref(),
                    &cookie_hdr,
                )
                .await?;
        }

        let out = self.finish(resp).await?;
        metrics::counter!("adt_calls_total", "method" => method.as_str().to_owned(), "result" => "ok").increment(1);
        Ok(out)
    }

    /// 组装并发送一次请求（无 403 重试逻辑——request 负责决策；refresh_csrf
    /// 直接走这里，避免 request↔refresh 递归）。
    #[allow(clippy::too_many_arguments)]
    async fn send_once(
        &self,
        method: &Method,
        url: &str,
        req: &DebugRequest<'_>,
        auth: Option<&str>,
        plugin_hdrs: &[(String, String)],
        plugin_cookie: Option<&str>,
        csrf: Option<&str>,
        cookie_hdr: &str,
    ) -> Result<reqwest::Response, RfcError> {
        let mut r = self
            .ep
            .http()
            .request(method.clone(), url)
            .header("Accept", req.accept)
            .header(STATEFUL_HEADER, "stateful");
        if let Some(a) = auth {
            r = r.header("Authorization", a);
        }
        for (k, v) in plugin_hdrs {
            r = r.header(k.as_str(), v.as_str());
        }
        if let Some(ct) = req.content_type {
            r = r.header("Content-Type", ct);
        }
        for (k, v) in &req.extra_headers {
            r = r.header(k.as_str(), v.as_str());
        }
        if let Some(tok) = csrf {
            r = r.header("X-CSRF-Token", tok);
        }
        let mut cookies = cookie_hdr.to_string();
        if let Some(pc) = plugin_cookie {
            if !cookies.is_empty() {
                cookies.push_str("; ");
            }
            cookies.push_str(pc);
        }
        if !cookies.is_empty() {
            r = r.header("Cookie", cookies);
        }
        if let Some(b) = req.body {
            r = r.body(b.to_vec());
        }
        // 显式值（listen 的 listen_secs+20 / teardown 的 10s）优先；否则调试器
        // 默认 15s——不用端点的 60s 全局值，死端点快速失败（见 DEBUG_REQ_TIMEOUT）。
        r = r.timeout(req.timeout.unwrap_or(DEBUG_REQ_TIMEOUT));
        r.send().await.map_err(adt_unreachable)
    }

    /// 轻量续租：有状态上下文闲置即有被 ICM 回收的风险（本机已知会话型抖动），
    /// 周期性打一发只读请求保活。失败 = 会话已死（调用方标 `session_lost`）。
    pub(crate) async fn keepalive(&self) -> Result<(), RfcError> {
        self.request(Method::GET, "debugger/breakpoints", &DebugRequest {
            accept: "*/*",
            ..Default::default()
        })
        .await?
        .expect_success("keepalive", 200)?;
        Ok(())
    }

    /// ADT 登录用户（listener 的 requestUser / 断点 requestUser）。
    pub(crate) fn user(&self) -> &str {
        self.ep.user()
    }

    /// 建会话至今。
    pub(crate) fn age(&self) -> Duration {
        self.created.elapsed()
    }

    /// 距上次使用（TTL 回收与诊断用）。
    pub(crate) fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .map(|g| g.elapsed())
            .unwrap_or_else(|_| Duration::ZERO)
    }

    /// jar → Cookie 头串（SSO2 形态并入 MYSAPSSO2——身份在票据里）。
    async fn cookie_header(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(sso2) = self.ep.sso2_cookie() {
            parts.push(sso2);
        }
        if let Ok(jar) = self.cookies.lock() {
            for (k, v) in jar.iter() {
                parts.push(format!("{k}={v}"));
            }
        }
        parts.join("; ")
    }

    /// 吸收响应 `set-cookie`（name=value 截断；空值 = 服务端删除该 cookie）。
    fn absorb_set_cookies(&self, resp: &reqwest::Response) {
        let Ok(mut jar) = self.cookies.lock() else { return };
        for raw in resp.headers().get_all("set-cookie") {
            let Ok(s) = raw.to_str() else { continue };
            let Some((pair, _attrs)) = s.split_once(';') else { continue };
            let pair = pair.trim();
            let Some((name, value)) = pair.split_once('=') else { continue };
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            if value.trim().is_empty() {
                jar.remove(name);
            } else {
                jar.insert(name.to_string(), value.trim().to_string());
            }
        }
    }
}

/// 传输层失败（连接重置/超时）的统一错误——与 adt.rs 的 adt_unreachable 同语义。
fn adt_unreachable(e: reqwest::Error) -> RfcError {
    RfcError {
        code: -1,
        status: 502,
        message: format!("ADT 服务不可达: {e}"),
        key: "ADT_UNREACHABLE".into(),
    }
}

fn poisoned() -> RfcError {
    RfcError {
        code: -1,
        status: 500,
        message: "调试会话内部锁损坏".into(),
        key: "ADT_INTERNAL".into(),
    }
}

/// ADT 异常文档里的机器语义（`com.sap.adt.communicationFramework.subType`）——
/// `noSessionAttached` / `invalidDebuggee` / `debuggeeEnded` 等状态机分支的依据。
pub(crate) fn adt_error_subtype(body: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(body);
    let key = "communicationFramework.subType";
    let i = s.find(key)?;
    let rest = &s[i + key.len()..];
    let j = rest.find('>')?;
    let end = rest[j + 1..].find('<')?;
    let v = rest[j + 1..j + 1 + end].trim();
    if v.is_empty() { None } else { Some(v.to_string()) }
}

impl DebugResponse {
    /// 断言状态码符合预期（错误体原样进 RfcError message，便于排障）。
    fn expect_success(&self, what: &str, expect: u16) -> Result<(), RfcError> {
        if self.status == expect {
            return Ok(());
        }
        let sub = adt_error_subtype(&self.body);
        Err(RfcError {
            code: -1,
            status: self.status,
            message: format!(
                "调试会话 {what} 失败: ADT {}{}{}",
                self.status,
                sub.map(|s| format!(" ({s})")).unwrap_or_default(),
                if self.body.is_empty() {
                    String::new()
                } else {
                    format!(": {}", String::from_utf8_lossy(&self.body))
                }
            ),
            key: "ADT_DEBUG_FAILED".into(),
        })
    }
}

// ============ 单测：mock ADT（axum）验证通道契约 ============

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Request;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum::Router;
    use std::sync::Mutex as StdMutex;

    /// mock 服务器记录的请求
    #[derive(Clone)]
    struct Rec {
        method: String,
        path: String,
        stateful: bool,
        csrf: Option<String>,
        cookie: Option<String>,
    }

    struct MockState {
        log: StdMutex<Vec<Rec>>,
        /// 当前有效 CSRF token（旋转后旧 token 403）
        token: StdMutex<String>,
        /// 是否已签发 contextid
        has_context: StdMutex<bool>,
    }

    fn rec_from(req: &Request) -> Rec {
        let h = req.headers();
        Rec {
            method: req.method().to_string(),
            path: req.uri().path().to_string(),
            stateful: h
                .get("x-sap-adt-sessiontype")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.eq_ignore_ascii_case("stateful"))
                .unwrap_or(false),
            csrf: h.get("x-csrf-token").and_then(|v| v.to_str().ok()).map(String::from),
            cookie: h.get("cookie").and_then(|v| v.to_str().ok()).map(String::from),
        }
    }

    async fn spawn_mock() -> (Arc<MockState>, String) {
        let state = Arc::new(MockState {
            log: StdMutex::new(Vec::new()),
            token: StdMutex::new("T1".into()),
            has_context: StdMutex::new(false),
        });
        let st_disc = state.clone();
        let st_get = state.clone();
        let st_post = state.clone();
        let app = Router::new()
            .route(
                "/sap/bc/adt/discovery",
                get(move |req: Request| async move {
                    st_disc.log.lock().unwrap().push(rec_from(&req));
                    // 首次签发 contextid
                    let fresh = {
                        let mut c = st_disc.has_context.lock().unwrap();
                        if *c { None } else { *c = true; Some("CTX1") }
                    };
                    let mut resp = "OK".into_response();
                    if let Some(ctx) = fresh {
                        resp.headers_mut().append(
                            "set-cookie",
                            format!("sap-contextid={ctx}; Path=/; HttpOnly").parse().unwrap(),
                        );
                    }
                    resp
                }),
            )
            .route(
                "/sap/bc/adt/debugger/breakpoints",
                get(move |req: Request| async move {
                    let rec = rec_from(&req);
                    st_get.log.lock().unwrap().push(rec.clone());
                    // 上下文检查：无 stateful 头或无 cookie → 模拟 ICM 丢弃上下文
                    if !rec.stateful || !rec.cookie.map(|c| c.contains("CTX1")).unwrap_or(false) {
                        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "no session").into_response();
                    }
                    if rec.csrf.as_deref() == Some("Fetch") {
                        let tok = st_get.token.lock().unwrap().clone();
                        let mut resp = "".into_response();
                        resp.headers_mut().insert("x-csrf-token", tok.parse().unwrap());
                        return resp;
                    }
                    (axum::http::StatusCode::OK, "").into_response()
                }),
            )
            .route(
                "/sap/bc/adt/debugger/breakpoints",
                post(move |req: Request| async move {
                    let rec = rec_from(&req);
                    st_post.log.lock().unwrap().push(rec.clone());
                    if !rec.stateful {
                        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "no session").into_response();
                    }
                    let cur = st_post.token.lock().unwrap().clone();
                    match rec.csrf.as_deref() {
                        Some(t) if t == cur => (axum::http::StatusCode::OK, "ok").into_response(),
                        _ => (axum::http::StatusCode::FORBIDDEN, "csrf").into_response(),
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (state, format!("http://{addr}"))
    }

    async fn open_session(base: &str) -> DebugSession {
        let ep = Arc::new(AdtEndpoint::new(base, "DEV", "pw", Duration::from_secs(5)));
        DebugSession::open(ep).await.unwrap()
    }

    /// 每个请求都带 stateful 头；cookie jar 跨请求延续；CSRF 只上写方法。
    #[tokio::test]
    async fn stateful_cookie_csrf_contract() {
        let (st, base) = spawn_mock().await;
        let s = open_session(&base).await;
        // GET：带 cookie、带 stateful、无 CSRF
        let r = s.request(Method::GET, "debugger/breakpoints", &DebugRequest {
            accept: "*/*",
            ..Default::default()
        }).await.unwrap();
        assert_eq!(r.status, 200);
        // POST：带 CSRF（走默认 token T1）
        let r = s.request(Method::POST, "debugger/breakpoints", &DebugRequest {
            accept: "*/*",
            content_type: Some("application/xml"),
            body: Some(b"<x/>".as_slice()),
            ..Default::default()
        }).await.unwrap();
        assert_eq!(r.status, 200);

        let log = st.log.lock().unwrap();
        let disc = &log[0];
        assert_eq!(disc.path, "/sap/bc/adt/discovery");
        assert!(disc.stateful, "discovery 必须带 stateful 头");
        for (i, rec) in log.iter().enumerate().skip(1) {
            assert!(rec.stateful, "第 {i} 个请求缺 stateful 头: {}", rec.path);
            assert!(rec.cookie.as_deref().unwrap_or("").contains("CTX1"), "第 {i} 个请求缺 contextid cookie: {}", rec.path);
        }
        let gets: Vec<_> = log.iter().filter(|r| r.method == "GET").collect();
        // GET 不携带真 token（None = discovery 首跳；"Fetch" = CSRF 签发）
        assert!(
            gets.iter()
                .all(|r| matches!(r.csrf.as_deref(), None | Some("Fetch"))),
            "GET 只允许无 token 或 Fetch 标记，不应带真 token"
        );
        let posts: Vec<_> = log.iter().filter(|r| r.method == "POST").collect();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].csrf.as_deref(), Some("T1"), "POST 必须带会话内签发的 CSRF");
    }

    /// 403 → 同会话重签 CSRF → 重试成功（token 已被服务端旋转到 T2）。
    #[tokio::test]
    async fn csrf_refresh_on_403() {
        let (st, base) = spawn_mock().await;
        let s = open_session(&base).await;
        // 服务端旋转 token：T1 → T2
        *st.token.lock().unwrap() = "T2".into();
        let r = s.request(Method::POST, "debugger/breakpoints", &DebugRequest {
            accept: "*/*",
            content_type: Some("application/xml"),
            body: Some(b"<x/>".as_slice()),
            ..Default::default()
        }).await.unwrap();
        assert_eq!(r.status, 200, "403 后同会话重签应恢复");
        // 日志里应有 403 + 二次 Fetch + 重试成功
        let log = st.log.lock().unwrap();
        let posts: Vec<_> = log.iter().filter(|r| r.method == "POST").collect();
        assert_eq!(posts.len(), 2, "应重试一次");
        assert_eq!(posts[0].csrf.as_deref(), Some("T1"));
        assert_eq!(posts[1].csrf.as_deref(), Some("T2"));
    }

    /// keepalive 打只读请求且不破坏会话（续租语义）。
    #[tokio::test]
    async fn keepalive_hits_readonly() {
        let (_st, base) = spawn_mock().await;
        let s = open_session(&base).await;
        s.keepalive().await.unwrap();
        s.keepalive().await.unwrap();
    }

    /// P1 活体烟雾（需真实 A4H：`cargo test -- --ignored debug_session`）：
    /// 真实 ICF 上建会话 → 设断点 → 60s+ 慢速保活 → 会话存活、contextid 不换。
    /// .env 提供 SAP_USER/SAP_PASSWD；ADT 基址默认本机 50000。
    #[tokio::test]
    #[ignore]
    async fn live_a4h_channel_smoke() {
        let _ = dotenvy::dotenv();
        let base = std::env::var("SAP_ADT_LIVE_BASE").unwrap_or_else(|_| "http://localhost:50000".into());
        let user = std::env::var("SAP_USER").unwrap_or_else(|_| "DEVELOPER".into());
        let passwd = std::env::var("SAP_PASSWD").expect("需要 SAP_PASSWD");
        let ep = Arc::new(AdtEndpoint::new(&base, &user, &passwd, Duration::from_secs(30)));
        let s = DebugSession::open(ep).await.expect("建会话");
        let ctx_before = s
            .cookies
            .lock()
            .unwrap()
            .get("sap-contextid")
            .cloned()
            .expect("discovery 应回 contextid");

        // 断点集合 POST（目标类可不存在——SAP 200 + errorMessage 也算通道成功）
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<dbg:breakpoints xmlns:dbg="http://www.sap.com/adt/debugger" xmlns:adtcore="http://www.sap.com/adt/core" debuggingMode="user" scope="external" requestUser="{user}" terminalId="084477083d23656b818cfb937e313f78" ideId="sfa">
<breakpoint kind="line" enabled="true" adtcore:uri="/sap/bc/adt/programs/programs/SAPLSTRD/source/main#start=1"/>
</dbg:breakpoints>"#
        );
        let r = s
            .request(
                Method::POST,
                "debugger/breakpoints",
                &DebugRequest {
                    accept: "*/*",
                    content_type: Some("application/xml"),
                    body: Some(xml.as_bytes()),
                    ..Default::default()
                },
            )
            .await
            .expect("断点 POST");
        assert_eq!(r.status, 200, "断点 POST 应 200: {}", String::from_utf8_lossy(&r.body));

        // 60s+ 慢速保活（4 × 15s，跨过常见 60s 闲置回收窗）
        for i in 0..4 {
            tokio::time::sleep(Duration::from_secs(15)).await;
            s.keepalive().await.unwrap_or_else(|e| panic!("保活第 {} 拍失败: {}", i + 1, e.message));
        }
        let ctx_after = s.cookies.lock().unwrap().get("sap-contextid").cloned();
        assert_eq!(ctx_after.as_deref(), Some(ctx_before.as_str()), "contextid 应全程不变");

        // 清场：POST 空断点集合（覆盖语义 = 清光），DELETE listener
        let clear = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<dbg:breakpoints xmlns:dbg="http://www.sap.com/adt/debugger" debuggingMode="user" scope="external" requestUser="{user}" terminalId="084477083d23656b818cfb937e313f78" ideId="sfa"/>"#
        );
        let r = s
            .request(
                Method::POST,
                "debugger/breakpoints",
                &DebugRequest {
                    accept: "*/*",
                    content_type: Some("application/xml"),
                    body: Some(clear.as_bytes()),
                    ..Default::default()
                },
            )
            .await
            .expect("清断点");
        assert_eq!(r.status, 200);
        let r = s
            .request(
                Method::DELETE,
                "debugger/listeners?debuggingMode=user&requestUser=DEVELOPER",
                &DebugRequest { accept: "*/*", ..Default::default() },
            )
            .await
            .expect("摘 listener");
        assert!(r.status == 200 || r.status == 404);
    }
}
