//! 配置加载：从环境变量（.env 文件或真实环境变量）读取 SAP 连接参数与监听地址。
//!
//! 必填项缺失时给出明确的中文报错，便于部署排查。

use std::env;

/// 服务运行配置
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// SAP 连接参数（已组装成 RfcConnection::new 所需的键值对）
    pub conn_params: Vec<(&'static str, String)>,
    /// HTTP 监听地址，如 "127.0.0.1:3000"
    pub listen_addr: String,
    /// SAP 连接池上限（并发 SAP 调用数）
    pub pool_size: usize,
    /// 连接空闲多久后，借出前需 `RFC_PING` 校验（默认 30s，
    /// 由 `SAP_POOL_IDLE_VALIDATE_SECS` 配置；设 0 禁用校验）
    pub pool_idle_validate: std::time::Duration,
    /// 可选 API key：设置后 `/api/*` 需 Bearer token；`None` = 免鉴权
    pub api_key: Option<String>,
    /// 单次 SAP 调用的全局超时（默认 60s，由 `SAP_REQUEST_TIMEOUT_SECS` 配置）
    pub request_timeout: std::time::Duration,
    /// 停机排空窗口秒数（`SAP_DRAIN_SECS`，默认 5；0 = 收到信号立刻停机）。
    /// 窗口内 `/ready` 返回 503 + `Retry-After`，监听继续服务，让上游摘流量。
    pub drain_secs: u64,
    /// 可选限流（按 IP 的每秒请求数；None=不限流，由 `SAP_RATE_LIMIT_RPS` 配置，≥1）
    pub rate_limit_rps: Option<u32>,
    /// ADT REST 代理基地址（`SAP_ADT_BASE_URL`；未设 → 默认 `http://<ashost>:50000`，
    /// 设为空串禁用）。用于 /api/adt/** 透传。
    pub adt_base_url: Option<String>,
    /// ADT 代理认证用户（`SAP_ADT_USER` 覆盖，默认同 SAP_USER）
    pub adt_user: String,
    /// ADT 代理认证密码（`SAP_ADT_PASSWD` 覆盖，默认同 SAP_PASSWD）
    pub adt_passwd: String,
    /// 只读模式（`SAP_READ_ONLY`，识别 1/true/yes/on）：拦截网关自身写端点
    /// （objects PUT/replace、ADT 写方法）。`/api/rfc` 不受影响——RFC 无法
    /// 按函数名可靠区分读写，SAP 端授权才是真正的边界。
    pub read_only: bool,
    /// API 注册表文件路径（`SAP_REGISTRY_FILE`，默认 `./registry.json`）。
    /// 网关本地 JSON（不依赖 SAP）；目录须可写。
    pub registry_file: std::path::PathBuf,
    /// 运行档位（`SAP_MODE=runtime`）：消费方运行模式——只保留
    /// 平坦调用 + 注册表读 + 公开页/探针，关闭开发面（元数据探索、
    /// 对象写、/api/rfc、MCP）。默认 full（开发工作台）。
    pub runtime_mode: bool,
    /// 新版本检查（`SAP_UPDATE_CHECK`，默认开启；off/false/0/no 关闭）：
    /// 启动时与每 24h 查 GitHub 最新 Release（仅一个匿名 GET，无遥测），
    /// 结果进 `/api/version` 的 latest 块与首页页脚。
    pub update_check: bool,
    /// 多系统配置文件路径（`SAP_SYSTEMS_FILE`，默认 `./sap_systems.toml`）。
    /// 文件不存在 → 从本配置的 SAP_* 环境变量合成单系统 `default`（存量路径）；
    /// 存在 → 系统集合完全由该文件定义（见 systems.rs）。
    pub systems_file: std::path::PathBuf,
    /// 网关设置文件路径（`SAP_GATEWAY_FILE`，默认 `./sap_gateway.toml`）。
    /// `[gateway]` 表存运行期热改的网关设置（PUT /api/settings 维护），
    /// 优先级 **文件 > 环境变量 > 默认值**（见 settings.rs overlay_from_file）。
    /// 独立于 sap_systems.toml——网关设置与系统集合互不牵连。
    pub gateway_file: std::path::PathBuf,
    /// 默认语言（`SAP_LANG`，文档/接口读取的 ?lang 缺省；连接语言在
    /// conn_params 的 LANG 里）。运行期可经 PUT /api/settings 热改。
    pub lang: String,
    /// 引导式配置（`SAP_SETUP`，默认开启；off/false/0/no 关闭）：
    /// 仅在**未配置任何系统**时允许 `POST /api/setup/system` 写入
    /// sap_systems.toml 并热加载（见 setup.rs）。配置完成后端点自动锁定——
    /// 防止无鉴权部署上被添加恶意"系统"做凭据中继/内网探测；
    /// 确需运行期加系统设 `SAP_SETUP=force`（此时 force=true）。
    pub setup_enabled: bool,
    /// `SAP_SETUP=force`：跳过"配置后锁定"，任何时刻都允许引导写入
    pub setup_force: bool,
    /// 未配置模式下是否自动在浏览器打开引导页 `/setup`
    /// （`SAP_OPEN_BROWSER`，默认开启；off/false/0/no 关闭——服务器/无头
    /// 部署防打扰）。已配置的部署从不自动打开。
    pub open_browser: bool,
}

/// 敏感文件落盘（unix 0600；已存在文件重写时一并收紧权限）。
/// `.env` / `sap_systems.toml` / `registry.json` 都持明文凭据，0644 会让同机
/// 其他用户可读（2026-11-05 审计 P1）。Windows 无 POSIX 权限模型——保持默认
/// ACL（配置位于用户目录时本就仅本人可读）。
pub(crate) fn write_private_file(path: &std::path::Path, content: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|e| format!("写文件失败 ({}): {}", path.display(), e))?;
        f.write_all(content.as_bytes())
            .map_err(|e| format!("写文件失败 ({}): {}", path.display(), e))?;
        use std::os::unix::fs::PermissionsExt;
        let mut perm = f
            .metadata()
            .map_err(|e| format!("读元数据失败 ({}): {}", path.display(), e))?
            .permissions();
        perm.set_mode(0o600);
        f.set_permissions(perm)
            .map_err(|e| format!("收紧权限失败 ({}): {}", path.display(), e))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, content)
            .map_err(|e| format!("写文件失败 ({}): {}", path.display(), e))?;
        Ok(())
    }
}

fn required(key: &str) -> Result<String, String> {
    env::var(key).map_err(|_| format!("缺少必填环境变量: {}", key))
}

/// 从环境变量读取配置。调用方应先执行 dotenvy::dotenv()。
///
/// 连接变量组（ASHOST/SYSNR/CLIENT/USER/PASSWD）**要么全缺要么全有**：
/// 全缺且无 sap_systems.toml → 未配置模式（conn_params 为空，网关照常监听，
/// 引导见 setup.rs）；缺一部分 → 报错（半截配置几乎必然是失误）。
pub fn load() -> Result<AppConfig, String> {
    let conn_keys = [
        "SAP_ASHOST",
        "SAP_SYSNR",
        "SAP_CLIENT",
        "SAP_USER",
        "SAP_PASSWD",
    ];
    let unconfigured = conn_keys.iter().all(|k| env::var(k).is_err());
    let (ashost, sysnr, client, user, passwd) = if unconfigured {
        (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        )
    } else {
        (
            required("SAP_ASHOST")?,
            required("SAP_SYSNR")?,
            required("SAP_CLIENT")?,
            required("SAP_USER")?,
            required("SAP_PASSWD")?,
        )
    };
    let lang = env::var("SAP_LANG").unwrap_or_else(|_| "EN".to_string());
    let listen_addr = env::var("SAP_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
    let pool_size = env::var("SAP_POOL_SIZE")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(8);
    // 空闲连接借出前校验阈值（默认 30s；0 = 禁用校验）
    let pool_idle_validate = env::var("SAP_POOL_IDLE_VALIDATE_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|n| {
            if n == 0 {
                std::time::Duration::MAX
            } else {
                std::time::Duration::from_secs(n)
            }
        })
        .unwrap_or(std::time::Duration::from_secs(30));
    // API key：留空/未设 → None（免鉴权）；设置后 /api/* 要求 Bearer token
    let api_key = env::var("SAP_API_KEY").ok().filter(|s| !s.is_empty());
    // 全局请求超时（默认 60s，≥1）
    let request_timeout = std::time::Duration::from_secs(
        env::var("SAP_REQUEST_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| n >= 1)
            .unwrap_or(60),
    );
    // 停机排空窗口（默认 5s；0 = 禁用，收到信号立刻停机）
    let drain_secs = env::var("SAP_DRAIN_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(5);
    // 可选限流（按 IP 的每秒请求数；未设/0/非法 → None=不限流）
    let rate_limit_rps = env::var("SAP_RATE_LIMIT_RPS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&r| r >= 1);
    // ADT 代理基地址：未设 → 按 ashost 推导默认；显式空串 → 禁用；
    // 未配置模式（无 ashost）→ 无法推导即禁用
    let adt_base_url = if unconfigured {
        match env::var("SAP_ADT_BASE_URL") {
            Ok(s) if s.trim().is_empty() => None,
            Ok(s) => Some(s.trim().to_string()),
            Err(_) => None,
        }
    } else {
        match env::var("SAP_ADT_BASE_URL") {
            Ok(s) if s.trim().is_empty() => None,
            Ok(s) => Some(s.trim().to_string()),
            Err(_) => Some(format!("http://{}:50000", ashost)),
        }
    };
    let adt_user = env::var("SAP_ADT_USER").unwrap_or_else(|_| user.clone());
    let adt_passwd = env::var("SAP_ADT_PASSWD").unwrap_or_else(|_| passwd.clone());
    // 只读模式：识别 1/true/yes/on（大小写不敏感），其余值视为关闭
    let read_only = env::var("SAP_READ_ONLY")
        .map(|v| {
            matches!(
                v.trim().to_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    // API 注册表文件（默认工作目录下的 registry.json）
    let registry_file = std::path::PathBuf::from(
        env::var("SAP_REGISTRY_FILE").unwrap_or_else(|_| "registry.json".to_string()),
    );
    // 运行档位：SAP_MODE=runtime（大小写不敏感精确匹配）→ 消费方运行模式
    let runtime_mode = env::var("SAP_MODE")
        .map(|v| v.trim().eq_ignore_ascii_case("runtime"))
        .unwrap_or(false);
    // 新版本检查：默认开启；识别 0/false/off/no（大小写不敏感）为关闭
    let update_check = env::var("SAP_UPDATE_CHECK")
        .map(|v| {
            !matches!(
                v.trim().to_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        })
        .unwrap_or(true);
    // 引导式配置开关（默认开启；force = 已配置后仍允许运行期加系统）
    let setup_enabled = env::var("SAP_SETUP")
        .map(|v| {
            !matches!(
                v.trim().to_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        })
        .unwrap_or(true);
    let setup_force = env::var("SAP_SETUP")
        .map(|v| v.trim().eq_ignore_ascii_case("force"))
        .unwrap_or(false);
    // 未配置模式自动打开引导页浏览器（默认开启；仅未配置时生效）
    let open_browser = env::var("SAP_OPEN_BROWSER")
        .map(|v| {
            !matches!(
                v.trim().to_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        })
        .unwrap_or(true);
    // 多系统配置文件（默认工作目录下的 sap_systems.toml；不存在则走环境变量合成）
    let systems_file = std::path::PathBuf::from(
        env::var("SAP_SYSTEMS_FILE").unwrap_or_else(|_| "sap_systems.toml".to_string()),
    );
    // 网关设置文件（默认工作目录下的 sap_gateway.toml；运行期热设置落盘处）
    let gateway_file = std::path::PathBuf::from(
        env::var("SAP_GATEWAY_FILE").unwrap_or_else(|_| "sap_gateway.toml".to_string()),
    );

    // 键为 'static 字面量，值使用环境变量的 String（运行期存活）。
    // 未配置模式 → 空表（系统集合由 setup API 写入 toml 后热加载）。
    let conn_params: Vec<(&'static str, String)> = if unconfigured {
        vec![]
    } else {
        vec![
            ("ASHOST", ashost),
            ("SYSNR", sysnr),
            ("CLIENT", client),
            ("USER", user),
            ("PASSWD", passwd),
            ("LANG", lang.clone()),
        ]
    };
    let cfg = AppConfig {
        conn_params,
        listen_addr,
        pool_size,
        pool_idle_validate,
        api_key,
        request_timeout,
        drain_secs,
        rate_limit_rps,
        adt_base_url,
        adt_user,
        adt_passwd,
        read_only,
        registry_file,
        runtime_mode,
        update_check,
        systems_file,
        gateway_file,
        setup_enabled,
        setup_force,
        open_browser,
        lang,
    };
    // sap_gateway.toml 的 [gateway] 表覆盖 env（文件 > env > 默认；settings.rs）。
    // 在此之后 cfg 即"生效配置"——热态、静态快照全部从这里初始化。
    crate::settings::overlay_from_file(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的唯一键前缀，避免与真实环境变量或并行测试冲突。
    /// load() 读的是固定键名，所以这里用 set/unset 真实键，但所有测试串行运行。
    /// Cargo 默认多线程并行，故用互斥的 set_var + remove_var 配合串行 attribute。
    use std::sync::Mutex;

    // 保证本模块内测试串行执行（环境变量是全局状态）
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env() {
        for k in [
            "SAP_ASHOST",
            "SAP_SYSNR",
            "SAP_CLIENT",
            "SAP_USER",
            "SAP_PASSWD",
            "SAP_LANG",
            "SAP_LISTEN_ADDR",
            "SAP_API_KEY",
            "SAP_REQUEST_TIMEOUT_SECS",
            "SAP_POOL_IDLE_VALIDATE_SECS",
            "SAP_RATE_LIMIT_RPS",
            "SAP_ADT_BASE_URL",
            "SAP_ADT_USER",
            "SAP_ADT_PASSWD",
            "SAP_READ_ONLY",
            "SAP_UPDATE_CHECK",
            "SAP_REGISTRY_FILE",
            "SAP_MODE",
            "SAP_SYSTEMS_FILE",
            "SAP_SETUP",
            "SAP_OPEN_BROWSER",
        ] {
            unsafe {
                std::env::remove_var(k);
            }
        }
    }

    fn set_all_required() {
        unsafe {
            std::env::set_var("SAP_ASHOST", "sap.example.com");
            std::env::set_var("SAP_SYSNR", "00");
            std::env::set_var("SAP_CLIENT", "100");
            std::env::set_var("SAP_USER", "TESTUSER");
            std::env::set_var("SAP_PASSWD", "secret");
        }
    }

    #[test]
    fn load_success_with_defaults() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set_all_required();
        let cfg = load().unwrap();
        assert_eq!(
            cfg.conn_params[0],
            ("ASHOST", "sap.example.com".to_string())
        );
        assert_eq!(cfg.conn_params[1], ("SYSNR", "00".to_string()));
        assert_eq!(cfg.listen_addr, "127.0.0.1:3000"); // 默认值
                                                       // LANG 默认 EN
        assert_eq!(cfg.conn_params[5], ("LANG", "EN".to_string()));
    }

    #[test]
    fn load_respects_lang_and_listen_addr() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set_all_required();
        unsafe {
            std::env::set_var("SAP_LANG", "ZH");
            std::env::set_var("SAP_LISTEN_ADDR", "0.0.0.0:8080");
        }
        let cfg = load().unwrap();
        assert_eq!(cfg.conn_params[5], ("LANG", "ZH".to_string()));
        assert_eq!(cfg.listen_addr, "0.0.0.0:8080");
    }

    #[test]
    fn load_respects_request_timeout() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set_all_required();
        unsafe {
            std::env::set_var("SAP_REQUEST_TIMEOUT_SECS", "120");
        }
        let cfg = load().unwrap();
        assert_eq!(cfg.request_timeout, std::time::Duration::from_secs(120));
    }

    #[test]
    fn load_respects_pool_idle_validate() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set_all_required();
        unsafe {
            std::env::set_var("SAP_POOL_IDLE_VALIDATE_SECS", "5");
        }
        let cfg = load().unwrap();
        assert_eq!(cfg.pool_idle_validate, std::time::Duration::from_secs(5));
        // 0 = 禁用（永不触发校验）
        unsafe {
            std::env::set_var("SAP_POOL_IDLE_VALIDATE_SECS", "0");
        }
        let cfg = load().unwrap();
        assert_eq!(cfg.pool_idle_validate, std::time::Duration::MAX);
    }

    #[test]
    fn load_defaults_pool_idle_validate_to_30s() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set_all_required();
        let cfg = load().unwrap();
        assert_eq!(cfg.pool_idle_validate, std::time::Duration::from_secs(30));
    }

    #[test]
    fn load_parses_read_only_truthy_values() {
        let _g = ENV_LOCK.lock().unwrap();
        for (val, expect) in [("1", true), ("true", true), ("YES", true), ("On", true)] {
            clear_env();
            set_all_required();
            unsafe {
                std::env::set_var("SAP_READ_ONLY", val);
            }
            let cfg = load().unwrap();
            assert_eq!(cfg.read_only, expect, "SAP_READ_ONLY={val} 应为 {expect}");
        }
        // 假值/未设 → 关闭
        for val in ["0", "false", ""] {
            clear_env();
            set_all_required();
            unsafe {
                std::env::set_var("SAP_READ_ONLY", val);
            }
            assert!(!load().unwrap().read_only, "SAP_READ_ONLY={val} 应为 false");
        }
        clear_env();
        set_all_required();
        assert!(!load().unwrap().read_only, "未设 SAP_READ_ONLY 应为 false");
    }

    #[test]
    fn load_update_check_defaults_on_and_disables() {
        let _g = ENV_LOCK.lock().unwrap();
        // 未设 → 默认开启
        clear_env();
        set_all_required();
        assert!(
            load().unwrap().update_check,
            "未设 SAP_UPDATE_CHECK 应默认开启"
        );
        // off/false/0/no（大小写不敏感）→ 关闭
        for val in ["off", "false", "0", "NO", " Off "] {
            clear_env();
            set_all_required();
            unsafe {
                std::env::set_var("SAP_UPDATE_CHECK", val);
            }
            assert!(
                !load().unwrap().update_check,
                "SAP_UPDATE_CHECK={val} 应关闭"
            );
        }
        // 其余值（含 on/1/true）→ 开启
        for val in ["on", "1", "true", "anything"] {
            clear_env();
            set_all_required();
            unsafe {
                std::env::set_var("SAP_UPDATE_CHECK", val);
            }
            assert!(
                load().unwrap().update_check,
                "SAP_UPDATE_CHECK={val} 应开启"
            );
        }
    }

    #[test]
    fn load_defaults_request_timeout_to_60s() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set_all_required();
        let cfg = load().unwrap();
        assert_eq!(cfg.request_timeout, std::time::Duration::from_secs(60));
    }

    #[test]
    fn load_unconfigured_when_all_conn_vars_missing() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        // 五个连接变量全缺 → 未配置模式：空 conn_params（网关可启动，走 setup 引导）
        let cfg = load().unwrap();
        assert!(cfg.conn_params.is_empty(), "未配置模式 conn_params 应为空");
        assert!(cfg.adt_base_url.is_none(), "无 ashost 无法推导 ADT，应禁用");
        assert!(cfg.setup_enabled, "SAP_SETUP 未设应默认开启");
    }

    #[test]
    fn load_partial_conn_vars_still_errors() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        // 半截配置（有 ASHOST 缺其余）→ 保留 fail-fast
        unsafe {
            std::env::set_var("SAP_ASHOST", "h");
        }
        let err = load().unwrap_err();
        assert!(err.contains("SAP_SYSNR"), "半截配置应指出缺失变量: {}", err);
    }

    #[test]
    fn load_setup_switch_disables() {
        let _g = ENV_LOCK.lock().unwrap();
        for (val, expect) in [("off", false), ("0", false), ("NO", false), ("on", true)] {
            clear_env();
            unsafe {
                std::env::set_var("SAP_SETUP", val);
            }
            assert_eq!(
                load().unwrap().setup_enabled,
                expect,
                "SAP_SETUP={val} 应为 {expect}"
            );
        }
    }

    #[test]
    fn load_open_browser_defaults_on_and_disables() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        assert!(
            load().unwrap().open_browser,
            "未设 SAP_OPEN_BROWSER 应默认开启"
        );
        for val in ["off", "false", "0", "no"] {
            clear_env();
            unsafe {
                std::env::set_var("SAP_OPEN_BROWSER", val);
            }
            assert!(
                !load().unwrap().open_browser,
                "SAP_OPEN_BROWSER={val} 应关闭"
            );
        }
    }

    #[test]
    fn load_missing_ashost_errors() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        // 只设其他必填项，故意漏 ASHOST
        unsafe {
            std::env::set_var("SAP_SYSNR", "00");
            std::env::set_var("SAP_CLIENT", "100");
            std::env::set_var("SAP_USER", "U");
            std::env::set_var("SAP_PASSWD", "P");
        }
        let err = load().unwrap_err();
        assert!(
            err.contains("SAP_ASHOST"),
            "错误信息应指出缺失的变量: {}",
            err
        );
    }

    #[test]
    fn load_missing_passwd_errors() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        unsafe {
            std::env::set_var("SAP_ASHOST", "h");
            std::env::set_var("SAP_SYSNR", "00");
            std::env::set_var("SAP_CLIENT", "100");
            std::env::set_var("SAP_USER", "U");
            // 故意漏 PASSWD
        }
        let err = load().unwrap_err();
        assert!(err.contains("SAP_PASSWD"));
    }

    #[test]
    fn load_adt_defaults_and_overrides() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        set_all_required();
        // 未设 → 按 ashost 推导默认端口 50000，认证沿用 SAP_USER/PASSWD
        let cfg = load().unwrap();
        assert_eq!(
            cfg.adt_base_url.as_deref(),
            Some("http://sap.example.com:50000")
        );
        assert_eq!(cfg.adt_user, "TESTUSER");
        assert_eq!(cfg.adt_passwd, "secret");

        unsafe {
            std::env::set_var("SAP_ADT_BASE_URL", "https://sap.example.com:44300");
            std::env::set_var("SAP_ADT_USER", "ADTUSER");
            std::env::set_var("SAP_ADT_PASSWD", "adtpass");
        }
        let cfg = load().unwrap();
        assert_eq!(
            cfg.adt_base_url.as_deref(),
            Some("https://sap.example.com:44300")
        );
        assert_eq!(cfg.adt_user, "ADTUSER");

        // 显式空串 → 禁用
        unsafe {
            std::env::set_var("SAP_ADT_BASE_URL", "");
        }
        let cfg = load().unwrap();
        assert!(cfg.adt_base_url.is_none());
    }
}
