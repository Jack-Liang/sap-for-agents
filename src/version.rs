//! 版本与能力自描述：`GET /api/version` 与 MCP 工具 `get_gateway_info` 的数据源。
//!
//! 解决的问题：Agent 每次新会话都要靠撞 401/403/503/429 才能发现部署差异
//! （要不要 token、只读模式、ADT 开没开、限流）。这里一次调用全部给出。
//!
//! 设计：
//! - **本地信息秒回、永不失败**：网关版本 / git commit / 能力开关全部读进程内
//!   静态，不碰 SAP——SAP 宕机时恰恰是最需要确认"在跟哪个网关对话"的时刻。
//! - **SAP 信息懒加载 + 永久缓存**：sysid/release 等在网关生命周期内不变，
//!   首次请求借一条连接调一次 `RFC_SYSTEM_INFO`，成功后缓存到进程退出；
//!   失败**不**缓存（`sap` 置 null + `sap_error` 带原因，下次请求可重试），
//!   端点仍返回 200。
//! - **client 从本地配置读**：登录客户端号不需要 RFC 调用。

use crate::connection::RemoteSystemInfo;
use crate::server::SharedPool;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
/// 登录客户端号（启动期由 [`init_sap_client`] 写入）。
static SAP_CLIENT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// 启动期写入 SAP 客户端号（main 调一次；读自 `SAP_CLIENT` 配置，零 RFC 成本）。
pub fn init_sap_client(client: String) {
    let _ = SAP_CLIENT.set(client);
}

/// git 提交号（build.rs 注入；源码包等无 git 环境为 "unknown"，脏树带 -dirty）。
pub fn git_commit() -> &'static str {
    option_env!("SAP_GIT_COMMIT").unwrap_or("unknown")
}

/// SAP 系统信息缓存：成功一次永久复用；失败不缓存（下次重试）。
/// RwLock 而非 OnceCell：OnceCell 一旦写入无法重试失败的获取。
static SAP_INFO: tokio::sync::RwLock<Option<Arc<RemoteSystemInfo>>> =
    tokio::sync::RwLock::const_new(None);

/// SAP 系统信息获取的超时上限（独立于全局请求超时：自描述端点不该被慢 SAP 拖住）。
const SAP_INFO_TIMEOUT: Duration = Duration::from_secs(10);

/// 组装完整的自描述 JSON（`/api/version` 与 `get_gateway_info` 共用）。
pub async fn gateway_info(pool: &SharedPool) -> Value {
    let mut out = local_info();
    // latest 与 SAP 块独立：任一不可用不影响另一个
    out["latest"] = latest_json().await;
    match fetch_cached_sap_info(pool).await {
        Ok(info) => out["sap"] = json!({
            "sysid": info.sysid,
            "release": info.release,
            "host": info.host,
            "os": info.os,
            "destination": info.destination,
            // 客户端号来自本地配置（RFC 层免费），失败场景也照常给出
            "client": SAP_CLIENT.get().cloned().unwrap_or_default(),
        }),
        Err(e) => {
            out["sap"] = Value::Null;
            out["sap_error"] = json!(e.message);
        }
    }
    out
}

/// 纯本地部分：版本 / commit / 能力开关。零 SAP 依赖、纯函数，单测锁定形状。
pub fn local_info() -> Value {
    json!({
        "name": "sap-for-agents",
        "version": env!("CARGO_PKG_VERSION"),
        "commit": git_commit(),
        "capabilities": {
            // true = 已设 SAP_API_KEY，/api/* 需 Bearer token（本端点除外）
            "auth": crate::auth::is_enabled(),
            // true = SAP_READ_ONLY=1，写端点（objects 写 / ADT 写方法）返回 403
            "read_only": crate::server::read_only_active(),
            // true = SAP_ADT_BASE_URL 非空，ADT 代理与 dumps 端点可用
            "adt": crate::adt::is_enabled(),
            // 按 IP 每秒请求上限；null = 不限流
            "rate_limit_rps": crate::server::rate_limit_rps(),
        },
    })
}

/// 取缓存的 SAP 系统信息；未缓存时借连接调一次 `RFC_SYSTEM_INFO`。
/// 成功 → 写缓存永久复用；失败 → 不缓存，调用方拿到错误（下次重试）。
async fn fetch_cached_sap_info(pool: &SharedPool) -> Result<Arc<RemoteSystemInfo>, crate::error::RfcError> {
    if let Some(cached) = SAP_INFO.read().await.clone() {
        return Ok(cached);
    }
    // 未缓存：取一次（并发首批请求可能重复取，无害——池有多条连接，结果幂等）
    let fetched = crate::server::run_blocking_with_timeout(
        Arc::clone(pool),
        SAP_INFO_TIMEOUT,
        |conn| conn.system_info(),
    )
    .await?;
    let info = Arc::new(fetched);
    *SAP_INFO.write().await = Some(Arc::clone(&info));
    tracing::info!(
        sysid = %info.sysid,
        release = %info.release,
        "SAP 系统信息已缓存（/api/version）"
    );
    Ok(info)
}

// ========================================================================
// 新版本检查（GitHub Releases；默认开启，SAP_UPDATE_CHECK=off 关闭）
// ========================================================================

/// 已知的最新发布信息（后台任务写入；None = 尚未取到/禁用/网络不可达）。
#[derive(Debug, Clone)]
pub struct LatestRelease {
    /// 最新 tag 去掉 `v` 前缀（如 "0.11.0"）
    pub version: String,
    /// Release 页面链接
    pub url: String,
}

static LATEST: tokio::sync::RwLock<Option<Arc<LatestRelease>>> =
    tokio::sync::RwLock::const_new(None);

/// 检查周期：启动即查一次，之后每 24h 复查（GitHub 匿名限额 60 次/时，绰绰有余）。
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// 后台版本检查循环（main 里 spawn）。失败静默降级（debug 级日志）——
/// 更新检查绝不能拖慢请求或刷屏；离线/隔离环境是常态而非异常。
pub async fn update_checker_loop() {
    loop {
        match fetch_latest_release().await {
            Ok(rel) => {
                if is_newer(&rel.version, env!("CARGO_PKG_VERSION")) {
                    tracing::info!(
                        current = env!("CARGO_PKG_VERSION"),
                        latest = %rel.version,
                        url = %rel.url,
                        "发现新版本（/api/version latest 块与首页页脚可见）"
                    );
                }
                *LATEST.write().await = Some(Arc::new(rel));
            }
            Err(e) => tracing::debug!(error = %e, "新版本检查失败（不影响服务）"),
        }
        tokio::time::sleep(UPDATE_CHECK_INTERVAL).await;
    }
}

/// 拉取 GitHub 最新 Release（匿名、带 UA、5s 超时）。仅向 api.github.com 发一个
/// 无任何用户数据的 GET——不做遥测，不携带部署环境信息。
async fn fetch_latest_release() -> Result<LatestRelease, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("sap-for-agents/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get("https://api.github.com/repos/Jack-Liang/sap-for-agents/releases/latest")
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    parse_latest_release(&resp).ok_or_else(|| "响应缺少 tag_name/html_url".into())
}

/// 解析 releases/latest 响应（独立成纯函数便于单测）。
fn parse_latest_release(body: &str) -> Option<LatestRelease> {
    let v: Value = serde_json::from_str(body).ok()?;
    let tag = v.get("tag_name")?.as_str()?;
    let url = v.get("html_url")?.as_str()?;
    if tag.is_empty() || url.is_empty() {
        return None;
    }
    Some(LatestRelease {
        version: tag.trim_start_matches('v').to_string(),
        url: url.to_string(),
    })
}

/// semver 三段比较：latest 是否**严格大于** current。
/// 解析失败一律 false——检查失败宁可漏报不可误报。
fn is_newer(latest: &str, current: &str) -> bool {
    fn parse(s: &str) -> Option<(u64, u64, u64)> {
        let mut p = s.split('.');
        let v = (
            p.next()?.parse().ok()?,
            p.next()?.parse().ok()?,
            p.next()?.parse().ok()?,
        );
        p.next().is_none().then_some(v)
    }
    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// `/api/version` 的 `latest` 块；null = 尚未取到 / 已禁用 / 不可达。
pub async fn latest_json() -> Value {
    match LATEST.read().await.clone() {
        Some(rel) => json!({
            "version": rel.version,
            "url": rel.url,
            "update_available": is_newer(&rel.version, env!("CARGO_PKG_VERSION")),
        }),
        None => Value::Null,
    }
}

/// 首页页脚的更新提示：仅当确认有更新时返回 Some((version, url))。
pub async fn update_hint() -> Option<(String, String)> {
    let rel = LATEST.read().await.clone()?;
    is_newer(&rel.version, env!("CARGO_PKG_VERSION"))
        .then(|| (rel.version.clone(), rel.url.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本地部分（版本/commit/capabilities）形状锁定：纯静态、无 SAP 依赖。
    #[test]
    fn local_info_shape() {
        let v = local_info();
        assert_eq!(v["name"], "sap-for-agents");
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert!(!v["commit"].as_str().unwrap().is_empty(), "commit 必有值");
        // 四个能力开关字段都在，且类型正确（rate_limit_rps 可为 null）
        for key in ["auth", "read_only", "adt"] {
            assert!(v["capabilities"][key].is_boolean(), "capabilities.{key} 应为布尔");
        }
        assert!(
            v["capabilities"]["rate_limit_rps"].is_null()
                || v["capabilities"]["rate_limit_rps"].is_u64()
        );
    }

    #[test]
    fn git_commit_is_populated() {
        // 本仓库构建必有注入值（CI 从源码包构建才可能是 unknown）
        assert!(!git_commit().is_empty());
    }

    #[test]
    fn semver_is_newer_compares_three_segments() {
        use is_newer as newer;
        assert!(newer("0.11.0", "0.10.0"), "次版本更高");
        assert!(newer("0.10.1", "0.10.0"), "补丁更高");
        assert!(newer("1.0.0", "0.99.99"), "主版本更高");
        assert!(!newer("0.10.0", "0.10.0"), "相等不算更新");
        assert!(!newer("0.9.9", "0.10.0"), "更低不算更新");
        // 解析失败宁可漏报不可误报
        assert!(!newer("", "0.10.0"));
        assert!(!newer("latest", "0.10.0"));
        assert!(!newer("0.11", "0.10.0"), "缺段视为非法");
        assert!(!newer("0.11.0.1", "0.10.0"), "多段视为非法");
        assert!(!newer("0.11.0-rc1", "0.10.0"), "带预发布后缀视为非法");
    }

    #[test]
    fn parse_latest_release_extracts_tag_and_url() {
        let rel = parse_latest_release(
            r#"{"tag_name":"v0.11.0","html_url":"https://github.com/Jack-Liang/sap-for-agents/releases/tag/v0.11.0","draft":false}"#,
        )
        .expect("合法响应应解析成功");
        assert_eq!(rel.version, "0.11.0", "tag 的 v 前缀应剥掉");
        assert!(rel.url.contains("/tag/v0.11.0"));
        // 缺字段 / 非法 JSON → None（调用方静默降级）
        assert!(parse_latest_release(r#"{"tag_name":"v1.0.0"}"#).is_none());
        assert!(parse_latest_release("not json").is_none());
        assert!(parse_latest_release(r#"{"tag_name":"","html_url":"u"}"#).is_none());
    }
}
