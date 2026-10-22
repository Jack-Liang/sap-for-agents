//! 多系统：`sap_systems.toml` 解析 + 每系统运行态句柄 + 作用域提取器。
//!
//! 一个网关进程同时连接 N 个 SAP 系统：
//!
//! - **配置层**（[`SystemSpec`]）：`SAP_SYSTEMS_FILE`（默认 `./sap_systems.toml`）
//!   声明命名系统；文件不存在时从现有 `SAP_*` 环境变量合成名为 `default` 的
//!   单系统——存量部署零感知。toml 存在时系统集合完全由 toml 定义（.env 的
//!   `SAP_ASHOST` 一族被忽略，启动打提示日志）；全局默认值（池大小/超时/只读）
//!   仍来自 .env，toml 每系统字段可覆盖。密码支持 `${VAR}` 环境变量插值。
//!   两者都缺 → **未配置模式**：空注册表照样监听，系统由 setup API
//!   （`POST /api/setup/system`，见 setup.rs）引导写入后热加载。
//! - **运行态**（[`SystemHandle`]/[`SystemRegistry`]）：每系统一个连接池 +
//!   ADT 端点 + 元数据缓存。默认系统急切建首连（保留 fail-fast），其余惰性
//!   （目标系统暂时不可达不阻塞网关启动）。
//! - **SYSID 查重**：SDK 的函数描述符缓存按 SYSID 分区（sapnwrfc.h），SYSID
//!   不同的系统天然隔离；**SYSID 相同**的系统会静默共享错误描述符——首连后
//!   从连接属性登记 SYSID（本地调用无 RFC 往返），冲突的系统被隔离
//!   （后续请求 503 `SYSTEM_SYSID_CONFLICT`）。
//! - **路由作用域**（[`Scope`]/[`Sap`]）：`/api/s/{system}/...` 前缀由
//!   server.rs 的改写服务解析并把 [`Scope`] 塞进 request extensions；
//!   [`Sap`] 提取器优先读作用域，无前缀请求落到默认系统。

use crate::config::AppConfig;
use crate::error::RfcError;
use crate::pool::RfcConnectionPool;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// ADT 默认端口（按 ashost 推导 base_url 时用）。
const ADT_DEFAULT_PORT: u16 = 50000;

// ========================================================================
// 配置态

/// 单系统的 resolved 配置（快捷字段 + conn 透传已合并为 SDK 连接参数）。
#[derive(Debug, Clone)]
pub struct SystemSpec {
    /// 系统名：URL 段（`/api/s/{name}/...`），小写 URL-safe。
    pub name: String,
    /// 是否默认系统（无前缀路径的路由目标）。恰一个。
    pub is_default: bool,
    /// `RfcOpenConnection` 键值对（键为 'static 字面量）。
    pub conn_params: Vec<(&'static str, String)>,
    /// 池上限覆盖（None = 用全局 `SAP_POOL_SIZE`）。
    pub pool_size: Option<usize>,
    /// 空闲校验阈值覆盖（None = 用全局）。
    pub idle_validate: Option<Duration>,
    /// 单请求超时覆盖（None = 用全局 `SAP_REQUEST_TIMEOUT_SECS`）。
    pub request_timeout: Option<Duration>,
    /// ADT 基地址：`None` = 按该系统 ashost 推导；`Some("")` = 显式禁用。
    pub adt_base_url: Option<String>,
    /// ADT 认证用户覆盖（None = 同该系统连接 USER）。
    pub adt_user: Option<String>,
    /// ADT 认证密码覆盖（None = 同该系统连接 PASSWD）。
    pub adt_passwd: Option<String>,
    /// 只读覆盖（None = 用全局 `SAP_READ_ONLY`）。
    pub read_only: Option<bool>,
}

/// `sap_systems.toml` 原始形态（serde 反序列化用；setup API 的请求体
/// 转成同构再走 [`build_spec`]，保证两条路径校验完全一致）。
#[derive(Debug, Deserialize)]
pub(crate) struct SystemsFile {
    pub(crate) systems: BTreeMap<String, RawSystem>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawSystem {
    pub(crate) ashost: Option<String>,
    pub(crate) sysnr: Option<String>,
    pub(crate) client: Option<String>,
    pub(crate) user: Option<String>,
    pub(crate) passwd: Option<String>,
    pub(crate) lang: Option<String>,
    #[serde(default)]
    pub(crate) default: bool,
    pub(crate) pool_size: Option<usize>,
    pub(crate) idle_validate_secs: Option<u64>,
    pub(crate) request_timeout_secs: Option<u64>,
    pub(crate) adt_base_url: Option<String>,
    pub(crate) adt_user: Option<String>,
    pub(crate) adt_passwd: Option<String>,
    pub(crate) read_only: Option<bool>,
    /// 自由键值透传（键大写化后直接进 `RfcOpenConnection`，
    /// 为 MSHOST/GROUP 负载均衡等形态留后门）。
    #[serde(default)]
    pub(crate) conn: BTreeMap<String, String>,
}

/// `${VAR}` 环境变量插值。未设置的变量报错（密码缺失应在启动期炸掉）。
fn interpolate(value: &str, ctx: &str) -> Result<String, String> {
    if !value.contains("${") {
        return Ok(value.to_string());
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("{} 插值语法错误（缺少 '}}'）: {}", ctx, value))?;
        let var = &after[..end];
        if var.is_empty() {
            return Err(format!("{} 插值变量名为空: {}", ctx, value));
        }
        let val = std::env::var(var)
            .map_err(|_| format!("{} 引用的环境变量 {} 未设置（{}）", ctx, var, value))?;
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// 系统名合法性：小写字母/数字/`-`/`_`，长度 1..=32（URL 段安全）。
fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "系统名 '{}' 非法：须为 1-32 位小写字母/数字/-/_（它是 URL 段 /api/s/{{name}}/...）",
            name
        ))
    }
}

/// 快捷字段 → SDK 连接参数键。conn 透传表里的同名键（大小写不敏感）视为冲突。
const SHORTCUT_KEYS: [(&str, &str); 6] = [
    ("ashost", "ASHOST"),
    ("sysnr", "SYSNR"),
    ("client", "CLIENT"),
    ("user", "USER"),
    ("passwd", "PASSWD"),
    ("lang", "LANG"),
];

/// 把原始系统定义组装成 [`SystemSpec`]：合并快捷字段与 conn 透传、插值、校验。
/// （启动期解析与 setup API 写入共用——两路径校验完全一致）
pub(crate) fn build_spec(name: &str, raw: &RawSystem) -> Result<SystemSpec, String> {
    validate_name(name)?;
    let ctx = format!("systems.{}", name);
    let mut params: Vec<(&'static str, String)> = Vec::new();
    let mut push = |key: &'static str, raw_val: Option<&String>| -> Result<(), String> {
        if let Some(v) = raw_val {
            let v = interpolate(v, &ctx)?;
            if v.is_empty() {
                return Err(format!("{} 的 {} 为空串", ctx, key));
            }
            params.push((key, v));
        }
        Ok(())
    };
    push("ASHOST", raw.ashost.as_ref())?;
    push("SYSNR", raw.sysnr.as_ref())?;
    push("CLIENT", raw.client.as_ref())?;
    push("USER", raw.user.as_ref())?;
    push("PASSWD", raw.passwd.as_ref())?;
    push("LANG", raw.lang.as_ref())?;

    // conn 透传：键大写化，与快捷字段冲突即报错（哪个生效不能有歧义）
    for (k, v) in &raw.conn {
        let upper = k.trim().to_uppercase();
        if upper.is_empty() {
            return Err(format!("{} 的 conn 含空键", ctx));
        }
        if SHORTCUT_KEYS.iter().any(|(_, sdk)| *sdk == upper) {
            return Err(format!(
                "{} 的 conn.{} 与快捷字段同名，请直接用快捷字段",
                ctx, k
            ));
        }
        if params.iter().any(|(k2, _)| *k2 == upper) {
            return Err(format!("{} 的 conn 重复键 {}", ctx, upper));
        }
        let v = interpolate(v, &ctx)?;
        // 泄漏安全：upper 是运行期 String，需要 'static —— 用 Box::leak
        // 固化（系统数量有限、进程生命周期，泄漏量有界）
        let key: &'static str = Box::leak(upper.into_boxed_str());
        params.push((key, v));
    }

    // 连接形态校验：直连（ASHOST+SYSNR）或负载均衡（MSHOST）；USER/PASSWD 必填
    let has = |k: &str| params.iter().any(|(k2, _)| *k2 == k);
    if !has("USER") || !has("PASSWD") {
        return Err(format!("{} 缺少 user/passwd（必填）", ctx));
    }
    if has("MSHOST") {
        Ok(())
    } else if has("ASHOST") {
        if has("SYSNR") {
            Ok(())
        } else {
            Err(format!("{} 指定了 ashost 但缺少 sysnr", ctx))
        }
    } else {
        Err(format!(
            "{} 缺少连接目标：需要 ashost+sysnr（直连）或 conn 里给 MSHOST（负载均衡）",
            ctx
        ))
    }?;

    Ok(SystemSpec {
        name: name.to_string(),
        is_default: raw.default,
        conn_params: params,
        pool_size: raw.pool_size.filter(|&n| n >= 1),
        idle_validate: raw.idle_validate_secs.map(|n| {
            if n == 0 {
                Duration::MAX
            } else {
                Duration::from_secs(n)
            }
        }),
        request_timeout: raw
            .request_timeout_secs
            .filter(|&n| n >= 1)
            .map(Duration::from_secs),
        adt_base_url: raw.adt_base_url.as_deref().map(str::to_string),
        adt_user: raw.adt_user.clone(),
        adt_passwd: raw.adt_passwd.clone(),
        read_only: raw.read_only,
    })
}

/// 从 `.env` 的 `SAP_*` 环境变量合成单系统 `default`（存量部署路径）。
/// `AppConfig.conn_params` 已是解析好的 SDK 键值对，直接复用。
fn synth_from_env(cfg: &AppConfig) -> SystemSpec {
    SystemSpec {
        name: "default".into(),
        is_default: true,
        conn_params: cfg.conn_params.clone(),
        pool_size: None,
        idle_validate: None,
        request_timeout: None,
        // 环境变量语义已在 config.rs 解析完毕：Some(url)=启用，None=显式禁用。
        // spec 语义里 None=按 ashost 推导，故禁用映射为 Some("") 保留意图。
        adt_base_url: cfg.adt_base_url.clone().or_else(|| Some(String::new())),
        adt_user: Some(cfg.adt_user.clone()),
        adt_passwd: Some(cfg.adt_passwd.clone()),
        read_only: None,
    }
}

/// 环境变量合成的存量系统（仅"无 toml 且有连接环境变量"的运行态存在）。
/// setup API 首写 toml 时把它一并落盘——toml 出现后 env 连接变量被忽略，
/// 不落盘的话重启后该系统会静默消失。
pub(crate) fn synth_env_spec(cfg: &AppConfig) -> Option<SystemSpec> {
    if !cfg.systems_file.exists() && !cfg.conn_params.is_empty() {
        Some(synth_from_env(cfg))
    } else {
        None
    }
}

/// 加载系统集合：`sap_systems.toml` 存在则解析之（.env 的连接变量被忽略，
/// 打提示日志）；不存在则从环境变量合成单系统；环境变量也全缺 → 空集合
/// （未配置模式：网关照常监听，系统由 setup API 引导写入后热加载）。
pub fn load_specs(cfg: &AppConfig) -> Result<Vec<SystemSpec>, String> {
    let path = cfg.systems_file.clone();
    if !path.exists() {
        if cfg.conn_params.is_empty() {
            tracing::warn!(
                file = %path.display(),
                "未检测到任何 SAP 连接配置（无 sap_systems.toml，SAP_* 环境变量全缺）——\
                 以未配置模式启动：SAP 业务端点返回 503 SYSTEM_NOT_CONFIGURED，\
                 引导式配置见 GET /api/setup"
            );
            return Ok(vec![]);
        }
        return Ok(vec![synth_from_env(cfg)]);
    }

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取 {} 失败: {}", path.display(), e))?;
    let file: SystemsFile =
        toml::from_str(&content).map_err(|e| format!("解析 {} 失败: {}", path.display(), e))?;
    if file.systems.is_empty() {
        return Err(format!(
            "{} 未定义任何系统（[systems.*] 为空）",
            path.display()
        ));
    }

    let mut specs = Vec::with_capacity(file.systems.len());
    for (name, raw) in &file.systems {
        specs.push(build_spec(name, raw)?);
    }

    // 默认系统唯一性：显式标记恰一个；未标记时单系统隐式默认、多系统报错
    let marked: Vec<&SystemSpec> = specs.iter().filter(|s| s.is_default).collect();
    match marked.len() {
        0 if specs.len() == 1 => specs[0].is_default = true,
        0 => {
            return Err(format!(
                "{} 定义了 {} 个系统但未标记 default = true（无前缀路径需要唯一默认系统）：{}",
                path.display(),
                specs.len(),
                specs
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
        1 => {}
        n => {
            return Err(format!(
                "{} 有 {} 个系统标记了 default = true（只能一个）",
                path.display(),
                n
            ))
        }
    }

    // toml 存在时 .env 的连接变量不再生效——给存量用户一条明确提示
    if std::env::var("SAP_ASHOST").is_ok() {
        tracing::info!(
            file = %path.display(),
            "检测到 sap_systems.toml 与 SAP_ASHOST 环境变量并存：系统集合以 toml 为准，.env 的 SAP_ASHOST/SYSNR/CLIENT/USER/PASSWD/LANG 被忽略"
        );
    }
    Ok(specs)
}

// ========================================================================
// 运行态

/// 进程级 SYSID 登记表：SYSID -> (后端标识, 首个登记的系统名)。
/// SDK 描述符缓存按 SYSID 分区且进程级，**不同后端**共用一个 SYSID
/// （克隆的开发容器）会静默共享错误描述符——这是 SDK 层的硬约束，网关侧
/// 必须挡住。**同一后端**（同一物理系统的不同 client）则相反：SAP 仓库
/// 对象跨 client，两个 client 里同名函数签名必然一致，共享描述符缓存恰好
/// 正确——放行。
static REGISTERED_SYSIDS: Mutex<Option<HashMap<String, (String, String)>>> = Mutex::new(None);

/// 连接目标（"后端"）标识，用于 SYSID 查重的同后端豁免：
/// 直连形态 = `ASHOST + SYSNR`；负载均衡形态 = `MSHOST + R3NAME`；
/// 其余形态保守退化用全部连接参数（剔除账号/密码/语言/**client**）。
/// 主机名大小写不敏感、去首尾空白；同一物理系统的别名/IP 形态会被视为
/// 不同后端（保守隔离，宁可误拦不放过——克隆系统签名漂移是静默错数据）。
fn backend_key(params: &[(&'static str, String)]) -> String {
    let get = |k: &str| {
        params
            .iter()
            .find(|(pk, _)| *pk == k)
            .map(|(_, v)| v.trim().to_lowercase())
    };
    if let (Some(host), Some(sysnr)) = (get("ASHOST"), get("SYSNR")) {
        format!("direct://{}/{}", host, sysnr)
    } else if let Some(mshost) = get("MSHOST") {
        format!("lb://{}/{}", mshost, get("R3NAME").unwrap_or_default())
    } else {
        let mut parts: Vec<String> = params
            .iter()
            .filter(|(k, _)| !matches!(*k, "USER" | "PASSWD" | "LANG" | "CLIENT"))
            .map(|(k, v)| format!("{}={}", k, v.trim().to_lowercase()))
            .collect();
        parts.sort();
        parts.join("&")
    }
}

/// 单系统运行句柄：连接池 + ADT 端点 + 元数据缓存 + 状态标记。
pub struct SystemHandle {
    /// 系统名（= URL 段）
    pub name: String,
    /// 该系统的连接池（默认系统急切建首连；其余惰性）
    pub pool: Arc<RfcConnectionPool>,
    /// 该系统的 ADT 端点（None = 未启用）
    pub adt: Option<crate::adt::AdtEndpoint>,
    /// 该系统的函数/DDIC 元数据缓存（按系统实例化，防跨系统同名污染）
    pub meta: crate::metadata::MetadataCache,
    /// 该系统只读（写端点 403）
    pub read_only: bool,
    /// 该系统单请求超时
    pub request_timeout: Duration,
    /// 连接目标标识（SYSID 查重的同后端豁免用，见 [`backend_key`]）
    backend: String,
    /// 首连成功后登记的 SYSID（连接属性读取，本地无 RFC 往返）
    sysid: OnceLock<String>,
    /// SYSID 与其他**后端**冲突 → 隔离（请求 503 SYSTEM_SYSID_CONFLICT）
    quarantined: AtomicBool,
    /// `/api/version` 的懒加载系统信息缓存（生命周期内不变，失败不缓存）
    pub sap_info: tokio::sync::RwLock<Option<Arc<crate::connection::RemoteSystemInfo>>>,
}

impl SystemHandle {
    /// 已登记的 SYSID（未知时 None——系统尚未成功连过）。
    pub fn sysid(&self) -> Option<String> {
        self.sysid.get().cloned()
    }

    /// 是否因 SYSID 冲突被隔离。
    pub fn is_quarantined(&self) -> bool {
        self.quarantined.load(Ordering::SeqCst)
    }

    /// 隔离检查：被隔离的系统拒绝服务（给出明确出路而非静默错数据）。
    pub fn check_available(&self) -> Result<(), RfcError> {
        if self.is_quarantined() {
            Err(RfcError {
                code: -1,
                status: 503,
                message: format!(
                    "系统 {} 已被隔离：其 SYSID 与另一**不同后端**的系统冲突（SDK 描述符缓存按 \
                     SYSID 分区，跨后端同 SYSID 会静默共享错误签名；同系统的其他 client 不受影响）。\
                     请修改其中一个系统的 SID 或从 sap_systems.toml 移除其一后重启网关",
                    self.name
                ),
                key: "SYSTEM_SYSID_CONFLICT".into(),
            })
        } else {
            Ok(())
        }
    }

    /// 登记该系统的 SYSID。与**不同后端**的同 SYSID 系统冲突（克隆系统，
    /// 签名可能漂移——共享 SDK 描述符缓存是静默错数据）时隔离自己并返回
    /// 冲突方名字；与**同一后端**的同 SYSID 条目（同一物理系统的不同
    /// client，仓库对象跨 client、签名必然一致）放行共享。
    /// 幂等：已登记时同值直接通过（连接重建后属性不变）。
    pub fn record_sysid(&self, sysid: &str) -> Result<(), String> {
        if let Some(own) = self.sysid.get() {
            return if own == sysid {
                Ok(())
            } else {
                Err(format!(
                    "系统 {} 的 SYSID 从 {} 变为 {}（同一系统不应变化）",
                    self.name, own, sysid
                ))
            };
        }
        let mut guard = REGISTERED_SYSIDS.lock().unwrap_or_else(|e| e.into_inner());
        let map = guard.get_or_insert_with(HashMap::new);
        if let Some((backend, owner)) = map.get(sysid) {
            if backend != &self.backend {
                // 不同后端同 SYSID：真冲突（SDK 缓存按 SYSID 分区，签名
                // 可能不同——静默共享比报错危险得多）
                self.quarantined.store(true, Ordering::SeqCst);
                return Err(owner.clone());
            }
            // 同后端同 SYSID：同一物理系统的不同 client，签名一致，
            // 共享描述符缓存语义正确——放行（登记表保留首个登记方即可）
            tracing::debug!(
                system = %self.name,
                sysid = %sysid,
                first_owner = %owner,
                "同后端多 client 共享 SYSID（同一物理系统，仓库对象跨 client）"
            );
        } else {
            map.insert(sysid.to_string(), (self.backend.clone(), self.name.clone()));
        }
        let _ = self.sysid.set(sysid.to_string());
        Ok(())
    }
}

/// 多系统注册表：axum 共享状态（`SharedState = Arc<SystemRegistry>`）。
///
/// 内部可变（RwLock）：setup API 可在运行期热加载新系统（`add_system`），
/// 无需重启网关；读路径（`resolve`/`default`/`list`）全部短临界区取 Arc，
/// 锁内不 await。**可为空**：未配置模式（无 toml、无环境变量）下
/// `default` 为 None，`Sap` 提取器对无前缀请求返回 503 SYSTEM_NOT_CONFIGURED。
pub struct SystemRegistry {
    inner: std::sync::RwLock<RegistryInner>,
    /// 全局默认值（池/超时/只读）：setup 热加载新系统时组装 handle 用
    cfg: AppConfig,
}

struct RegistryInner {
    systems: HashMap<String, Arc<SystemHandle>>,
    /// 声明序（`/api/systems` 稳定输出）
    order: Vec<String>,
    /// 默认系统（无前缀路径的目标）。未配置模式为 None。
    default: Option<Arc<SystemHandle>>,
}

impl SystemRegistry {
    /// 按名解析系统（`/api/s/{name}` 的 URL 段）。
    pub fn resolve(&self, name: &str) -> Option<Arc<SystemHandle>> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .systems
            .get(name)
            .cloned()
    }

    /// 默认系统（无前缀路径的目标）。未配置模式返回 None。
    pub fn default(&self) -> Option<Arc<SystemHandle>> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .default
            .clone()
    }

    /// 全部系统（声明序 + setup 热加载序）。
    pub fn list(&self) -> Vec<Arc<SystemHandle>> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner
            .order
            .iter()
            .filter_map(|n| inner.systems.get(n).cloned())
            .collect()
    }

    /// 全部系统名（错误信息用）。
    pub fn names(&self) -> Vec<String> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .order
            .clone()
    }

    /// 全局配置（setup 热加载组装 handle 时取默认值用）。
    pub fn cfg(&self) -> &AppConfig {
        &self.cfg
    }

    /// 热加载一个系统（setup API）：插入注册表并按需提升为默认。
    /// `make_default` 且已有默认 → 取代旧默认；当前无默认 → 无条件成为默认
    /// （无前缀路径必须有目标）。同名系统已存在是调用方（setup）的前置校验。
    pub fn add_system(&self, handle: Arc<SystemHandle>, make_default: bool) {
        let name = handle.name.clone();
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let became_default = make_default || inner.default.is_none();
        if became_default {
            inner.default = Some(Arc::clone(&handle));
        }
        inner.order.push(name.clone());
        inner.systems.insert(name.clone(), handle);
        tracing::info!(
            system = %name,
            default = became_default,
            "系统已热加载（setup API，无需重启）"
        );
    }
}

/// 从连接参数表取值。
fn conn_param(params: &[(&'static str, String)], key: &str) -> Option<String> {
    params
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.clone())
}

/// 组装该系统的 ADT 端点：
/// 显式空串 = 禁用；未配置 = 按该系统 ashost 推导（MSHOST 形态无法推导 → 禁用）；
/// 认证默认同该系统连接的 USER/PASSWD。
fn build_adt(spec: &SystemSpec, timeout: Duration) -> Option<crate::adt::AdtEndpoint> {
    let base = match &spec.adt_base_url {
        Some(s) if s.trim().is_empty() => return None,
        Some(s) => s.clone(),
        None => {
            let ashost = conn_param(&spec.conn_params, "ASHOST")?;
            format!("http://{}:{}", ashost, ADT_DEFAULT_PORT)
        }
    };
    let user = spec
        .adt_user
        .clone()
        .or_else(|| conn_param(&spec.conn_params, "USER"))
        .unwrap_or_default();
    let passwd = spec
        .adt_passwd
        .clone()
        .or_else(|| conn_param(&spec.conn_params, "PASSWD"))
        .unwrap_or_default();
    let ep = crate::adt::AdtEndpoint::new(&base, &user, &passwd, timeout);
    tracing::info!(system = %spec.name, base = %ep.base_url(), "ADT 代理已启用（/api/adt/**）");
    Some(ep)
}

/// 组装单个系统运行句柄（启动装配与 setup 热加载共用）。
/// `eager` = 立即建首连（仅启动期默认系统用，密码错启动即炸的 fail-fast）；
/// 惰性池不触 FFI，不可达交给调用期重试。
pub(crate) fn make_handle(
    spec: &SystemSpec,
    cfg: &AppConfig,
    eager: bool,
) -> Result<Arc<SystemHandle>, String> {
    let pool_size = spec.pool_size.unwrap_or(cfg.pool_size);
    let idle_validate = spec.idle_validate.unwrap_or(cfg.pool_idle_validate);
    let request_timeout = spec.request_timeout.unwrap_or(cfg.request_timeout);
    let read_only = spec.read_only.unwrap_or(cfg.read_only);

    let pool = if eager {
        Arc::new(
            RfcConnectionPool::with_max_size(spec.conn_params.clone(), pool_size, idle_validate)
                .map_err(|e| {
                    format!(
                        "默认系统 {} 建立连接失败: {} ({})",
                        spec.name, e.message, e.key
                    )
                })?,
        )
    } else {
        Arc::new(RfcConnectionPool::lazy(
            spec.conn_params.clone(),
            pool_size,
            idle_validate,
        ))
    };
    let adt = build_adt(spec, request_timeout);

    Ok(Arc::new(SystemHandle {
        name: spec.name.clone(),
        pool,
        adt,
        meta: crate::metadata::MetadataCache::new(),
        read_only,
        request_timeout,
        backend: backend_key(&spec.conn_params),
        sysid: OnceLock::new(),
        quarantined: AtomicBool::new(false),
        sap_info: tokio::sync::RwLock::new(None),
    }))
}

/// 组装运行态注册表。默认系统急切建首连（失败即返回错误——保留单系统部署
/// "密码错启动即炸"的 fail-fast）；其余系统惰性，可达性交给探测任务。
/// 空集合合法（未配置模式，default = None）。
pub fn build_registry(specs: Vec<SystemSpec>, cfg: &AppConfig) -> Result<SystemRegistry, String> {
    build_registry_inner(specs, cfg, true)
}

/// 全惰性组装（不触 FFI）：路由级单测在无 SAP SDK 环境下构造 app() 用。
/// 生产路径走 [`build_registry`]（默认系统急切）。
// 非 test 构建无调用方（测试代码在 cfg(test) 里，非测试编译看不到）
#[cfg_attr(not(test), allow(dead_code))]
pub fn build_registry_all_lazy(
    specs: Vec<SystemSpec>,
    cfg: &AppConfig,
) -> Result<SystemRegistry, String> {
    build_registry_inner(specs, cfg, false)
}

fn build_registry_inner(
    specs: Vec<SystemSpec>,
    cfg: &AppConfig,
    eager_default: bool,
) -> Result<SystemRegistry, String> {
    let mut systems = HashMap::with_capacity(specs.len());
    let mut order = Vec::with_capacity(specs.len());
    let mut default = None;

    for spec in specs {
        let handle = make_handle(&spec, cfg, spec.is_default && eager_default)?;
        tracing::info!(
            system = %spec.name,
            default = spec.is_default,
            read_only = spec.read_only.unwrap_or(cfg.read_only),
            "系统已配置"
        );
        if spec.is_default {
            default = Some(Arc::clone(&handle));
        }
        order.push(spec.name.clone());
        systems.insert(spec.name, handle);
    }

    // load_specs 校验保证非空集合恰有一个默认系统；空集合 = 未配置模式
    debug_assert!(default.is_some() || order.is_empty());

    Ok(SystemRegistry {
        inner: std::sync::RwLock::new(RegistryInner {
            systems,
            order,
            default,
        }),
        cfg: cfg.clone(),
    })
}

/// 启动期探测任务：逐系统（并行）建连取 SYSID 并登记。
/// 默认系统急切池里已有连接，这一步同时是「其余系统可达性」的首次体检；
/// 只 warn 不阻塞——探测失败留给调用期错误上报。
pub async fn probe_all(registry: Arc<SystemRegistry>) {
    let probes: Vec<_> = registry
        .list()
        .into_iter()
        .map(|sys| {
            tokio::spawn(async move {
                let s = sys.clone();
                let info = tokio::time::timeout(
                    Duration::from_secs(5),
                    tokio::task::spawn_blocking(move || {
                        s.pool.with_connection(|conn| conn.system_info())
                    }),
                )
                .await;
                match info {
                    Ok(Ok(Ok(info))) => {
                        if let Err(owner) = sys.record_sysid(&info.sysid) {
                            tracing::error!(
                                system = %sys.name,
                                conflict_with = %owner,
                                sysid = %info.sysid,
                                "SYSID 冲突，系统已隔离（SDK 描述符缓存按 SYSID 分区，同 SYSID 会共享错误签名）"
                            );
                        } else {
                            tracing::info!(
                                system = %sys.name,
                                sysid = %info.sysid,
                                release = %info.release,
                                "系统探测成功"
                            );
                        }
                    }
                    Ok(Ok(Err(e))) => {
                        tracing::warn!(
                            system = %sys.name,
                            msg = %e.message,
                            "系统探测失败（不影响网关监听，调用期将重试建连）"
                        );
                    }
                    Ok(Err(join)) => tracing::warn!(system = %sys.name, %join, "探测任务 join 失败"),
                    Err(_) => {
                        tracing::warn!(system = %sys.name, "系统探测超时（5s，不影响网关监听）")
                    }
                }
            })
        })
        .collect();
    for p in probes {
        let _ = p.await;
    }
}

// ========================================================================
// 作用域提取器（路由层用）

/// request extension：`/api/s/{system}` 前缀解析结果（由 server.rs 的
/// ScopeService 塞入）。无此扩展的请求走默认系统。
#[derive(Clone)]
pub struct Scope(pub Arc<SystemHandle>);

/// handler 提取器：优先取作用域系统（`/api/s/{name}/...`），否则默认系统。
/// 未配置模式（注册表为空）下无前缀请求 → 503 `SYSTEM_NOT_CONFIGURED`
/// （指向 GET /api/setup 引导）；作用域请求在 ScopeService 已按名解析，
/// 未知系统在那里就是 404，不会走到本提取器的默认分支。
///
/// ```ignore
/// async fn handler(State(_): State<SharedState>, sap: Sap) { sap.pool... }
/// ```
pub struct Sap(pub Arc<SystemHandle>);

impl std::ops::Deref for Sap {
    type Target = SystemHandle;
    fn deref(&self) -> &SystemHandle {
        &self.0
    }
}

impl Sap {
    /// 克隆内部句柄（传给 run_blocking 系辅助函数用）。
    pub fn arc(&self) -> Arc<SystemHandle> {
        Arc::clone(&self.0)
    }
}

impl<S> axum::extract::FromRequestParts<S> for Sap
where
    S: Send + Sync,
    Arc<SystemRegistry>: axum::extract::FromRef<S>,
{
    type Rejection = RfcError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        if let Some(scope) = parts.extensions.get::<Scope>() {
            return Ok(Sap(scope.0.clone()));
        }
        let registry = <Arc<SystemRegistry> as axum::extract::FromRef<S>>::from_ref(state);
        registry.default().map(Sap).ok_or_else(|| RfcError {
            code: -1,
            status: 503,
            message: "网关尚未配置任何 SAP 系统（未配置模式）。\
                          引导式配置：GET /api/setup 查看所需字段，向用户采集后 \
                          POST /api/setup/system 写入并热加载；或手工创建 sap_systems.toml 后重启"
                .into(),
            key: "SYSTEM_NOT_CONFIGURED".into(),
        })
    }
}

// SAFETY: 与 RfcConnectionPool 同理——句柄内全是 Arc/Mutex/RwLock 组合，
// 连接本身的串行性由池的借还协议保证。
unsafe impl Send for SystemHandle {}
unsafe impl Sync for SystemHandle {}

// ========================================================================
// 测试（FFI-free：lazy 池不触 SDK）

#[cfg(test)]
mod tests {
    use super::*;

    /// 环境变量是进程全局态，插值测试需要互斥串行（同 config.rs 的模式）。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 构造最小 AppConfig（环境变量合成路径的测试底座）。
    fn test_cfg(conn_params: Vec<(&'static str, String)>) -> AppConfig {
        AppConfig {
            conn_params,
            listen_addr: "127.0.0.1:3000".into(),
            pool_size: 8,
            pool_idle_validate: Duration::from_secs(30),
            api_key: None,
            request_timeout: Duration::from_secs(60),
            rate_limit_rps: None,
            adt_base_url: Some("http://h:50000".into()),
            adt_user: "U".into(),
            adt_passwd: "P".into(),
            read_only: false,
            registry_file: "registry.json".into(),
            runtime_mode: false,
            update_check: false,
            systems_file: "no-such-file.toml".into(),
            setup_enabled: true,
            open_browser: false,
        }
    }

    fn direct_params() -> Vec<(&'static str, String)> {
        vec![
            ("ASHOST", "h".into()),
            ("SYSNR", "00".into()),
            ("CLIENT", "100".into()),
            ("USER", "U".into()),
            ("PASSWD", "P".into()),
            ("LANG", "EN".into()),
        ]
    }

    #[test]
    fn unconfigured_mode_builds_empty_registry_and_hot_adds() {
        // 未配置模式：无 toml + 无环境变量 → 空注册表；首个热加载系统无条件成为默认
        let cfg = test_cfg(vec![]);
        let specs = load_specs(&cfg).unwrap();
        assert!(specs.is_empty(), "未配置模式应得到空系统集合");
        let reg = build_registry_all_lazy(specs, &cfg).unwrap();
        assert!(reg.default().is_none(), "空注册表无默认系统");
        assert!(reg.names().is_empty());

        let toml = "[systems]\n[systems.a4h]\nashost=\"h\"\nsysnr=\"00\"\nuser=\"u\"\npasswd=\"p\"";
        let file: SystemsFile = toml::from_str(toml).unwrap();
        let (name, raw) = file.systems.iter().next().unwrap();
        let spec = build_spec(name, raw).unwrap();
        let handle = make_handle(&spec, &cfg, false).unwrap();
        reg.add_system(handle, false); // 即便 make_default=false，无默认时也提升
        assert_eq!(reg.default().unwrap().name, "a4h");
        assert!(reg.resolve("a4h").is_some());
        assert_eq!(reg.names(), vec!["a4h".to_string()]);

        // 第二个系统 make_default=true → 取代默认
        let toml2 =
            "[systems]\n[systems.prd]\nashost=\"h2\"\nsysnr=\"01\"\nuser=\"u\"\npasswd=\"p\"";
        let file2: SystemsFile = toml::from_str(toml2).unwrap();
        let (name2, raw2) = file2.systems.iter().next().unwrap();
        let spec2 = build_spec(name2, raw2).unwrap();
        reg.add_system(make_handle(&spec2, &cfg, false).unwrap(), true);
        assert_eq!(reg.default().unwrap().name, "prd");
        assert_eq!(reg.names().len(), 2);
    }

    #[test]
    fn synth_from_env_yields_default_system() {
        let cfg = test_cfg(direct_params());
        let specs = load_specs(&cfg).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "default");
        assert!(specs[0].is_default);
        assert_eq!(specs[0].conn_params, cfg.conn_params);
    }

    #[test]
    fn synth_preserves_adt_disabled() {
        let mut cfg = test_cfg(direct_params());
        cfg.adt_base_url = None; // SAP_ADT_BASE_URL="" 的语义
        let spec = synth_from_env(&cfg);
        // None 在 spec 语义里是"推导"，禁用必须显式 Some("")
        assert_eq!(spec.adt_base_url.as_deref(), Some(""));
    }

    #[test]
    fn parse_single_system_implicit_default() {
        let toml = r#"
[systems.a4h]
ashost = "vhcala4h"
sysnr = "00"
client = "001"
user = "dev"
passwd = "pw"
"#;
        let file: SystemsFile = toml::from_str(toml).unwrap();
        let mut specs: Vec<_> = file
            .systems
            .iter()
            .map(|(n, r)| build_spec(n, r).unwrap())
            .collect();
        assert_eq!(specs.len(), 1);
        assert!(!specs[0].is_default);
        // 单系统未标记 → 隐式默认（load_specs 的规则，这里模拟同一分支）
        specs[0].is_default = true;
        assert_eq!(specs[0].conn_params[0], ("ASHOST", "vhcala4h".to_string()));
        assert_eq!(specs[0].conn_params[3], ("USER", "dev".to_string()));
    }

    #[test]
    fn parse_multi_system_with_overrides() {
        let toml = r#"
[systems.a4h]
ashost = "h1"
sysnr = "00"
client = "001"
user = "u1"
passwd = "p1"
default = true

[systems.prd]
ashost = "h2"
sysnr = "10"
client = "100"
user = "u2"
passwd = "p2"
pool_size = 4
read_only = true
adt_base_url = ""
"#;
        let file: SystemsFile = toml::from_str(toml).unwrap();
        let specs: Vec<_> = file
            .systems
            .iter()
            .map(|(n, r)| build_spec(n, r).unwrap())
            .collect();
        let prd = specs.iter().find(|s| s.name == "prd").unwrap();
        assert_eq!(prd.pool_size, Some(4));
        assert_eq!(prd.read_only, Some(true));
        assert_eq!(prd.adt_base_url.as_deref(), Some(""));
        assert!(!prd.is_default);
        assert!(specs.iter().find(|s| s.name == "a4h").unwrap().is_default);
    }

    #[test]
    fn conn_passthrough_and_conflicts() {
        let toml = r#"
[systems.lb]
user = "u"
passwd = "p"
[systems.lb.conn]
MSHOST = "lb.example.com"
GROUP = "SPACE"
"#;
        let file: SystemsFile = toml::from_str(toml).unwrap();
        let (name, raw) = file.systems.iter().next().unwrap();
        let spec = build_spec(name, raw).unwrap();
        assert!(spec
            .conn_params
            .iter()
            .any(|(k, v)| *k == "MSHOST" && v == "lb.example.com"));

        // 与快捷字段同名 → 报错
        let bad = r#"
[systems.x]
user = "u"
passwd = "p"
[systems.x.conn]
ashost = "h"
"#;
        let file: SystemsFile = toml::from_str(bad).unwrap();
        let (name, raw) = file.systems.iter().next().unwrap();
        assert!(build_spec(name, raw).is_err());
    }

    #[test]
    fn reject_incomplete_or_bad_names() {
        let mk = |toml: &str| {
            let file: SystemsFile = toml::from_str(toml).unwrap();
            let (name, raw) = file.systems.iter().next().unwrap();
            build_spec(name, raw)
        };
        // 名字非法（大写）
        assert!(mk("[systems.Bad]\nuser=\"u\"\npasswd=\"p\"\nashost=\"h\"\nsysnr=\"00\"").is_err());
        // 缺 USER
        assert!(mk("[systems.ok]\npasswd=\"p\"\nashost=\"h\"\nsysnr=\"00\"").is_err());
        // ASHOST 无 SYSNR
        assert!(mk("[systems.ok]\nuser=\"u\"\npasswd=\"p\"\nashost=\"h\"").is_err());
        // 无连接目标
        assert!(mk("[systems.ok]\nuser=\"u\"\npasswd=\"p\"").is_err());
        // MSHOST 形态合法（无 ashost/sysnr）
        assert!(
            mk("[systems.ok]\nuser=\"u\"\npasswd=\"p\"\n[systems.ok.conn]\nMSHOST=\"m\"").is_ok()
        );
    }

    #[test]
    fn interpolation_expands_and_errors() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("SAP_TEST_PW", "s3cret");
        }
        assert_eq!(
            interpolate("pass${SAP_TEST_PW}!", "ctx").unwrap(),
            "passs3cret!"
        );
        unsafe {
            std::env::remove_var("SAP_TEST_MISSING_VAR_XYZ");
        }
        assert!(interpolate("${SAP_TEST_MISSING_VAR_XYZ}", "ctx").is_err());
        assert!(interpolate("${UNCLOSED", "ctx").is_err());
        // 无插值的值原样返回
        assert_eq!(interpolate("plain", "ctx").unwrap(), "plain");
    }

    /// 构造测试句柄（lazy 池不触 FFI）。host 决定后端标识。
    fn mk_handle(name: &str, host: &str) -> SystemHandle {
        let params = vec![
            ("ASHOST", host.to_string()),
            ("SYSNR", "00".to_string()),
            ("CLIENT", "001".to_string()),
            ("USER", "u".to_string()),
            ("PASSWD", "p".to_string()),
        ];
        SystemHandle {
            name: name.into(),
            pool: Arc::new(RfcConnectionPool::lazy(
                params.clone(),
                2,
                Duration::from_secs(30),
            )),
            adt: None,
            meta: crate::metadata::MetadataCache::new(),
            read_only: false,
            request_timeout: Duration::from_secs(5),
            backend: backend_key(&params),
            sysid: OnceLock::new(),
            quarantined: AtomicBool::new(false),
            sap_info: tokio::sync::RwLock::new(None),
        }
    }

    #[test]
    fn record_sysid_same_backend_different_clients_allowed() {
        // 同一物理系统的不同 client 条目（同 ASHOST/SYSNR，CLIENT 不同）读出
        // 同一个 SYSID——仓库对象跨 client、签名必然一致，共享 SDK 描述符
        // 缓存恰好正确，必须放行（登记表是进程级全局，各测试用独立 SYSID
        // 值避免串扰）
        let a = mk_handle("dev001", "vhcala4h");
        let b = mk_handle("dev200", "vhcala4h");
        assert!(a.record_sysid("TS1").is_ok());
        // 同后端同 SYSID → 豁免，不隔离
        assert!(b.record_sysid("TS1").is_ok());
        assert!(!b.is_quarantined());
        assert!(b.check_available().is_ok());
        // 幂等：同句柄重复登记同值直接通过
        assert!(a.record_sysid("TS1").is_ok());
        assert!(b.record_sysid("TS1").is_ok());
    }

    #[test]
    fn record_sysid_cross_backend_conflict_quarantines() {
        // 不同后端共用 SYSID（克隆的开发容器）→ 真冲突：隔离并给出冲突方
        let a = mk_handle("a", "host-a");
        let b = mk_handle("b", "host-b");
        assert!(a.record_sysid("TS2").is_ok());
        let err = b.record_sysid("TS2").unwrap_err();
        assert_eq!(err, "a");
        assert!(b.is_quarantined());
        assert!(b.check_available().is_err());
        assert!(a.check_available().is_ok());
        // 不同 SYSID 正常登记
        let c = mk_handle("c", "host-c");
        assert!(c.record_sysid("TS3").is_ok());
        assert!(!c.is_quarantined());
    }

    #[test]
    fn backend_key_matches_and_differs_correctly() {
        let mk_params = |host: &str, sysnr: &str, client: &str| {
            vec![
                ("ASHOST", host.to_string()),
                ("SYSNR", sysnr.to_string()),
                ("CLIENT", client.to_string()),
                ("USER", "u".to_string()),
                ("PASSWD", "p".to_string()),
            ]
        };
        // 同系统不同 client：同键
        assert_eq!(
            backend_key(&mk_params("vhcala4h", "00", "001")),
            backend_key(&mk_params("vhcala4h", "00", "002"))
        );
        // 大小写/空白不影响
        assert_eq!(
            backend_key(&mk_params("  VHCALA4H ", "00", "001")),
            backend_key(&mk_params("vhcala4h", "00", "001"))
        );
        // 不同系统号/主机：不同键
        assert_ne!(
            backend_key(&mk_params("vhcala4h", "00", "001")),
            backend_key(&mk_params("vhcala4h", "01", "001"))
        );
        assert_ne!(
            backend_key(&mk_params("host-a", "00", "001")),
            backend_key(&mk_params("host-b", "00", "001"))
        );
        // 负载均衡形态：MSHOST+R3NAME
        let lb = vec![
            ("MSHOST", "msg.example.com".to_string()),
            ("R3NAME", "PRD".to_string()),
            ("USER", "u".to_string()),
            ("PASSWD", "p".to_string()),
        ];
        assert_eq!(backend_key(&lb), "lb://msg.example.com/prd");
    }

    #[test]
    fn adt_derivation_rules() {
        // 无 adt 配置 + 有 ashost → 按 ashost 推导（启用）
        let direct = SystemSpec {
            name: "a".into(),
            is_default: false,
            conn_params: direct_params(),
            pool_size: None,
            idle_validate: None,
            request_timeout: None,
            adt_base_url: None,
            adt_user: None,
            adt_passwd: None,
            read_only: None,
        };
        assert!(build_adt(&direct, Duration::from_secs(5)).is_some());
        // 显式空串 → 禁用
        let mut disabled = direct.clone();
        disabled.adt_base_url = Some(String::new());
        assert!(build_adt(&disabled, Duration::from_secs(5)).is_none());
        // MSHOST 形态（无 ashost）且未配置 adt → 无法推导 → 禁用
        let lb = SystemSpec {
            name: "b".into(),
            is_default: false,
            conn_params: vec![
                ("MSHOST", "m".into()),
                ("USER", "u".into()),
                ("PASSWD", "p".into()),
            ],
            pool_size: None,
            idle_validate: None,
            request_timeout: None,
            adt_base_url: None,
            adt_user: None,
            adt_passwd: None,
            read_only: None,
        };
        assert!(build_adt(&lb, Duration::from_secs(5)).is_none());
    }
}
