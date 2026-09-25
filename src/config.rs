//! 配置加载：从环境变量（.env 文件或真实环境变量）读取 SAP 连接参数与监听地址。
//!
//! 必填项缺失时给出明确的中文报错，便于部署排查。

use std::env;

/// 服务运行配置
#[derive(Debug)]
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
}

fn required(key: &str) -> Result<String, String> {
    env::var(key).map_err(|_| format!("缺少必填环境变量: {}", key))
}

/// 从环境变量读取配置。调用方应先执行 dotenvy::dotenv()。
pub fn load() -> Result<AppConfig, String> {
    let ashost = required("SAP_ASHOST")?;
    let sysnr = required("SAP_SYSNR")?;
    let client = required("SAP_CLIENT")?;
    let user = required("SAP_USER")?;
    let passwd = required("SAP_PASSWD")?;
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
    // 可选限流（按 IP 的每秒请求数；未设/0/非法 → None=不限流）
    let rate_limit_rps = env::var("SAP_RATE_LIMIT_RPS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&r| r >= 1);
    // ADT 代理基地址：未设 → 按 ashost 推导默认；显式空串 → 禁用
    let adt_base_url = match env::var("SAP_ADT_BASE_URL") {
        Ok(s) if s.trim().is_empty() => None,
        Ok(s) => Some(s.trim().to_string()),
        Err(_) => Some(format!("http://{}:50000", ashost)),
    };
    let adt_user = env::var("SAP_ADT_USER").unwrap_or_else(|_| user.clone());
    let adt_passwd = env::var("SAP_ADT_PASSWD").unwrap_or_else(|_| passwd.clone());
    // 只读模式：识别 1/true/yes/on（大小写不敏感），其余值视为关闭
    let read_only = env::var("SAP_READ_ONLY")
        .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
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

    // 键为 'static 字面量，值使用环境变量的 String（运行期存活）
    Ok(AppConfig {
        conn_params: vec![
            ("ASHOST", ashost),
            ("SYSNR", sysnr),
            ("CLIENT", client),
            ("USER", user),
            ("PASSWD", passwd),
            ("LANG", lang),
        ],
        listen_addr,
        pool_size,
        pool_idle_validate,
        api_key,
        request_timeout,
        rate_limit_rps,
        adt_base_url,
        adt_user,
        adt_passwd,
        read_only,
        registry_file,
        runtime_mode,
        update_check,
    })
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
        assert!(load().unwrap().update_check, "未设 SAP_UPDATE_CHECK 应默认开启");
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
            assert!(load().unwrap().update_check, "SAP_UPDATE_CHECK={val} 应开启");
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
        assert_eq!(cfg.adt_base_url.as_deref(), Some("http://sap.example.com:50000"));
        assert_eq!(cfg.adt_user, "TESTUSER");
        assert_eq!(cfg.adt_passwd, "secret");

        unsafe {
            std::env::set_var("SAP_ADT_BASE_URL", "https://sap.example.com:44300");
            std::env::set_var("SAP_ADT_USER", "ADTUSER");
            std::env::set_var("SAP_ADT_PASSWD", "adtpass");
        }
        let cfg = load().unwrap();
        assert_eq!(cfg.adt_base_url.as_deref(), Some("https://sap.example.com:44300"));
        assert_eq!(cfg.adt_user, "ADTUSER");

        // 显式空串 → 禁用
        unsafe {
            std::env::set_var("SAP_ADT_BASE_URL", "");
        }
        let cfg = load().unwrap();
        assert!(cfg.adt_base_url.is_none());
    }
}
