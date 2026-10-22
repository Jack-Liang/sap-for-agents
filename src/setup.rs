//! 引导式配置（setup）：首次启动零配置时，Agent 通过 HTTP 引导用户完成配置。
//!
//! 解决的问题：新部署（docker run / cargo run）在写好 `sap_systems.toml` 或
//! `.env` 之前，网关旧版直接启动失败——用户面对的是终端报错而非可交互的服务。
//! 现在无配置也能启动（未配置模式，见 systems.rs），Agent 的会话流程变成：
//!
//! ```text
//! GET  /api/setup            → 未配置？需要问用户哪些字段（字段清单内置）
//! （Agent 向用户提问：主机/系统号/客户端/账号/密码/系统名）
//! POST /api/setup/system     → 校验 → 试连（真实打开一条 RFC 连接取系统信息）
//!                              → SYSID 登记 → 写 sap_systems.toml → 热加载
//! GET  /api/systems          → 确认系统已上线（无需重启网关）
//! ```
//!
//! 设计要点：
//! - **写前试连**：连接打不开就不落盘——错误配置永远不会进入 toml；
//!   `test:false` 例外（离线预配置场景），风险自担并显式回显 `tested:false`。
//! - **两条路径一套校验**：请求体转成与 toml 同构的 [`RawSystem`] 走
//!   `build_spec`（插值/命名/连接形态校验与启动期解析完全一致）。
//! - **落盘保守**：已有 toml 解析成 `toml::Value` 原地合并（未知字段保留），
//!   新文件带注释头；写入期间进程内互斥防并发读改写竞争。
//! - **环境变量合成的存量系统**：首写 toml 时把 env 合成的 `default` 系统
//!   一并写入（否则重启后它悄无声息消失——toml 存在时 env 被忽略）。
//! - **开关**：`SAP_SETUP=off` 禁用写入（GET 仍可看状态）；GET 公开免鉴权，
//!   POST 与其余 /api 同栈（鉴权/限流/runtime 守卫）。
//! - 密码只进文件不进日志；`GET /api/setup` 永不回显任何已存系统的凭据。
//! - **人类入口 `/setup`**（server.rs 的双语引导页）：双击运行/零配置启动时
//!   自动在浏览器打开，页面提供「复制给 AI 助手」的交接文字（地址自动
//!   填入），agent 收到即可按上面的流程接手。

use crate::error::RfcError;
use crate::server::SharedState;
use crate::systems::{RawSystem, SystemSpec};
use axum::Json;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// setup 写入开关（main 启动期从 `SAP_SETUP` 写入；默认开）。
static SETUP_ENABLED: OnceLock<bool> = OnceLock::new();

/// 未配置模式下是否自动在浏览器打开引导页 `/setup`（main 启动期写入：
/// 未配置 && `SAP_OPEN_BROWSER` 未关；server::run 监听成功后触发一次）。
static AUTO_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 启动期写入开关（main 调一次）。
pub fn init(enabled: bool) {
    let _ = SETUP_ENABLED.set(enabled);
}

/// setup 写入当前是否允许（`/api/version` capabilities.setup 同源）。
pub fn is_enabled() -> bool {
    *SETUP_ENABLED.get().unwrap_or(&true)
}

/// 启动期写入「自动打开引导页」开关（main 调一次）。
pub fn init_auto_open(enabled: bool) {
    AUTO_OPEN.store(enabled, std::sync::atomic::Ordering::SeqCst);
}

/// 是否应自动打开引导页（server::run 在监听成功后查询）。
pub fn should_auto_open() -> bool {
    AUTO_OPEN.load(std::sync::atomic::Ordering::SeqCst)
}

/// 连接测试超时：试连打不开通常秒级失败，这里只兜底网络黑洞。
const TEST_TIMEOUT: Duration = Duration::from_secs(15);

/// sap_systems.toml 读改写的进程内互斥（防两个并发 setup 丢更新）。
static FILE_LOCK: Mutex<()> = Mutex::new(());

// ========================================================================
// GET /api/setup —— 引导状态 + 字段清单（公开免鉴权）
// ========================================================================

/// `GET /api/setup`：Agent 会话第一步的引导自描述——是否已配置、配置文件
/// 在哪、写入是否允许、需要向用户采集哪些字段（含示例值与连接形态说明）。
pub async fn status_handler(
    axum::extract::State(state): axum::extract::State<SharedState>,
) -> Json<serde_json::Value> {
    let names = state.names();
    let cfg = state.cfg();
    let enabled = is_enabled();
    Json(serde_json::json!({
        "configured": !names.is_empty(),
        "default": state.default().map(|d| d.name.clone()),
        "systems_count": names.len(),
        "systems": names,
        "config_file": cfg.systems_file.display().to_string(),
        "config_file_exists": cfg.systems_file.exists(),
        // 环境变量合成路径提示：toml 首写时 env 系统会一并写入（见模块注释）
        "env_synth_will_be_persisted": crate::systems::synth_env_spec(cfg).is_some(),
        "writable": enabled,
        "disabled_reason": if enabled { None } else { Some("SAP_SETUP=off") },
        "fields": field_schema(),
        "connection_forms": {
            "direct": "ashost + sysnr（+ client）——直连单个应用服务器",
            "load_balanced": "conn.MSHOST（+ conn.R3NAME / conn.GROUP）——消息服务器负载均衡，此形态不需要 ashost/sysnr",
        },
        "example": {
            "name": "a4h",
            "ashost": "vhcala4h",
            "sysnr": "00",
            "client": "001",
            "user": "DEVELOPER",
            "passwd": "${SAP_A4H_PASSWD}",
            "lang": "EN",
            "default": true,
        },
        "note": "未配置时 Agent 引导用户：按 fields 清单向用户采集 → POST /api/setup/system \
                 （先试连后落盘，成功即热加载，无需重启）。密码建议让用户提供环境变量引用 \
                 （${VAR} 形式，插值规则同 toml），避免明文出现在对话里。\
                 人类引导页（含可复制的 agent 交接文字）：/setup",
    }))
}

/// Agent 采访用户的字段清单（GET /api/setup 的 `fields` 块数据源）。
fn field_schema() -> serde_json::Value {
    serde_json::json!([
        {"name": "name", "required": true,
         "description": "系统名：URL 段 /api/s/{name}/... 的标识，1-32 位小写字母/数字/-/_（如 a4h、prd）",
         "example": "a4h"},
        {"name": "ashost", "required": "直连形态必填（或改用 conn.MSHOST 负载均衡形态）",
         "description": "SAP 应用服务器主机名/IP（直连形态）", "example": "vhcala4h"},
        {"name": "sysnr", "required": "直连形态必填",
         "description": "系统号（两位数字，SAP 管理员提供，常为 00）", "example": "00"},
        {"name": "client", "required": false,
         "description": "登录客户端号（三位数字，如开发 001 / 生产 100）", "example": "001"},
        {"name": "user", "required": true,
         "description": "SAP 登录账号（建议专用通信账号，非个人账号）", "example": "DEVELOPER"},
        {"name": "passwd", "required": true,
         "description": "SAP 登录密码；支持 ${VAR} 环境变量引用（推荐，避免明文落盘/入对话）",
         "example": "${SAP_A4H_PASSWD}"},
        {"name": "lang", "required": false,
         "description": "登录语言（默认 EN）", "example": "EN"},
        {"name": "default", "required": false,
         "description": "是否设为默认系统（无前缀路径的目标）。首个系统无论如何都会成为默认",
         "example": true},
        {"name": "conn", "required": false,
         "description": "自由连接参数表（键大写化直通 SDK）：负载均衡形态在此给 MSHOST/R3NAME/GROUP",
         "example": {"MSHOST": "msg.example.com", "GROUP": "SPACE"}},
        {"name": "read_only", "required": false,
         "description": "该系统只读（写端点 403），默认跟随全局 SAP_READ_ONLY", "example": false},
        {"name": "test", "required": false,
         "description": "写入前是否真实试连（默认 true；false = 离线预配置，落盘但不验证可达性）",
         "example": true},
    ])
}

// ========================================================================
// POST /api/setup/system —— 采集 → 校验 → 试连 → 落盘 → 热加载
// ========================================================================

/// setup 写入请求体。字段与 `sap_systems.toml` 的 `[systems.{name}]` 同构
/// （`deny_unknown_fields` 拦拼写错误——和 /api/invokes 同哲学）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupSystemRequest {
    name: String,
    ashost: Option<String>,
    sysnr: Option<String>,
    client: Option<String>,
    user: Option<String>,
    passwd: Option<String>,
    lang: Option<String>,
    /// 是否设为默认系统（首个系统无视此值、必然默认）
    default: Option<bool>,
    /// 写入前是否真实试连（默认 true）
    test: Option<bool>,
    pool_size: Option<usize>,
    idle_validate_secs: Option<u64>,
    request_timeout_secs: Option<u64>,
    adt_base_url: Option<String>,
    adt_user: Option<String>,
    adt_passwd: Option<String>,
    read_only: Option<bool>,
    conn: Option<BTreeMap<String, String>>,
}

impl SetupSystemRequest {
    /// 转成与 toml 同构的原始形态（保留原值——`${VAR}` 不展开落盘）。
    fn to_raw(&self) -> RawSystem {
        RawSystem {
            ashost: self.ashost.clone(),
            sysnr: self.sysnr.clone(),
            client: self.client.clone(),
            user: self.user.clone(),
            passwd: self.passwd.clone(),
            lang: self.lang.clone(),
            default: self.default.unwrap_or(false),
            pool_size: self.pool_size,
            idle_validate_secs: self.idle_validate_secs,
            request_timeout_secs: self.request_timeout_secs,
            adt_base_url: self.adt_base_url.clone(),
            adt_user: self.adt_user.clone(),
            adt_passwd: self.adt_passwd.clone(),
            read_only: self.read_only,
            conn: self.conn.clone().unwrap_or_default(),
        }
    }
}

/// `POST /api/setup/system`：引导写入一个 SAP 系统并热加载。
pub async fn add_handler(
    axum::extract::State(state): axum::extract::State<SharedState>,
    Json(body): Json<SetupSystemRequest>,
) -> Result<Json<serde_json::Value>, RfcError> {
    if !is_enabled() {
        return Err(RfcError {
            code: -1,
            status: 403,
            message: "引导式配置已禁用（SAP_SETUP=off）：请手工编辑 sap_systems.toml 后重启".into(),
            key: "SETUP_DISABLED".into(),
        });
    }

    // 1. 校验（与启动期 toml 解析同一套 build_spec：命名/插值/连接形态）
    let raw = body.to_raw();
    let spec = crate::systems::build_spec(&body.name, &raw).map_err(|e| RfcError {
        code: -1,
        status: 400,
        message: format!("系统定义不合法（未写入）: {}", e),
        key: "SETUP_PARAM_INVALID".into(),
    })?;

    // 2. 同名冲突（运行态与配置文件都查）
    if state.resolve(&spec.name).is_some() {
        return Err(RfcError {
            code: -1,
            status: 409,
            message: format!(
                "系统名 '{}' 已存在（可用: {}）。换一个名字，或手工编辑 {} 后重启",
                spec.name,
                state.names().join(", "),
                state.cfg().systems_file.display()
            ),
            key: "SETUP_NAME_EXISTS".into(),
        });
    }

    // 3. 组装运行句柄（lazy 池，不触 FFI）+ 决定默认归属
    let handle = crate::systems::make_handle(&spec, state.cfg(), false).map_err(|e| RfcError {
        code: -1,
        status: 500,
        message: format!("系统句柄组装失败: {}", e),
        key: "SETUP_INTERNAL".into(),
    })?;
    // 与 add_system 的提升规则保持一致：显式 default 或当前无默认
    let will_be_default = body.default.unwrap_or(false) || state.default().is_none();
    let had_default = state.default().is_some();

    // 4. 试连（真实打开一条 RFC 连接读系统信息——账号/密码/网络一次验真）
    let mut tested_info: Option<crate::connection::RemoteSystemInfo> = None;
    if body.test.unwrap_or(true) {
        let pool = Arc::clone(&handle.pool);
        let joined = tokio::time::timeout(
            TEST_TIMEOUT,
            tokio::task::spawn_blocking(move || pool.with_connection(|conn| conn.system_info())),
        )
        .await;
        let info = match joined {
            Err(_) => {
                return Err(RfcError {
                    code: -1,
                    status: 502,
                    message: format!(
                        "连接测试超时（{}s，未写入）。检查 ashost/网络/防火墙后重试",
                        TEST_TIMEOUT.as_secs()
                    ),
                    key: "SETUP_CONNECT_TIMEOUT".into(),
                })
            }
            Ok(Err(join)) => {
                return Err(RfcError {
                    code: -1,
                    status: 502,
                    message: format!("连接测试任务异常（未写入）: {}", join),
                    key: "SETUP_CONNECT_FAILED".into(),
                })
            }
            Ok(Ok(Err(e))) => {
                return Err(RfcError {
                    code: e.code,
                    status: 502,
                    message: format!(
                        "连接测试失败（未写入）: {} [{}]。检查地址/系统号/账号/密码",
                        e.message, e.key
                    ),
                    key: "SETUP_CONNECT_FAILED".into(),
                })
            }
            Ok(Ok(Ok(info))) => info,
        };
        // SYSID 登记：与其他后端撞 SYSID（克隆系统）直接拒绝写入——
        // 存进去也会被隔离（SDK 描述符缓存按 SYSID 分区，见 systems.rs）
        if let Err(owner) = handle.record_sysid(&info.sysid) {
            return Err(RfcError {
                code: -1,
                status: 409,
                message: format!(
                    "系统 '{}' 的 SYSID {} 与系统 '{}' 冲突（SDK 描述符缓存按 SYSID 分区，\
                     跨后端同 SYSID 会静默共享错误签名；未写入）",
                    spec.name, info.sysid, owner
                ),
                key: "SYSTEM_SYSID_CONFLICT".into(),
            });
        }
        tracing::info!(
            system = %spec.name,
            sysid = %info.sysid,
            release = %info.release,
            "setup 连接测试成功"
        );
        tested_info = Some(info);
    } else {
        tracing::warn!(system = %spec.name, "setup 跳过连接测试（test:false，配置未验证可达性）");
    }

    // 5. 落盘（读改写合并，互斥防并发；失败则不动运行态）
    let cfg = state.cfg();
    let file_existed = cfg.systems_file.exists();
    let persisted_env =
        write_system_to_toml(cfg, &spec, &raw, will_be_default).map_err(|e| RfcError {
            code: -1,
            status: 500,
            message: format!(
                "写入 {} 失败（运行态未变）: {}",
                cfg.systems_file.display(),
                e
            ),
            key: "SETUP_WRITE_FAILED".into(),
        })?;

    // 6. 热加载（首个系统补进程级静态：能力自描述/客户端号）
    state.add_system(Arc::clone(&handle), will_be_default);
    if !had_default {
        let client = spec
            .conn_params
            .iter()
            .find(|(k, _)| *k == "CLIENT")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        crate::version::init_sap_client(client);
        crate::adt::init_capability_adt(handle.adt.is_some());
    }

    tracing::info!(
        system = %spec.name,
        file = %cfg.systems_file.display(),
        created_file = !file_existed,
        persisted_env_default = persisted_env,
        "setup：系统已写入配置并热加载"
    );

    Ok(Json(serde_json::json!({
        "saved": true,
        "name": spec.name,
        "default": will_be_default,
        "file": cfg.systems_file.display().to_string(),
        "file_created": !file_existed,
        // 环境变量合成的存量 default 系统一并落盘（否则重启即丢）
        "env_default_persisted": persisted_env,
        "connection": match tested_info {
            Some(info) => serde_json::json!({
                "tested": true, "sysid": info.sysid, "release": info.release,
                "host": info.host, "destination": info.destination,
            }),
            None => serde_json::json!({"tested": false,
                "note": "test:false 跳过试连——配置未验证可达性，调用期首次建连才见真章"}),
        },
        "systems_count": state.names().len(),
        "note": "已热加载，无需重启网关。GET /api/systems 查看；再添系统重复 POST 即可",
    })))
}

// ========================================================================
// toml 落盘
// ========================================================================

/// 把系统写入 `sap_systems.toml`（读改写合并）。
///
/// - 文件已存在 → 解析为 `toml::Value` 原地插入（其他系统的未知字段保留）；
/// - 新建 → 带注释头；若当前是"环境变量合成单系统"运行态，把该系统一并
///   写入（返回 true）——否则首写 toml 后重启，env 系统会静默消失。
/// - `new_is_default` → 其他系统的 `default` 标志清掉（文件与运行态一致）。
fn write_system_to_toml(
    cfg: &crate::config::AppConfig,
    spec: &SystemSpec,
    raw: &RawSystem,
    new_is_default: bool,
) -> Result<bool, String> {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = &cfg.systems_file;

    let (mut root, file_existed) = if path.exists() {
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
    let systems = root_table
        .entry("systems".to_string())
        .or_insert_with(|| toml::Value::Table(Default::default()));
    let systems = systems
        .as_table_mut()
        .ok_or_else(|| "[systems] 不是 toml 表".to_string())?;

    // 环境变量合成的存量系统首写时一并落盘（保持重启前后系统集合一致）
    let mut persisted_env = false;
    if !file_existed {
        if let Some(env_spec) = crate::systems::synth_env_spec(cfg) {
            systems.insert(
                env_spec.name.clone(),
                spec_to_table(&env_spec, !new_is_default),
            );
            persisted_env = true;
        }
    }

    // 新系统成为默认 → 清掉文件里其他系统的 default 标志
    if new_is_default {
        for (_, v) in systems.iter_mut() {
            if let Some(t) = v.as_table_mut() {
                t.remove("default");
            }
        }
    }
    systems.insert(spec.name.clone(), raw_to_table(raw, new_is_default));

    let mut out = String::new();
    if !file_existed {
        out.push_str("# sap_systems.toml —— sap-for-agents 多系统配置\n");
        out.push_str("# 由 setup API 生成（POST /api/setup/system）；手工编辑后重启网关生效。\n");
        out.push_str("# 字段说明见 sap_systems.toml.example 或 README。\n\n");
    }
    let body = toml::to_string_pretty(&root).map_err(|e| format!("序列化失败: {}", e))?;
    out.push_str(&body);
    std::fs::write(path, out).map_err(|e| format!("写文件失败: {}", e))?;
    Ok(persisted_env)
}

/// 请求体的原始字段 → toml 表（保留 `${VAR}` 原样；只写显式提供的键）。
fn raw_to_table(raw: &RawSystem, is_default: bool) -> toml::Value {
    fn put_str(t: &mut toml::map::Map<String, toml::Value>, k: &str, v: &Option<String>) {
        if let Some(v) = v {
            t.insert(k.to_string(), toml::Value::from(v.clone()));
        }
    }
    let mut t = toml::map::Map::new();
    put_str(&mut t, "ashost", &raw.ashost);
    put_str(&mut t, "sysnr", &raw.sysnr);
    put_str(&mut t, "client", &raw.client);
    put_str(&mut t, "user", &raw.user);
    put_str(&mut t, "passwd", &raw.passwd);
    put_str(&mut t, "lang", &raw.lang);
    if let Some(n) = raw.pool_size {
        t.insert("pool_size".into(), toml::Value::Integer(n as i64));
    }
    if let Some(n) = raw.idle_validate_secs {
        t.insert("idle_validate_secs".into(), toml::Value::Integer(n as i64));
    }
    if let Some(n) = raw.request_timeout_secs {
        t.insert(
            "request_timeout_secs".into(),
            toml::Value::Integer(n as i64),
        );
    }
    put_str(&mut t, "adt_base_url", &raw.adt_base_url);
    put_str(&mut t, "adt_user", &raw.adt_user);
    put_str(&mut t, "adt_passwd", &raw.adt_passwd);
    if let Some(b) = raw.read_only {
        t.insert("read_only".into(), toml::Value::Boolean(b));
    }
    if !raw.conn.is_empty() {
        let mut conn = toml::map::Map::new();
        for (k, v) in &raw.conn {
            conn.insert(k.clone(), toml::Value::from(v.clone()));
        }
        t.insert("conn".into(), toml::Value::Table(conn));
    }
    if is_default {
        t.insert("default".into(), toml::Value::Boolean(true));
    }
    toml::Value::Table(t)
}

/// resolved 的 [`SystemSpec`] → toml 表（env 合成系统落盘用；字段从
/// conn_params 反解，ADT 覆盖按原语义写回）。
fn spec_to_table(spec: &SystemSpec, is_default: bool) -> toml::Value {
    let mut t = toml::map::Map::new();
    let get = |k: &str| {
        spec.conn_params
            .iter()
            .find(|(pk, _)| *pk == k)
            .map(|(_, v)| v.clone())
    };
    for (key, sdk_key) in [
        ("ashost", "ASHOST"),
        ("sysnr", "SYSNR"),
        ("client", "CLIENT"),
        ("user", "USER"),
        ("passwd", "PASSWD"),
        ("lang", "LANG"),
    ] {
        if let Some(v) = get(sdk_key) {
            t.insert(key.into(), toml::Value::from(v));
        }
    }
    // adt_base_url 语义：None=按 ashost 推导（不写）、Some("")=禁用（写空串）、
    // Some(url)=显式（写 url）。env 合成路径禁用时是 Some("")，推导时不写。
    match &spec.adt_base_url {
        Some(url) if !url.is_empty() => {
            t.insert("adt_base_url".into(), toml::Value::from(url.clone()));
        }
        Some(_) => {
            t.insert("adt_base_url".into(), toml::Value::from(""));
        }
        None => {}
    }
    if let Some(u) = &spec.adt_user {
        if Some(u.as_str()) != get("USER").as_deref() {
            t.insert("adt_user".into(), toml::Value::from(u.clone()));
        }
    }
    if let Some(p) = &spec.adt_passwd {
        if Some(p.as_str()) != get("PASSWD").as_deref() {
            t.insert("adt_passwd".into(), toml::Value::from(p.clone()));
        }
    }
    if is_default {
        t.insert("default".into(), toml::Value::Boolean(true));
    }
    toml::Value::Table(t)
}

// ========================================================================
// 测试（FFI-free：全部走 lazy/纯函数路径）
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 环境变量是进程全局态，插值测试需要互斥串行（同 systems.rs 的模式）。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 测试用密码：${VAR} 形式（写文件测试同时验证原样落盘不展开）。
    const TEST_PW_VAR: &str = "SAP_SETUP_TEST_PW";

    /// 最小 AppConfig（systems_file 指向临时目录，测试内各自覆盖）。
    fn test_cfg(systems_file: &str) -> crate::config::AppConfig {
        crate::config::AppConfig {
            conn_params: vec![],
            listen_addr: "127.0.0.1:3000".into(),
            pool_size: 2,
            pool_idle_validate: Duration::from_secs(30),
            api_key: None,
            request_timeout: Duration::from_secs(5),
            rate_limit_rps: None,
            adt_base_url: None,
            adt_user: String::new(),
            adt_passwd: String::new(),
            read_only: false,
            registry_file: "test-registry.json".into(),
            runtime_mode: false,
            update_check: false,
            systems_file: systems_file.into(),
            setup_enabled: true,
            open_browser: false,
        }
    }

    fn sample_raw() -> RawSystem {
        RawSystem {
            ashost: Some("vhcala4h".into()),
            sysnr: Some("00".into()),
            client: Some("001".into()),
            user: Some("dev".into()),
            passwd: Some(format!("${{{}}}", TEST_PW_VAR)),
            lang: None,
            default: false,
            pool_size: None,
            idle_validate_secs: None,
            request_timeout_secs: None,
            adt_base_url: None,
            adt_user: None,
            adt_passwd: None,
            read_only: None,
            conn: BTreeMap::new(),
        }
    }

    /// 设插值环境变量并拿互斥锁（build_spec 校验期会真实插值）。
    fn lock_and_set_pw() -> std::sync::MutexGuard<'static, ()> {
        let g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var(TEST_PW_VAR, "s3cret");
        }
        g
    }

    #[test]
    fn write_creates_file_with_comment_header_and_raw_fields() {
        let _g = lock_and_set_pw();
        let dir = std::env::temp_dir().join(format!("sap-setup-t-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("systems.toml");
        let _ = std::fs::remove_file(&path);
        let cfg = test_cfg(path.to_str().unwrap());

        let raw = sample_raw();
        let spec = crate::systems::build_spec("a4h", &raw).unwrap();
        let persisted_env = write_system_to_toml(&cfg, &spec, &raw, true).unwrap();
        assert!(!persisted_env, "空 conn_params 无 env 合成系统可落盘");

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.starts_with("# sap_systems.toml"),
            "新文件应带注释头"
        );
        assert!(content.contains("[systems.a4h]"));
        // ${VAR} 原样落盘（插值只发生在连接时，不固化明文）
        assert!(content.contains(&format!("${{{}}}", TEST_PW_VAR)));
        assert!(content.contains("default = true"));
        // 回读可解析（与启动期解析器同一格式）
        let file: crate::systems::SystemsFile = toml::from_str(&content).unwrap();
        assert!(file.systems.contains_key("a4h"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_merges_into_existing_and_demotes_old_default() {
        let _g = lock_and_set_pw();
        let dir = std::env::temp_dir().join(format!("sap-setup-m-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("systems.toml");
        std::fs::write(
            &path,
            "[systems.old]\nashost=\"h1\"\nsysnr=\"00\"\nuser=\"u\"\npasswd=\"p\"\ndefault=true\n",
        )
        .unwrap();
        let cfg = test_cfg(path.to_str().unwrap());

        let raw = sample_raw();
        let spec = crate::systems::build_spec("new", &raw).unwrap();
        write_system_to_toml(&cfg, &spec, &raw, true).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let file: crate::systems::SystemsFile = toml::from_str(&content).unwrap();
        let old = file.systems.get("old").unwrap();
        assert!(!old.default, "旧默认应被降级");
        assert!(file.systems.get("new").unwrap().default);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_persists_env_synth_default_on_first_write() {
        // env 合成运行态（有 conn_params、无 toml）首写 → default 系统一并落盘
        let _g = lock_and_set_pw();
        let dir = std::env::temp_dir().join(format!("sap-setup-e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("systems.toml");
        let _ = std::fs::remove_file(&path);
        let mut cfg = test_cfg(path.to_str().unwrap());
        cfg.conn_params = vec![
            ("ASHOST", "envhost".into()),
            ("SYSNR", "00".into()),
            ("CLIENT", "100".into()),
            ("USER", "u".into()),
            ("PASSWD", "p".into()),
            ("LANG", "EN".into()),
        ];

        let raw = sample_raw();
        let spec = crate::systems::build_spec("a4h", &raw).unwrap();
        let persisted_env = write_system_to_toml(&cfg, &spec, &raw, true).unwrap();
        assert!(persisted_env);

        let content = std::fs::read_to_string(&path).unwrap();
        let file: crate::systems::SystemsFile = toml::from_str(&content).unwrap();
        assert!(
            file.systems.contains_key("default"),
            "env 系统应落盘为 default"
        );
        // 新系统成为默认 → env 系统降级
        assert!(!file.systems.get("default").unwrap().default);
        assert!(file.systems.get("a4h").unwrap().default);
        let d = file.systems.get("default").unwrap();
        assert_eq!(d.ashost.as_deref(), Some("envhost"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn name_conflicts_and_bad_names_rejected_by_build_spec() {
        let _g = lock_and_set_pw();
        // 同一套校验：非法名（大写）与缺连接目标都过不了 build_spec
        assert!(crate::systems::build_spec("Bad", &sample_raw()).is_err());
        let mut raw = sample_raw();
        raw.ashost = None; // 直连形态缺 ashost → 报错
        assert!(crate::systems::build_spec("ok", &raw).is_err());
        // 负载均衡形态（conn.MSHOST）无需 ashost
        let mut lb = sample_raw();
        lb.ashost = None;
        lb.sysnr = None;
        lb.conn.insert("MSHOST".into(), "msg.example.com".into());
        assert!(crate::systems::build_spec("lb", &lb).is_ok());
    }
}
