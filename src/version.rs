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
}
