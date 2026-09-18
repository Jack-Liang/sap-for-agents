//! axum HTTP 服务：路由、handler、共享状态。
//!
//! 共享状态是 `Arc<RfcConnectionPool>`：多连接池 + 自动重连。
//! handler 内通过 `tokio::task::spawn_blocking` 把 FFI 执行丢到阻塞线程池，
//! 不同请求可拿到不同连接并行执行 SAP 调用；同时让非 Send 的裸指针类型
//! 只存在于阻塞闭包内，不跨 await 点，保证 future 干净 Send。

use crate::api::{
    direction_name, rfctype_name, DdicTypeResponse, FieldDef, FieldSemanticsResponse, FixedValueDto,
    FunctionDocResponse, FunctionInterface, FunctionParam, InvokeRequest, InvokeResponse, ParamDoc,
    ScalarValue, SearchFunctionEntry, SearchResponse,
};
use crate::connection::{get_field_infos, RfcConnection};
use crate::error::RfcError;
use crate::executor::execute_collect;
use crate::pool::RfcConnectionPool;
use axum::response::IntoResponse;
use axum::{routing::post, Json, Router};
use governor::{
    clock::DefaultClock,
    state::keyed::DefaultKeyedStateStore,
    Quota, RateLimiter,
};
use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// 全局共享状态：连接池（内部含连接 + 重连参数）
pub type SharedPool = Arc<RfcConnectionPool>;

/// 全局请求超时（启动期由 [`init_request_timeout`] 写入，默认 60s）。
static REQUEST_TIMEOUT: OnceLock<Duration> = OnceLock::new();

/// 启动期设置全局请求超时（main 调一次）。
pub fn init_request_timeout(d: Duration) {
    let _ = REQUEST_TIMEOUT.set(d);
}

/// 当前全局请求超时；未初始化时回退 60s。
fn request_timeout() -> Duration {
    REQUEST_TIMEOUT
        .get()
        .copied()
        .unwrap_or_else(|| Duration::from_secs(60))
}

/// Prometheus 指标记录器句柄（启动期由 [`init_metrics`] 写入）。
static METRICS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// 启动期初始化 Prometheus 指标记录器（main 调一次）。之后 `counter!`/`gauge!`/`histogram!` 才生效。
pub fn init_metrics() {
    // 降级：指标安装失败不拖垮主服务（log warn + /metrics 返回空，不影响业务）
    match PrometheusBuilder::new().install_recorder() {
        Ok(handle) => {
            let _ = METRICS_HANDLE.set(handle);
        }
        Err(e) => tracing::warn!(
            error = %e,
            "Prometheus 指标记录器安装失败，/metrics 将返回空（不影响主功能）"
        ),
    }
}

/// 按 IP 限流器（启动期由 [`init_rate_limiter`] 写入；`None` = 不限流）。
type IpLimiter = RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>;
static RATE_LIMITER: OnceLock<Option<IpLimiter>> = OnceLock::new();

/// 启动期设置限流：`rps=None` 或 0 → 不限流；≥1 → 按 IP 每秒 rps 个请求。
pub fn init_rate_limiter(rps: Option<u32>) {
    // NonZeroU32::new(r) 对 r=0 返 None（= 不启用），不会 panic
    let limiter = rps.and_then(|r| {
        NonZeroU32::new(r).map(|n| RateLimiter::keyed(Quota::per_second(n)))
    });
    let _ = RATE_LIMITER.set(limiter);
}

/// 构造超时错误（504 Gateway Timeout，body 走 ErrorResponse）。
fn timeout_error(timeout: Duration) -> RfcError {
    RfcError {
        code: -1,
        status: 504,
        message: format!("SAP 调用超时（{}s）", timeout.as_secs()),
        ..Default::default()
    }
}

/// 用指定超时在阻塞线程池内执行 SAP 调用。
///
/// `spawn_blocking` + `with_connection`（含自动重连）+ `tokio::time::timeout` 收敛到一处。
/// 超时返回 504。注意：超时后 `spawn_blocking` 线程无法取消，FFI 会跑到 SAP 响应才归还
/// 连接（NWRFC 固有限制，靠协议层超时兜底）。FFI 与连接池内部状态都被限制在阻塞闭包中，
/// 绝不跨 await 点，保证 future 干净 Send。
async fn run_blocking_with_timeout<F, R>(
    pool: SharedPool,
    timeout: Duration,
    f: F,
) -> Result<R, RfcError>
where
    F: FnMut(&RfcConnection) -> Result<R, RfcError> + Send + 'static,
    R: Send + 'static,
{
    let pool = Arc::clone(&pool);
    let join = tokio::task::spawn_blocking(move || pool.with_connection(f));
    match tokio::time::timeout(timeout, join).await {
        Ok(inner) => inner.map_err(|e| RfcError {
            code: -1,
            message: format!("阻塞任务失败: {}", e),
            key: String::new(),
            ..Default::default()
        })?,
        Err(_elapsed) => {
            // 超时：spawn_blocking 任务无法取消（NWRFC 固有限制），它会在 SAP 真正响应后
            // 自行结束并归还连接。这里只负责及时返回 504；记告警以便运维监控慢调用堆积。
            tracing::warn!(?timeout, "SAP 调用超时，阻塞任务将在 SAP 响应后自行归还连接");
            Err(timeout_error(timeout))
        }
    }
}

/// 用全局默认超时执行（元数据查询等无需 per-request 超时的端点用这个）。
async fn run_blocking<F, R>(pool: SharedPool, f: F) -> Result<R, RfcError>
where
    F: FnMut(&RfcConnection) -> Result<R, RfcError> + Send + 'static,
    R: Send + 'static,
{
    run_blocking_with_timeout(pool, request_timeout(), f).await
}

/// 仅静态路由（不依赖 SAP 连接池）：首页 / Agent 文档 / 健康检查。
/// 供集成测试用，无需构造 pool 即可验证这几个端点。
pub fn static_app<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/", axum::routing::get(index_handler))
        .route("/agents.md", axum::routing::get(agents_handler))
        // OpenAPI 规范导出：机器可读接口契约（免鉴权公开页，规范内不含敏感信息）
        .route("/openapi.json", axum::routing::get(crate::openapi::openapi_handler))
        .route("/health", axum::routing::get(health_handler))
}

/// 构建带共享连接池的 Router（静态路由 + SAP 业务路由）
pub fn app(pool: SharedPool) -> Router {
    // 受认证保护的 /api 业务路由：设置 SAP_API_KEY 后要求 Bearer token
    let api = Router::new()
        .route("/api/rfc", post(invoke_handler))
        .route("/api/functions/search", post(search_functions_handler))
        // 函数名可能带 /NS/ 命名空间前缀（如 /SDF/X），路径参数无法匹配多段路径，
        // 统一用通配路由捕获后按尾部 /doc、/source 分发（见 function_route_dispatcher）。
        .route("/api/functions/*name", axum::routing::get(function_route_dispatcher))
        .route("/api/programs/:name/source", axum::routing::get(program_source_handler))
        .route("/api/table/read", post(table_read_handler))
        .route("/api/ddic/type/:name", axum::routing::get(ddic_type_handler))
        .route("/api/ddic/field/:table/:field", axum::routing::get(ddic_field_handler))
        // ABAP 短转储结构化读取（解析自 ADT，免拉 45KB–1MB 原始文本）
        .route("/api/dumps", axum::routing::get(dumps_list_handler))
        .route("/api/dumps/grouped", axum::routing::get(dumps_grouped_handler))
        .route("/api/dumps/*key", axum::routing::get(dump_detail_handler))
        // ABAP 对象写入编排（锁→写→解锁→激活一体；replace/syntax 见 dispatcher）
        .route(
            "/api/objects/*path",
            axum::routing::put(object_write_handler).post(object_post_handler),
        )
        // ADT REST 通用代理（dump 正文、类/程序源码等，任何方法透传）
        .route("/api/adt/*path", axum::routing::any(crate::adt::adt_proxy))
        .layer(axum::middleware::from_fn(crate::auth::require_api_key))
        .layer(axum::middleware::from_fn(rate_limit_middleware));

    // 探针与公开页免鉴权：编排系统探针不便带 token，且无业务数据泄露
    static_app()
        .route("/ready", axum::routing::get(ready_handler))
        .route("/metrics", axum::routing::get(metrics_handler))
        .merge(api)
        .fallback(fallback_handler)
        .layer(axum::middleware::from_fn(unify_method_not_allowed))
        .with_state(pool)
}

/// 兜底 404：路由不存在时统一返回 JSON 错误体（而非 axum 默认的空 body）。
async fn fallback_handler() -> (axum::http::StatusCode, Json<serde_json::Value>) {
    (
        axum::http::StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error":{"code":404,"message":"Not found","key":"ROUTE_NOT_FOUND"}})),
    )
}

/// `GET /metrics` —— Prometheus 指标（连接池 + RFC 调用计数/耗时）。免鉴权（运维探针）。
async fn metrics_handler() -> impl axum::response::IntoResponse {
    let body = METRICS_HANDLE
        .get()
        .map(|h| h.render())
        .unwrap_or_default();
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
}

/// 限流中间件：按 IP 限制每秒请求数；超限返回 429（统一 JSON）。未配置则放行。
async fn rate_limit_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let Some(limiter) = RATE_LIMITER.get().and_then(|opt| opt.as_ref()) else {
        return next.run(req).await; // 未启用限流
    };
    let ip = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip());
    let Some(ip) = ip else {
        // 拿不到 ConnectInfo 不该发生（server 经 into_make_service_with_connect_info 启动）。
        // 容错过放行而非误伤全部请求，但记告警以便发现中间件剥离 ConnectInfo 的情况。
        tracing::warn!("限流中间件取不到客户端 IP（ConnectInfo 缺失），本次放行");
        return next.run(req).await;
    };
    if limiter.check_key(&ip).is_ok() {
        next.run(req).await
    } else {
        (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error":{"code":429,"message":"Rate limit exceeded (too many requests from this IP)","key":"RATE_LIMITED"}})),
        )
            .into_response()
    }
}

/// 统一框架级错误：axum 对"方法不匹配"默认返回空 body 的 405，这里改写成统一 JSON 契约。
async fn unify_method_not_allowed(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let resp = next.run(req).await;
    if resp.status() == axum::http::StatusCode::METHOD_NOT_ALLOWED {
        return (
            axum::http::StatusCode::METHOD_NOT_ALLOWED,
            Json(serde_json::json!({"error":{"code":405,"message":"Method not allowed","key":"METHOD_NOT_ALLOWED"}})),
        )
            .into_response();
    }
    resp
}

/// 启动 HTTP 服务（阻塞当前异步任务直到服务器结束）。
/// `shutdown` 是一个 future，完成时触发优雅停机。
pub async fn run(
    pool: SharedPool,
    listen_addr: &str,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), std::io::Error> {
    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    tracing::info!(addr = listen_addr, "HTTP 服务监听");
    // listen_addr 可能是 0.0.0.0:3000，给用户提示时用 127.0.0.1 更友好（本机访问）
    let display_host = if listen_addr.starts_with("0.0.0.0") {
        listen_addr.replacen("0.0.0.0", "127.0.0.1", 1)
    } else if listen_addr.starts_with("::") {
        listen_addr.replacen("::", "[::1]", 1)
    } else {
        listen_addr.to_string()
    };
    tracing::info!("✅ 服务就绪！");
    tracing::info!("   👉 浏览器打开:         http://{}", display_host);
    tracing::info!("   👉 给 AI/Agent 的文档: http://{}/agents.md", display_host);
    tracing::info!("   👉 OpenAPI 规范:        http://{}/openapi.json", display_host);
    tracing::info!("   端点速览: POST /api/rfc | GET /api/functions/:name | POST /api/functions/search");
    tracing::info!("           GET /api/functions/:name/doc | GET /api/ddic/type/:name | GET /api/ddic/field/:t/:f");
    axum::serve(
        listener,
        app(pool).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

/// GET /health —— 不触碰 SAP，便于外部探活（liveness）
async fn health_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

/// `GET /ready` —— readiness 探针：借连接池调 `RFC_PING` 验证 SAP 可达（带 5s 超时）。
///
/// 与 `/health`（liveness）分离：进程活着但连不上 SAP 时返回 503，编排系统
/// 据此摘流而非重启。失败统一用 503——语义最贴合 readiness（"暂时不可用，
/// 别给我流量"），故不走 `RfcError::IntoResponse` 的 502/504 映射。
///
/// 超时后 `spawn_blocking` 内的 ping 仍会跑完（无法取消），但 ping 本身很快；
/// 最坏占用一个连接几秒，探针频率（默认 10s）下可接受。
async fn ready_handler(
    axum::extract::State(pool): axum::extract::State<SharedPool>,
) -> (
    axum::http::StatusCode,
    Json<serde_json::Value>,
) {
    const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    let ping =
        tokio::time::timeout(READY_TIMEOUT, run_blocking(pool, |conn| conn.ping())).await;

    match ping {
        Ok(Ok(())) => (
            axum::http::StatusCode::OK,
            Json(serde_json::json!({ "status": "ready", "sap": "ok" })),
        ),
        Ok(Err(e)) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "unavailable",
                "code": e.code,
                "message": e.message,
            })),
        ),
        Err(_elapsed) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "timeout",
                "timeout_ms": READY_TIMEOUT.as_millis() as u64,
            })),
        ),
    }
}

/// GET /agents.md —— 返回嵌入的 AGENTS.md（供 AI/Agent 读取）
/// 编译期 include_str! 嵌入，预编译包也自带，不依赖磁盘文件。
async fn agents_handler() -> axum::response::Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        include_str!("../AGENTS.md"),
    )
        .into_response()
}

/// GET / —— 浏览器欢迎页（含 Agent 文档入口 + 接口速览）
/// HTML 模板见 src/index.html（编译期 include_str! 嵌入，不依赖磁盘文件）。
/// 从请求 Host 头动态推导访问地址，链接自动匹配用户实际访问的 host:port。
async fn index_handler(req: axum::http::Request<axum::body::Body>) -> axum::response::Html<String> {
    // 从 Host 头取访问地址（如 127.0.0.1:3000 或 192.168.1.5:3000）
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("127.0.0.1:3000");
    let base = format!("http://{host}");
    let agents_url = format!("{base}/agents.md");

    // 认证状态：用于首页条件显示认证提示条
    let auth_visibility = if crate::auth::is_enabled() {
        "block"
    } else {
        "none"
    };

    // 语言选择：?lang=zh → 中文，?lang=<其他> → 英文；未带 lang 时按 Accept-Language
    // （含 zh 开头 → 中文）；默认英文。与 README/AGENTS 双语策略一致（英文为主）。
    let lang_param = req.uri().query().and_then(|q| {
        q.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k == "lang").then_some(v)
        })
    });
    let prefer_zh = match lang_param {
        Some("zh") => true,
        Some(_) => false,
        None => req
            .headers()
            .get(axum::http::header::ACCEPT_LANGUAGE)
            .and_then(|h| h.to_str().ok())
            .map(|al| al.split(',').any(|r| r.trim().to_lowercase().starts_with("zh")))
            .unwrap_or(false),
    };
    let template = if prefer_zh {
        include_str!("index.zh.html")
    } else {
        include_str!("index.html")
    };
    let html = template
        .replace("{{BASE_URL}}", &base)
        .replace("{{AGENTS_URL}}", &agents_url)
        .replace("{{AUTH_BANNER_VISIBILITY}}", auth_visibility);
    axum::response::Html(html)
}

/// 审计用：把请求参数压缩成可读摘要（敏感值脱敏、长值截断、键排序稳定）。
fn summarize_params(req: &InvokeRequest) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !req.inputs.is_empty() {
        let mut keys: Vec<&String> = req.inputs.keys().collect();
        keys.sort();
        let kvs: Vec<String> = keys
            .iter()
            .map(|k| format!("{}={}", k, mask_value(k, &req.inputs[*k])))
            .collect();
        parts.push(format!("inputs{{{}}}", kvs.join(", ")));
    }
    if !req.table_inputs.is_empty() {
        let mut keys: Vec<&String> = req.table_inputs.keys().collect();
        keys.sort();
        let tabs: Vec<String> = keys
            .iter()
            .map(|k| format!("{}[{}行]", k, req.table_inputs[*k].len()))
            .collect();
        parts.push(format!("tables{{{}}}", tabs.join(", ")));
    }
    if !req.struct_inputs.is_empty() {
        let mut keys: Vec<&String> = req.struct_inputs.keys().collect();
        keys.sort();
        let sts: Vec<String> = keys
            .iter()
            .map(|k| format!("{}{{{}}}", k, req.struct_inputs[*k].len()))
            .collect();
        parts.push(format!("structs{{{}}}", sts.join(", ")));
    }
    if parts.is_empty() {
        "(无参数)".into()
    } else {
        parts.join(" ")
    }
}

/// 脱敏 + 截断单个标量值（用于审计摘要）。敏感 key（密码/token 等）→ `***`，长值截断 80 字符。
fn mask_value(key: &str, value: &ScalarValue) -> String {
    const SENSITIVE: &[&str] = &[
        "PASSWD", "PASSWORD", "PASS", "SECRET", "TOKEN", "CREDENTIAL", "KEY",
    ];
    let upper = key.to_uppercase();
    if SENSITIVE.iter().any(|s| upper.contains(s)) {
        return "***".into();
    }
    let s = value.clone().into_chars();
    const MAX: usize = 80;
    if s.chars().count() > MAX {
        format!("{}…", s.chars().take(MAX).collect::<String>())
    } else {
        s
    }
}

/// POST /api/rfc —— 通用 RFC 调用
///
/// 流程：反序列化请求 → spawn_blocking 内通过连接池执行（含自动重连）→ 返回 JSON 结果。
/// FFI 与连接池内部状态都被限制在阻塞闭包中，绝不跨 await 点。
async fn invoke_handler(
    axum::extract::State(pool): axum::extract::State<SharedPool>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    req: Result<Json<InvokeRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<InvokeResponse>, RfcError> {
    let started = std::time::Instant::now();
    let caller_ip = addr.ip().to_string();
    // JSON 解析/反序列化失败 → 统一错误格式（status 取 axum 语义码，body_text 作 message）
    let Json(req) = req.map_err(|r| RfcError {
        code: -1,
        status: r.status().as_u16(),
        message: r.body_text(),
        key: "JSON_INVALID".into(),
    })?;
    let func_name = req.func_name.clone();
    let params = summarize_params(&req);
    let pool_stats = pool.stats(); // 采样当前池状态（pool 即将 move 进闭包）

    // per-request 超时：调用方可对慢接口自主放宽；不传/传 0 → 用全局默认。
    // clamp 到 [1, MAX_TIMEOUT_SECS]，防止恶意传天文数字超时长期占用连接/阻塞线程。
    const MAX_TIMEOUT_SECS: u64 = 1800; // 30 分钟上限
    let timeout = req
        .timeout_secs
        .filter(|&s| s >= 1)
        .map(|s| Duration::from_secs(s.min(MAX_TIMEOUT_SECS)))
        .unwrap_or_else(request_timeout);
    // 通过 with_connection 执行：遇通信错误自动重连重试一次
    let result =
        run_blocking_with_timeout(pool, timeout, move |conn| execute_collect(conn, &req)).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    // 指标：池状态 gauge（每次调用采样）+ 调用计数/耗时（按 函数×结果 分维）
    gauge!("pool_idle").set(pool_stats.idle as f64);
    gauge!("pool_total").set(pool_stats.total as f64);
    gauge!("pool_max").set(pool_stats.max as f64);

    // 审计日志 + 指标：成功 info / 失败 warn（失败 = 告警信号）
    match result {
        Ok(resp) => {
            counter!("rfc_calls_total", "func" => func_name.clone(), "result" => "ok").increment(1);
            histogram!("rfc_call_duration_ms", "func" => func_name.clone()).record(elapsed_ms as f64);
            tracing::info!(
                func = %func_name,
                caller_ip = %caller_ip,
                elapsed_ms,
                params = %params,
                "RFC 调用成功"
            );
            Ok(Json(resp))
        }
        Err(e) => {
            counter!("rfc_calls_total", "func" => func_name.clone(), "result" => "err").increment(1);
            histogram!("rfc_call_duration_ms", "func" => func_name.clone()).record(elapsed_ms as f64);
            tracing::warn!(
                func = %func_name,
                caller_ip = %caller_ip,
                elapsed_ms,
                params = %params,
                status = e.status,
                code = e.code,
                key = %e.key,
                message = %e.message,
                "RFC 调用失败"
            );
            Err(e)
        }
    }
}

// ========================================================================
// 面向 AI 的元数据查询 handler（端点 ①~⑤）
// ========================================================================

/// 默认语言（从 SAP_LANG 环境变量读，回退 EN）。
/// 端点⑤④可用 ?lang= 覆盖。
fn default_lang() -> String {
    std::env::var("SAP_LANG").unwrap_or_else(|_| "EN".to_string())
}

/// 把 ParamInfo 转成 FieldDef，STRUCTURE/TABLE 类型递归展开子字段（深度上限由 get_field_infos 的句柄链决定）。
fn param_info_to_field_def(
    p: &crate::connection::ParamInfo,
) -> Result<FieldDef, RfcError> {
    let fields = if p.type_ == crate::ffi::RFCTYPE_TABLE
        || p.type_ == crate::ffi::RFCTYPE_STRUCTURE
    {
        if let Some(handle) = p.type_desc_handle {
            // SAFETY: handle 来自刚拉取的有效元数据，连接仍有效
            let subs = unsafe { get_field_infos(handle) }?;
            let defs: Vec<FieldDef> = subs
                .iter()
                .map(|sf| FieldDef {
                    name: sf.name.clone(),
                    type_name: rfctype_name(sf.type_),
                    length: sf.char_length,
                    decimals: sf.decimals,
                    description: sf.parameter_text.clone(),
                    fields: None, // 深度递归由 metadata 缓存负责；此处仅展开一层供 AI 快速预览
                })
                .collect::<Vec<_>>();
            Some(defs)
        } else {
            None
        }
    } else {
        None
    };
    Ok(FieldDef {
        name: p.name.clone(),
        type_name: rfctype_name(p.type_),
        length: p.char_length,
        decimals: p.decimals,
        description: p.parameter_text.clone(),
        fields,
    })
}

/// GET /api/functions/{name}[/doc|/source] 的统一入口。
/// 命名空间函数名自带斜杠（如 /SDF/X），通配路由整段捕获后在此按尾部拆分分发；
/// 原始斜杠（/api/functions//SDF/X）与 %2F 编码两种 URL 形式均能命中。
/// 小写 /doc、/source 不会出现在合法函数名中（SAP 函数名为大写），按尾部识别是安全的。
async fn function_route_dispatcher(
    axum::extract::State(pool): axum::extract::State<SharedPool>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<LangQuery>,
) -> axum::response::Response {
    use axum::extract::{Path, Query, State};
    if let Some(base) = name.strip_suffix("/doc").filter(|b| !b.is_empty()) {
        return function_doc_handler(State(pool), Path(base.to_string()), Query(q))
            .await
            .into_response();
    }
    if let Some(base) = name.strip_suffix("/source").filter(|b| !b.is_empty()) {
        return function_source_handler(State(pool), Path(base.to_string()), q.prologue.clone())
            .await
            .into_response();
    }
    function_interface_handler(State(pool), Path(name))
        .await
        .into_response()
}

/// ① GET /api/functions/:name —— 查函数完整接口（参数/类型/方向/嵌套字段）
async fn function_interface_handler(
    axum::extract::State(pool): axum::extract::State<SharedPool>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<Json<FunctionInterface>, RfcError> {
    crate::api::validate_func_name(&name)?;
    let result = run_blocking(pool, move |conn| {
        let param_infos = conn.get_param_infos(&name)?;
        let params: Vec<FunctionParam> = param_infos
            .iter()
            .map(|p| {
                Ok(FunctionParam {
                    name: p.name.clone(),
                    type_name: rfctype_name(p.type_),
                    direction: direction_name(p.direction),
                    length: p.char_length,
                    decimals: p.decimals,
                    optional: p.optional,
                    default: p.default_value.clone(),
                    description: p.parameter_text.clone(),
                    fields: param_info_to_field_def(p)?.fields,
                }) as Result<FunctionParam, RfcError>
            })
            .collect::<Result<_, _>>()?;
        Ok(FunctionInterface {
            name: name.clone(),
            params,
        })
    })
    .await?;
    Ok(Json(result))
}

/// ② POST /api/functions/search —— 搜索函数模块
#[derive(serde::Deserialize)]
pub(crate) struct SearchRequest {
    /// 函数名通配符，如 "BAPI_USER_*"
    #[serde(default)]
    pattern: String,
    /// 函数组过滤（可选）
    #[serde(default)]
    group: String,
    /// 最多返回条数，默认 50
    #[serde(default)]
    max_results: Option<usize>,
}
/// ② POST /api/functions/search —— 搜索函数模块
async fn search_functions_handler(
    axum::extract::State(pool): axum::extract::State<SharedPool>,
    req: Result<Json<SearchRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<SearchResponse>, RfcError> {
    let Json(req) = req.map_err(|r| RfcError {
        code: -1,
        status: r.status().as_u16(),
        message: r.body_text(),
        key: "JSON_INVALID".into(),
    })?;
    // 空 pattern 校验：pattern + group 都空 → 拒绝（防无意义枚举全库）
    if req.pattern.trim().is_empty() && req.group.trim().is_empty() {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "pattern 和 group 不能同时为空（至少提供一个过滤条件）".into(),
            key: "PATTERN_EMPTY".into(),
        });
    }
    let max = req.max_results.unwrap_or(50).min(500);
    let pattern = req.pattern.clone();
    let functions = run_blocking(pool, move |conn| {
        crate::discovery::search_functions(conn, &req.pattern, &req.group, max)
    })
    .await?;
    let count = functions.len();
    let functions = functions
        .into_iter()
        .map(|f| SearchFunctionEntry {
            name: f.name,
            group: f.group,
            description: f.description,
        })
        .collect();
    Ok(Json(SearchResponse {
        pattern,
        count,
        functions,
    }))
}

/// ③ GET /api/ddic/type/:name —— 查 DDIC 结构/表的字段定义
async fn ddic_type_handler(
    axum::extract::State(pool): axum::extract::State<SharedPool>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<Json<DdicTypeResponse>, RfcError> {
    let req_name = name.clone();
    let result =
        run_blocking(pool, move |conn| crate::metadata::get_type_fields(conn, &name)).await?;
    let fields = result.iter().map(FieldDef::from_type_field).collect();
    Ok(Json(DdicTypeResponse {
        name: req_name,
        fields,
    }))
}

/// ④ GET /api/ddic/field/:table/:field —— 查字段的语义元数据（数据元素/域/固定值）
#[derive(serde::Deserialize)]
struct LangQuery {
    #[serde(default)]
    lang: Option<String>,
    /// /source 端点专用：是否附依赖签名前言（"true"/"1"）
    #[serde(default)]
    prologue: Option<String>,
}
/// ④ GET /api/ddic/field/:table/:field —— 查字段的语义元数据（数据元素/域/固定值）
async fn ddic_field_handler(
    axum::extract::State(pool): axum::extract::State<SharedPool>,
    axum::extract::Path((table, field)): axum::extract::Path<(String, String)>,
    axum::extract::Query(q): axum::extract::Query<LangQuery>,
) -> Result<Json<FieldSemanticsResponse>, RfcError> {
    let lang = q.lang.unwrap_or_else(default_lang);
    let req_table = table.clone();
    let sem = run_blocking(pool, move |conn| {
        crate::discovery::read_ddic_field_info(conn, &table, &field, &lang)
    })
    .await?;
    Ok(Json(FieldSemanticsResponse {
        table: req_table,
        field: sem.field,
        data_element: sem.data_element,
        domain: sem.domain,
        check_table: sem.check_table,
        description: sem.description,
        medium_label: sem.medium_label,
        fixed_values: sem
            .fixed_values
            .into_iter()
            .map(|fv| FixedValueDto {
                value: fv.value,
                text: fv.text,
            })
            .collect(),
    }))
}

/// ⑥ `GET /api/functions/:name/source` —— 读函数 ABAP 源代码（调 `RPY_FUNCTIONMODULE_READ`）
/// `?prologue=true` 时附带依赖签名前言：扫描源码里的 `CALL FUNCTION 'X'`，
/// 逐个取其接口，内联成紧凑签名块（ABAP 注释风格，可直接粘贴到源码上方），
/// 让调用方一次拿到「源码 + 依赖契约」，省去 N 次接口往返。
///
/// RFC 读取失败（非 NOT_FOUND，典型如 FL 180「Source wider than 72 char」）
/// 自动降级 ADT 通道：`RFC_FUNCTION_SEARCH` 反解组名 →
/// `/sap/bc/adt/functions/groups/{组}/fmodules/{名}/source/main`。
/// 响应的 `source_via` 字段标明来源（`rfc` / `adt`）。
async fn function_source_handler(
    axum::extract::State(pool): axum::extract::State<SharedPool>,
    axum::extract::Path(name): axum::extract::Path<String>,
    prologue: Option<String>,
) -> Result<Json<serde_json::Value>, RfcError> {
    crate::api::validate_func_name(&name)?;
    let want_prologue = matches!(prologue.as_deref(), Some("true") | Some("1"));

    let lookup = name.clone();
    let (lines, via) = match run_blocking(Arc::clone(&pool), move |conn| {
        crate::discovery::read_function_source(conn, &lookup)
    })
    .await
    {
        Ok(lines) => (lines, "rfc"),
        Err(rfc_err) => {
            if !crate::discovery::should_fallback_to_adt(&rfc_err) {
                return Err(rfc_err);
            }
            // 组名反解失败或 ADT 也读不到 → 保留原 RFC 错误（信息量更大）
            let group = {
                let lookup = name.clone();
                match run_blocking(Arc::clone(&pool), move |conn| {
                    crate::discovery::resolve_function_group(conn, &lookup)
                })
                .await
                {
                    Ok(g) => g,
                    Err(e) => {
                        tracing::warn!(key = %e.key, "ADT 降级：函数组反解失败，返回原 RFC 错误");
                        return Err(rfc_err);
                    }
                }
            };
            tracing::info!(
                key = %rfc_err.key,
                status = rfc_err.status,
                msg = %rfc_err.message,
                group = %group,
                "RPY 读函数源码失败，降级 ADT 通道"
            );
            match crate::adt::read_fm_source(&group, &name).await {
                Ok(lines) => (lines, "adt"),
                Err(e) => {
                    tracing::warn!(key = %e.key, "ADT 降级读也失败，返回原 RFC 错误");
                    return Err(rfc_err);
                }
            }
        }
    };

    // prologue 扫描是纯函数；接口读取走 C API（get_param_infos），不受 RPY 限制
    let prologue_json = if want_prologue {
        let deps = crate::discovery::scan_called_functions(&lines, &name, 30);
        let deps_found = deps.len();
        let p = run_blocking(Arc::clone(&pool), move |conn| {
            Ok(crate::discovery::build_function_prologue(conn, &deps))
        })
        .await?;
        Some(serde_json::json!({
            "deps_found": deps_found,
            "resolved": p.resolved,
            "failed": p.failed,
            "text": p.text,
        }))
    } else {
        None
    };

    let count = lines.len();
    let mut body = serde_json::json!({
        "name": name,
        "count": count,
        "source_via": via,
        "lines": lines,
    });
    if let Some(p) = prologue_json {
        body["prologue"] = p;
    }
    Ok(Json(body))
}

/// ⑦ `GET /api/programs/:name/source` —— 读 ABAP 程序源代码（调 `RPY_PROGRAM_READ`，含 include/报表）。
/// 与函数源码同理：RFC 失败（非 NOT_FOUND）自动降级 ADT
/// `/sap/bc/adt/programs/programs/{名}/source/main`，`source_via` 标明来源。
async fn program_source_handler(
    axum::extract::State(pool): axum::extract::State<SharedPool>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, RfcError> {
    crate::api::validate_func_name(&name)?;
    let lookup = name.clone();
    let (lines, via) = match run_blocking(Arc::clone(&pool), move |conn| {
        crate::discovery::read_program_source(conn, &lookup)
    })
    .await
    {
        Ok(lines) => (lines, "rfc"),
        Err(rfc_err) => {
            if !crate::discovery::should_fallback_to_adt(&rfc_err) {
                return Err(rfc_err);
            }
            tracing::info!(
                key = %rfc_err.key,
                status = rfc_err.status,
                msg = %rfc_err.message,
                "RPY 读程序源码失败，降级 ADT 通道"
            );
            match crate::adt::read_program_source(&name).await {
                Ok(lines) => (lines, "adt"),
                Err(e) => {
                    tracing::warn!(key = %e.key, "ADT 降级读也失败，返回原 RFC 错误");
                    return Err(rfc_err);
                }
            }
        }
    };
    let count = lines.len();
    Ok(Json(serde_json::json!({
        "name": name,
        "count": count,
        "source_via": via,
        "lines": lines,
    })))
}

/// 读 SAP 透明表数据的请求体。
#[derive(serde::Deserialize)]
struct TableReadRequest {
    /// 表名（如 T000、USR01）
    table: String,
    /// 要查的字段名（必填，决定列顺序 + 字段名映射）
    fields: Vec<String>,
    /// WHERE 条件（ABAP Open SQL 片段，每段一行；空 = 无过滤）
    #[serde(default, rename = "where")]
    where_clauses: Vec<String>,
    /// 最多返回行数（默认 1000，上限 10000，防全表）
    #[serde(default)]
    rowcount: Option<u32>,
    /// 字段分隔符（解析用，建议罕见字符；默认 \u0001 避免值冲突）
    #[serde(default)]
    delimiter: Option<String>,
}

/// ⑧ `POST /api/table/read` —— 读 SAP 透明表数据（封装 RFC_READ_TABLE，用 ET_DATA 避免截断）
async fn table_read_handler(
    axum::extract::State(pool): axum::extract::State<SharedPool>,
    req: Result<Json<TableReadRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<serde_json::Value>, RfcError> {
    let Json(req) = req.map_err(|r| RfcError {
        code: -1,
        status: r.status().as_u16(),
        message: r.body_text(),
        key: "JSON_INVALID".into(),
    })?;
    if req.table.trim().is_empty() {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "table 不能为空".into(),
            key: "TABLE_EMPTY".into(),
        });
    }
    if req.fields.is_empty() {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "fields 不能为空（必须指定要查的字段）".into(),
            key: "FIELDS_EMPTY".into(),
        });
    }
    let rowcount = req.rowcount.unwrap_or(1000).min(10000);
    let delimiter = req
        .delimiter
        .as_deref()
        .and_then(|s| s.chars().next())
        .unwrap_or('\u{1}');
    let (table, fields) = (req.table.clone(), req.fields.clone());
    let rows = run_blocking(pool, move |conn| {
        crate::discovery::read_table(
            conn,
            &req.table,
            &req.fields,
            &req.where_clauses,
            rowcount,
            delimiter,
        )
    })
    .await?;
    Ok(Json(
        serde_json::json!({"table": table, "fields": fields, "count": rows.len(), "rows": rows}),
    ))
}

// ========================================================================
// ABAP 短转储（ST22）结构化 handler（端点 ⑨~⑪）
// ========================================================================

/// `/api/dumps` 系列查询参数。
#[derive(serde::Deserialize)]
struct DumpsQuery {
    /// 起始时间（UTC `yyyyMMddHHmmss`，透传给 ADT 让其服务端翻页）
    #[serde(default)]
    from: Option<String>,
    /// 结束时间（同上）
    #[serde(default)]
    to: Option<String>,
    /// 最多处理条数（默认 100，上限 1000；grouped 在截断后聚合）
    #[serde(default)]
    limit: Option<usize>,
}

/// 校验 `yyyyMMddHHmmss`（14 位数字）。
fn validate_dump_stamp(s: &str) -> Result<(), RfcError> {
    if s.len() == 14 && s.bytes().all(|b| b.is_ascii_digit()) {
        Ok(())
    } else {
        Err(RfcError {
            code: -1,
            status: 400,
            message: format!(
                "from/to 须为 14 位数字时间戳 yyyyMMddHHmmss（UTC），收到: {}",
                s
            ),
            key: "DUMP_QUERY_INVALID".into(),
        })
    }
}

/// ⑨ `GET /api/dumps` —— 短转储列表（ADT Atom feed 解析为结构化条目，newest-first）。
/// 每条含 error_type / program / user / at / message / key，key 可接 `/api/dumps/{key}/detail`。
async fn dumps_list_handler(
    axum::extract::Query(q): axum::extract::Query<DumpsQuery>,
) -> Result<Json<serde_json::Value>, RfcError> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    if let Some(v) = q.from.as_deref() {
        validate_dump_stamp(v)?;
    }
    if let Some(v) = q.to.as_deref() {
        validate_dump_stamp(v)?;
    }
    let mut dumps = crate::dumps::fetch_feed(q.from.as_deref(), q.to.as_deref()).await?;
    dumps.truncate(limit);
    Ok(Json(
        serde_json::json!({"count": dumps.len(), "dumps": dumps}),
    ))
}

/// ⑩ `GET /api/dumps/grouped` —— 按 (错误类型, 终止程序) 聚合，
/// 回答「什么在反复失败」。组按条数降序，`latest_key` 可直接接详情端点。
async fn dumps_grouped_handler(
    axum::extract::Query(q): axum::extract::Query<DumpsQuery>,
) -> Result<Json<serde_json::Value>, RfcError> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    if let Some(v) = q.from.as_deref() {
        validate_dump_stamp(v)?;
    }
    if let Some(v) = q.to.as_deref() {
        validate_dump_stamp(v)?;
    }
    let mut dumps = crate::dumps::fetch_feed(q.from.as_deref(), q.to.as_deref()).await?;
    dumps.truncate(limit);
    let groups = crate::dumps::group_dumps(&dumps);
    Ok(Json(
        serde_json::json!({"count": groups.len(), "groups": groups}),
    ))
}

/// ⑪ `GET /api/dumps/{key}/detail` —— 单个转储的结构化详情（头表/终止点/调用栈）。
/// key 取列表返回的 `key` 字段；原始 `%20` 编码或解码形态均可。
/// 路由是通配捕获（key 本身可含 `/`），此处剥掉文档形态的尾部 `/detail`。
async fn dump_detail_handler(
    axum::extract::Path(key): axum::extract::Path<String>,
) -> Result<Json<crate::dumps::DumpDetail>, RfcError> {
    let key = key.strip_suffix("/detail").unwrap_or(&key);
    let detail = crate::dumps::fetch_detail(key).await?;
    Ok(Json(detail))
}

// ========================================================================
// ABAP 对象写入（/api/objects/**，端点 ⑫~⑭）
// ========================================================================

/// 解析 `/api/objects/{type}/{name...}/{action}` 通配路径。
/// name 可含 `/`（命名空间对象，如 `/UI5/CL_X`）；action 取最后一段。
fn split_object_path(
    path: &str,
) -> Result<(crate::objects::ObjectType, String, &'static str), RfcError> {
    let bad = |msg: &str| RfcError {
        code: -1,
        status: 400,
        message: format!("对象路径非法（{}）: {}", msg, path),
        key: "OBJECT_PATH_INVALID".into(),
    };
    let (type_seg, rest) = path.split_once('/').ok_or_else(|| bad("缺少类型段"))?;
    let obj_type = crate::objects::ObjectType::parse(type_seg)
        .ok_or_else(|| bad("类型须为 prog/class/func"))?;
    let (name, action) = rest.rsplit_once('/').ok_or_else(|| bad("缺少动作段"))?;
    let action = match action {
        "source" => "source",
        "replace" => "replace",
        "syntax" => "syntax",
        _ => return Err(bad("动作须为 source/replace/syntax")),
    };
    if name.is_empty() || name.len() > 60 || name.split('/').any(|s| s.is_empty() && s != name) {
        // 允许整体以 / 开头（命名空间），但不允许内部空段（/UI5//X）
        let has_empty_inner = name
            .strip_prefix('/')
            .map(|r| r.split('/').any(|s| s.is_empty()))
            .unwrap_or_else(|| name.split('/').any(|s| s.is_empty()));
        if name.is_empty() || name.len() > 60 || has_empty_inner {
            return Err(bad("对象名为空/超长/含空段"));
        }
    }
    if name.chars().any(|c| c.is_control()) || name.contains("..") {
        return Err(bad("对象名含控制字符或 .."));
    }
    Ok((obj_type, name.to_string(), action))
}

/// 对象名宽松校验（类/程序名长于 FM 名上限，不走 validate_func_name）。
fn validate_object_name(name: &str) -> Result<(), RfcError> {
    if name.is_empty() || name.len() > 60 {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: format!("对象名为空或超过 60 字符: {}", name),
            key: "OBJECT_NAME_INVALID".into(),
        });
    }
    Ok(())
}

/// 函数对象的组名解析：显式给出优先，否则经 RFC_FUNCTION_SEARCH 反解。
async fn resolve_group_if_needed(
    pool: &SharedPool,
    obj_type: crate::objects::ObjectType,
    name: &str,
    group_hint: Option<String>,
) -> Result<String, RfcError> {
    use crate::objects::ObjectType;
    if obj_type != ObjectType::Function {
        return Ok(String::new());
    }
    if let Some(g) = group_hint.filter(|g| !g.trim().is_empty()) {
        return Ok(g.trim().to_uppercase());
    }
    let lookup = name.to_string();
    run_blocking(Arc::clone(pool), move |conn| {
        crate::discovery::resolve_function_group(conn, &lookup)
    })
    .await
}

/// PUT /api/objects/{type}/{name}/source 的请求体。
#[derive(serde::Deserialize)]
struct ObjectWriteBody {
    /// 全量源码（必填）
    source: String,
    /// 传输请求号（可选；未给时复用对象已绑定的请求）
    #[serde(default)]
    transport: Option<String>,
    /// 写后是否激活（默认 true）
    #[serde(default = "default_true")]
    activate: bool,
    /// 函数对象的组名（可选，缺省自动反解）
    #[serde(default)]
    group: Option<String>,
}

fn default_true() -> bool {
    true
}

/// ⑫ `PUT /api/objects/{type}/{name}/source` —— 全量写源码。
/// 网关内编排「锁 → PUT → 解锁 → 激活」，lockHandle 不出请求；
/// 激活失败是逻辑结果（HTTP 200 + activation.messages），不是传输错误。
async fn object_write_handler(
    axum::extract::State(pool): axum::extract::State<SharedPool>,
    axum::extract::Path(path): axum::extract::Path<String>,
    req: Result<Json<ObjectWriteBody>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<serde_json::Value>, RfcError> {
    let (obj_type, name, action) = split_object_path(&path)?;
    if action != "source" {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: format!("PUT 仅支持 /source；/{} 请用 POST", action),
            key: "METHOD_ACTION_MISMATCH".into(),
        });
    }
    validate_object_name(&name)?;
    let Json(body) = req.map_err(|r| RfcError {
        code: -1,
        status: r.status().as_u16(),
        message: r.body_text(),
        key: "JSON_INVALID".into(),
    })?;
    if body.source.trim().is_empty() {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "source 不能为空".into(),
            key: "SOURCE_EMPTY".into(),
        });
    }
    let group = resolve_group_if_needed(&pool, obj_type, &name, body.group.clone()).await?;
    let outcome = crate::objects::write_object_source(
        obj_type,
        &name,
        &group,
        &body.source,
        body.transport.as_deref(),
        body.activate,
    )
    .await?;
    Ok(Json(serde_json::to_value(outcome).unwrap_or_default()))
}

/// POST /api/objects/{type}/{name}/replace 与 /syntax 的分发。
async fn object_post_handler(
    axum::extract::State(pool): axum::extract::State<SharedPool>,
    axum::extract::Path(path): axum::extract::Path<String>,
    req: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<serde_json::Value>, RfcError> {
    let (obj_type, name, action) = split_object_path(&path)?;
    validate_object_name(&name)?;
    let Json(raw) = req.map_err(|r| RfcError {
        code: -1,
        status: r.status().as_u16(),
        message: r.body_text(),
        key: "JSON_INVALID".into(),
    })?;
    let group_hint = raw
        .get("group")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let group = resolve_group_if_needed(&pool, obj_type, &name, group_hint).await?;

    match action {
        // ⑬ AI 编辑形态：唯一匹配查找替换后走写入编排
        "replace" => {
            let body: ObjectReplaceBody = serde_json::from_value(raw).map_err(|e| RfcError {
                code: -1,
                status: 400,
                message: format!("请求体字段非法（old_string/new_string 必填）: {}", e),
                key: "JSON_INVALID".into(),
            })?;
            let current = crate::objects::read_current_source(obj_type, &name, &group).await?;
            let updated =
                crate::objects::find_and_replace(&current, &body.old_string, &body.new_string)
                    .map_err(|m| RfcError {
                        code: -1,
                        status: 400,
                        message: m,
                        key: "REPLACE_FAILED".into(),
                    })?;
            if updated == current {
                return Err(RfcError {
                    code: -1,
                    status: 400,
                    message: "替换后内容与原文相同".into(),
                    key: "REPLACE_NO_CHANGE".into(),
                });
            }
            let outcome = crate::objects::write_object_source(
                obj_type,
                &name,
                &group,
                &updated,
                body.transport.as_deref(),
                body.activate,
            )
            .await?;
            let mut v = serde_json::to_value(&outcome).unwrap_or_default();
            v["replaced"] = serde_json::json!(true);
            Ok(Json(v))
        }
        // ⑭ 语法检查（不写库：源码内嵌提交）
        "syntax" => {
            let source = raw
                .get("source")
                .and_then(|v| v.as_str())
                .ok_or_else(|| RfcError {
                    code: -1,
                    status: 400,
                    message: "source 必填（要检查的源码全文）".into(),
                    key: "SOURCE_MISSING".into(),
                })?;
            let base = obj_type.base_rel(&name, &group);
            let issues = crate::objects::syntax_check(&base, source).await?;
            Ok(Json(serde_json::json!({
                "type": "syntax",
                "name": name,
                "count": issues.len(),
                "issues": issues,
            })))
        }
        _ => Err(RfcError {
            code: -1,
            status: 400,
            message: "POST 仅支持 /replace 与 /syntax；全量写用 PUT /source".into(),
            key: "METHOD_ACTION_MISMATCH".into(),
        }),
    }
}

/// POST /api/objects/{type}/{name}/replace 的请求体。
/// （group 若给出，已在反序列化前从原始 JSON 提取，此处不再建模）
#[derive(serde::Deserialize)]
struct ObjectReplaceBody {
    old_string: String,
    new_string: String,
    #[serde(default)]
    transport: Option<String>,
    #[serde(default = "default_true")]
    activate: bool,
}

/// ⑤ GET /api/functions/:name/doc —— 查函数文档（短文本 + SE37 长文本 + 参数说明）
async fn function_doc_handler(
    axum::extract::State(pool): axum::extract::State<SharedPool>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<LangQuery>,
) -> Result<Json<FunctionDocResponse>, RfcError> {
    crate::api::validate_func_name(&name)?;
    let lang = q.lang.unwrap_or_else(default_lang);
    let result = run_blocking(pool, move |conn| {
        // 先取参数描述（parameterText 作为参数文档），同时取短文本
        let param_infos = conn.get_param_infos(&name)?;
        let parameter_docs: Vec<ParamDoc> = param_infos
            .iter()
            .filter(|p| !p.parameter_text.is_empty())
            .map(|p| ParamDoc {
                name: p.name.clone(),
                text: p.parameter_text.clone(),
            })
            .collect();
        // 函数级 short_text 无可靠来源（SAP 元数据里只有参数级 parameterText），
        // 不用首个参数描述冒充函数说明；留空，由 long_text / parameter_docs 提供信息。
        let short_text = String::new();
        // 读 SE37 长文档（失败降级为空 + warning）
        let doc = crate::discovery::read_function_doc(conn, &name, &lang, &short_text)?;
        Ok(FunctionDocResponse {
            name: name.clone(),
            short_text: doc.short_text,
            long_text: doc.long_text,
            warning: doc.warning,
            parameter_docs,
        })
    })
    .await?;
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[test]
    fn timeout_error_is_504() {
        let e = timeout_error(Duration::from_secs(60));
        assert_eq!(e.status, 504, "超时错误应为 504");
        assert!(
            e.message.contains("60"),
            "消息应含超时秒数: {}",
            e.message
        );
    }

    #[test]
    fn mask_value_redacts_sensitive_keys() {
        let secret = ScalarValue::Chars("s3cr3t".into());
        assert_eq!(mask_value("PASSWORD", &secret), "***");
        assert_eq!(mask_value("USER_PASSWD", &secret), "***");
        assert_eq!(mask_value("API_KEY", &secret), "***");
        assert_eq!(mask_value("TOKEN", &secret), "***");
        // 非敏感 key 保留值
        assert_eq!(mask_value("REQUTEXT", &ScalarValue::Chars("hi".into())), "hi");
        assert_eq!(mask_value("MAX_ROWS", &ScalarValue::Int(100)), "100");
    }

    #[test]
    fn mask_value_truncates_long() {
        let long = ScalarValue::Chars("x".repeat(100));
        let m = mask_value("REQUTEXT", &long);
        assert!(m.ends_with('…'), "长值应以省略号结尾: {}", m);
        assert_eq!(m.chars().count(), 81); // 80 个 x + …
    }

    #[test]
    fn summarize_params_redacts_and_structures() {
        let mut req = InvokeRequest {
            func_name: "STFC_CONNECTION".into(),
            ..Default::default()
        };
        req.inputs
            .insert("REQUTEXT".into(), ScalarValue::Chars("hi".into()));
        req.inputs
            .insert("PASSWORD".into(), ScalarValue::Chars("secret".into()));
        let s = summarize_params(&req);
        assert!(s.contains("inputs{"), "应含 inputs 块: {}", s);
        assert!(s.contains("REQUTEXT=hi"), "应含明文值: {}", s);
        assert!(s.contains("PASSWORD=***"), "密码应脱敏: {}", s);
        assert!(!s.contains("secret"), "不应泄露明文密码: {}", s);
    }

    #[test]
    fn summarize_params_empty() {
        let req = InvokeRequest::default();
        assert_eq!(summarize_params(&req), "(无参数)");
    }

    /// 读取 axum 响应 body 为 String
    async fn body_string(body: axum::body::Body) -> String {
        let bytes = body.collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let resp = static_app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = body_string(resp.into_body()).await;
        assert!(body.contains("\"status\":\"ok\""));
    }

    #[tokio::test]
    async fn agents_md_returns_markdown() {
        let resp = static_app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/agents.md")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let ct = resp.headers().get(axum::http::header::CONTENT_TYPE).unwrap();
        assert!(ct.to_str().unwrap().contains("text/markdown"));
        let body = body_string(resp.into_body()).await;
        assert!(!body.is_empty());
        // 内容应包含项目名（验证编译期嵌入成功）
        assert!(body.contains("sap-for-agents") || body.contains("SAP"));
    }

    #[tokio::test]
    async fn index_html_replaces_base_url_from_host_header() {
        let resp = static_app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .header("host", "192.168.1.5:9999")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = body_string(resp.into_body()).await;
        // Host 头应被替换进模板（不再含占位符）
        assert!(!body.contains("{{BASE_URL}}"));
        assert!(!body.contains("{{AGENTS_URL}}"));
        assert!(body.contains("http://192.168.1.5:9999"));
    }

    #[tokio::test]
    async fn index_html_defaults_when_no_host_header() {
        let resp = static_app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = body_string(resp.into_body()).await;
        assert!(body.contains("http://127.0.0.1:3000"));
    }

    #[tokio::test]
    async fn openapi_json_serves_spec_with_host_derived_server() {
        let resp = static_app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/openapi.json")
                    .header("host", "192.168.1.5:9999")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let ct = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("application/json"), "Content-Type 应为 JSON: {}", ct);
        let body = body_string(resp.into_body()).await;
        let spec: serde_json::Value = serde_json::from_str(&body).expect("body 应为合法 JSON");
        assert_eq!(spec["openapi"], "3.0.3");
        // servers 按请求 Host 头推导
        assert_eq!(spec["servers"][0]["url"], "http://192.168.1.5:9999");
        // 核心端点在规范里
        assert!(spec["paths"]["/api/rfc"].is_object());
        assert!(spec["paths"]["/api/functions/search"].is_object());
    }
}
