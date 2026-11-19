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
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// ADT 默认端口（按 ashost 推导 base_url 时用）。
const ADT_DEFAULT_PORT: u16 = 50000;

// ========================================================================
// 配置态

// ========================================================================
// 鉴权配置（结构化变体；互斥性由结构保证，校验期只做「恰一」检查）
// ========================================================================

/// 主鉴权通道（三选一，[`AuthConfig::from_raw`] 强制恰一）。
#[derive(Debug, Clone)]
pub enum AuthPrimary {
    /// user + passwd（经典形态；passwd 支持 `${VAR}` 插值）
    Basic { user: String, passwd: String },
    /// SAP 登录票据（自带身份；`${VAR}` 插值）
    Sso2 { ticket: String },
    /// 外部鉴权插件（ADT 通道专用——RFC 侧无对应物）
    Plugin(crate::adt::AuthPluginSpec),
    /// OAuth2 密码授权（BTP/ABAP Cloud ADT 专用；RFC 侧不支持——系统级使用
    /// 会在 build_spec 被显式拒绝并指引到 `[systems.x.adt.auth.oauth]`）
    OAuth {
        token_url: String,
        client_id: String,
        client_secret: String,
    },
}

/// SNC 叠加层（Kerberos / Secure Login Client；可与任一主通道组合）。
#[derive(Debug, Clone, Default)]
pub struct SncConfig {
    pub qop: Option<String>,
    pub myname: Option<String>,
    pub partnername: Option<String>,
    pub lib: Option<String>,
}

impl SncConfig {
    /// 是否至少给了一个字段（空表 = 没配）。
    fn is_present(&self) -> bool {
        self.qop.is_some() || self.myname.is_some() || self.partnername.is_some() || self.lib.is_some()
    }
    /// 拼 SDK 键值对（给出哪个拼哪个）。
    fn to_conn_params(&self) -> Vec<(&'static str, String)> {
        let mut v = Vec::new();
        if let Some(x) = &self.qop { v.push(("SNC_QOP", x.clone())); }
        if let Some(x) = &self.myname { v.push(("SNC_MYNAME", x.clone())); }
        if let Some(x) = &self.partnername { v.push(("SNC_PARTNERNAME", x.clone())); }
        if let Some(x) = &self.lib { v.push(("SNC_LIB", x.clone())); }
        v
    }
}

/// 解析后的系统鉴权配置：主通道 + 可选 SNC 叠加。
#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub primary: AuthPrimary,
    pub snc: Option<SncConfig>,
}

impl AuthPrimary {
    /// Basic 形态的 (user, passwd)；其余形态 None。
    pub(crate) fn basic_ref(&self) -> Option<(&str, &str)> {
        match self {
            AuthPrimary::Basic { user, passwd } => Some((user, passwd)),
            _ => None,
        }
    }
}

impl AuthConfig {
    /// `/api/systems` 的 `auth` 字段值（basic|sso2|plugin，可 +snc 后缀）。
    pub fn mode(&self) -> &'static str {
        let base = match &self.primary {
            AuthPrimary::Basic { .. } => "basic",
            AuthPrimary::Sso2 { .. } => "sso2",
            AuthPrimary::Plugin(_) => "plugin",
            AuthPrimary::OAuth { .. } => "oauth",
        };
        if self.snc.is_some() {
            // 仅这些 &'static 组合存在，Box::leak 有界（枚举有限）
            match base {
                "basic" => "basic+snc",
                "sso2" => "sso2+snc",
                _ => "snc",
            }
        } else {
            base
        }
    }

    /// 主通道贡献的 SDK 连接键（SNC 叠加另有 [`SncConfig::to_conn_params`]）。
    fn primary_conn_params(&self) -> Vec<(&'static str, String)> {
        match &self.primary {
            AuthPrimary::Basic { user, passwd } => {
                vec![("USER", user.clone()), ("PASSWD", passwd.clone())]
            }
            AuthPrimary::Sso2 { ticket } => vec![("MYSAPSSO2", ticket.clone())],
            AuthPrimary::Plugin(_) | AuthPrimary::OAuth { .. } => vec![],
        }
    }

    /// 基本用户名（对象创建 XML 的 responsible / 临时类名用；无则 "AGENT"）。
    pub fn display_user(&self) -> String {
        self.primary
            .basic_ref()
            .map(|(u, _)| u.trim().to_uppercase())
            .unwrap_or_else(|| "AGENT".to_string())
    }

}

/// ADT 通道 TLS 素材（与鉴权正交）。
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// 客户端证书 PEM 路径（单独文件，或与 key 合并单文件）
    pub cert: String,
    /// 私钥 PEM 路径（None = cert 内含私钥）
    pub key: Option<String>,
    /// 自定义 CA bundle 路径
    pub ca: Option<String>,
}

/// ADT 通道配置（`[systems.x.adt]`）。缺省全部继承系统级：
/// base_url 按 ashost:50000 推导、auth 复用 `[systems.x.auth]`。
#[derive(Debug, Clone)]
pub struct AdtConfig {
    /// `None` = 按 ashost 推导；`Some("")` = 显式禁用；`Some(url)` = 显式基地址
    pub base_url: Option<String>,
    /// 鉴权覆盖（None = 继承系统级 auth）
    pub auth: Option<AuthConfig>,
    pub tls: Option<TlsConfig>,
}

/// 单系统的 resolved 配置（快捷字段 + conn 透传已合并为 SDK 连接参数）。
#[derive(Debug, Clone)]
pub struct SystemSpec {
    /// 系统名：URL 段（`/api/s/{name}/...`），小写 URL-safe。
    pub name: String,
    /// 是否默认系统（无前缀路径的路由目标）。恰一个。
    pub is_default: bool,
    /// `RfcOpenConnection` 键值对（键为 'static 字面量；含 auth 贡献的键）。
    pub conn_params: Vec<(&'static str, String)>,
    /// 解析后的系统鉴权（RFC 连接与 ADT 继承共用）。
    pub auth: AuthConfig,
    /// ADT 通道配置（None = 全缺省：URL 推导 + auth 继承）。
    pub adt: Option<AdtConfig>,
    /// 池上限覆盖（None = 用全局 `SAP_POOL_SIZE`）。
    pub pool_size: Option<usize>,
    /// 空闲校验阈值覆盖（None = 用全局）。
    pub idle_validate: Option<Duration>,
    /// 单请求超时覆盖（None = 用全局 `SAP_REQUEST_TIMEOUT_SECS`）。
    pub request_timeout: Option<Duration>,
    /// 只读覆盖（None = 用全局 `SAP_READ_ONLY`）。
    pub read_only: Option<bool>,
    /// 落盘时的密码引用形态（如 `${SAP_PASSWD}`）：env 合成系统的 auth
    /// 持有**解密后**的密码，直接落 toml 会把 `${VAR}` 约定打穿成明文
    /// （2026-11-05 审计 P1）。带引用时 `spec_to_table` 用引用代替解析值。
    pub passwd_ref: Option<String>,
}

/// `sap_systems.toml` 原始形态（serde 反序列化用；setup API 的请求体
/// 转成同构再走 [`build_spec`]，保证两条路径校验完全一致）。
#[derive(Debug, Deserialize)]
pub(crate) struct SystemsFile {
    pub(crate) systems: BTreeMap<String, RawSystem>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RawSystem {
    pub(crate) ashost: Option<String>,
    pub(crate) sysnr: Option<String>,
    pub(crate) client: Option<String>,
    pub(crate) lang: Option<String>,
    #[serde(default)]
    pub(crate) default: bool,
    pub(crate) pool_size: Option<usize>,
    pub(crate) idle_validate_secs: Option<u64>,
    pub(crate) request_timeout_secs: Option<u64>,
    /// 鉴权块（`[systems.x.auth]`）：basic / sso2 / plugin 主通道三选一 +
    /// 可选 snc 叠加。给出即整体生效（无平铺字段）。
    pub(crate) auth: Option<AuthRaw>,
    /// ADT 通道块（`[systems.x.adt]`）：base_url / tls / auth 覆盖。
    pub(crate) adt: Option<AdtRaw>,
    pub(crate) read_only: Option<bool>,
    /// 自由键值透传（键大写化后直接进 `RfcOpenConnection`，
    /// 为 MSHOST/GROUP 负载均衡等形态留后门）。
    #[serde(default)]
    pub(crate) conn: BTreeMap<String, String>,
}

// ---- auth 块的原始形态（toml/JSON 同构；deny_unknown_fields 拦拼写） ----

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthRaw {
    pub(crate) basic: Option<BasicRaw>,
    pub(crate) sso2: Option<Sso2Raw>,
    pub(crate) plugin: Option<PluginRaw>,
    /// OAuth2（ADT 专用）
    pub(crate) oauth: Option<OAuthRaw>,
    /// SNC 叠加层
    pub(crate) snc: Option<SncRaw>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OAuthRaw {
    pub(crate) token_url: String,
    pub(crate) client_id: String,
    pub(crate) client_secret: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BasicRaw {
    pub(crate) user: String,
    pub(crate) passwd: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Sso2Raw {
    pub(crate) ticket: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PluginRaw {
    /// 插件命令（绝对路径，防 PATH 劫持）
    pub(crate) command: String,
    /// 透传参数（原样进插件 stdin JSON 的 parameters）
    #[serde(default)]
    pub(crate) params: BTreeMap<String, String>,
    /// 响应缓存秒数（默认 300）
    pub(crate) cache_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SncRaw {
    pub(crate) qop: Option<String>,
    pub(crate) myname: Option<String>,
    pub(crate) partnername: Option<String>,
    pub(crate) lib: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdtRaw {
    /// 基地址；`""` = 显式禁用
    pub(crate) base_url: Option<String>,
    /// 鉴权覆盖（缺省继承系统级 auth）
    pub(crate) auth: Option<AuthRaw>,
    pub(crate) tls: Option<TlsRaw>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TlsRaw {
    pub(crate) cert: String,
    pub(crate) key: Option<String>,
    pub(crate) ca: Option<String>,
}

impl AuthConfig {
    /// 原始块 → 解析配置：主通道恰一、必填字段齐全、`${VAR}` 插值。
    pub(crate) fn from_raw(raw: &AuthRaw, ctx: &str) -> Result<Self, String> {
        let primaries = [
            raw.basic.is_some(),
            raw.sso2.is_some(),
            raw.plugin.is_some(),
            raw.oauth.is_some(),
        ];
        let count = primaries.iter().filter(|b| **b).count();
        if count > 1 {
            return Err(format!(
                "{ctx} 主通道互斥：basic/sso2/plugin/oauth 至多一个（收到 {count} 个）"
            ));
        }
        if count == 0 {
            return Err(format!(
                "{ctx} 缺主通道：basic / sso2 / plugin / oauth 至少配置一个"
            ));
        }
        let primary = if let Some(b) = &raw.basic {
            let user = interpolate(&b.user, ctx)?;
            let passwd = interpolate(&b.passwd, ctx)?;
            if user.trim().is_empty() {
                return Err(format!("{ctx}.basic.user 为空串"));
            }
            if passwd.is_empty() {
                return Err(format!("{ctx}.basic.passwd 为空串"));
            }
            AuthPrimary::Basic { user, passwd }
        } else if let Some(t_raw) = &raw.sso2 {
            let t = interpolate(&t_raw.ticket, ctx)?;
            if t.is_empty() {
                return Err(format!("{ctx}.sso2.ticket 为空串"));
            }
            AuthPrimary::Sso2 { ticket: t }
        } else if let Some(o) = &raw.oauth {
            let token_url = interpolate(&o.token_url, ctx)?;
            let client_id = interpolate(&o.client_id, ctx)?;
            let client_secret = interpolate(&o.client_secret, ctx)?;
            for (k, v) in [("token_url", &token_url), ("client_id", &client_id), ("client_secret", &client_secret)] {
                if v.trim().is_empty() {
                    return Err(format!("{ctx}.oauth.{k} 为空串"));
                }
            }
            AuthPrimary::OAuth {
                token_url,
                client_id,
                client_secret,
            }
        } else {
            let p = raw.plugin.as_ref().unwrap();
            let command = interpolate(&p.command, ctx)?;
            if command.trim().is_empty() {
                return Err(format!("{ctx}.plugin.command 为空串"));
            }
            if !command.starts_with('/') {
                return Err(format!(
                    "{ctx}.plugin.command 必须是绝对路径（防 PATH 劫持）: {command}"
                ));
            }
            let mut parameters = BTreeMap::new();
            for (k, v) in &p.params {
                parameters.insert(k.clone(), interpolate(v, ctx)?);
            }
            AuthPrimary::Plugin(crate::adt::AuthPluginSpec {
                command,
                parameters,
                cache_ttl: p
                    .cache_secs
                    .map(Duration::from_secs)
                    .unwrap_or(crate::adt::PLUGIN_DEFAULT_TTL),
            })
        };
        let snc = match &raw.snc {
            None => None,
            Some(s) => {
                let cfg = SncConfig {
                    qop: s.qop.as_ref().map(|v| interpolate(v, ctx)).transpose()?,
                    myname: s.myname.as_ref().map(|v| interpolate(v, ctx)).transpose()?,
                    partnername: s
                        .partnername
                        .as_ref()
                        .map(|v| interpolate(v, ctx))
                        .transpose()?,
                    lib: s.lib.as_ref().map(|v| interpolate(v, ctx)).transpose()?,
                };
                if !cfg.is_present() {
                    return Err(format!("{ctx}.snc 为空表（至少给一个字段，或整块删除）"));
                }
                Some(cfg)
            }
        };
        Ok(Self { primary, snc })
    }

    /// ADT 块 → 解析配置。
    pub(crate) fn adt_from_raw(raw: &AdtRaw, ctx: &str) -> Result<AdtConfig, String> {
        Ok(AdtConfig {
            base_url: raw.base_url.clone(),
            auth: match &raw.auth {
                Some(a) => Some(AuthConfig::from_raw(a, &format!("{ctx}.auth"))?),
                None => None,
            },
            tls: match &raw.tls {
                Some(t) => {
                    if t.cert.trim().is_empty() {
                        return Err(format!("{ctx}.tls.cert 为空串"));
                    }
                    Some(TlsConfig {
                        cert: t.cert.clone(),
                        key: t.key.clone(),
                        ca: t.ca.clone(),
                    })
                }
                None => None,
            },
        })
    }
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
/// 鉴权键（USER/PASSWD/MYSAPSSO2/SNC_*）由 `auth` 块贡献，不在此列——
/// conn 里给这些键会被 auth 块的参数冲突检查拦下。
const SHORTCUT_KEYS: [(&str, &str); 4] = [
    ("ashost", "ASHOST"),
    ("sysnr", "SYSNR"),
    ("client", "CLIENT"),
    ("lang", "LANG"),
];

/// 把原始系统定义组装成 [`SystemSpec`]：auth 块解析、快捷字段与 conn 透传
/// 合并、插值、校验。（启动期解析与 setup API 写入共用——两路径校验完全一致）
pub(crate) fn build_spec(name: &str, raw: &RawSystem) -> Result<SystemSpec, String> {
    validate_name(name)?;
    let ctx = format!("systems.{}", name);

    // ① auth 块：必填（互斥与必填字段由 AuthConfig::from_raw 校验），
    //    其 SDK 键值对（USER/PASSWD | MYSAPSSO2 | SNC_*）进连接参数。
    let auth_raw = raw.auth.as_ref().ok_or_else(|| {
        format!(
            "{ctx} 缺少 auth 配置——加 [systems.{name}.auth.basic]（user+passwd）\n或 auth.sso2 / auth.plugin（形态见 sap_systems.toml.example）"
        )
    })?;
    let auth = AuthConfig::from_raw(auth_raw, &format!("{ctx}.auth"))?;
    if matches!(auth.primary, AuthPrimary::OAuth { .. }) {
        return Err(format!(
            "{ctx} 的 OAuth 是 ADT 专用通道（RFC 无 OAuth）——请放到 [systems.{name}.adt.auth.oauth]"
        ));
    }

    let mut params: Vec<(&'static str, String)> = auth.primary_conn_params();
    if let Some(snc) = &auth.snc {
        params.extend(snc.to_conn_params());
    }
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
    push("LANG", raw.lang.as_ref())?;

    // ② conn 透传：键大写化，与快捷字段冲突报错；鉴权 SDK 键一律拒收
    //    （auth 走 auth 块——conn 夹带票据/SNC 会让双通道凭据同时生效，
    //    SDK 行为不可预期，配置歧义在装载期炸掉）
    const AUTH_CONN_KEYS: [&str; 7] = [
        "USER",
        "PASSWD",
        "MYSAPSSO2",
        "SNC_QOP",
        "SNC_MYNAME",
        "SNC_PARTNERNAME",
        "SNC_LIB",
    ];
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
        if AUTH_CONN_KEYS.contains(&upper.as_str()) {
            return Err(format!(
                "{} 的 conn.{} 是鉴权键——请配置在 auth 块里（basic/sso2/snc）",
                ctx, k
            ));
        }
        let v = interpolate(v, &ctx)?;
        // 泄漏安全：upper 是运行期 String，需要 'static —— 用 Box::leak
        // 固化（系统数量有限、进程生命周期，泄漏量有界）
        let key: &'static str = Box::leak(upper.into_boxed_str());
        params.push((key, v));
    }

    // ③ 连接形态校验：直连（ASHOST+SYSNR）或负载均衡（MSHOST）
    let has = |k: &str| params.iter().any(|(k2, _)| *k2 == k);
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

    // ④ ADT 块（可选；缺省 = URL 推导 + auth 继承）
    let adt = match &raw.adt {
        Some(a) => Some(AuthConfig::adt_from_raw(a, &format!("{ctx}.adt"))?),
        None => None,
    };

    Ok(SystemSpec {
        name: name.to_string(),
        is_default: raw.default,
        conn_params: params,
        auth,
        adt,
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
        read_only: raw.read_only,
        passwd_ref: None,
    })
}

/// 从 `.env` 的 `SAP_*` 环境变量合成单系统 `default`（存量部署路径）。
/// `AppConfig.conn_params` 已是解析好的 SDK 键值对，直接复用。
fn synth_from_env(cfg: &AppConfig) -> SystemSpec {
    // env 路径只有 basic：USER/PASSWD 从 conn_params 取（SAP_USER/SAP_PASSWD）；
    // ADT 凭据覆盖（SAP_ADT_USER/SAP_ADT_PASSWD）与连接凭据**不同**时才发
    // adt.auth 覆盖，否则 adt 块整体缺省（URL 推导 + auth 继承）。
    let conn_user = cfg
        .conn_params
        .iter()
        .find(|(k, _)| *k == "USER")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let conn_passwd = cfg
        .conn_params
        .iter()
        .find(|(k, _)| *k == "PASSWD")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let adt_overridden = cfg.adt_user != conn_user || cfg.adt_passwd != conn_passwd;
    let adt = if adt_overridden || cfg.adt_base_url.is_some() {
        Some(AdtConfig {
            // env 语义（config.rs 已解析）：None=禁用 → spec 里显式 Some("")
            base_url: cfg.adt_base_url.clone().or_else(|| Some(String::new())),
            // 凭据与连接一致 → None = 继承系统级 auth；不同 → 覆盖
            auth: adt_overridden
                .then(|| AuthConfig {
                    primary: AuthPrimary::Basic {
                        user: cfg.adt_user.clone(),
                        passwd: cfg.adt_passwd.clone(),
                    },
                    snc: None,
                }),
            tls: None,
        })
    } else {
        None
    };
    SystemSpec {
        name: "default".into(),
        is_default: true,
        conn_params: cfg.conn_params.clone(),
        auth: AuthConfig {
            primary: AuthPrimary::Basic {
                user: conn_user,
                passwd: conn_passwd,
            },
            snc: None,
        },
        adt,
        pool_size: None,
        idle_validate: None,
        request_timeout: None,
        read_only: None,
        // 落盘引用形态：env 密码来自 SAP_PASSWD，落 toml 时保留 ${VAR} 引用
        passwd_ref: Some("${SAP_PASSWD}".into()),
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
    /// 该系统的 ADT 端点（None = 未启用；Arc 共享——debug 会话持有独立引用计数）
    pub adt: Option<Arc<crate::adt::AdtEndpoint>>,
    /// 该系统的函数/DDIC 元数据缓存（按系统实例化，防跨系统同名污染）
    pub meta: crate::metadata::MetadataCache,
    /// 该系统只读（写端点 403）
    pub read_only: bool,
    /// 连接鉴权形态（sapcli 借鉴 B1）：basic | sso2 | snc | sso2+snc。
    /// /api/systems 自描述用——agent 据此知道该系统是密码、票据还是 SNC 登录。
    pub auth: &'static str,
    /// 该系统单请求超时**覆盖**（toml 的 request_timeout_secs；None = 跟随
    /// 全局热设置——v0.20 起 PUT /api/settings 即改即生效，见 effective_timeout）
    pub request_timeout: Option<Duration>,
    /// 连接目标标识（SYSID 查重的同后端豁免用，见 [`backend_key`]）
    backend: String,
    /// 首连成功后登记的 SYSID（连接属性读取，本地无 RFC 往返）
    sysid: OnceLock<String>,
    /// SYSID 与其他**后端**冲突 → 隔离（请求 503 SYSTEM_SYSID_CONFLICT）
    quarantined: AtomicBool,
    /// 最近一次连接失败的原因（探测任务维护；None = 最近一轮探测成功或尚未探测过）。
    /// 所有系统一律惰性连接，本字段就是「连接异常」状态的载体——`/api/systems`
    /// 的 `connected` / `connect_error` 与首页状态区都从这里取数。
    last_connect_error: std::sync::RwLock<Option<String>>,
    /// `/api/version` 的懒加载系统信息缓存（生命周期内不变，失败不缓存）
    pub sap_info: tokio::sync::RwLock<Option<Arc<crate::connection::RemoteSystemInfo>>>,
    /// sap_info 的**负缓存**：(记录时刻, 错误消息)。失败后 60s 内直接回这个
    /// 错误不再真连——否则"握手挂起"型不可达系统（VPN 半开等）让每次
    /// `/api/version` 都拖满 10s 超时。状态面的 connected/connect_error 由
    /// 探测任务维护，与此互不依赖。
    sap_info_err: tokio::sync::RwLock<Option<(std::time::Instant, String)>>,
}

/// 单系统的 reload 结果（`POST /api/admin/reload` 响应体）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReloadReport {
    /// 系统名（URL 段）
    pub system: String,
    /// 换代时立刻关闭的空闲连接数（借出中的连接会在归还时关闭，不计入）
    pub connections_closed: usize,
    /// 清掉的函数元数据条数
    pub func_metadata_cleared: usize,
    /// 清掉的 DDIC 类型元数据条数
    pub type_metadata_cleared: usize,
    /// 从 SDK 仓库缓存点名移除的函数描述符条数（缓存中本就无此名的跳过）
    pub sdk_desc_removed: usize,
    /// 从 SDK 仓库缓存点名移除的类型描述符条数
    pub sdk_type_removed: usize,
}

impl SystemHandle {
    /// 让该系统"像刚重启过"：连接池换代 + 清空元数据缓存 + **点名移除 SDK
    /// 描述符缓存条目**。
    ///
    /// 场景：SAP 端改了某 RFC 的参数（增删改）。三层缓存会让人以为"必须重启
    /// 网关"——SAP 侧函数组加载按**连接**缓存（换代丢弃旧连接即可）、SDK 的
    /// 函数/类型描述符按 **SYSID 进程级**缓存（与连接无关，sapnwrfc.h 明言
    /// "cache for every R/3 System ID"，必须用 `RfcRemoveFunctionDesc` 点名
    /// 移除）、网关自己的 [`MetadataCache`] 按**函数名**缓存。这里把三层一起
    /// 作废，且**不需要重启进程**：监听端口不变、在途请求用旧连接跑完（归还
    /// 时丢弃），新请求重新向 SAP 拉取签名。
    ///
    /// SDK 层的移除范围 = [`MetadataCache`] 清出的名字 + `extra_funcs`（调用方
    /// 传入的点名清单，覆盖"调用过但从未自省过"的函数）。未成功连过（无
    /// SYSID）的系统没有 SDK 缓存可言，跳过该层。
    ///
    /// 注意：只影响运行时状态，不重读 `.env` / `sap_systems.toml`。
    /// 改配置（监听地址、API key、连接参数）仍需进程重启。
    pub fn reload(&self, extra_funcs: &[String]) -> ReloadReport {
        let connections_closed = self.pool.reload();
        let cleared = self.meta.clear_all();
        let crate::metadata::ClearedMetadata {
            func_names,
            type_names,
        } = cleared;
        let func_metadata_cleared = func_names.len();
        let type_metadata_cleared = type_names.len();
        // SDK 层移除范围 = 元数据缓存清出的名字 + 调用方点名（覆盖"调用过但
        // 从未自省过"的函数）。去重合并。
        let mut names = func_names;
        for f in extra_funcs {
            let up = f.trim().to_uppercase();
            if !up.is_empty() && !names.contains(&up) {
                names.push(up);
            }
        }
        let (mut sdk_desc_removed, mut sdk_type_removed) = (0usize, 0usize);
        if let Some(sysid) = self.sysid() {
            for name in &names {
                if crate::connection::drop_sdk_function_desc(&sysid, name) {
                    sdk_desc_removed += 1;
                }
            }
            for name in &type_names {
                if crate::connection::drop_sdk_type_desc(&sysid, name) {
                    sdk_type_removed += 1;
                }
            }
        }
        tracing::info!(
            system = %self.name,
            connections_closed,
            func_metadata_cleared,
            type_metadata_cleared,
            sdk_desc_removed,
            sdk_type_removed,
            "系统已 reload：连接池换代 + 元数据清空 + SDK 描述符点名移除（RFC 签名变更生效）"
        );
        ReloadReport {
            system: self.name.clone(),
            connections_closed,
            func_metadata_cleared,
            type_metadata_cleared,
            sdk_desc_removed,
            sdk_type_removed,
        }
    }

    /// 生效的单请求超时：系统级覆盖优先，否则取全局热设置
    /// （settings.rs——PUT /api/settings 改全局默认后，未覆盖的系统立即生效）。
    pub fn effective_timeout(&self) -> Duration {
        self.request_timeout
            .unwrap_or_else(|| crate::settings::current().request_timeout)
    }

    /// 已登记的 SYSID（未知时 None——系统尚未成功连过）。
    pub fn sysid(&self) -> Option<String> {
        self.sysid.get().cloned()
    }

    /// 是否因 SYSID 冲突被隔离。
    pub fn is_quarantined(&self) -> bool {
        self.quarantined.load(Ordering::SeqCst)
    }

    /// 最近一次连接失败的原因（探测任务维护；None = 最近一轮成功或尚未探测）。
    pub fn connect_error(&self) -> Option<String> {
        self.last_connect_error.read().ok()?.clone()
    }

    /// 写入/清除连接失败原因（探测任务与调用期上报共用）。超长截断到 300 字符，
    /// 避免整段 RFC 栈回灌进状态面 JSON。
    pub fn set_connect_error(&self, err: Option<String>) {
        let truncated = err.map(|mut e| {
            if e.len() > 300 {
                e.truncate(300);
                e.push('…');
            }
            e
        });
        if let Ok(mut slot) = self.last_connect_error.write() {
            *slot = truncated;
        }
    }

    /// 连接状态：最近一轮探测成功（从未连上过 → false）。
    /// 探测间隔（60s）内的瞬时断连要等下一轮才反映——页面状态以此为准。
    pub fn connected(&self) -> bool {
        self.sysid().is_some() && self.connect_error().is_none()
    }

    /// sap_info 负缓存读取（fresh 与否由调用方按 TTL 判断）。
    pub async fn sap_info_negative(&self) -> Option<(std::time::Instant, String)> {
        self.sap_info_err.read().await.clone()
    }

    /// 写入/清除 sap_info 负缓存（version 模块取数失败/成功时调用）。
    pub async fn set_sap_info_negative(&self, err: Option<String>) {
        *self.sap_info_err.write().await = err.map(|msg| (std::time::Instant::now(), msg));
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

    /// 运行期替换一个系统的句柄（setup 编辑路径）：连接池/ADT/元数据缓存
    /// 全部换新，旧句柄由在途请求持有的 Arc 引用维持到归还后自然释放。
    /// 系统在默认位时，即便 `make_default=false` 也保持默认指向新句柄
    /// （默认不能悬空指向旧句柄）。同名不存在返回 false（调用方前置校验）。
    pub fn replace_system(
        &self,
        name: &str,
        handle: Arc<SystemHandle>,
        make_default: bool,
    ) -> bool {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if !inner.systems.contains_key(name) {
            return false;
        }
        inner.systems.insert(name.to_string(), Arc::clone(&handle));
        let was_default = inner
            .default
            .as_ref()
            .map(|d| d.name == name)
            .unwrap_or(false);
        if make_default || was_default {
            inner.default = Some(handle);
        }
        tracing::info!(system = %name, default = make_default || was_default, "系统已热替换（setup 编辑，无需重启）");
        true
    }

    /// 运行期移除系统：返回被移除的句柄与其**移除前**是否为默认。默认位
    /// 随移除置空——由调用方提升剩余系统为新默认（或接受回到未配置模式）。
    pub fn remove_system(&self, name: &str) -> Option<(Arc<SystemHandle>, bool)> {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let handle = inner.systems.remove(name)?;
        inner.order.retain(|n| n != name);
        let was_default = inner
            .default
            .as_ref()
            .map(|d| d.name == name)
            .unwrap_or(false);
        if was_default {
            inner.default = None;
        }
        tracing::info!(system = %name, was_default, "系统已移除（setup 删除，无需重启）");
        Some((handle, was_default))
    }

    /// 提升默认系统（无前缀路径的新目标）。名字未知返回 Err（带合法名单）。
    pub fn set_default(&self, name: &str) -> Result<(), String> {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let handle =
            inner.systems.get(name).cloned().ok_or_else(|| {
                format!("系统 '{}' 不存在（可用: {}）", name, inner.order.join(", "))
            })?;
        inner.default = Some(handle);
        tracing::info!(system = %name, "默认系统已切换（setup API，无需重启）");
        Ok(())
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
fn build_adt(spec: &SystemSpec, timeout: Duration) -> Option<Arc<crate::adt::AdtEndpoint>> {
    let adt_cfg = spec.adt.as_ref();

    // 基地址：显式 > ashost 推导；"" = 显式禁用
    let base = match adt_cfg.and_then(|a| a.base_url.as_deref()) {
        Some("") => return None,
        Some(s) => s.to_string(),
        None => {
            let ashost = conn_param(&spec.conn_params, "ASHOST")?;
            format!("http://{}:{}", ashost, ADT_DEFAULT_PORT)
        }
    };

    // 鉴权：adt.auth 覆盖 > 系统级 auth 继承
    let auth_cfg = adt_cfg.and_then(|a| a.auth.as_ref()).unwrap_or(&spec.auth);
    let (auth, responsible) = match &auth_cfg.primary {
        AuthPrimary::Basic { user, passwd } => (
            crate::adt::AdtAuthSpec::Basic {
                user: user.clone(),
                passwd: passwd.clone(),
            },
            user.trim().to_uppercase(),
        ),
        AuthPrimary::Sso2 { ticket } => (
            crate::adt::AdtAuthSpec::Sso2 {
                ticket: ticket.clone(),
                user: auth_cfg.display_user(),
            },
            auth_cfg.display_user(),
        ),
        AuthPrimary::Plugin(p) => (
            crate::adt::AdtAuthSpec::Plugin(p.clone()),
            auth_cfg.display_user(),
        ),
        AuthPrimary::OAuth {
            token_url,
            client_id,
            client_secret,
        } => (
            crate::adt::AdtAuthSpec::OAuth {
                token_url: token_url.clone(),
                client_id: client_id.clone(),
                client_secret: client_secret.clone(),
                user: None,
                passwd: None,
            },
            auth_cfg.display_user(),
        ),
    };

    // TLS 素材（mTLS identity / CA bundle）
    let mut tls_files: Vec<&str> = Vec::new();
    let mut ca: Option<&str> = None;
    if let Some(t) = adt_cfg.and_then(|a| a.tls.as_ref()) {
        tls_files.push(t.cert.as_str());
        if let Some(key) = t.key.as_deref() {
            tls_files.push(key);
        }
        ca = t.ca.as_deref();
    }

    let ep = match crate::adt::AdtEndpoint::new_with_auth(&base, auth, &tls_files, ca, timeout) {
        Ok(ep) => ep,
        Err(e) => {
            tracing::error!(system = %spec.name, error = %e, "ADT 端点构建失败——该系统 ADT 通道停用");
            return None;
        }
    };
    tracing::info!(
        system = %spec.name,
        base = %ep.base_url(),
        auth = auth_cfg.mode(),
        responsible = %responsible,
        "ADT 代理已启用（/api/adt/**）"
    );
    Some(Arc::new(ep))
}

/// 组装单个系统运行句柄（启动装配与 setup 热加载共用）。
/// 一律惰性池：不触 FFI、启动永不因 SAP 不可达而失败；可达性交给探测任务
/// （写 [`SystemHandle::last_connect_error`] 供状态面展示）与调用期按需重试。
pub(crate) fn make_handle(spec: &SystemSpec, cfg: &AppConfig) -> Arc<SystemHandle> {
    let pool_size = spec.pool_size.unwrap_or(cfg.pool_size);
    let idle_validate = spec.idle_validate.unwrap_or(cfg.pool_idle_validate);
    let read_only = spec.read_only.unwrap_or(cfg.read_only);

    let pool = Arc::new(RfcConnectionPool::lazy(
        spec.conn_params.clone(),
        pool_size,
        idle_validate,
    ));
    // ADT 端点超时是构建期快照（AdtEndpoint 不持热设置引用）
    let adt = build_adt(spec, spec.request_timeout.unwrap_or(cfg.request_timeout));

    Arc::new(SystemHandle {
        name: spec.name.clone(),
        pool,
        adt,
        meta: crate::metadata::MetadataCache::new(),
        read_only,
        auth: spec.auth.mode(),
        request_timeout: spec.request_timeout,
        backend: backend_key(&spec.conn_params),
        sysid: OnceLock::new(),
        quarantined: AtomicBool::new(false),
        last_connect_error: std::sync::RwLock::new(None),
        sap_info: tokio::sync::RwLock::new(None),
        sap_info_err: tokio::sync::RwLock::new(None),
    })
}

/// 组装运行态注册表。所有系统一律惰性池——**SAP 连不上也不阻止启动**，
/// 可达性交给探测任务（`spawn_probe_loop`）与调用期按需建连。
/// 空集合合法（未配置模式，default = None）。
pub fn build_registry(specs: Vec<SystemSpec>, cfg: &AppConfig) -> SystemRegistry {
    let mut systems = HashMap::with_capacity(specs.len());
    let mut order = Vec::with_capacity(specs.len());
    let mut default = None;

    for spec in specs {
        let handle = make_handle(&spec, cfg);
        tracing::info!(
            system = %spec.name,
            default = spec.is_default,
            read_only = spec.read_only.unwrap_or(cfg.read_only),
            "系统已配置（惰性连接，可达性由探测任务与调用期上报）"
        );
        if spec.is_default {
            default = Some(Arc::clone(&handle));
        }
        order.push(spec.name.clone());
        systems.insert(spec.name, handle);
    }

    // load_specs 校验保证非空集合恰有一个默认系统；空集合 = 未配置模式
    debug_assert!(default.is_some() || order.is_empty());

    SystemRegistry {
        inner: std::sync::RwLock::new(RegistryInner {
            systems,
            order,
            default,
        }),
        cfg: cfg.clone(),
    }
}

/// 启动期探测任务：逐系统（并行）建连取 SYSID 并登记。
/// 所有系统均为惰性池，这一步是「可达性」的首次体检，结果写入
/// [`SystemHandle::last_connect_error`]（`/api/systems` 与首页状态区取数）；
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
                        sys.set_connect_error(None);
                    }
                    Ok(Ok(Err(e))) => {
                        tracing::warn!(
                            system = %sys.name,
                            msg = %e.message,
                            "系统暂不可达（不影响网关监听；探测任务与调用期都会按需重试）"
                        );
                        sys.set_connect_error(Some(e.message));
                    }
                    Ok(Err(join)) => {
                        tracing::warn!(system = %sys.name, %join, "探测任务 join 失败");
                        sys.set_connect_error(Some(format!("探测任务失败: {join}")));
                    }
                    Err(_) => {
                        tracing::warn!(system = %sys.name, "系统探测超时（5s，不影响网关监听）");
                        sys.set_connect_error(Some("连接超时（探测 5s 无响应）".into()));
                    }
                }
            })
        })
        .collect();
    for p in probes {
        let _ = p.await;
    }
}

/// 周期探测循环：首轮立即，之后每轮间隔 `interval`（生产用 60s）重跑
/// [`probe_all`]。连接状态（`connected` / `connect_error`、首页指示）靠它
/// 自动翻新——SAP 恢复后的一个周期内页面就会转绿，无需任何手动操作。
pub async fn probe_loop(registry: Arc<SystemRegistry>, interval: Duration) {
    loop {
        probe_all(Arc::clone(&registry)).await;
        tokio::time::sleep(interval).await;
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
            drain_secs: 5,
            rate_limit_rps: None,
            adt_base_url: Some("http://h:50000".into()),
            adt_user: "U".into(),
            adt_passwd: "P".into(),
            read_only: false,
            registry_file: "registry.json".into(),
            runtime_mode: false,
            update_check: false,
            systems_file: "no-such-file.toml".into(),
            gateway_file: "no-such-gateway.toml".into(),
            setup_enabled: true,
            setup_force: false,
            open_browser: false,
            lang: "EN".into(),
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
        let reg = build_registry(specs, &cfg);
        assert!(reg.default().is_none(), "空注册表无默认系统");
        assert!(reg.names().is_empty());

        let toml = "[systems]\n[systems.a4h]\nashost=\"h\"\nsysnr=\"00\"\n[systems.a4h.auth.basic]\nuser=\"u\"\npasswd=\"p\"";
        let file: SystemsFile = toml::from_str(toml).unwrap();
        let (name, raw) = file.systems.iter().next().unwrap();
        let spec = build_spec(name, raw).unwrap();
        let handle = make_handle(&spec, &cfg);
        reg.add_system(handle, false); // 即便 make_default=false，无默认时也提升
        assert_eq!(reg.default().unwrap().name, "a4h");
        assert!(reg.resolve("a4h").is_some());
        assert_eq!(reg.names(), vec!["a4h".to_string()]);

        // 第二个系统 make_default=true → 取代默认
        let toml2 = "[systems]\n[systems.prd]\nashost=\"h2\"\nsysnr=\"01\"\n[systems.prd.auth.basic]\nuser=\"u\"\npasswd=\"p\"";
        let file2: SystemsFile = toml::from_str(toml2).unwrap();
        let (name2, raw2) = file2.systems.iter().next().unwrap();
        let spec2 = build_spec(name2, raw2).unwrap();
        reg.add_system(make_handle(&spec2, &cfg), true);
        assert_eq!(reg.default().unwrap().name, "prd");
        assert_eq!(reg.names().len(), 2);
    }

    #[tokio::test]
    async fn unreachable_system_starts_and_reports_connect_error() {
        // 全惰性（v0.19）：连不上的系统照常组装注册表，探测失败写入
        // connect_error（/api/systems 与首页状态区取数）——v0.16-0.18 的
        // 「默认系统连不上 → 启动即退出」行为就此终结。
        let cfg = test_cfg(vec![
            ("ASHOST", "127.0.0.1".into()),
            ("SYSNR", "00".into()),
            ("CLIENT", "001".into()),
            ("USER", "u".into()),
            ("PASSWD", "p".into()),
        ]);
        let spec = synth_from_env(&cfg);
        assert!(spec.is_default);
        let reg = Arc::new(build_registry(vec![spec], &cfg));
        let d = reg.default().unwrap().clone();
        assert!(!d.connected(), "从未连上 → connected=false");

        // 探测走真实 FFI 连接：127.0.0.1:3300 无监听 → 立即拒绝，不挂起
        probe_all(Arc::clone(&reg)).await;
        assert!(!d.connected());
        let err = d.connect_error();
        assert!(err.is_some(), "探测失败应留下 connect_error 供状态面展示");
        assert!(
            !err.unwrap().is_empty(),
            "connect_error 应携带可读的失败原因"
        );
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
        // 无显式 ADT 配置 + 凭据一致 → adt 块整体缺省（URL 推导 + auth 继承）
        assert!(spec.adt.is_none());
        // 显式空串 = 禁用 → adt.base_url = Some("")（auth 仍继承）
        cfg.adt_base_url = Some(String::new());
        let spec2 = synth_from_env(&cfg);
        let adt2 = spec2.adt.as_ref().expect("禁用标记必须保留");
        assert_eq!(adt2.base_url.as_deref(), Some(""));
        assert!(adt2.auth.is_none());
        // ADT 凭据覆盖（SAP_ADT_USER 不同）→ adt.auth.basic 覆盖
        cfg.adt_user = "adtu".into();
        let spec3 = synth_from_env(&cfg);
        let adt3 = spec3.adt.as_ref().unwrap();
        assert!(adt3
            .auth
            .as_ref()
            .unwrap()
            .primary
            .basic_ref()
            .is_some_and(|(u, _)| u == "adtu"));
    }

    #[test]
    fn parse_single_system_implicit_default() {
        let toml = r#"
[systems.a4h]
ashost = "vhcala4h"
sysnr = "00"
client = "001"
[systems.a4h.auth.basic]
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
        assert!(specs[0].conn_params.iter().any(|(k, v)| *k == "ASHOST" && v == "vhcala4h"));
        assert!(specs[0].conn_params.iter().any(|(k, v)| *k == "USER" && v == "dev"));
        assert_eq!(specs[0].auth.mode(), "basic");
    }

    #[test]
    fn parse_multi_system_with_overrides() {
        let toml = r#"
[systems.a4h]
ashost = "h1"
sysnr = "00"
client = "001"
default = true
[systems.a4h.auth.basic]
user = "u1"
passwd = "p1"

[systems.prd]
ashost = "h2"
sysnr = "10"
client = "100"
pool_size = 4
read_only = true
[systems.prd.auth.basic]
user = "u2"
passwd = "p2"
[systems.prd.adt]
base_url = ""
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
        assert_eq!(
            prd.adt.as_ref().and_then(|a| a.base_url.as_deref()),
            Some("")
        );
        // adt.auth 缺省 → 继承系统级 basic（凭据 u2/p2）
        assert!(prd.adt.as_ref().unwrap().auth.is_none());
        assert!(prd.auth.primary.basic_ref().is_some_and(|(u, _)| u == "u2"));
        assert!(!prd.is_default);
        assert!(specs.iter().find(|s| s.name == "a4h").unwrap().is_default);
    }

    #[test]
    fn conn_passthrough_and_conflicts() {
        let toml = r#"
[systems.lb]
[systems.lb.auth.basic]
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
[systems.x.auth.basic]
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
        const BASIC: &str = "[systems.ok.auth.basic]\nuser=\"u\"\npasswd=\"p\"\n";
        // 名字非法（大写）
        assert!(mk("[systems.Bad]\n[BAD.auth.basic]\nuser=\"u\"\npasswd=\"p\"\nashost=\"h\"\nsysnr=\"00\"").is_err());
        // 缺 auth 块
        assert!(mk("[systems.ok]\nashost=\"h\"\nsysnr=\"00\"").is_err());
        // auth 块缺 basic
        assert!(mk("[systems.ok]\nashost=\"h\"\nsysnr=\"00\"\n[systems.ok.auth]\nsnc = {}").is_err());
        // ASHOST 无 SYSNR
        assert!(mk(format!("[systems.ok]\nashost=\"h\"\n{BASIC}").as_str()).is_err());
        // 无连接目标
        assert!(mk(format!("[systems.ok]\n{BASIC}").as_str()).is_err());
        // MSHOST 形态合法（无 ashost/sysnr）
        assert!(
            mk(format!("[systems.ok]\n{BASIC}[systems.ok.conn]\nMSHOST=\"m\"").as_str()).is_ok()
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
            auth: "basic",
            request_timeout: Some(Duration::from_secs(5)),
            backend: backend_key(&params),
            sysid: OnceLock::new(),
            quarantined: AtomicBool::new(false),
            last_connect_error: std::sync::RwLock::new(None),
            sap_info: tokio::sync::RwLock::new(None),
            sap_info_err: tokio::sync::RwLock::new(None),
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

    fn mk_spec(name: &str, conn_params: Vec<(&'static str, String)>) -> SystemSpec {
        SystemSpec {
            name: name.into(),
            is_default: false,
            conn_params,
            auth: AuthConfig {
                primary: AuthPrimary::Basic {
                    user: "u".into(),
                    passwd: "p".into(),
                },
                snc: None,
            },
            adt: None,
            pool_size: None,
            idle_validate: None,
            request_timeout: None,
            read_only: None,
            passwd_ref: None,
        }
    }

    #[test]
    fn adt_derivation_rules() {
        // 无 adt 配置 + 有 ashost → 按 ashost 推导（启用）
        let direct = mk_spec("a", direct_params());
        assert!(build_adt(&direct, Duration::from_secs(5)).is_some());
        // 显式空串 → 禁用
        let mut disabled = direct.clone();
        disabled.adt = Some(AdtConfig {
            base_url: Some(String::new()),
            auth: None,
            tls: None,
        });
        assert!(build_adt(&disabled, Duration::from_secs(5)).is_none());
        // MSHOST 形态（无 ashost）且未配置 adt → 无法推导 → 禁用
        let lb = mk_spec(
            "b",
            vec![
                ("MSHOST", "m".into()),
                ("USER", "u".into()),
                ("PASSWD", "p".into()),
            ],
        );
        assert!(build_adt(&lb, Duration::from_secs(5)).is_none());
        // adt.auth 覆盖（SSO2）生效：base 推导 + 鉴权走票据
        let mut sso2 = mk_spec("c", direct_params());
        sso2.adt = Some(AdtConfig {
            base_url: None,
            auth: Some(AuthConfig {
                primary: AuthPrimary::Sso2 {
                    ticket: "t".into(),
                },
                snc: None,
            }),
            tls: None,
        });
        assert!(build_adt(&sso2, Duration::from_secs(5)).is_some());
    }
        #[test]
    fn replace_remove_and_set_default_mutate_registry() {
        let cfg = test_cfg(vec![]);
        let reg = build_registry(vec![], &cfg);
        let a = mk_handle("a", "h-a");
        let b = mk_handle("b", "h-b");
        reg.add_system(Arc::new(a), false);
        reg.add_system(Arc::new(b), true);
        assert_eq!(reg.default().unwrap().name, "b");

        // 替换默认系统：make_default=false 也必须保持默认指向新句柄
        let a2 = mk_handle("a", "h-a2");
        assert!(reg.replace_system("a", Arc::new(a2), false));
        assert_eq!(reg.default().unwrap().name, "b", "替换 b 无关默认");
        let b2 = mk_handle("b", "h-b2");
        assert!(reg.replace_system("b", Arc::new(b2), false));
        assert_eq!(
            reg.default().unwrap().name,
            "b",
            "默认位系统被替换后默认仍指向新句柄"
        );
        // 同名不存在 → false
        assert!(!reg.replace_system("ghost", Arc::new(mk_handle("ghost", "h-g")), true));

        // 切默认
        reg.set_default("a").unwrap();
        assert_eq!(reg.default().unwrap().name, "a");
        assert!(reg.set_default("ghost").is_err());

        // 移除非默认系统：默认不受影响、order 收缩
        let (removed, was_default) = reg.remove_system("b").unwrap();
        assert_eq!(removed.name, "b");
        assert!(!was_default);
        assert_eq!(reg.names(), vec!["a".to_string()]);
        assert_eq!(reg.default().unwrap().name, "a");
        assert!(reg.remove_system("b").is_none(), "重复移除 → None");

        // 移除默认系统：default 置空（调用方决定提升谁或接受未配置模式）
        let (removed, was_default) = reg.remove_system("a").unwrap();
        assert_eq!(removed.name, "a");
        assert!(was_default);
        assert!(reg.default().is_none());
        assert!(reg.names().is_empty());
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    fn mk_raw(auth: AuthRaw) -> RawSystem {
        RawSystem {
            ashost: Some("h".into()),
            sysnr: Some("00".into()),
            client: Some("001".into()),
            lang: None,
            auth: Some(auth),
            adt: None,
            default: false,
            pool_size: None,
            idle_validate_secs: None,
            request_timeout_secs: None,
            read_only: None,
            conn: Default::default(),
        }
    }

    #[test]
    fn mode_classifies_primary_and_snc() {
        let basic = AuthConfig {
            primary: AuthPrimary::Basic {
                user: "u".into(),
                passwd: "p".into(),
            },
            snc: None,
        };
        assert_eq!(basic.mode(), "basic");
        let sso2_snc = AuthConfig {
            primary: AuthPrimary::Sso2 {
                ticket: "t".into(),
            },
            snc: Some(SncConfig {
                partnername: Some("p:CN=x".into()),
                ..Default::default()
            }),
        };
        assert_eq!(sso2_snc.mode(), "sso2+snc");
        let plugin = AuthConfig {
            primary: AuthPrimary::Plugin(crate::adt::AuthPluginSpec::default()),
            snc: None,
        };
        assert_eq!(plugin.mode(), "plugin");
    }

    #[test]
    fn from_raw_basic_yields_conn_params() {
        let raw = AuthRaw {
            basic: Some(BasicRaw {
                user: "dev".into(),
                passwd: "${SAP_T}".into(),
            }),
            sso2: None,
            plugin: None,
            oauth: None,
            snc: None,
        };
        unsafe { std::env::set_var("SAP_T", "s3cret") };
        let cfg = AuthConfig::from_raw(&raw, "systems.x.auth").unwrap();
        assert_eq!(cfg.mode(), "basic");
        let params = cfg.primary_conn_params();
        assert!(params.iter().any(|(k, v)| *k == "USER" && v == "dev"));
        assert!(params.iter().any(|(k, v)| *k == "PASSWD" && v == "s3cret"));
        assert_eq!(cfg.display_user(), "DEV");
        unsafe { std::env::remove_var("SAP_T") };
    }

    #[test]
    fn from_raw_enforces_exactly_one_primary() {
        let both = AuthRaw {
            basic: Some(BasicRaw {
                user: "u".into(),
                passwd: "p".into(),
            }),
            sso2: Some(Sso2Raw {
                ticket: "t".into(),
            }),
            plugin: None,
            oauth: None,
            snc: None,
        };
        let err = AuthConfig::from_raw(&both, "auth").unwrap_err();
        assert!(err.contains("互斥"), "got: {err}");

        let none = AuthRaw {
            basic: None,
            sso2: None,
            plugin: None,
            oauth: None,
            snc: Some(SncRaw {
                qop: Some("8".into()),
                myname: None,
                partnername: None,
                lib: None,
            }),
        };
        let err = AuthConfig::from_raw(&none, "auth").unwrap_err();
        assert!(err.contains("缺主通道"), "got: {err}");
    }

    #[test]
    fn from_raw_sso2_with_snc_overlay() {
        let raw = AuthRaw {
            basic: None,
            sso2: Some(Sso2Raw {
                ticket: "${SAP_T}".into(),
            }),
            plugin: None,
            oauth: None,
            snc: Some(SncRaw {
                qop: Some("8".into()),
                myname: None,
                partnername: Some("p:CN=A4H".into()),
                lib: Some("gsskrb5".into()),
            }),
        };
        unsafe { std::env::set_var("SAP_T", "ticket-value") };
        let cfg = AuthConfig::from_raw(&raw, "auth").unwrap();
        assert_eq!(cfg.mode(), "sso2+snc");
        let mut params = cfg.primary_conn_params();
        params.extend(cfg.snc.as_ref().unwrap().to_conn_params());
        assert!(params.iter().any(|(k, v)| *k == "MYSAPSSO2" && v == "ticket-value"));
        assert!(params.iter().any(|(k, _)| *k == "SNC_QOP"));
        assert!(params.iter().any(|(k, v)| *k == "SNC_PARTNERNAME" && v == "p:CN=A4H"));
        unsafe { std::env::remove_var("SAP_T") };
    }

    #[test]
    fn from_raw_plugin_requires_absolute_command() {
        let raw = AuthRaw {
            basic: None,
            sso2: None,
            plugin: Some(PluginRaw {
                command: "relative-helper".into(),
                params: Default::default(),
                cache_secs: None,
            }),
            oauth: None,
            snc: None,
        };
        let err = AuthConfig::from_raw(&raw, "auth").unwrap_err();
        assert!(err.contains("绝对路径"), "got: {err}");
    }

    #[test]
    fn build_spec_sso2_has_no_user_key() {
        let raw = mk_raw(AuthRaw {
            basic: None,
            sso2: Some(Sso2Raw {
                ticket: "${SAP_T}".into(),
            }),
            plugin: None,
            oauth: None,
            snc: None,
        });
        unsafe { std::env::set_var("SAP_T", "ticket-value") };
        let spec = build_spec("sso", &raw).expect("sso2 spec");
        assert!(spec
            .conn_params
            .iter()
            .any(|(k, v)| *k == "MYSAPSSO2" && v == "ticket-value"));
        assert!(!spec.conn_params.iter().any(|(k, _)| *k == "USER"));
        assert_eq!(spec.auth.mode(), "sso2");
        unsafe { std::env::remove_var("SAP_T") };
    }

    #[test]
    fn build_spec_requires_auth_block() {
        let raw = mk_raw(AuthRaw {
            basic: None,
            sso2: None,
            plugin: None,
            oauth: None,
            snc: None,
        });
        // 无主通道 → 报缺主通道（auth 块存在但空）
        let err = build_spec("noauth", &raw).unwrap_err();
        assert!(err.contains("缺主通道"), "got: {err}");
        // 整块缺失 → 报缺 auth 配置
        let mut raw2 = raw;
        raw2.auth = None;
        let err = build_spec("noauth2", &raw2).unwrap_err();
        assert!(err.contains("缺少 auth 配置"), "got: {err}");
    }

    #[test]
    fn build_spec_conn_auth_key_conflict_rejected() {
        let raw = mk_raw(AuthRaw {
            basic: Some(BasicRaw {
                user: "u".into(),
                passwd: "p".into(),
            }),
            sso2: None,
            plugin: None,
            oauth: None,
            snc: None,
        });
        let mut with_conn = raw.clone();
        with_conn.conn.insert("MYSAPSSO2".into(), "other".into());
        let err = build_spec("conflict", &with_conn).unwrap_err();
        assert!(err.contains("鉴权键"), "got: {err}");
    }

    #[test]
    fn adt_auth_override_beats_inheritance() {
        let mut raw = mk_raw(AuthRaw {
            basic: Some(BasicRaw {
                user: "u".into(),
                passwd: "p".into(),
            }),
            sso2: None,
            plugin: None,
            oauth: None,
            snc: None,
        });
        raw.ashost = Some("h".into());
        raw.adt = Some(AdtRaw {
            base_url: None,
            auth: Some(AuthRaw {
                basic: None,
                sso2: Some(Sso2Raw {
                    ticket: "adt-ticket".into(),
                }),
                plugin: None,
                oauth: None,
                snc: None,
            }),
            tls: None,
        });
        let spec = build_spec("ovr", &raw).unwrap();
        let adt = spec.adt.as_ref().unwrap();
        assert!(adt.auth.is_some());
        assert!(adt
            .auth
            .as_ref()
            .unwrap()
            .primary
            .basic_ref()
            .is_none());
        // 继承路径：无 adt.auth 时 build_adt 应可用（basic）
        let inherit = build_spec("inherit", &raw).unwrap();
        assert!(build_adt(&inherit, Duration::from_secs(5)).is_some());
    }
}
