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
//!
//! v0.16 起配置**按系统实例化**（[`AdtEndpoint`] 挂在
//! [`crate::systems::SystemHandle`] 上）：不同系统的 ADT 基地址、账号、
//! CSRF 会话各自独立——CSRF token 本就绑定 ADT 会话，跨系统共享必然互踩。

use crate::error::RfcError;
use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, Uri};
use axum::response::Response;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// ADT 端点鉴权配置（构造期解析，运行期只读）。
///
/// - `Basic`：现状默认（user/passwd）；
/// - `Sso2`：SAP 登录票据走 `Cookie: MYSAPSSO2=…`（sapcli 借鉴 B2；票据自带
///   身份，不送 Basic 头）；
/// - `OAuth`：OAuth 2.0 密码授权（BTP/ABAP Cloud ADT 只认这个）——Bearer
///   token 懒取 + 进程内缓存 + 401 强制刷新重试一次；password 可省
///   （缓存里有 refresh token 时），刷新失败回落 password grant。
///   - `grant = authorization_code`（v0.23）：BTP XSUAA `binding-secret` 类
///     credential 只放 client_credentials——trial 的 ABAP 系统拒收这种无用户
///     身份的 token（实测 401）。改走浏览器授权：`POST /api/setup/system/{n}/authorize`
///     拿 authorize_url → 用户登录 → 回调换 token → refresh_token 种子注入 +
///     落盘。运行期只靠 refresh_token grant 静默续期，**刷新失败不回落**
///     （client_credentials 产物必被 ABAP 拒收，静默回落只会把错误变成更难
///     排查的 401），直接 fail-fast 指引重授权。
///
///   mTLS 客户端证书与鉴权**正交**（identity 见 `AdtEndpointOpts`）。
pub(crate) enum AdtAuthSpec {
    Basic {
        user: String,
        passwd: String,
    },
    Sso2 {
        ticket: String,
        user: String,
    },
    /// 外部鉴权插件（懒执行 + TTL 缓存；最高优先级，sapcli 同序）
    Plugin(AuthPluginSpec),
    OAuth {
        /// token 端点基址（实际请求 `{base}/oauth/token`）
        token_url: String,
        client_id: String,
        client_secret: String,
        user: Option<String>,
        passwd: Option<String>,
        grant: OAuthGrant,
    },
}

/// OAuth grant 形态：直取族（password / client_credentials，按 user/passwd
/// 有无派生——旧行为）vs authorization_code 种子族（依赖 refresh_token）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OAuthGrant {
    /// 完整 grant 直取：配了 user/passwd 走 password，否则 client_credentials。
    ClientCredentials,
    /// authorization_code 种子流：refresh_token 由浏览器授权注入，无完整
    /// grant 可回落（见 [`AdtAuthSpec::OAuth`] 文档）。
    AuthorizationCode,
}

impl OAuthGrant {
    /// 从 toml 字符串解析（非法值给出合法名单）。
    pub(crate) fn parse(value: &str, ctx: &str) -> Result<Self, String> {
        match value.trim() {
            "client_credentials" => Ok(OAuthGrant::ClientCredentials),
            "authorization_code" => Ok(OAuthGrant::AuthorizationCode),
            other => Err(format!(
                "{ctx}.oauth.grant 取值 '{other}' 不支持（合法: client_credentials, authorization_code）"
            )),
        }
    }
}

/// OAuth 缓存状态（token + 拿到时刻；提前 60s 视为过期，sapcli 同款余量）
struct OAuthState {
    token_url: String,
    client_id: String,
    client_secret: String,
    user: Option<String>,
    passwd: Option<String>,
    grant: OAuthGrant,
    /// 持久化键（= 系统名；None = 不落盘，测试/工具调用形态）
    state_key: Option<String>,
    token: Mutex<Option<(String, Instant)>>,
    /// refresh token（grant 响应带就存 + authcode 形态落盘；重启后水合回填）
    refresh: Mutex<Option<String>>,
}

const OAUTH_REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// 归一 token 端点：配置的 `token_url` 有两种约定——裸 base
/// （`https://tenant.authentication…`）与带 `/oauth/token` 后缀
/// （AGENTS.md §13 文档形态）。两种都收：已带后缀的剥掉再统一拼，
/// 防止 `…/oauth/token/oauth/token` 这种双重拼接。
pub(crate) fn oauth_token_base(token_url: &str) -> String {
    let base = token_url.trim_end_matches('/');
    base.strip_suffix("/oauth/token").unwrap_or(base).to_string()
}

/// token 端点全 URL（`{base}/oauth/token`，base 见 [`oauth_token_base`]）。
pub(crate) fn oauth_token_endpoint(token_url: &str) -> String {
    format!("{}/oauth/token", oauth_token_base(token_url))
}

/// 外部鉴权插件配置（sapcli auth_plugin 借鉴 B3，网关版收缩到 ADT 通道）。
///
/// 协议：子进程 stdin 收 `{"connection":{...},"parameters":{...}}`，
/// stdout 回 `{"message":str,"content":{...},"cache_secs":num?}`。
/// `content.type ∈ cookie | http_authorization_header`（certificates 形态
/// 网关不支持——用 adt_cert/adt_key 静态配置替代）。
#[derive(Debug, Clone)]
pub(crate) struct AuthPluginSpec {
    /// 插件可执行文件**绝对路径**（常驻服务防 PATH 劫持）
    pub command: String,
    /// 透传参数（原样进请求 JSON 的 parameters）
    pub parameters: std::collections::BTreeMap<String, String>,
    /// 响应缓存秒数（默认 300；响应里的 cache_secs 可覆盖）
    pub cache_ttl: Duration,
}

impl Default for AuthPluginSpec {
    fn default() -> Self {
        Self {
            command: String::new(),
            parameters: Default::default(),
            cache_ttl: Duration::from_secs(300),
        }
    }
}

/// 插件凭证缓存：应用为请求的额外头 + Cookie 串
#[derive(Debug, Clone)]
struct PluginCred {
    /// headers 映射（可含 Authorization）——原样作为请求头注入
    headers: Vec<(String, String)>,
    /// cookies 合成的 Cookie 头值（"k=v; k2=v2"）
    cookies: Option<String>,
    /// 插件自述 message（401 诊断日志透出）
    message: String,
    expires_at: Instant,
}

const PLUGIN_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const PLUGIN_DEFAULT_TTL: Duration = Duration::from_secs(300);
const PLUGIN_MAX_OUTPUT: usize = 1 << 20;

/// 私钥文件的 PEM 加密标记（PKCS#8 与 legacy PKCS#1）
const PEM_ENCRYPTION_MARKERS: [&str; 2] = ["BEGIN ENCRYPTED PRIVATE KEY", "Proc-Type: 4,ENCRYPTED"];

/// 拒收加密私钥（reqwest 的 OpenSSL 报错不可读；给出可执行的修复路径）。
fn check_key_not_encrypted(bytes: &[u8], path: &str) -> Result<(), String> {
    let text = String::from_utf8_lossy(bytes);
    if PEM_ENCRYPTION_MARKERS.iter().any(|m| text.contains(m)) {
        return Err(format!(
            "ADT 客户端私钥是加密的，无法直接使用: {path}\n要么先解密（openssl pkey -in encrypted.key -out plain.key），\n要么走 auth 外置方案后提供解密后的 key 文件"
        ));
    }
    Ok(())
}

/// 单系统的 ADT 端点（基地址 + 认证 + HTTP 客户端 + CSRF 会话缓存）。
pub struct AdtEndpoint {
    /// 基地址，如 `http://localhost:50000`（不带尾斜杠）
    base_url: String,
    /// 鉴权状态（Basic / SSO2 / OAuth）
    auth: AuthState,
    /// 登录用户（对象创建 XML 的 adtcore:responsible 用；大写）
    user: String,
    client: reqwest::Client,
    /// CSRF token 缓存：写操作共用一个会话 token，过期或 403 后刷新
    csrf: Mutex<Option<CsrfToken>>,
}

enum AuthState {
    /// 预组装的 `Basic xxx` Authorization 头
    Basic(String),
    Sso2(String),
    OAuth(OAuthState),
    /// 外部插件（懒执行 + TTL 缓存；401 时失效重跑一次）
    Plugin(std::sync::RwLock<Option<PluginCred>>, AuthPluginSpec),
}

const CSRF_TTL: Duration = Duration::from_secs(30 * 60);

/// CSRF token 缓存条目
struct CsrfToken {
    token: String,
    /// 会话 Cookie（sap-usercontext 等），随 token 一起使用
    cookie: Option<String>,
    fetched: Instant,
}

/// 该系统未启用 ADT 代理时的统一错误（未配置 adt_base_url / SAP_ADT_BASE_URL）。
pub(crate) fn adt_disabled() -> RfcError {
    RfcError {
        code: -1,
        status: 503,
        message: "该系统未启用 ADT 代理（配置 adt_base_url / SAP_ADT_BASE_URL 后重启）".into(),
        key: "ADT_DISABLED".into(),
    }
}

/// `/api/version` capabilities.adt 的数据源（**默认系统**视角）。
/// v0.16 起 ADT 按系统实例化，能力自描述保留进程级布尔——main 组装注册表后
/// 按默认系统写入；逐系统的真实开关看 `systems.{name}.adt`。
/// AtomicBool（而非 OnceLock）：未配置模式启动为 false，setup API 热加载
/// 首个默认系统时翻转（见 setup.rs），全程与运行态一致。
static CAPABILITY_ADT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 写入 capabilities.adt（main 启动期 + setup 首个默认系统热加载时）。
pub fn init_capability_adt(enabled: bool) {
    CAPABILITY_ADT.store(enabled, std::sync::atomic::Ordering::SeqCst);
}

/// capabilities.adt 当前值。
pub fn capabilities_adt_enabled() -> bool {
    CAPABILITY_ADT.load(std::sync::atomic::Ordering::SeqCst)
}

impl AdtEndpoint {
    /// 创建端点（Basic 快捷形态）。`base_url` 不带尾斜杠；`timeout` 为单请求超时。
    #[allow(dead_code)] // build_adt 走 new_with_auth；Basic 快捷形态留给测试与工具调用
    pub fn new(base_url: &str, user: &str, passwd: &str, timeout: Duration) -> Self {
        Self::new_with_auth(
            base_url,
            AdtAuthSpec::Basic {
                user: user.to_string(),
                passwd: passwd.to_string(),
            },
            &[],
            None,
            None,
            timeout,
        )
        .expect("Basic 形态构造不会失败（无 TLS 素材读取）")
    }

    /// 完整构造：鉴权形态 + TLS 素材（mTLS identity / CA bundle）。
    /// `tls_files`：cert PEM 路径 + 可选 key PEM 路径（key 缺省 = cert 单文件含私钥）。
    /// `state_key`：OAuth 持久化键（= 系统名，refresh token 轮换时落盘；
    /// None = 不落盘——测试/工具调用形态）。
    pub(crate) fn new_with_auth(
        base_url: &str,
        auth: AdtAuthSpec,
        tls_files: &[&str],
        ca_bundle: Option<&str>,
        state_key: Option<&str>,
        timeout: Duration,
    ) -> Result<Self, String> {
        use base64::Engine;
        let (auth_state, user) = match auth {
            AdtAuthSpec::Basic { user, passwd } => {
                let basic = format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD
                        .encode(format!("{}:{}", user, passwd))
                );
                (AuthState::Basic(basic), user)
            }
            AdtAuthSpec::Sso2 { ticket, user } => (AuthState::Sso2(ticket), user),
            AdtAuthSpec::OAuth {
                token_url,
                client_id,
                client_secret,
                user,
                passwd,
                grant,
            } => (
                AuthState::OAuth(OAuthState {
                    token_url,
                    client_id,
                    client_secret,
                    user: user.clone(),
                    passwd,
                    grant,
                    state_key: state_key.map(|s| s.to_string()),
                    token: Mutex::new(None),
                    refresh: Mutex::new(None),
                }),
                user.unwrap_or_else(|| "AGENT".to_string()),
            ),
            AdtAuthSpec::Plugin(spec) => (
                AuthState::Plugin(std::sync::RwLock::new(None), spec),
                "AGENT".to_string(),
            ),
        };
        // TLS identity：PEM（证书 + 私钥可合并单文件）。加密私钥提前拒收——
        // reqwest/OSSL 报错难读，sapcli 的处置照抄（提示 openssl pkey 解密）
        let mut pem_parts: Vec<Vec<u8>> = Vec::new();
        for (i, path) in tls_files.iter().enumerate() {
            let bytes = std::fs::read(path).map_err(|e| {
                format!(
                    "ADT TLS 素材读取失败（第 {} 个文件 {}）: {}",
                    i + 1,
                    path,
                    e
                )
            })?;
            // key 文件（第二个，或单文件形态里的合并 PEM）含加密私钥 → 拒收
            if i > 0 || tls_files.len() == 1 {
                check_key_not_encrypted(&bytes, path)?;
            }
            pem_parts.push(bytes);
        }
        let client = {
            let mut b = reqwest::Client::builder()
                // ADT 永远直连 SAP，无视 HTTP_PROXY/HTTPS_PROXY/ALL_PROXY 环境变量。
                // 本机代理是白名单型的，对内网 IP 一律返 403 空 body（<30ms），
                // 一旦网关进程在带代理的 shell 里启动，ADT 全线挂掉而不自知
                // （RFC 走 CPIC socket 不走 HTTP，所以表现为"RFC 全通、ADT 全 403"）。
                .no_proxy()
                .timeout(timeout);
            if !pem_parts.is_empty() {
                let mut all = Vec::new();
                for p in &pem_parts {
                    all.extend_from_slice(p);
                    if !p.ends_with(b"\n") {
                        all.push(b'\n');
                    }
                }
                let identity = reqwest::Identity::from_pem(&all)
                    .map_err(|e| format!("ADT TLS identity 构建失败（cert/key 无效？）: {e}"))?;
                b = b.identity(identity);
            }
            if let Some(ca) = ca_bundle {
                let pem = std::fs::read(ca)
                    .map_err(|e| format!("ADT CA bundle 读取失败 ({}): {}", ca, e))?;
                let cert = reqwest::Certificate::from_pem(&pem)
                    .map_err(|e| format!("ADT CA bundle 解析失败 ({}): {}", ca, e))?;
                b = b.add_root_certificate(cert);
            }
            b.build().unwrap_or_default()
        };
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            auth: auth_state,
            user: user.trim().to_uppercase(),
            client,
            csrf: Mutex::new(None),
        })
    }

    /// ADT 登录用户名（对象创建 XML 的 responsible 字段）。
    pub(crate) fn user(&self) -> &str {
        &self.user
    }

    /// 基地址（日志/诊断用）。
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 取缓存的 CSRF token（未失效直接用）。token 优先从 `url` 自身签发：
    /// 对该 URL 发 GET + Fetch 头，ICF 对已认证请求通常都会返回 token。
    ///
    /// **回落**：部分资源 GET 直接 404/405 且不带 `X-CSRF-Token`
    /// （ED1/752 实证：`GET /sap/bc/adt/ddic/tabletypes` → 404 无 token，
    /// 导致建表类型全线 `ADT_CSRF_FAILED`）。CSRF token 是**会话级**的
    /// （按 client+user 签发，与具体资源无关），所以签不出时改由
    /// `/sap/bc/adt/discovery` 签发——该资源恒可用且稳定回 token。
    async fn csrf_token(
        &self,
        url: &str,
        force_refresh: bool,
    ) -> Result<Option<(String, Option<String>)>, RfcError> {
        if !force_refresh {
            if let Ok(guard) = self.csrf.lock() {
                if let Some(t) = guard.as_ref() {
                    if t.fetched.elapsed() < CSRF_TTL {
                        return Ok(Some((t.token.clone(), t.cookie.clone())));
                    }
                }
            }
        }

        // ① 目标 URL 自身
        let mut got = self.fetch_csrf_token(url).await?;
        // ② 回落 discovery（token 与资源无关，只与会话有关）
        if got.is_none() {
            let disco = format!("{}/sap/bc/adt/discovery", self.base_url);
            tracing::debug!(url = %url, "目标 URL 未签发 CSRF token，回落 discovery 签发");
            got = self.fetch_csrf_token(&disco).await?;
        }

        match got {
            Some((token, cookie)) => {
                if let Ok(mut guard) = self.csrf.lock() {
                    *guard = Some(CsrfToken {
                        token: token.clone(),
                        cookie: cookie.clone(),
                        fetched: Instant::now(),
                    });
                }
                tracing::debug!("ADT CSRF token 已获取并缓存");
                Ok(Some((token, cookie)))
            }
            None => Err(RfcError {
                code: -1,
                status: 502,
                message: "ADT CSRF token 获取失败（响应缺少 X-CSRF-Token 头）".into(),
                key: "ADT_CSRF_FAILED".into(),
            }),
        }
    }

    /// 单次 GET + `X-CSRF-Token: Fetch`，取回 (token, cookie)。
    /// 响应不带可用 token 时返回 `Ok(None)`（由调用方决定回落还是报错）。
    async fn fetch_csrf_token(
        &self,
        url: &str,
    ) -> Result<Option<(String, Option<String>)>, RfcError> {
        let auth = self.auth_header().await?;
        let mut req = self
            .client
            .get(url)
            .header("X-CSRF-Token", "Fetch")
            .header("Accept", "*/*");
        if let Some(a) = &auth {
            req = req.header("Authorization", a);
        }
        if let Some(c) = self.sso2_cookie() {
            req = req.header("Cookie", c);
        }
        let resp = req.send().await.map_err(adt_unreachable)?;

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
            Some(t) if !t.is_empty() && t != "Fetch" => Ok(Some((t, cookie))),
            _ => Ok(None),
        }
    }

    /// **强制刷新**共享缓存的 CSRF token 并返回 (token, cookie 头串)。
    ///
    /// 用途：临时对象执行编排（`/api/abap/run`）。classrun 服务按 ABAP 会话
    /// 缓存类元数据——同一会话里「刚激活的类」常被判成不存在/不完整
    /// （sapcli 同款报错 "Class does not implement if_oo_adt_classrun~main
    /// method!"，它靠 `connection.new_session()` 规避）。强制刷新会从服务器
    /// 拿一套**全新**的 sap-contextid cookie，把执行请求放进新会话。
    /// token 与资源无关、按 client+user 会话签发，用 discovery URL 签发即可。
    pub(crate) async fn fresh_session_token(&self) -> Result<(String, Option<String>), RfcError> {
        let disco = format!("{}/sap/bc/adt/discovery", self.base_url);
        let got = self.csrf_token(&disco, true).await?;
        got.ok_or_else(|| RfcError {
            code: -1,
            status: 502,
            message: "ADT CSRF token 获取失败（响应缺少 X-CSRF-Token 头）".into(),
            key: "ADT_CSRF_FAILED".into(),
        })
    }

    /// HTTP 客户端（debug_session 复用；单请求超时可在 RequestBuilder 上覆盖）。
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.client
    }

    /// 当前鉴权对应的 `Authorization` 头值（Sso2 返回 None——身份在 Cookie 里；
    /// OAuth 取缓存 token，必要时先发 grant）。
    pub(crate) async fn auth_header(&self) -> Result<Option<String>, RfcError> {
        match &self.auth {
            AuthState::Basic(h) => Ok(Some(h.clone())),
            AuthState::Sso2(_) | AuthState::Plugin(..) => Ok(None),
            AuthState::OAuth(_) => Ok(Some(format!("Bearer {}", self.oauth_token(false).await?))),
        }
    }

    /// SSO2 票据 Cookie 串（非 Sso2 形态恒 None）。
    pub(crate) fn sso2_cookie(&self) -> Option<String> {
        match &self.auth {
            AuthState::Sso2(ticket) => Some(format!("MYSAPSSO2={ticket}")),
            _ => None,
        }
    }

    /// 插件凭证（缓存未过期直接用；过期重跑插件）。`force` = 失效重跑（401 后）。
    async fn plugin_cred(&self, force: bool) -> Result<PluginCred, RfcError> {
        let (cache, spec) = match &self.auth {
            AuthState::Plugin(cache, spec) => (cache, spec),
            _ => {
                return Err(RfcError {
                    code: -1,
                    status: 500,
                    message: "plugin_cred 仅 Plugin 形态可调".into(),
                    key: "ADT_INTERNAL".into(),
                })
            }
        };
        if !force {
            if let Ok(guard) = cache.read() {
                if let Some(c) = guard.as_ref() {
                    if Instant::now() < c.expires_at {
                        return Ok(c.clone());
                    }
                }
            }
        }
        // 连接上下文从基地址解析（插件可据此选后端/租户）
        let (proto, hostport) = match self.base_url.split_once("://") {
            Some((p, rest)) => (p.to_string(), rest.to_string()),
            None => ("http".to_string(), self.base_url.clone()),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.to_string()),
            None => (
                hostport.clone(),
                if proto == "https" { "443" } else { "80" }.to_string(),
            ),
        };
        let cred = run_auth_plugin(spec, &proto, &host, &port).await?;
        if let Ok(mut g) = cache.write() {
            *g = Some(cred.clone());
        }
        Ok(cred)
    }

    /// 插件产物 → (额外请求头, 额外 Cookie 串)。非 Plugin 形态返回空。
    pub(crate) async fn plugin_extras(&self) -> Result<(Vec<(String, String)>, Option<String>), RfcError> {
        if !matches!(self.auth, AuthState::Plugin(..)) {
            return Ok((Vec::new(), None));
        }
        let cred = self.plugin_cred(false).await?;
        let cookie = cred.cookies.clone();
        // 插件可自带 Authorization——它优先于计算出的 auth_header
        let _ = &cred;
        Ok((cred.headers.to_vec(), cookie))
    }

    /// 插件产物里是否带 Authorization 头（决定 auth_header 是否让位）。
    fn plugin_has_authorization(extras: &[(String, String)]) -> bool {
        extras
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
    }

    /// OAuth token：缓存未过期直接用 → refresh_token 换新 → password grant。
    /// `force=true` 丢弃缓存强制重取（401 后用）。
    /// 注入 authorization_code 流的 refresh_token 种子（浏览器授权完成后由
    /// setup 层热注入**运行中的端点**，无需重建句柄；`None` 仅清空——重授权
    /// 前使旧种子失效）。仅 OAuth 形态且为 authcode grant 时生效。
    pub(crate) fn seed_oauth_refresh(&self, token: Option<String>) -> bool {
        if let AuthState::OAuth(st) = &self.auth {
            if st.grant == OAuthGrant::AuthorizationCode {
                if let Ok(mut g) = st.refresh.lock() {
                    *g = token;
                }
                return true;
            }
        }
        false
    }

    async fn oauth_token(&self, force: bool) -> Result<String, RfcError> {
        let state = match &self.auth {
            AuthState::OAuth(st) => st,
            _ => {
                return Err(RfcError {
                    code: -1,
                    status: 500,
                    message: "oauth_token 仅 OAuth 形态可调".into(),
                    key: "ADT_INTERNAL".into(),
                })
            }
        };
        if !force {
            if let Ok(guard) = state.token.lock() {
                if let Some((t, expires_at)) = guard.as_ref() {
                    // 提前 OAUTH_REFRESH_MARGIN 轮换（并发窗口）
                    if Instant::now() + OAUTH_REFRESH_MARGIN < *expires_at {
                        return Ok(t.clone());
                    }
                }
            }
        }
        // refresh 优先（有就先试，失败回落 password grant——sapcli get_token 同序）
        let refresh = state.refresh.lock().ok().and_then(|g| g.clone());
        let token_url = oauth_token_endpoint(&state.token_url);
        let send_grant = |form: Vec<(&str, String)>| {
            self.client
                .post(&token_url)
                .basic_auth(&state.client_id, Some(&state.client_secret))
                .header("Accept", "application/json")
                .form(&form)
        };
        // 刷新失败不致命：走完整 grant（refresh token 可能已过期/吊销）。
        // authcode 形态必须区分「被明确拒收」（token 真死了 → 指引重授权）与
        // 「请求本身失败」（网络/5xx 抖动 → 可重试，别误导用户去重新授权）。
        enum RefreshOutcome {
            Ok,
            Rejected { status: u16, body: String },
            Failed(String),
        }
        let mut refresh_outcome: Option<RefreshOutcome> = None;
        let response: Option<reqwest::Response> = if let Some(ref rt) = refresh {
            match send_grant(vec![
                ("grant_type", "refresh_token".into()),
                ("refresh_token", rt.clone()),
            ])
            .send()
            .await
            {
                Ok(r) if r.status().is_success() => {
                    refresh_outcome = Some(RefreshOutcome::Ok);
                    Some(r)
                }
                Ok(r) => {
                    // XSUAA 的错误体必须留痕——静默吞掉会把可诊断问题变成谜团
                    let status = r.status().as_u16();
                    let body = r.text().await.unwrap_or_default();
                    tracing::warn!(
                        system = state.state_key.as_deref().unwrap_or("?"),
                        status,
                        resp = %crate::explore::first_line(&body),
                        "OAuth refresh token 换新被拒"
                    );
                    refresh_outcome = Some(RefreshOutcome::Rejected {
                        status,
                        body: crate::explore::first_line(&body),
                    });
                    None
                }
                Err(e) => {
                    tracing::warn!(
                        system = state.state_key.as_deref().unwrap_or("?"),
                        error = %e,
                        "OAuth refresh 请求发送失败"
                    );
                    refresh_outcome = Some(RefreshOutcome::Failed(e.to_string()));
                    None
                }
            }
        } else {
            None
        };

        let response = match response {
            Some(r) => r,
            None if state.grant == OAuthGrant::AuthorizationCode => {
                // authcode 种子流没有可回落的完整 grant：client_credentials 产物
                // 无用户身份（BTP binding-secret 只放这个 grant），ABAP 系统必拒
                // 收（实测 401）——静默回落只会把错误变成更难排查的下游 401。
                let sys = state.state_key.as_deref().unwrap_or("<system>");
                return Err(match refresh_outcome.as_ref() {
                    // 明确被拒：token 真死了（过期/吊销）→ 指引重授权
                    Some(RefreshOutcome::Rejected { status, body }) => RfcError {
                        code: -1,
                        status: 401,
                        message: format!(
                            "OAuth authorization_code 形态：refresh token 被 XSUAA 拒收\
                             （HTTP {status}: {body}）。重新授权：\
                             POST /api/setup/system/{sys}/authorize \
                             （打开返回的 authorize_url 完成浏览器登录即可）"
                        ),
                        key: "ADT_OAUTH_REAUTH".into(),
                    },
                    // 请求本身失败：可能瞬时（网络抖动/限流）→ 可重试，别误导重授权
                    Some(RefreshOutcome::Failed(detail)) => RfcError {
                        code: -1,
                        status: 502,
                        message: format!(
                            "OAuth refresh 请求失败（{detail}）——可能瞬时，下一周期探测会重试；\
                             持续失败再重新授权：POST /api/setup/system/{sys}/authorize"
                        ),
                        key: "ADT_OAUTH_REFRESH_RETRY".into(),
                    },
                    // 无种子：还没做过浏览器授权
                    _ => RfcError {
                        code: -1,
                        status: 401,
                        message: format!(
                            "OAuth authorization_code 形态：尚未完成浏览器授权（没有 refresh token 种子）。\
                             授权：POST /api/setup/system/{sys}/authorize \
                             （打开返回的 authorize_url 完成浏览器登录即可）"
                        ),
                        key: "ADT_OAUTH_REAUTH".into(),
                    },
                });
            }
            None => {
                // 无 user/passwd 的 OAuth(BTP XSUAA service key)走 client_credentials;
                // 配了 user/passwd(经典 SAP ICM OAuth 账号)保持 password grant。
                let is_client_credentials = state.user.is_none() && state.passwd.is_none();
                let mut form = vec![(
                    "grant_type",
                    if is_client_credentials {
                        "client_credentials".to_string()
                    } else {
                        "password".to_string()
                    },
                )];
                if let Some(u) = &state.user {
                    form.push(("username", u.clone()));
                }
                if let Some(p) = &state.passwd {
                    form.push(("password", p.clone()));
                }
                send_grant(form).send().await.map_err(adt_unreachable)?
            }
        };

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(RfcError {
                code: -1,
                status: if status == 401 || status == 400 { 401 } else { 502 },
                message: format!(
                    "OAuth token 获取失败（HTTP {status}）——检查 token_url/client_id/client_secret{}: {}",
                    if state.passwd.is_none() { "（password grant 需要 user/passwd，除非缓存里有有效 refresh token）" } else { "" },
                    crate::explore::first_line(&body)
                ),
                key: "ADT_OAUTH_FAILED".into(),
            });
        }

        let data: serde_json::Value = response.json().await.map_err(|e| RfcError {
            code: -1,
            status: 502,
            message: format!("OAuth token 响应不是 JSON: {e}"),
            key: "ADT_OAUTH_FAILED".into(),
        })?;
        let access = data
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RfcError {
                code: -1,
                status: 502,
                message: "OAuth token 响应缺少 access_token".into(),
                key: "ADT_OAUTH_FAILED".into(),
            })?
            .to_string();
        if let Some(rt) = data
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        {
            // 值真的轮换了才动（XSUAA refresh grant 可能原样返回旧 token）
            let rotated = state.refresh.lock().ok().map(|mut g| {
                let changed = g.as_deref() != Some(rt.as_str());
                *g = Some(rt.clone());
                changed
            });
            // authcode 形态必须落盘：XSUAA 轮换 refresh token 时旧值可能即时
            // 失效，新值赶在下次换新前持久化，否则重启即丢授权。
            if rotated == Some(true) && state.grant == OAuthGrant::AuthorizationCode {
                if let Some(key) = &state.state_key {
                    if let Err(e) = crate::oauth_state::store_refresh(key, &rt) {
                        tracing::warn!(
                            system = %key,
                            error = %e,
                            "refresh token 落盘失败——重启后需重新浏览器授权"
                        );
                    }
                }
            }
        }
        let expires_in = data
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(3600)
            .max(0) as u64;
        if let Ok(mut g) = state.token.lock() {
            *g = Some((
                access.clone(),
                Instant::now() + Duration::from_secs(expires_in),
            ));
        }
        Ok(access)
    }

    /// ADT 就绪探针（ADT-only 系统的 "ping"）：轻量 GET discovery，带鉴权与
    /// 401 重试（`get_raw` 内建）。200 级 = 可用；其余状态/网络错误 = 不可达。
    /// 不做语义校验——body 内容无关紧要，能拿到响应就是通道健康。
    pub(crate) async fn ping(&self) -> Result<(), RfcError> {
        let (status, _, _) = self
            .get_raw("discovery", "application/atomsvc+xml")
            .await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(RfcError {
                code: -1,
                status: if status == 401 || status == 403 { 503 } else { 502 },
                message: format!("ADT 探测失败（HTTP {status}）"),
                key: "ADT_PING_FAILED".into(),
            })
        }
    }

    /// 内部 GET 通道：供网关自身的结构化端点（`/api/dumps/**`）复用 ADT 配置、
    /// 认证与错误契约，避免各自直接拼 URL。
    ///
    /// `rel_raw` 是 `/sap/bc/adt/` 之后的相对路径（原始 percent-encoded 形式，
    /// 可带 query）。返回 `(HTTP 状态码, Content-Type, body)`，状态与内容由
    /// ADT 原样给出，调用方自行解读。
    pub(crate) async fn get_raw(
        &self,
        rel_raw: &str,
        accept: &str,
    ) -> Result<(u16, Option<String>, Bytes), RfcError> {
        let rel_raw = validate_raw_path(rel_raw)?;
        let url = format!("{}/sap/bc/adt/{}", self.base_url, rel_raw);

        let auth = self.auth_header().await?;
        let mut req = self.client.get(&url).header("Accept", accept);
        if let Some(a) = &auth {
            req = req.header("Authorization", a);
        }
        if let Some(c) = self.sso2_cookie() {
            req = req.header("Cookie", c);
        }
        let resp = req.send().await.map_err(|e| {
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

    /// 内部读取 ADT 纯文本资源（源码等），按行返回（去 \r）。
    /// 404 → NOT_FOUND（保留调用方的 404 语义）；其余非 200 → 502。
    pub(crate) async fn get_text_lines(&self, rel_path: &str) -> Result<Vec<String>, RfcError> {
        let (status, _ct, body) = self.get_raw(rel_path, "*/*").await?;
        match status {
            200 => Ok(String::from_utf8_lossy(&body)
                .split('\n')
                .map(|l| l.trim_end_matches('\r').to_string())
                .collect()),
            404 => Err(RfcError {
                code: -1,
                status: 404,
                message: format!("ADT 资源不存在: {}", rel_path),
                key: "NOT_FOUND".into(),
            }),
            s => Err(RfcError {
                code: -1,
                status: 502,
                message: format!("ADT 源码读取失败（ADT 返回 {}）", s),
                key: "ADT_SOURCE_UNAVAILABLE".into(),
            }),
        }
    }

    /// 读函数模块源码（RFC 读取失败时的 ADT 降级通道）。
    /// 资源嵌在函数组下：`/sap/bc/adt/functions/groups/{组}/fmodules/{FM}/source/main`。
    /// 组名由调用方经 [`crate::discovery::resolve_function_group`] 反解（FM 名全局唯一，
    /// 但资源 URL 需要两级名字——vsp 同样先从搜索结果的 URI 里反解组名）。
    pub(crate) async fn read_fm_source(
        &self,
        group: &str,
        name: &str,
    ) -> Result<Vec<String>, RfcError> {
        let rel = format!(
            "functions/groups/{}/fmodules/{}/source/main",
            encode_path_segment(&group.trim().to_uppercase()),
            encode_path_segment(&name.trim().to_uppercase())
        );
        self.get_text_lines(&rel).await
    }

    /// 读程序源码（RFC 读取失败时的 ADT 降级通道）：
    /// `/sap/bc/adt/programs/programs/{名}/source/main`（报表/include 主程序）。
    pub(crate) async fn read_program_source(&self, name: &str) -> Result<Vec<String>, RfcError> {
        let rel = format!(
            "programs/programs/{}/source/main",
            encode_path_segment(&name.trim().to_uppercase())
        );
        self.get_text_lines(&rel).await
    }

    /// 为有状态编排**建立专用 stateful 会话**并取 CSRF token。
    ///
    /// 这是 ADT 写入序列的根：锁必须签发在一个 stateful 会话里（无状态请求的
    /// 服务端会话随响应即回收，签出的 lockHandle 生来无效 → PUT 报 423
    /// InvalidLockHandle）；且 token 必须与持有锁的会话同源（共享缓存的无状态
    /// token 配 stateful 会话会被 ICF 以「Service cannot be reached」拒绝）。
    /// 因此编排入口先做一次 `GET + Fetch + stateful`，token 与 `sap-contextid`
    /// 一并落进编排会话，全程专用、结束即弃。
    pub(crate) async fn establish_stateful(
        &self,
        rel_path: &str,
    ) -> Result<(String, Vec<(String, String)>), RfcError> {
        let rel_path = validate_raw_path(rel_path)?;
        let url = format!("{}/sap/bc/adt/{}", self.base_url, rel_path);
        let auth = self.auth_header().await?;
        let mut req = self
            .client
            .get(&url)
            .header("Accept", "*/*")
            .header("X-CSRF-Token", "Fetch")
            .header("X-sap-adt-sessiontype", "stateful");
        if let Some(a) = &auth {
            req = req.header("Authorization", a);
        }
        if let Some(c) = self.sso2_cookie() {
            req = req.header("Cookie", c);
        }
        let resp = req.send().await.map_err(|e| {
            metrics::counter!("adt_calls_total", "method" => "GET", "result" => "err").increment(1);
            adt_unreachable(e)
        })?;
        let token = resp
            .headers()
            .get("x-csrf-token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let cookies = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|c| {
                let pair = c.split(';').next()?.trim();
                let (name, value) = pair.split_once('=')?;
                Some((name.trim().to_string(), value.trim().to_string()))
            })
            .collect();
        metrics::counter!("adt_calls_total", "method" => "GET", "result" => "ok").increment(1);
        match token {
            Some(t) if !t.is_empty() && t != "Fetch" => Ok((t, cookies)),
            _ => Err(RfcError {
                code: -1,
                status: 502,
                message: "建立 stateful 会话失败（响应缺少 X-CSRF-Token）".into(),
                key: "ADT_CSRF_FAILED".into(),
            }),
        }
    }

    /// 内部**写**通道：供网关自身的编排端点（`/api/objects/**`）发任意方法的
    /// ADT 请求。写方法自动携带 CSRF token + 会话 Cookie（与代理共用缓存），
    /// 遇 403（token 过期）自动刷新重试一次。
    ///
    /// ADT 的编辑锁存在 ABAP 会话（roll area）里，靠 `sap-contextid` cookie 关联——
    /// 所以有状态编排必须：每一步都带 `X-sap-adt-sessiontype: stateful` 头（经
    /// `extra_headers`），并把上一步响应的 set-cookie（`cookie_override`）回传，
    /// 否则 PUT 会得到 423 `InvalidLockHandle`（abap-adt-api / vsp 同样的教训）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn request_raw(
        &self,
        method: Method,
        rel_path: &str,
        query: &[(&str, &str)],
        body: Option<&[u8]>,
        content_type: Option<&str>,
        accept: &str,
        extra_headers: &[(&str, &str)],
        cookie_override: Option<&str>,
        // 有状态编排专用 token（establish_stateful 签发）。给出时不再用共享
        // 缓存、也不做 403 刷新重试——业务 403（如锁冲突）必须原样上抛。
        csrf_token_override: Option<&str>,
    ) -> Result<AdtRawResponse, RfcError> {
        let rel_path = validate_raw_path(rel_path)?;
        let mut url = format!("{}/sap/bc/adt/{}", self.base_url, rel_path);
        let mut sep = if url.contains('?') { '&' } else { '?' };
        for (k, v) in query {
            url.push(sep);
            url.push_str(k);
            url.push('=');
            url.push_str(&encode_path_segment(v));
            sep = '&';
        }

        // 鉴权头一次计算（OAuth 会先取缓存 token）；插件产物（任意头 + Cookie）
        // 与 Sso2 票据 Cookie、会话 Cookie 合并成单个 Cookie 头（重复 Cookie 头
        // 服务器行为不可靠）。插件自带 Authorization 时让位。
        let auth0 = self.auth_header().await?;
        let (plugin_hdrs, plugin_cookie) = self.plugin_extras().await?;
        let auth0 = if Self::plugin_has_authorization(&plugin_hdrs) {
            None
        } else {
            auth0
        };
        let send = |csrf: Option<(String, Option<String>)>,
                    auth: &Option<String>,
                    extra_hdrs: &[(String, String)],
                    extra_cookie: &Option<String>| {
            let mut req = self
                .client
                .request(method.clone(), &url)
                .header("Accept", accept);
            if let Some(a) = auth {
                req = req.header("Authorization", a);
            }
            if let Some(ct) = content_type {
                req = req.header("Content-Type", ct);
            }
            if let Some(b) = body {
                req = req.body(b.to_vec());
            }
            for (k, v) in extra_headers {
                req = req.header(*k, *v);
            }
            for (k, v) in extra_hdrs {
                req = req.header(k.as_str(), v.as_str());
            }
            let mut cookie_parts: Vec<String> = Vec::new();
            if let Some(sc) = self.sso2_cookie() {
                cookie_parts.push(sc);
            }
            if let Some(pc) = extra_cookie {
                cookie_parts.push(pc.clone());
            }
            if let Some((token, cookie)) = csrf {
                req = req.header("X-CSRF-Token", token);
                // 编排会话的 Cookie 优先（含 sap-contextid），否则用共享缓存会话
                let cookie = cookie_override
                    .map(str::to_string)
                    .or_else(|| cookie.clone());
                if let Some(c) = cookie {
                    cookie_parts.push(c);
                }
            }
            if !cookie_parts.is_empty() {
                req = req.header("Cookie", cookie_parts.join("; "));
            }
            req
        };

        let write = is_write_method(&method);
        // 编排专用 token 优先；否则写方法走共享缓存
        let mut csrf = if let Some(t) = csrf_token_override {
            Some((t.to_string(), cookie_override.map(str::to_string)))
        } else if write {
            self.csrf_token(&url, false).await?
        } else {
            None
        };
        // 传输层重试一次：ED1 的坏节点对部分请求直接断连（connection reset），
        // 重发通常落到健康节点。与下面 uriMappingError 重试互补。
        let mut resp = match send(csrf.clone(), &auth0, &plugin_hdrs, &plugin_cookie)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "ADT 请求传输层失败（坏节点断连？），重发一次");
                send(csrf.clone(), &auth0, &plugin_hdrs, &plugin_cookie).send().await.map_err(|e2| {
                    metrics::counter!("adt_calls_total", "method" => method.as_str().to_owned(), "result" => "err").increment(1);
                    adt_unreachable(e2)
                })?
            }
        };

        // 403 通常是 token 过期：刷新后重试一次（仅共享缓存的写方法；
        // 编排专用 token 下 403 是业务错误——锁冲突等——必须原样上抛）
        if resp.status() == StatusCode::FORBIDDEN && write && csrf_token_override.is_none() {
            tracing::debug!("ADT 内部写请求 403，刷新 CSRF token 后重试");
            if let Ok(Some(new)) = self.csrf_token(&url, true).await {
                csrf = Some(new);
                resp = send(csrf.clone(), &auth0, &plugin_hdrs, &plugin_cookie).send().await.map_err(|e| {
                    metrics::counter!("adt_calls_total", "method" => method.as_str().to_owned(), "result" => "err").increment(1);
                    adt_unreachable(e)
                })?;
            }
        }

        // 401 on OAuth：token 被吊销/过期——强制重新 grant 后重发一次
        if resp.status() == StatusCode::UNAUTHORIZED && matches!(self.auth, AuthState::OAuth(_)) {
            tracing::debug!("ADT OAuth 401，强制刷新 token 后重发一次");
            let fresh = self.oauth_token(true).await?;
            resp = send(csrf.clone(), &Some(format!("Bearer {fresh}")), &plugin_hdrs, &plugin_cookie).send().await.map_err(|e| {
                metrics::counter!("adt_calls_total", "method" => method.as_str().to_owned(), "result" => "err").increment(1);
                adt_unreachable(e)
            })?;
        }
        // 401 on Plugin：凭证失效——失效缓存重跑插件后重发一次
        if resp.status() == StatusCode::UNAUTHORIZED && matches!(self.auth, AuthState::Plugin(..)) {
            tracing::debug!("ADT plugin 401，失效缓存重跑插件后重发一次");
            let _ = self.plugin_cred(true).await?;
            let (hdrs2, cookie2) = self.plugin_extras().await?;
            // 插件路径的 Authorization 由 hdrs2 自带——auth2 恒为 None（避免与
            // 共享认证头重复）；plugin_has_authorization 检查留给未来分流
            let auth2: Option<String> = None;
            let _ = Self::plugin_has_authorization(&hdrs2);
            resp = send(csrf.clone(), &auth2, &hdrs2, &cookie2).send().await.map_err(|e| {
                metrics::counter!("adt_calls_total", "method" => method.as_str().to_owned(), "result" => "err").increment(1);
                adt_unreachable(e)
            })?;
        }

        // 响应读取最多两轮：ED1 这类 Web Dispatcher 负载均衡下，坏节点的 ADT
        // URI mapper 会稳定返回 500 + `uriMappingError`（实测 200/500 完美交替），
        // 原样重发一次通常就轮到健康节点。会话 cookie / CSRF 原样带上。
        let mut retries_left = 1;
        let (status, etag, cookies, body, location) = loop {
            let status = resp.status().as_u16();
            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let etag = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let cookies = resp
                .headers()
                .get_all("set-cookie")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .filter_map(|c| {
                    let pair = c.split(';').next()?.trim();
                    let (name, value) = pair.split_once('=')?;
                    Some((name.trim().to_string(), value.trim().to_string()))
                })
                .collect();
            let body = resp.bytes().await.map_err(|e| {
                metrics::counter!("adt_calls_total", "method" => method.as_str().to_owned(), "result" => "err").increment(1);
                adt_unreachable(e)
            })?;
            const URI_MAP_ERR: &[u8] = b"uriMappingError";
            let retryable = status == 500
                && retries_left > 0
                && body
                    .windows(URI_MAP_ERR.len())
                    .any(|w| w.eq_ignore_ascii_case(URI_MAP_ERR));
            if !retryable {
                metrics::counter!("adt_calls_total", "method" => method.as_str().to_owned(), "result" => "ok")
                    .increment(1);
                break (status, etag, cookies, body, location);
            }
            retries_left -= 1;
            tracing::warn!("ADT 响应 500 uriMappingError（负载均衡坏节点），原样重发一次");
            resp = send(csrf.clone(), &auth0, &plugin_hdrs, &plugin_cookie).send().await.map_err(|e| {
                metrics::counter!("adt_calls_total", "method" => method.as_str().to_owned(), "result" => "err").increment(1);
                adt_unreachable(e)
            })?;
        };

        Ok(AdtRawResponse {
            status,
            etag,
            body,
            cookies,
            location,
        })
    }

    /// `ANY /api/adt/{*path}` —— ADT REST 通用代理。
    ///
    /// 透传规则：
    /// - 请求：`Accept` / `Content-Type` / `If-Match` / `If-None-Match` 与 body 原样转发；
    ///   路径用**原始编码形式**转发（保留调用方的 `%20`/`%2F` 等编码意图）；
    /// - 响应：状态码、`Content-Type`、`ETag`、`Last-Modified` 与 body 原样返回。
    pub async fn proxy(
        &self,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<Response, RfcError> {
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
        let mut url = format!("{}/sap/bc/adt/{}", self.base_url, rel_raw);
        if let Some(q) = uri.query() {
            url.push('?');
            url.push_str(q);
        }

        // 鉴权头一次计算（OAuth 取缓存 token）；插件产物与 Sso2 票据 Cookie 并入
        let auth0 = self.auth_header().await?;
        let (plugin_hdrs, plugin_cookie) = self.plugin_extras().await?;
        let auth0 = if Self::plugin_has_authorization(&plugin_hdrs) {
            None
        } else {
            auth0
        };
        let send = |csrf: Option<(String, Option<String>)>,
                    auth: &Option<String>,
                    extra_hdrs: &[(String, String)],
                    extra_cookie: &Option<String>| {
            let mut req = self.client.request(method.clone(), &url).header(
                "Accept",
                headers
                    .get("accept")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("*/*"),
            );
            if let Some(a) = auth {
                req = req.header("Authorization", a);
            }
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
            for (k, v) in extra_hdrs {
                req = req.header(k.as_str(), v.as_str());
            }
            let mut cookie_parts: Vec<String> = Vec::new();
            if let Some(sc) = self.sso2_cookie() {
                cookie_parts.push(sc);
            }
            if let Some(pc) = extra_cookie {
                cookie_parts.push(pc.clone());
            }
            if let Some((token, cookie)) = csrf {
                req = req.header("X-CSRF-Token", token);
                if let Some(c) = cookie {
                    cookie_parts.push(c);
                }
            }
            if !cookie_parts.is_empty() {
                req = req.header("Cookie", cookie_parts.join("; "));
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
            self.csrf_token(&url, false).await?
        } else {
            None
        };
        let mut resp = send(csrf.clone(), &auth0, &plugin_hdrs, &plugin_cookie)
            .send()
            .await
            .map_err(|e| {
                metrics_result(false);
                adt_unreachable(e)
            })?;

        // 403 通常是 token 过期：刷新后重试一次（仅写方法）
        if resp.status() == StatusCode::FORBIDDEN && is_write_method(&method) {
            tracing::debug!("ADT 写请求 403，刷新 CSRF token 后重试");
            if let Ok(Some(new)) = self.csrf_token(&url, true).await {
                csrf = Some(new);
                resp = send(csrf.clone(), &auth0, &plugin_hdrs, &plugin_cookie)
                    .send()
                    .await
                    .map_err(|e| {
                        metrics_result(false);
                        adt_unreachable(e)
                    })?;
            }
        }

        // OAuth 401：token 失效——强制重新 grant 重发一次
        if resp.status() == StatusCode::UNAUTHORIZED && matches!(self.auth, AuthState::OAuth(_)) {
            tracing::debug!("ADT OAuth 401（代理通道），强制刷新 token 后重发一次");
            let fresh = self.oauth_token(true).await?;
            resp = send(
                csrf.clone(),
                &Some(format!("Bearer {fresh}")),
                &plugin_hdrs,
                &plugin_cookie,
            )
            .send()
            .await
            .map_err(|e| {
                metrics_result(false);
                adt_unreachable(e)
            })?;
        }

        // Plugin 401：凭证失效——失效缓存重跑插件后重发一次
        if resp.status() == StatusCode::UNAUTHORIZED && matches!(self.auth, AuthState::Plugin(..)) {
            let cred = self.plugin_cred(true).await?;
            tracing::debug!(message = %cred.message, "ADT plugin 401（代理通道），失效缓存重跑插件后重发一次");
            let (hdrs2, cookie2) = self.plugin_extras().await?;
            let auth2 = None;
            resp = send(csrf.clone(), &auth2, &hdrs2, &cookie2)
                .send()
                .await
                .map_err(|e| {
                    metrics_result(false);
                    adt_unreachable(e)
                })?;
        }

        let status = resp.status();
        let mut builder = Response::builder().status(status);
        for h in ["content-type", "etag", "last-modified"] {
            if let Some(v) = resp.headers().get(h) {
                // 已知安全的少量透传头，构建失败视为网关内部错误
                builder = builder.header(HeaderName::from_static(h), v.clone());
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
}

/// 内部通道响应（状态 + ETag + body + 响应 Cookie）。etag 为后续乐观锁预留。
pub(crate) struct AdtRawResponse {
    pub status: u16,
    #[allow(dead_code)]
    pub etag: Option<String>,
    pub body: Bytes,
    /// 响应 `set-cookie` 的 (name, value) 对——有状态编排（锁→写→解锁）
    /// 靠回传 `sap-contextid` 等维持同一 ABAP 会话
    pub cookies: Vec<(String, String)>,
    /// `Location` 响应头——aunit 异步 run 创建（201 + Location: …/runs/{id}）
    /// 等需要靠它拿新资源 ID 的端点用；其余端点恒 None
    pub location: Option<String>,
}

/// 执行外部鉴权插件：stdin JSON 进、stdout JSON 出，30s 硬超时。
/// 失败（非零退出/超时/坏 JSON/未知 content.type）一律 502 ADT_PLUGIN_FAILED。
async fn run_auth_plugin(
    spec: &AuthPluginSpec,
    conn_proto: &str,
    conn_host: &str,
    conn_port: &str,
) -> Result<PluginCred, RfcError> {
    use tokio::io::AsyncWriteExt;
    if !spec.command.starts_with('/') {
        return Err(RfcError {
            code: -1,
            status: 502,
            message: format!(
                "auth_plugin.command 必须是绝对路径（防 PATH 劫持，网关常驻服务）: {:?}",
                spec.command
            ),
            key: "ADT_PLUGIN_FAILED".into(),
        });
    }
    let request = serde_json::json!({
        "connection": {
            "proto": conn_proto,
            "host": conn_host,
            "port": conn_port,
            "type": "adt",
            "path": "/sap/bc/adt/core/discovery",
            "verify": true,
        },
        "parameters": spec.parameters,
    });

    let mut child = tokio::process::Command::new(&spec.command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| RfcError {
            code: -1,
            status: 502,
            message: format!("auth_plugin 启动失败 ({}): {}", spec.command, e),
            key: "ADT_PLUGIN_FAILED".into(),
        })?;
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(request.to_string().as_bytes()).await;
        let _ = si.shutdown().await;
    }
    let out = tokio::time::timeout(PLUGIN_TIMEOUT, child.wait_with_output()).await;
    let out = match out {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(RfcError {
                code: -1,
                status: 502,
                message: format!("auth_plugin 执行失败: {e}"),
                key: "ADT_PLUGIN_FAILED".into(),
            })
        }
        Err(_) => {
            return Err(RfcError {
                code: -1,
                status: 502,
                message: format!("auth_plugin 超时（{}s）", PLUGIN_TIMEOUT.as_secs()),
                key: "ADT_PLUGIN_FAILED".into(),
            })
        }
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(RfcError {
            code: -1,
            status: 502,
            message: format!(
                "auth_plugin 非零退出（{}）: {}",
                out.status,
                crate::explore::first_line(&err)
            ),
            key: "ADT_PLUGIN_FAILED".into(),
        });
    }
    if out.stdout.len() > PLUGIN_MAX_OUTPUT {
        return Err(RfcError {
            code: -1,
            status: 502,
            message: format!("auth_plugin 输出超限（>{}KB）", PLUGIN_MAX_OUTPUT / 1024),
            key: "ADT_PLUGIN_FAILED".into(),
        });
    }
    let data: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| RfcError {
        code: -1,
        status: 502,
        message: format!("auth_plugin 输出不是 JSON: {e}"),
        key: "ADT_PLUGIN_FAILED".into(),
    })?;
    let message = data
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let content = data
        .get("content")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let typ = content.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let (headers, cookies) = match typ {
        // {type:"cookie", cookies:[{name,value,...}]}
        "cookie" => {
            let mut cs = Vec::new();
            for c in content
                .get("cookies")
                .and_then(|v| v.as_array())
                .map(|a| a.as_slice())
                .unwrap_or(&[])
            {
                let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let value = c.get("value").and_then(|v| v.as_str()).unwrap_or("");
                if !name.is_empty() {
                    cs.push(format!("{name}={value}"));
                }
            }
            (Vec::new(), if cs.is_empty() { None } else { Some(cs.join("; ")) })
        }
        // {type:"http_authorization_header", headers:{Name:Value,...}}
        "http_authorization_header" => {
            let map = content
                .get("headers")
                .and_then(|v| v.as_object())
                .ok_or_else(|| RfcError {
                    code: -1,
                    status: 502,
                    message: "auth_plugin headers 形态需要非空 headers 对象".into(),
                    key: "ADT_PLUGIN_FAILED".into(),
                })?;
            let hs: Vec<(String, String)> = map
                .iter()
                .filter_map(|(k, v)| {
                    v.as_str().map(|s| (k.trim().to_string(), s.to_string()))
                })
                .collect();
            (hs, None)
        }
        "certificates" => {
            return Err(RfcError {
                code: -1,
                status: 502,
                message: "auth_plugin 的 certificates 形态网关不支持——mTLS 请直接配置 adt_cert/adt_key（静态路径）".into(),
                key: "ADT_PLUGIN_FAILED".into(),
            })
        }
        other => {
            return Err(RfcError {
                code: -1,
                status: 502,
                message: format!(
                    "auth_plugin 不支持的 content.type {other:?}（可用 cookie | http_authorization_header）"
                ),
                key: "ADT_PLUGIN_FAILED".into(),
            })
        }
    };
    // 响应可带 cache_secs 覆盖配置 TTL；clamp 1s..24h
    let ttl = data
        .get("cache_secs")
        .and_then(|v| v.as_u64())
        .map(|s| Duration::from_secs(s.clamp(1, 86_400)))
        .unwrap_or(spec.cache_ttl);
    Ok(PluginCred {
        headers,
        cookies,
        message,
        expires_at: Instant::now() + ttl,
    })
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

/// 是否为写方法（ADT 代理的 CSRF 处理与只读模式的写拦截共用）。
pub(crate) fn is_write_method(m: &Method) -> bool {
    matches!(
        *m,
        Method::POST | Method::PUT | Method::DELETE | Method::PATCH
    )
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
pub(crate) fn validate_raw_path(raw: &str) -> Result<&str, RfcError> {
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

/// percent-encode 路径段：仅保留未保留字符，其余 %XX（大写十六进制）。
/// 供网关内部端点（dumps、源码降级读）拼 ADT URL 用。
pub(crate) fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- OAuth authorization_code 门禁（v0.23） ----------------

    /// authcode 形态端点（state_key 登记——持久化钩子只在真正拿到 token 时触发，
    /// 本组测试不产生落盘）。
    fn authcode_ep(token_url: String) -> AdtEndpoint {
        AdtEndpoint::new_with_auth(
            "http://127.0.0.1:1",
            AdtAuthSpec::OAuth {
                token_url,
                client_id: "cid".into(),
                client_secret: "sec".into(),
                user: None,
                passwd: None,
                grant: OAuthGrant::AuthorizationCode,
            },
            &[],
            None,
            Some("sfa-test-sys"),
            Duration::from_secs(5),
        )
        .expect("authcode endpoint")
    }

    #[tokio::test]
    async fn oauth_authcode_without_seed_fails_fast_with_reauth_hint() {
        let ep = authcode_ep("http://127.0.0.1:1".into());
        let e = ep.oauth_token(false).await.unwrap_err();
        assert_eq!(e.key, "ADT_OAUTH_REAUTH");
        assert_eq!(e.status, 401);
        assert!(e.message.contains("authorize"), "got: {}", e.message);
    }

    #[tokio::test]
    async fn oauth_authcode_dead_refresh_never_falls_back_to_client_credentials() {
        // mock token server：refresh grant 一律 400——旧行为会静默回落
        // client_credentials 拿到一个 ABAP 必拒收的 token；新行为 fail-fast。
        let app = axum::Router::new().route(
            "/oauth/token",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    r#"{"error":"invalid_grant"}"#,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let ep = authcode_ep(format!("http://{addr}"));
        assert!(ep.seed_oauth_refresh(Some("stale-token".into())));
        let e = ep.oauth_token(false).await.unwrap_err();
        assert_eq!(e.key, "ADT_OAUTH_REAUTH", "got: {}", e.message);
        // 明确拒收 → 指引重授权，且 XSUAA 错误体留痕（可诊断）
        assert!(e.message.contains("被 XSUAA 拒收"), "got: {}", e.message);
        assert!(e.message.contains("invalid_grant"), "got: {}", e.message);
        assert!(e.message.contains("authorize"), "got: {}", e.message);
    }

    #[tokio::test]
    async fn adt_ping_2xx_ok_and_error_status() {
        // 200 mock → ping Ok
        let app = axum::Router::new().route(
            "/sap/bc/adt/discovery",
            axum::routing::get(|| async { (axum::http::StatusCode::OK, "<service/>") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let ep = AdtEndpoint::new(
            &format!("http://{addr}"),
            "u",
            "p",
            Duration::from_secs(5),
        );
        assert!(ep.ping().await.is_ok(), "200 → ping Ok");

        // 500 mock → ping Err（ADT_PING_FAILED）
        let app2 = axum::Router::new().route(
            "/sap/bc/adt/discovery",
            axum::routing::get(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "") }),
        );
        let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = listener2.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener2, app2).await.unwrap() });
        let ep2 = AdtEndpoint::new(
            &format!("http://{addr2}"),
            "u",
            "p",
            Duration::from_secs(5),
        );
        let e = ep2.ping().await.unwrap_err();
        assert_eq!(e.key, "ADT_PING_FAILED");
        assert_eq!(e.status, 502);
    }

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

    #[test]
    fn endpoint_new_normalizes_base_and_uppercases_user() {
        let ep = AdtEndpoint::new("http://h:50000/", "devuser", "pw", Duration::from_secs(5));
        assert_eq!(ep.base_url(), "http://h:50000");
        assert_eq!(ep.user(), "DEVUSER");
    }
}
