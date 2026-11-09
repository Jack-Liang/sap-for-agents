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
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// setup 写入开关（main 启动期从 `SAP_SETUP` 写入；默认开）。
/// AtomicBool 而非 OnceLock：路由级测试需要按用例翻转（OnceLock 一次即定）。
static SETUP_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// `SAP_SETUP=force`：跳过"配置完成后锁定"，任何时刻都允许引导写入。
static SETUP_FORCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 未配置模式下是否自动在浏览器打开引导页 `/setup`（main 启动期写入：
/// 未配置 && `SAP_OPEN_BROWSER` 未关；server::run 监听成功后触发一次）。
static AUTO_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 启动期写入开关（main 调一次）。
pub fn init(enabled: bool, force: bool) {
    SETUP_ENABLED.store(enabled, std::sync::atomic::Ordering::SeqCst);
    SETUP_FORCE.store(force, std::sync::atomic::Ordering::SeqCst);
}

/// setup 写入当前是否允许（`/api/version` capabilities.setup 同源）。
pub fn is_enabled() -> bool {
    SETUP_ENABLED.load(std::sync::atomic::Ordering::SeqCst)
}

/// `SAP_SETUP=force` 是否生效（跳过配置后锁定）。
pub fn is_forced() -> bool {
    SETUP_FORCE.load(std::sync::atomic::Ordering::SeqCst)
}

/// setup 写入对**当前部署形态**是否可用 + 不可写原因（GET /api/setup 的
/// `writable` 与 `/api/version` 的 `capabilities.setup` 同源）。
///
/// v0.20 方案 A：取代旧"配置后锁定"——运行期改配置是核心诉求，锁不再按
/// "是否已配置"判定，而按**部署形态**判定（见 [`admin_write_gate`] 的门规则）。
pub fn writable_status(listen_addr: &str) -> (bool, Option<String>) {
    if !is_enabled() {
        (false, Some("SAP_SETUP=off".into()))
    } else if is_forced() || crate::auth::is_enabled() || listen_is_loopback(listen_addr) {
        (true, None)
    } else {
        (
            false,
            Some(format!(
                "网关监听在非回环地址 {listen_addr} 且未启用鉴权（SAP_API_KEY 未设）——\
                 配置写入已锁定；远程访问设 SAP_API_KEY 走 Bearer，本机调试可设 SAP_SETUP=force"
            )),
        )
    }
}

/// 不可写原因的机器键（`GET /api/setup` 的 `disabled_reason_key`；前端
/// 按语言渲染用，人读文本仍在 `disabled_reason`）：`setup_off` /
/// `no_loopback_unauth`；可写时 None。
pub fn writable_reason_key(listen_addr: &str) -> Option<&'static str> {
    if !is_enabled() {
        Some("setup_off")
    } else if is_forced() || crate::auth::is_enabled() || listen_is_loopback(listen_addr) {
        None
    } else {
        Some("no_loopback_unauth")
    }
}

/// 监听/Host 地址的主机部分是否回环（127.0.0.0/8、::1、localhost）。
/// 支持 `host:port` 与 `[v6]:port`；裸 IPv6（无端口）不作为监听/Host 出现。
fn addr_host_is_loopback(addr: &str) -> bool {
    let addr = addr.trim();
    let host = if let Some(rest) = addr.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr)
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// 监听地址是否只绑在回环上（本机专用部署的判定）。
fn listen_is_loopback(listen_addr: &str) -> bool {
    addr_host_is_loopback(listen_addr)
}

/// 请求 Host 头的主机部分是否回环——DNS rebinding 防御：恶意页把自己的
/// 域名解析到 127.0.0.1 后，浏览器视角下对其网关的请求是"同源"，可携带
/// 任意头与 JSON 体，但 Host 头仍暴露真实访问名，非回环即拒。
fn host_header_is_loopback(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(addr_host_is_loopback)
        .unwrap_or(false)
}

/// setup 管理写入（增/删/改系统、切默认）的安全门。
///
/// - `SAP_SETUP=off`：一切写入禁用（403 `SETUP_DISABLED`）；
/// - `SAP_SETUP=force`：全放行（部署者明确自担风险）；
/// - 启用鉴权（SAP_API_KEY）：`require_api_key` 中间件已验过 Bearer，放行；
/// - 未鉴权：仅当**监听地址**为回环且请求 Host 也是回环时放行。
///
/// 为什么不需要 CSRF token：写端点全部要求 `Content-Type: application/json`
/// （axum Json 提取器强制）或非简单方法（PUT/DELETE）——浏览器跨站表单发不
/// 出 JSON，无预检 fetch 发不出非简单方法，而预检请求网关不回 CORS 头即被
/// 拦截。唯一剩下的浏览器向量为 DNS rebinding，由 Host 回环校验挡住。
pub(crate) fn admin_write_gate(
    state: &SharedState,
    headers: &axum::http::HeaderMap,
) -> Result<(), RfcError> {
    if !is_enabled() {
        return Err(RfcError {
            code: -1,
            status: 403,
            message: "引导式配置已禁用（SAP_SETUP=off）：请手工编辑 sap_systems.toml 后重启".into(),
            key: "SETUP_DISABLED".into(),
        });
    }
    if is_forced() || crate::auth::is_enabled() {
        return Ok(());
    }
    let locked = |detail: String| RfcError {
        code: -1,
        status: 403,
        message: format!(
            "配置写入已锁定：{}（无鉴权部署的安全门：仅本机回环访问可写；\
             远程访问设 SAP_API_KEY 走 Bearer，本机调试可设 SAP_SETUP=force）",
            detail
        ),
        key: "SETUP_LOCKED".into(),
    };
    if !listen_is_loopback(&state.cfg().listen_addr) {
        return Err(locked(format!(
            "网关监听在非回环地址 {} 且未启用鉴权",
            state.cfg().listen_addr
        )));
    }
    if !host_header_is_loopback(headers) {
        return Err(locked(
            "请求 Host 不是本机回环地址（疑似 DNS rebinding / 跨主机访问）".into(),
        ));
    }
    Ok(())
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
    let (writable, disabled_reason) = writable_status(&cfg.listen_addr);
    Json(serde_json::json!({
        "configured": !names.is_empty(),
        "default": state.default().map(|d| d.name.clone()),
        "systems_count": names.len(),
        "systems": names,
        "config_file": cfg.systems_file.display().to_string(),
        "config_file_exists": cfg.systems_file.exists(),
        // 环境变量合成路径提示：toml 首写时 env 系统会一并写入（见模块注释）
        "env_synth_will_be_persisted": crate::systems::synth_env_spec(cfg).is_some(),
        "writable": writable,
        "disabled_reason": disabled_reason,
        "disabled_reason_key": writable_reason_key(&cfg.listen_addr),
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
                 （先试连后落盘，成功即热加载，无需重启）。运行期增删改系统/切默认：\
                 PUT|DELETE /api/setup/system/{name}、PUT /api/setup/default（可写时同样免重启）。\
                 密码建议让用户提供环境变量引用（${VAR} 形式，插值规则同 toml），避免明文出现在对话里。\
                 人类配置台（含可复制的 agent 交接文字）：/setup",
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

/// 试连一个候选系统句柄并登记 SYSID：真实打开一条 RFC 连接读系统信息
/// （账号/密码/网络一次验真）；与其他**后端**撞 SYSID（克隆系统）直接拒绝——
/// 存进去也会被隔离（SDK 描述符缓存按 SYSID 分区，见 systems.rs）。
/// 新增（`add_handler`）与改配置（`update_handler`）共用；调用点都在落盘前，
/// 报错文案里的"未写入"对两条路径同样成立。
async fn test_and_register(
    handle: &Arc<crate::systems::SystemHandle>,
) -> Result<crate::connection::RemoteSystemInfo, RfcError> {
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
    if let Err(owner) = handle.record_sysid(&info.sysid) {
        return Err(RfcError {
            code: -1,
            status: 409,
            message: format!(
                "系统 '{}' 的 SYSID {} 与系统 '{}' 冲突（SDK 描述符缓存按 SYSID 分区，\
                 跨后端同 SYSID 会静默共享错误签名；未写入）",
                handle.name, info.sysid, owner
            ),
            key: "SYSTEM_SYSID_CONFLICT".into(),
        });
    }
    tracing::info!(
        system = %handle.name,
        sysid = %info.sysid,
        release = %info.release,
        "setup 连接测试成功"
    );
    Ok(info)
}

/// `POST /api/setup/system`：引导写入一个 SAP 系统并热加载。
pub async fn add_handler(
    axum::extract::State(state): axum::extract::State<SharedState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SetupSystemRequest>,
) -> Result<Json<serde_json::Value>, RfcError> {
    admin_write_gate(&state, &headers)?;

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

    // 3. 组装运行句柄（lazy 池，不触 FFI，构造不可失败）+ 决定默认归属
    let handle = crate::systems::make_handle(&spec, state.cfg());
    // 与 add_system 的提升规则保持一致：显式 default 或当前无默认
    let will_be_default = body.default.unwrap_or(false) || state.default().is_none();
    let had_default = state.default().is_some();

    // 4. 试连（真实打开一条 RFC 连接读系统信息——账号/密码/网络一次验真）
    let mut tested_info: Option<crate::connection::RemoteSystemInfo> = None;
    if body.test.unwrap_or(true) {
        tested_info = Some(test_and_register(&handle).await?);
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
// 运行期配置管理（v0.20 方案 A）：读取 / 编辑 / 删除系统 + 切默认
// ========================================================================

/// `GET /api/setup/system/{name}`：读取一个系统的原始配置（编辑预填用）。
/// **密码永不回显**——只给引用形态（`${VAR}`）或"已设置"标志。
pub async fn get_system_handler(
    axum::extract::State(state): axum::extract::State<SharedState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, RfcError> {
    // 读取与写入同门：配置面（含掩码后的结构信息）不对非本机/未鉴权方暴露
    admin_write_gate(&state, &headers)?;
    let raw = read_system_raw(state.cfg(), &name).map_err(|e| RfcError {
        code: -1,
        status: 500,
        message: format!("读取系统 '{}' 配置失败: {}", name, e),
        key: "SETUP_READ_FAILED".into(),
    })?;
    let Some(raw) = raw else {
        return Err(system_unknown(&state, &name));
    };
    let from_toml = state.cfg().systems_file.exists();
    Ok(Json(system_raw_json(&name, &raw, &state, from_toml)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSystemRequest {
    /// 半更新语义：未提供的字段保持原值（密码不回显，留空 = 不改密码）。
    ashost: Option<String>,
    sysnr: Option<String>,
    client: Option<String>,
    user: Option<String>,
    passwd: Option<String>,
    lang: Option<String>,
    default: Option<bool>,
    /// 改后是否真实试连（默认 true）
    test: Option<bool>,
    pool_size: Option<usize>,
    idle_validate_secs: Option<u64>,
    request_timeout_secs: Option<u64>,
    adt_base_url: Option<String>,
    adt_user: Option<String>,
    adt_passwd: Option<String>,
    read_only: Option<bool>,
    /// 连接参数透传表（提供即整体替换原 conn）
    conn: Option<BTreeMap<String, String>>,
}

/// 半更新合并：body 里 Some 的字段覆盖原条目，其余原样保留。
fn merge_update_raw(mut base: RawSystem, body: &UpdateSystemRequest) -> RawSystem {
    if body.ashost.is_some() {
        base.ashost = body.ashost.clone();
    }
    if body.sysnr.is_some() {
        base.sysnr = body.sysnr.clone();
    }
    if body.client.is_some() {
        base.client = body.client.clone();
    }
    if body.user.is_some() {
        base.user = body.user.clone();
    }
    if body.passwd.is_some() {
        base.passwd = body.passwd.clone();
    }
    if body.lang.is_some() {
        base.lang = body.lang.clone();
    }
    if let Some(d) = body.default {
        base.default = d;
    }
    if body.pool_size.is_some() {
        base.pool_size = body.pool_size;
    }
    if body.idle_validate_secs.is_some() {
        base.idle_validate_secs = body.idle_validate_secs;
    }
    if body.request_timeout_secs.is_some() {
        base.request_timeout_secs = body.request_timeout_secs;
    }
    if body.adt_base_url.is_some() {
        base.adt_base_url = body.adt_base_url.clone();
    }
    if body.adt_user.is_some() {
        base.adt_user = body.adt_user.clone();
    }
    if body.adt_passwd.is_some() {
        base.adt_passwd = body.adt_passwd.clone();
    }
    if body.read_only.is_some() {
        base.read_only = body.read_only;
    }
    if let Some(c) = &body.conn {
        base.conn = c.clone();
    }
    base
}

/// `PUT /api/setup/system/{name}`：修改一个系统的连接配置并热替换。
///
/// - **不支持改名**（名字是 URL 段 + toml 键 + 审计字段的三重标识，改名等于
///   删除重建）；其余字段半更新：未提供的保持原值（改密码只传 `passwd`）。
/// - 流程与新增同链路：合并 → build_spec 校验 → 试连（可 `test:false` 跳过）
///   → 落盘（toml 条目原地替换）→ 热替换句柄（旧池由在途请求自然排空）。
pub async fn update_handler(
    axum::extract::State(state): axum::extract::State<SharedState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<UpdateSystemRequest>,
) -> Result<Json<serde_json::Value>, RfcError> {
    admin_write_gate(&state, &headers)?;
    if state.resolve(&name).is_none() {
        return Err(system_unknown(&state, &name));
    }
    if body.passwd.as_ref().is_some_and(|p| p.trim().is_empty()) {
        return Err(RfcError {
            code: -1,
            status: 400,
            message:
                "passwd 不能为空串——不传该字段即保持原密码（改回环境变量引用则直接传 \"${VAR}\"）"
                    .into(),
            key: "SETUP_PARAM_INVALID".into(),
        });
    }
    let Some(existing) = read_system_raw(state.cfg(), &name).map_err(|e| RfcError {
        code: -1,
        status: 500,
        message: format!("读取系统 '{}' 原配置失败（未写入）: {}", name, e),
        key: "SETUP_READ_FAILED".into(),
    })?
    else {
        // 运行态有、配置源没有：状态不一致（toml 被外部删了?）——拒绝盲改
        return Err(RfcError {
            code: -1,
            status: 409,
            message: format!(
                "系统 '{}' 在运行态存在但配置源缺失（sap_systems.toml 被外部改动?），\
                 拒绝半更新合并；请删除后重建该系统",
                name
            ),
            key: "SETUP_STATE_INCONSISTENT".into(),
        });
    };
    let will_be_default = body.default.unwrap_or(existing.default);
    let raw = merge_update_raw(existing, &body);

    // 1. 校验（与新增同一套 build_spec）
    let spec = crate::systems::build_spec(&name, &raw).map_err(|e| RfcError {
        code: -1,
        status: 400,
        message: format!("系统定义不合法（未写入）: {}", e),
        key: "SETUP_PARAM_INVALID".into(),
    })?;

    // 2. 组装新句柄（lazy 池不触 FFI）+ 试连
    let handle = crate::systems::make_handle(&spec, state.cfg());
    let mut tested_info: Option<crate::connection::RemoteSystemInfo> = None;
    if body.test.unwrap_or(true) {
        tested_info = Some(test_and_register(&handle).await?);
    } else {
        tracing::warn!(system = %name, "setup 编辑跳过连接测试（test:false，配置未验证可达性）");
    }

    // 3. 落盘（toml 条目原地替换，互斥防并发；失败则不动运行态）
    let cfg = state.cfg();
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

    // 4. 热替换句柄（旧池由在途请求的 Arc 引用维持到归还）
    if !state.replace_system(&name, Arc::clone(&handle), will_be_default) {
        return Err(RfcError {
            code: -1,
            status: 500,
            message: format!(
                "系统 '{}' 的配置已落盘但运行态替换失败（并发变更?）。\
                 GET /api/systems 核对现状，必要时重启网关对齐",
                name
            ),
            key: "SETUP_RACE".into(),
        });
    }

    tracing::info!(
        system = %name,
        file = %cfg.systems_file.display(),
        default = will_be_default,
        tested = tested_info.is_some(),
        "setup：系统配置已修改并热替换"
    );

    Ok(Json(serde_json::json!({
        "saved": true,
        "updated": true,
        "name": name,
        "default": will_be_default,
        "file": cfg.systems_file.display().to_string(),
        "connection": match tested_info {
            Some(info) => serde_json::json!({
                "tested": true, "sysid": info.sysid, "release": info.release,
                "host": info.host, "destination": info.destination,
            }),
            None => serde_json::json!({"tested": false,
                "note": "test:false 跳过试连——配置未验证可达性，调用期首次建连才见真章"}),
        },
        "note": "已热替换，无需重启网关；旧连接由在途请求自然排空",
    })))
}

#[derive(Debug, Deserialize)]
pub struct DeleteQuery {
    /// 删除最后一个系统时必须显式 ?force=true（回到未配置模式：业务端点 503）
    force: Option<bool>,
}

/// `DELETE /api/setup/system/{name}`：删除一个系统并热卸载。
///
/// - 默认系统被删且还有剩余系统 → 自动提升声明序第一个为新默认（toml 同步）；
/// - 删除**最后一个**系统要求 `?force=true`（显式确认回到未配置模式）；
/// - toml 条目同步删除；删空整个文件移除（重启后回到未配置/`.env` 合成模式）；
/// - 系统来自 `.env` 合成（无 toml）时只卸载运行态——重启网关它会重新出现。
pub async fn delete_handler(
    axum::extract::State(state): axum::extract::State<SharedState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<DeleteQuery>,
) -> Result<Json<serde_json::Value>, RfcError> {
    admin_write_gate(&state, &headers)?;
    if state.resolve(&name).is_none() {
        return Err(system_unknown(&state, &name));
    }
    let is_last = state.names().len() == 1;
    if is_last && !q.force.unwrap_or(false) {
        return Err(RfcError {
            code: -1,
            status: 409,
            message: format!(
                "'{}' 是最后一个系统——删除后网关回到未配置模式（业务端点 503 \
                 SYSTEM_NOT_CONFIGURED）。确认请加 ?force=true",
                name
            ),
            key: "SETUP_LAST_SYSTEM".into(),
        });
    }

    let Some((_handle, was_default)) = state.remove_system(&name) else {
        return Err(system_unknown(&state, &name));
    };

    // toml 同步：条目删除；删空整个文件移除
    let file_outcome = remove_system_from_toml(state.cfg(), &name).map_err(|e| RfcError {
        code: -1,
        status: 500,
        message: format!(
            "运行态已卸载但更新 {} 失败: {}",
            state.cfg().systems_file.display(),
            e
        ),
        key: "SETUP_WRITE_FAILED".into(),
    })?;

    // 默认系统被删且还有剩余 → 提升声明序第一个（toml 标志同步）
    let mut default_now: Option<String> = state.default().map(|d| d.name.clone());
    if was_default && default_now.is_none() {
        if let Some(first) = state.names().first().cloned() {
            state.set_default(&first).map_err(|e| RfcError {
                code: -1,
                status: 500,
                message: format!("提升默认系统失败: {}", e),
                key: "SETUP_INTERNAL".into(),
            })?;
            set_default_in_toml(state.cfg(), &first).map_err(|e| RfcError {
                code: -1,
                status: 500,
                message: format!("更新默认标志失败: {}", e),
                key: "SETUP_WRITE_FAILED".into(),
            })?;
            default_now = Some(first);
        }
    }

    let from_env = !state.cfg().systems_file.exists();
    tracing::info!(
        system = %name,
        was_default,
        default_now = default_now.as_deref().unwrap_or("<none>"),
        file_removed = file_outcome.1,
        "setup：系统已删除"
    );

    Ok(Json(serde_json::json!({
        "deleted": true,
        "name": name,
        "was_default": was_default,
        "default": default_now,
        "file": {
            "path": state.cfg().systems_file.display().to_string(),
            "entry_removed": file_outcome.0,
            "file_removed": file_outcome.1,
        },
        "note": if from_env {
            "该系统来自 .env 合成（无 sap_systems.toml）——运行态已卸载，重启网关后会重新出现"
        } else if file_outcome.1 {
            "配置文件已删空并移除——重启网关后回到未配置模式（.env 有连接变量则重新合成）"
        } else {
            "已热卸载，无需重启网关。注册表（registry.json）中该系统的条目保留——重新添加同名系统即可复用"
        },
    })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetDefaultRequest {
    name: String,
}

/// `PUT /api/setup/default`：切换默认系统（无前缀路径的新目标），热生效。
pub async fn set_default_handler(
    axum::extract::State(state): axum::extract::State<SharedState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SetDefaultRequest>,
) -> Result<Json<serde_json::Value>, RfcError> {
    admin_write_gate(&state, &headers)?;
    if state.resolve(&body.name).is_none() {
        return Err(system_unknown(&state, &body.name));
    }
    state.set_default(&body.name).map_err(|e| RfcError {
        code: -1,
        status: 500,
        message: format!("切换默认系统失败: {}", e),
        key: "SETUP_INTERNAL".into(),
    })?;
    let file_updated = set_default_in_toml(state.cfg(), &body.name).map_err(|e| RfcError {
        code: -1,
        status: 500,
        message: format!("更新默认标志失败: {}", e),
        key: "SETUP_WRITE_FAILED".into(),
    })?;
    tracing::info!(system = %body.name, "setup：默认系统已切换");
    Ok(Json(serde_json::json!({
        "saved": true,
        "default": body.name,
        "file_updated": file_updated,
        "note": "已热切换，无需重启网关。无前缀路径（/api/rfc 等）此后打到该系统",
    })))
}

/// 404 SYSTEM_UNKNOWN（与 ScopeService 的未知系统错误同键同形）。
fn system_unknown(state: &SharedState, name: &str) -> RfcError {
    RfcError {
        code: -1,
        status: 404,
        message: format!(
            "系统 '{}' 不存在（可用: {}）",
            name,
            state.names().join(", ")
        ),
        key: "SYSTEM_UNKNOWN".into(),
    }
}

/// 原始系统条目的响应 JSON（编辑预填）。**密码永不回显**：引用形态给
/// `passwd.ref`，其余给 `passwd.set`。`default` 取运行态（隐式默认对 toml
/// 不可见）。
fn system_raw_json(
    name: &str,
    raw: &RawSystem,
    state: &SharedState,
    from_toml: bool,
) -> serde_json::Value {
    let runtime_default = state.default().map(|d| d.name == name).unwrap_or(false);
    let passwd = match &raw.passwd {
        Some(p) if p.contains("${") => serde_json::json!({"ref": p}),
        Some(_) => serde_json::json!({"set": true}),
        None => serde_json::json!({"set": false}),
    };
    serde_json::json!({
        "name": name,
        "default": runtime_default,
        "ashost": raw.ashost,
        "sysnr": raw.sysnr,
        "client": raw.client,
        "user": raw.user,
        "passwd": passwd,
        "lang": raw.lang,
        "pool_size": raw.pool_size,
        "idle_validate_secs": raw.idle_validate_secs,
        "request_timeout_secs": raw.request_timeout_secs,
        "adt_base_url": raw.adt_base_url,
        "adt_user": raw.adt_user,
        "adt_passwd": raw.adt_passwd,
        "read_only": raw.read_only,
        "conn": raw.conn,
        "source": if from_toml { "toml" } else { "env_synthetic" },
    })
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
    crate::config::write_private_file(path, &out)?;
    Ok(persisted_env)
}

/// 读取一个系统的原始 toml 条目（编辑预填/半更新合并用；**不解析密码**）。
/// toml 不存在时回退 `.env` 合成系统的等价 RawSystem（同名才命中）。
fn read_system_raw(
    cfg: &crate::config::AppConfig,
    name: &str,
) -> Result<Option<RawSystem>, String> {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !cfg.systems_file.exists() {
        return Ok(crate::systems::synth_env_spec(cfg)
            .filter(|s| s.name == name)
            .map(|s| spec_to_raw(&s)));
    }
    let content =
        std::fs::read_to_string(&cfg.systems_file).map_err(|e| format!("读取失败: {}", e))?;
    let file: crate::systems::SystemsFile =
        toml::from_str(&content).map_err(|e| format!("解析失败（文件被改坏?）: {}", e))?;
    Ok(file.systems.get(name).cloned())
}

/// resolved [`SystemSpec`] 反解为原始形态（`.env` 合成系统的读取/合并场景；
/// conn 透传键的原始大小写在解析期已丢失——以大写键写回，语义等价）。
fn spec_to_raw(spec: &SystemSpec) -> RawSystem {
    let get = |k: &str| {
        spec.conn_params
            .iter()
            .find(|(pk, _)| *pk == k)
            .map(|(_, v)| v.clone())
    };
    let mut conn = BTreeMap::new();
    for (k, v) in &spec.conn_params {
        if !matches!(
            *k,
            "ASHOST" | "SYSNR" | "CLIENT" | "USER" | "PASSWD" | "LANG"
        ) {
            conn.insert(k.to_string(), v.clone());
        }
    }
    RawSystem {
        ashost: get("ASHOST"),
        sysnr: get("SYSNR"),
        client: get("CLIENT"),
        user: get("USER"),
        // 引用形态优先：`${VAR}` 不落明文的约定在反解路径同样成立
        passwd: spec.passwd_ref.clone().or_else(|| get("PASSWD")),
        lang: get("LANG"),
        default: spec.is_default,
        pool_size: spec.pool_size,
        idle_validate_secs: spec.idle_validate.map(|d| d.as_secs()),
        request_timeout_secs: spec.request_timeout.map(|d| d.as_secs()),
        adt_base_url: spec.adt_base_url.clone(),
        adt_user: spec.adt_user.clone(),
        adt_passwd: spec.adt_passwd.clone(),
        read_only: spec.read_only,
        conn,
    }
}

/// 从 `sap_systems.toml` 删除一个系统条目；删空则整个文件移除（重启后回到
/// 未配置/`.env` 合成模式）。返回 (条目是否删除, 文件是否被整体移除)；
/// 文件不存在或条目不在 → (false, false)。进程内互斥（同 [`FILE_LOCK`]）。
fn remove_system_from_toml(
    cfg: &crate::config::AppConfig,
    name: &str,
) -> Result<(bool, bool), String> {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = &cfg.systems_file;
    if !path.exists() {
        return Ok((false, false));
    }
    let content = std::fs::read_to_string(path).map_err(|e| format!("读取失败: {}", e))?;
    let mut root: toml::Value =
        toml::from_str(&content).map_err(|e| format!("解析失败（文件被改坏?）: {}", e))?;
    let Some(systems) = root.get_mut("systems").and_then(|v| v.as_table_mut()) else {
        return Err("文件没有 [systems] 表".into());
    };
    if systems.remove(name).is_none() {
        return Ok((false, false));
    }
    if systems.is_empty() {
        std::fs::remove_file(path).map_err(|e| format!("移除空配置文件失败: {}", e))?;
        return Ok((true, true));
    }
    let body = toml::to_string_pretty(&root).map_err(|e| format!("序列化失败: {}", e))?;
    crate::config::write_private_file(path, &body)?;
    Ok((true, false))
}

/// 重写 toml 的 `default` 标志：只保留 `name` 的 `default = true`，其余清除。
/// 文件不存在（`.env` 合成运行态恒单系统默认）→ 返回 false 无操作。
/// 返回文件是否被实际改写。
fn set_default_in_toml(cfg: &crate::config::AppConfig, name: &str) -> Result<bool, String> {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = &cfg.systems_file;
    if !path.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(path).map_err(|e| format!("读取失败: {}", e))?;
    let mut root: toml::Value =
        toml::from_str(&content).map_err(|e| format!("解析失败（文件被改坏?）: {}", e))?;
    let Some(systems) = root.get_mut("systems").and_then(|v| v.as_table_mut()) else {
        return Err("文件没有 [systems] 表".into());
    };
    let mut changed = false;
    for (k, v) in systems.iter_mut() {
        let Some(t) = v.as_table_mut() else {
            continue;
        };
        let is_target = k == name;
        let marked = t.get("default").and_then(|d| d.as_bool()).unwrap_or(false);
        if is_target && !marked {
            t.insert("default".into(), toml::Value::Boolean(true));
            changed = true;
        } else if !is_target && marked {
            t.remove("default");
            changed = true;
        }
    }
    if changed {
        let body = toml::to_string_pretty(&root).map_err(|e| format!("序列化失败: {}", e))?;
        crate::config::write_private_file(path, &body)?;
    }
    Ok(changed)
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
        ("lang", "LANG"),
    ] {
        if let Some(v) = get(sdk_key) {
            t.insert(key.into(), toml::Value::from(v));
        }
    }
    // 密码：env 合成系统带 ${VAR} 引用形态（passwd_ref）——引用优先于解析值，
    // 避免 "${VAR} 不落明文" 的约定被打穿（2026-11-05 审计 P1）
    if let Some(v) = spec.passwd_ref.clone().or_else(|| get("PASSWD")) {
        t.insert("passwd".into(), toml::Value::from(v));
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
            drain_secs: 5,
            rate_limit_rps: None,
            adt_base_url: None,
            adt_user: String::new(),
            adt_passwd: String::new(),
            read_only: false,
            registry_file: "test-registry.json".into(),
            runtime_mode: false,
            update_check: false,
            systems_file: systems_file.into(),
            gateway_file: std::env::temp_dir().join("sap-setup-test-gateway.toml"),
            setup_enabled: true,
            setup_force: false,
            open_browser: false,
            lang: "EN".into(),
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

    #[test]
    fn toml_remove_keeps_file_until_empty_then_removes_it() {
        let dir = std::env::temp_dir().join(format!("sap-setup-rm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("systems.toml");
        std::fs::write(
            &path,
            "[systems.a]\nashost=\"h\"\nsysnr=\"00\"\nuser=\"u\"\npasswd=\"p\"\n\
             [systems.b]\nashost=\"h2\"\nsysnr=\"01\"\nuser=\"u\"\npasswd=\"p\"\n",
        )
        .unwrap();
        let cfg = test_cfg(path.to_str().unwrap());

        // 删一个：文件保留、其余条目原样
        let out = remove_system_from_toml(&cfg, "a").unwrap();
        assert_eq!(out, (true, false));
        let content = std::fs::read_to_string(&path).unwrap();
        let file: crate::systems::SystemsFile = toml::from_str(&content).unwrap();
        assert!(file.systems.contains_key("b") && !file.systems.contains_key("a"));

        // 删最后一个：整个文件移除（重启后回到未配置/.env 合成模式）
        let out = remove_system_from_toml(&cfg, "b").unwrap();
        assert_eq!(out, (true, true));
        assert!(!path.exists(), "删空后配置文件应被移除");

        // 文件不存在 / 条目不存在 → (false, false)
        assert_eq!(remove_system_from_toml(&cfg, "b").unwrap(), (false, false));
        std::fs::write(
            &path,
            "[systems.a]\nashost=\"h\"\nsysnr=\"00\"\nuser=\"u\"\npasswd=\"p\"\n",
        )
        .unwrap();
        assert_eq!(
            remove_system_from_toml(&cfg, "ghost").unwrap(),
            (false, false)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn toml_set_default_rewrites_flags_exactly_one() {
        let dir = std::env::temp_dir().join(format!("sap-setup-def-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("systems.toml");
        std::fs::write(
            &path,
            "[systems.a]\nashost=\"h\"\nsysnr=\"00\"\nuser=\"u\"\npasswd=\"p\"\n\
             [systems.b]\nashost=\"h2\"\nsysnr=\"01\"\nuser=\"u\"\npasswd=\"p\"\ndefault=true\n",
        )
        .unwrap();
        let cfg = test_cfg(path.to_str().unwrap());

        // b(default) → a：标志互换
        let changed = set_default_in_toml(&cfg, "a").unwrap();
        assert!(changed);
        let file: crate::systems::SystemsFile =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(file.systems["a"].default && !file.systems["b"].default);

        // 已是默认：幂等，文件不动
        assert!(!set_default_in_toml(&cfg, "a").unwrap());

        // 隐式默认（无标志）的单系统：补标志
        let path2 = dir.join("single.toml");
        std::fs::write(
            &path2,
            "[systems.only]\nashost=\"h\"\nsysnr=\"00\"\nuser=\"u\"\npasswd=\"p\"\n",
        )
        .unwrap();
        let cfg2 = test_cfg(path2.to_str().unwrap());
        assert!(set_default_in_toml(&cfg2, "only").unwrap());
        let file: crate::systems::SystemsFile =
            toml::from_str(&std::fs::read_to_string(&path2).unwrap()).unwrap();
        assert!(file.systems["only"].default);

        // 文件不存在 → 无操作
        let cfg3 = test_cfg(dir.join("nope.toml").to_str().unwrap());
        assert!(!set_default_in_toml(&cfg3, "x").unwrap());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }

    #[test]
    fn addr_host_loopback_parsing() {
        assert!(super::addr_host_is_loopback("127.0.0.1:3000"));
        assert!(super::addr_host_is_loopback("localhost:3000"));
        assert!(super::addr_host_is_loopback("LOCALHOST:3000"));
        assert!(super::addr_host_is_loopback("[::1]:3000"));
        assert!(super::addr_host_is_loopback("127.8.8.8:9999"));
        // 非回环
        assert!(!super::addr_host_is_loopback("0.0.0.0:3000"));
        assert!(!super::addr_host_is_loopback("10.0.0.5:3000"));
        assert!(!super::addr_host_is_loopback("evil.example.com:3000"));
        assert!(!super::addr_host_is_loopback("192.168.1.2:3000"));
    }

    #[test]
    fn merge_update_keeps_unspecified_fields() {
        let _g = lock_and_set_pw();
        let existing = sample_raw(); // a4h 形态：ashost/sysnr/client/user/passwd(${VAR})
        let body_json = serde_json::json!({"user": "newuser"});
        let body: UpdateSystemRequest = serde_json::from_value(body_json).unwrap();
        let merged = merge_update_raw(existing, &body);
        assert_eq!(merged.user.as_deref(), Some("newuser"));
        assert_eq!(
            merged.ashost.as_deref(),
            Some("vhcala4h"),
            "未提供的字段保持原值"
        );
        assert_eq!(
            merged.passwd.as_deref(),
            Some(format!("${{{}}}", TEST_PW_VAR).as_str()),
            "密码未提供即保持"
        );

        // conn 整体替换 + default 翻转
        let body2: UpdateSystemRequest = serde_json::from_value(serde_json::json!({
            "conn": {"MSHOST": "m"},
            "default": true,
        }))
        .unwrap();
        let merged2 = merge_update_raw(sample_raw(), &body2);
        assert_eq!(merged2.conn.get("MSHOST").map(String::as_str), Some("m"));
        assert!(merged2.default);
        assert_eq!(merged2.ashost.as_deref(), Some("vhcala4h"));
    }
}
