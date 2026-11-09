//! 运行期热设置（v0.20 第二期）：网关级配置的读取 / 修改 / 持久化。
//!
//! 解决的问题：此前全部网关设置在启动期从 `.env` 读进 [`AppConfig`] 后
//! 不可变——改超时、限流、只读开关、API key 都要改文件 + 重启。现在分三层：
//!
//! - **热生效**（本模块的 [`HotSettings`]，每请求现读）：请求超时、限流、
//!   只读、API key、排空秒数、更新检查、默认语言。`PUT /api/settings` 即改
//!   即生效，同时落盘 `sap_gateway.toml`（重启后仍生效）。
//! - **系统重建时生效**：池大小/空闲校验/ADT 默认值烤在系统句柄里——全局
//!   默认值只影响之后新建/编辑的系统；改运行中系统用
//!   `PUT /api/setup/system/{name}`（同样热）。
//! - **需重启**：监听地址、运行档位、两个文件路径。
//!
//! 持久化格式（`sap_gateway.toml`，0600——存 api_key 明文，与 `.env` 同级）：
//!
//! ```toml
//! [gateway]
//! request_timeout_secs = 120
//! rate_limit_rps = 0      # 0 = 不限流
//! read_only = false
//! # api_key = "..."       # 留空键不写；写空串 = 关闭鉴权
//! drain_secs = 5
//! update_check = true
//! lang = "EN"
//! ```
//!
//! 优先级：**文件 > 环境变量 > 默认值**（[`overlay_from_file`] 在启动期把
//! 文件覆盖到 [`AppConfig`]，[`settings::init`] 再从 cfg 初始化热态）。
//! 文件里**没写**的键回落 env/默认——存量部署零感知。

use crate::config::AppConfig;
use crate::error::RfcError;
use crate::server::SharedState;
use axum::Json;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// 可热修改的网关设置（每请求现读；写路径经 [`update`] 原子换新）。
#[derive(Debug, Clone)]
pub struct HotSettings {
    /// 全局默认单请求超时。系统句柄可用 toml 的 request_timeout_secs 覆盖。
    pub request_timeout: Duration,
    /// 按 IP 限流（None = 不限流）。
    pub rate_limit_rps: Option<u32>,
    /// 全局只读（与各系统自己的 read_only 取 OR——系统级只读不能被全局放开）。
    pub read_only: bool,
    /// 停机排空窗口秒数。
    pub drain_secs: u64,
    /// 每 24h 的 GitHub Release 检查。
    pub update_check: bool,
    /// 默认语言（文档/接口读取的 ?lang 缺省；连接语言在系统配置里）。
    pub lang: String,
}

static CURRENT: RwLock<Option<Arc<HotSettings>>> = RwLock::new(None);

/// 启动期初始化（main 在 config::load 之后调一次）：热态从生效配置取值，
/// 并同步各消费点的启动快照（限流器/排空/auth key）。
pub fn init(cfg: &AppConfig) {
    let s = HotSettings {
        request_timeout: cfg.request_timeout,
        rate_limit_rps: cfg.rate_limit_rps,
        read_only: cfg.read_only,
        drain_secs: cfg.drain_secs,
        update_check: cfg.update_check,
        lang: cfg.lang.clone(),
    };
    crate::server::init_rate_limiter(s.rate_limit_rps);
    crate::server::set_drain_secs(s.drain_secs);
    crate::auth::set_key(cfg.api_key.clone());
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(s));
}

/// 当前热设置。未 init 时回落内置默认（仅测试/极端场景触达——main 启动
/// 即 init；这里选择兜底而非 panic，让任何消费点都不可能炸掉请求）。
pub fn current() -> Arc<HotSettings> {
    CURRENT
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| {
            Arc::new(HotSettings {
                request_timeout: Duration::from_secs(60),
                rate_limit_rps: None,
                read_only: false,
                drain_secs: 5,
                update_check: true,
                lang: "EN".into(),
            })
        })
}

// ========================================================================
// sap_gateway.toml：读取覆盖（启动期）与写入（PUT /api/settings）
// ========================================================================

/// 网关设置文件读改写的进程内互斥（独立于 setup.rs 的 systems 文件锁）。
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// 启动期把 `sap_gateway.toml` 的 `[gateway]` 表覆盖到 cfg（文件 > env）。
/// 文件不存在 → 原样返回。未知键 warn 跳过（向前兼容）。
pub(crate) fn overlay_from_file(mut cfg: AppConfig) -> Result<AppConfig, String> {
    if !cfg.gateway_file.exists() {
        return Ok(cfg);
    }
    let content = std::fs::read_to_string(&cfg.gateway_file)
        .map_err(|e| format!("读取 {} 失败: {}", cfg.gateway_file.display(), e))?;
    let root: toml::Value = toml::from_str(&content).map_err(|e| {
        format!(
            "解析 {} 失败（文件被改坏?）: {}",
            cfg.gateway_file.display(),
            e
        )
    })?;
    let Some(gw) = root.get("gateway").and_then(|v| v.as_table()) else {
        return Ok(cfg); // 没有 [gateway] 表 → 无覆盖
    };
    let num = |k: &str| gw.get(k).and_then(|v| v.as_integer());
    if let Some(n) = num("request_timeout_secs").filter(|&n| n >= 1) {
        cfg.request_timeout = Duration::from_secs(n as u64);
    }
    if let Some(n) = num("rate_limit_rps") {
        cfg.rate_limit_rps = u32::try_from(n).ok().filter(|&r| r >= 1);
    }
    if let Some(b) = gw.get("read_only").and_then(|v| v.as_bool()) {
        cfg.read_only = b;
    }
    if let Some(k) = gw.get("api_key").and_then(|v| v.as_str()) {
        cfg.api_key = (!k.trim().is_empty()).then(|| k.to_string());
    }
    if let Some(n) = num("drain_secs") {
        cfg.drain_secs = n.clamp(0, 600) as u64;
    }
    if let Some(b) = gw.get("update_check").and_then(|v| v.as_bool()) {
        cfg.update_check = b;
    }
    if let Some(l) = gw.get("lang").and_then(|v| v.as_str()) {
        cfg.lang = l.to_uppercase();
    }
    for k in gw.keys() {
        if !matches!(
            k.as_str(),
            "request_timeout_secs"
                | "rate_limit_rps"
                | "read_only"
                | "api_key"
                | "drain_secs"
                | "update_check"
                | "lang"
        ) {
            tracing::warn!(key = %k, file = %cfg.gateway_file.display(), "sap_gateway.toml 含未知键（已忽略）");
        }
    }
    Ok(cfg)
}

/// 把若干键值合并进 `sap_gateway.toml` 的 `[gateway]` 表（读改写，互斥；
/// 新建文件带注释头）。空表不落盘。
fn persist_gateway(
    cfg: &AppConfig,
    updates: BTreeMap<&'static str, toml::Value>,
) -> Result<(), String> {
    if updates.is_empty() {
        return Ok(());
    }
    let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = &cfg.gateway_file;
    let (mut root, existed) = if path.exists() {
        let content = std::fs::read_to_string(path).map_err(|e| format!("读取失败: {}", e))?;
        let root: toml::Value =
            toml::from_str(&content).map_err(|e| format!("解析失败（文件被改坏?）: {}", e))?;
        (root, true)
    } else {
        (toml::Value::Table(Default::default()), false)
    };
    let root_table = root
        .as_table_mut()
        .ok_or_else(|| "文件根不是 toml 表".to_string())?;
    let gw = root_table
        .entry("gateway".to_string())
        .or_insert_with(|| toml::Value::Table(Default::default()));
    let gw = gw
        .as_table_mut()
        .ok_or_else(|| "[gateway] 不是 toml 表".to_string())?;
    for (k, v) in updates {
        gw.insert(k.to_string(), v);
    }
    let mut out = String::new();
    if !existed {
        out.push_str(
            "# sap_gateway.toml —— sap-for-agents 网关设置（运行期热改，PUT /api/settings 维护）\n",
        );
        out.push_str("# 文件值覆盖 .env；删掉某个键即回落 .env/默认值。系统（SAP 连接）配置在 sap_systems.toml。\n\n");
    }
    let body = toml::to_string_pretty(&root).map_err(|e| format!("序列化失败: {}", e))?;
    out.push_str(&body);
    crate::config::write_private_file(path, &out)
}

/// `[gateway]` 表里出现过的键（GET /api/settings 的来源标注用）。文件不存在
/// 或无表 → 空集。
fn gateway_file_keys(cfg: &AppConfig) -> Vec<String> {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(content) = std::fs::read_to_string(&cfg.gateway_file) else {
        return vec![];
    };
    let Ok(root) = toml::from_str::<toml::Value>(&content) else {
        return vec![];
    };
    root.get("gateway")
        .and_then(|v| v.as_table())
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default()
}

// ========================================================================
// GET / PUT /api/settings
// ========================================================================

/// 单个设置项的展示形态：值 + 来源（default / env / file）。
fn item(value: impl serde::Serialize, source: &'static str) -> serde_json::Value {
    serde_json::json!({"value": value, "source": source})
}

/// `GET /api/settings`：三层设置清单（每项带来源标注；api_key 永不回显值）。
pub async fn get_handler(
    axum::extract::State(state): axum::extract::State<SharedState>,
) -> Json<serde_json::Value> {
    let s = current();
    let cfg = state.cfg();
    let file_keys = gateway_file_keys(cfg);
    let src = |file_key: &str, env_key: &str| -> &'static str {
        if file_keys.iter().any(|k| k == file_key) {
            "file"
        } else if std::env::var(env_key).is_ok() {
            "env"
        } else {
            "default"
        }
    };
    let idle_secs = if cfg.pool_idle_validate == Duration::MAX {
        0
    } else {
        cfg.pool_idle_validate.as_secs()
    };
    Json(serde_json::json!({
        "hot": {
            "request_timeout_secs": item(s.request_timeout.as_secs(), src("request_timeout_secs", "SAP_REQUEST_TIMEOUT_SECS")),
            "rate_limit_rps": item(s.rate_limit_rps, src("rate_limit_rps", "SAP_RATE_LIMIT_RPS")),
            "read_only": item(s.read_only, src("read_only", "SAP_READ_ONLY")),
            "api_key": item(crate::auth::is_enabled(), src("api_key", "SAP_API_KEY")),
            "drain_secs": item(s.drain_secs, src("drain_secs", "SAP_DRAIN_SECS")),
            "update_check": item(s.update_check, src("update_check", "SAP_UPDATE_CHECK")),
            "lang": item(s.lang.clone(), src("lang", "SAP_LANG")),
        },
        // 全局默认值烤在系统句柄里：只影响之后新建/编辑的系统
        "apply_on_system_rebuild": {
            "pool_size": item(cfg.pool_size, src("", "SAP_POOL_SIZE")),
            "pool_idle_validate_secs": item(idle_secs, src("", "SAP_POOL_IDLE_VALIDATE_SECS")),
            "adt_base_url": item(cfg.adt_base_url.clone().filter(|s| !s.is_empty()), src("", "SAP_ADT_BASE_URL")),
        },
        "apply_on_restart": {
            "listen_addr": item(cfg.listen_addr.clone(), src("", "SAP_LISTEN_ADDR")),
            "mode": item(if cfg.runtime_mode { "runtime" } else { "full" }, src("", "SAP_MODE")),
            "systems_file": item(cfg.systems_file.display().to_string(), src("", "SAP_SYSTEMS_FILE")),
            "registry_file": item(cfg.registry_file.display().to_string(), src("", "SAP_REGISTRY_FILE")),
        },
        "file": cfg.gateway_file.display().to_string(),
        "file_exists": cfg.gateway_file.exists(),
        "note": "hot=PUT 即生效并落盘；apply_on_system_rebuild=全局默认，仅影响之后新建/编辑的系统（改运行中系统用 PUT /api/setup/system/{name}）；apply_on_restart=改 .env 后重启。api_key 只回显是否已设置",
    }))
}

/// `PUT /api/settings` 请求体。半更新：未提供的键保持原值。
/// `rate_limit_rps: null` = 关闭限流；`api_key: ""` = 关闭鉴权。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSettingsRequest {
    request_timeout_secs: Option<u64>,
    /// 0 = 关闭限流（与 .env 的 SAP_RATE_LIMIT_RPS=0 语义一致）；缺省 = 保持
    rate_limit_rps: Option<u32>,
    read_only: Option<bool>,
    api_key: Option<String>,
    drain_secs: Option<u64>,
    update_check: Option<bool>,
    lang: Option<String>,
}

/// `PUT /api/settings`：批量修改热设置（逐项校验，任一项非法则整体拒绝），
/// 同步各消费点 → 原子换新热态 → 落盘 `sap_gateway.toml`。
pub async fn update_handler(
    axum::extract::State(state): axum::extract::State<SharedState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<UpdateSettingsRequest>,
) -> Result<Json<serde_json::Value>, RfcError> {
    // 与系统配置写同一道门：无鉴权时仅本机回环可改
    crate::setup::admin_write_gate(&state, &headers)?;

    let bad = |msg: String| RfcError {
        code: -1,
        status: 400,
        message: format!("设置不合法（未写入）: {}", msg),
        key: "SETTING_PARAM_INVALID".into(),
    };

    let mut s = (*current()).clone();
    let mut applied: Vec<&'static str> = Vec::new();
    let mut persist: BTreeMap<&'static str, toml::Value> = BTreeMap::new();

    if let Some(v) = body.request_timeout_secs {
        if v < 1 {
            return Err(bad("request_timeout_secs 必须 ≥ 1".into()));
        }
        s.request_timeout = Duration::from_secs(v);
        applied.push("request_timeout_secs");
        persist.insert("request_timeout_secs", toml::Value::Integer(v as i64));
    }
    if let Some(r) = body.rate_limit_rps {
        // 0 = 关闭（None），≥1 = 启用；文件里同样以 0 表示关闭
        s.rate_limit_rps = (r >= 1).then_some(r);
        applied.push("rate_limit_rps");
        persist.insert("rate_limit_rps", toml::Value::Integer(r as i64));
    }
    if let Some(b) = body.read_only {
        s.read_only = b;
        applied.push("read_only");
        persist.insert("read_only", toml::Value::Boolean(b));
    }
    if let Some(k) = body.api_key {
        let key = (!k.trim().is_empty()).then_some(k);
        crate::auth::set_key(key.clone());
        applied.push("api_key");
        persist.insert("api_key", toml::Value::from(key.unwrap_or_default()));
    }
    if let Some(v) = body.drain_secs {
        if v > 600 {
            return Err(bad("drain_secs 必须 ≤ 600".into()));
        }
        s.drain_secs = v;
        applied.push("drain_secs");
        persist.insert("drain_secs", toml::Value::Integer(v as i64));
    }
    if let Some(b) = body.update_check {
        s.update_check = b;
        applied.push("update_check");
        persist.insert("update_check", toml::Value::Boolean(b));
    }
    if let Some(l) = body.lang {
        let up = l.trim().to_uppercase();
        if !(1..=2).contains(&up.len()) || !up.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(bad(format!(
                "lang 须为 1-2 位字母（SAP 语言键，如 EN/ZH），收到 '{}'",
                l
            )));
        }
        s.lang = up;
        applied.push("lang");
        persist.insert("lang", toml::Value::from(s.lang.clone()));
    }

    // 同步消费点快照 → 原子换新热态
    crate::server::init_rate_limiter(s.rate_limit_rps);
    crate::server::set_drain_secs(s.drain_secs);
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(s));

    let cfg = state.cfg();
    persist_gateway(cfg, persist).map_err(|e| RfcError {
        code: -1,
        status: 500,
        message: format!(
            "设置已生效但写入 {} 失败（重启后回落 .env/默认值）: {}",
            cfg.gateway_file.display(),
            e
        ),
        key: "SETTING_WRITE_FAILED".into(),
    })?;

    tracing::info!(
        applied = ?applied,
        file = %cfg.gateway_file.display(),
        "settings：网关设置已热更新并落盘"
    );

    Ok(Json(serde_json::json!({
        "saved": true,
        "applied": applied,
        "file": cfg.gateway_file.display().to_string(),
        "note": "已即时生效（限流/超时/只读/鉴权下一请求起按新值）。GET /api/settings 复核",
    })))
}

// ========================================================================
// 测试（纯函数路径：overlay / persist / 请求解析）
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 最小 AppConfig（gateway_file 指向临时路径，测试内各自覆盖）。
    fn test_cfg(gateway_file: &str) -> AppConfig {
        AppConfig {
            conn_params: vec![],
            listen_addr: "127.0.0.1:3000".into(),
            pool_size: 2,
            pool_idle_validate: Duration::from_secs(30),
            api_key: None,
            request_timeout: Duration::from_secs(60),
            drain_secs: 5,
            rate_limit_rps: None,
            adt_base_url: None,
            adt_user: String::new(),
            adt_passwd: String::new(),
            read_only: false,
            registry_file: "test-registry.json".into(),
            runtime_mode: false,
            update_check: false,
            systems_file: std::env::temp_dir().join("no-such-systems.toml"),
            gateway_file: gateway_file.into(),
            setup_enabled: true,
            setup_force: false,
            open_browser: false,
            lang: "EN".into(),
        }
    }

    #[test]
    fn overlay_applies_file_values_over_env_defaults() {
        let dir = std::env::temp_dir().join(format!("sap-settings-o-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gateway.toml");
        std::fs::write(
            &path,
            "[gateway]\nrequest_timeout_secs = 120\nrate_limit_rps = 0\nread_only = true\nlang = \"zh\"\n",
        )
        .unwrap();
        let cfg = overlay_from_file(test_cfg(path.to_str().unwrap())).unwrap();
        assert_eq!(cfg.request_timeout, Duration::from_secs(120));
        assert_eq!(cfg.rate_limit_rps, None, "0 = 关闭限流");
        assert!(cfg.read_only);
        assert_eq!(cfg.lang, "ZH");
        // 没写的键保持原值
        assert_eq!(cfg.drain_secs, 5);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn overlay_ignores_missing_file_and_empty_table() {
        let cfg = overlay_from_file(test_cfg("/no/such/gateway.toml")).unwrap();
        assert_eq!(cfg.request_timeout, Duration::from_secs(60));
        let dir = std::env::temp_dir().join(format!("sap-settings-e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.toml");
        std::fs::write(&path, "# 只有注释\n").unwrap();
        let cfg = overlay_from_file(test_cfg(path.to_str().unwrap())).unwrap();
        assert_eq!(cfg.request_timeout, Duration::from_secs(60));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persist_then_overlay_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sap-settings-p-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gateway.toml");
        let _ = std::fs::remove_file(&path);
        let cfg = test_cfg(path.to_str().unwrap());

        let mut updates = BTreeMap::new();
        updates.insert("request_timeout_secs", toml::Value::Integer(90));
        updates.insert("api_key", toml::Value::from("s3cret"));
        persist_gateway(&cfg, updates).unwrap();

        // 新建文件带注释头 + 0600
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("# sap_gateway.toml"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "含 api_key 的文件必须 0600");
        }

        let cfg = overlay_from_file(cfg).unwrap();
        assert_eq!(cfg.request_timeout, Duration::from_secs(90));
        assert_eq!(cfg.api_key.as_deref(), Some("s3cret"));

        // 再写一次：合并不覆盖无关键；api_key 置空 = 关闭鉴权
        let mut updates = BTreeMap::new();
        updates.insert("drain_secs", toml::Value::Integer(9));
        updates.insert("api_key", toml::Value::from(""));
        persist_gateway(&cfg, updates).unwrap();
        let cfg = overlay_from_file(cfg).unwrap();
        assert_eq!(cfg.request_timeout, Duration::from_secs(90), "未动的键保留");
        assert_eq!(cfg.drain_secs, 9);
        assert_eq!(cfg.api_key, None, "空串 = 关闭");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_request_rate_limit_zero_means_off() {
        let r: UpdateSettingsRequest = serde_json::from_str(r#"{"rate_limit_rps": 0}"#).unwrap();
        assert_eq!(r.rate_limit_rps, Some(0), "0 = 关闭限流");
        let r: UpdateSettingsRequest = serde_json::from_str(r#"{"rate_limit_rps": 5}"#).unwrap();
        assert_eq!(r.rate_limit_rps, Some(5));
        let r: UpdateSettingsRequest = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(r.rate_limit_rps, None, "缺省 = 不动");
        // 未知键拒绝
        assert!(serde_json::from_str::<UpdateSettingsRequest>(r#"{"listen_addr":"x"}"#).is_err());
    }
}
