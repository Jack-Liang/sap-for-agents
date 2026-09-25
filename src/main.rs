mod adt;
mod api;
mod auth;
mod config;
mod connection;
mod discovery;
mod dumps;
mod error;
mod executor;
mod ffi;
mod function;
mod invoke;
mod metadata;
mod objects;
mod openapi;
mod mcp;
mod pool;
mod registry;
mod server;
mod server_config;
mod server_rfc;
mod string_utils;
mod version;

use crate::pool::RfcConnectionPool;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. 初始化结构化日志（受 RUST_LOG 环境变量控制，默认 info）
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("=== Rust SAP RFC 服务启动 ===");

    // 2. 加载 .env（找不到文件不报错，可由真实环境变量替代）
    let _ = dotenvy::dotenv();

    // 3. 决定运行模式：client（默认）/ server / both
    let role = std::env::var("SAP_ROLE").unwrap_or_else(|_| "client".to_string());
    tracing::info!(%role, "运行模式");

    match role.as_str() {
        "client" => run_client().await?,
        "server" => run_server().await?,
        "both" => {
            // both 模式：server 在独立 OS 线程（dispatch 阻塞），client 在当前 tokio
            let server_thread = std::thread::spawn(run_server_blocking);

            // client 并行跑（直到停机或出错）
            tokio::select! {
                res = run_client() => {
                    if let Err(e) = res {
                        tracing::error!(?e, "client 模式异常退出");
                    }
                }
                _ = wait_shutdown_signal() => {
                    tracing::info!("收到停机信号");
                }
            }
            // 等 server 线程（gateway 断开后自动退出）
            let _ = server_thread.join();
        }
        other => {
            return Err(
                format!("未知的 SAP_ROLE='{}'，可选: client / server / both", other).into(),
            );
        }
    }

    tracing::info!("服务已停止");
    Ok(())
}

/// client 模式：现有 HTTP server（SAP client → REST）
async fn run_client() -> Result<(), Box<dyn std::error::Error>> {
    let dotenv_result = dotenvy::dotenv();
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => {
            print_config_guide(&e, dotenv_result.is_err());
            return Err(e.into());
        }
    };
    tracing::info!(listen = cfg.listen_addr, "client 配置加载完成");

    // macOS：SAP SDK 的 ICU 依赖是裸名引用，rpath 救不了——直跑二进制且未设
    // DYLD_LIBRARY_PATH 时 SDK 会以晦涩的 255 退出。提前 dlopen 探测给友好指引。
    #[cfg(target_os = "macos")]
    if let Err(e) = probe_macos_icu() {
        eprintln!();
        eprintln!("❌ SAP SDK 的 ICU 库加载失败: {}", e);
        eprintln!("   macOS 上 libsapnwrfc.dylib 以裸名引用 ICU（rpath 不生效）。两种解法：");
        eprintln!("   1. 用启动脚本:  ./start.sh   (自动设置库路径)");
        eprintln!("   2. 直跑二进制前设置:  export DYLD_LIBRARY_PATH=<项目>/nwrfcsdk/lib/darwin-aarch64");
        eprintln!("   （详见 README §Quick Start）");
        std::process::exit(255);
    }

    // 版本自描述（/api/version）：客户端号来自本地配置，零 RFC 成本
    // （须在 conn_params move 进连接池之前取出）
    let sap_client = cfg
        .conn_params
        .iter()
        .find(|(k, _)| *k == "CLIENT")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    version::init_sap_client(sap_client);

    let pool = RfcConnectionPool::with_max_size(
        cfg.conn_params,
        cfg.pool_size,
        cfg.pool_idle_validate,
    )?;
    tracing::info!(pool_size = cfg.pool_size, "SAP 系统连接成功（多连接池）");
    // 认证：未设 SAP_API_KEY → None（免鉴权）；设置后 /api/* 要求 Bearer token
    let auth_enabled = cfg.api_key.is_some();
    auth::init(cfg.api_key);
    tracing::info!(auth_enabled, "API 认证配置");
    // 全局请求超时（/api/rfc 还支持 per-request timeout_secs 覆盖）
    server::init_request_timeout(cfg.request_timeout);
    // 只读模式（SAP_READ_ONLY）：拦截网关写端点（objects 写 / ADT 写方法）
    server::init_read_only(cfg.read_only);
    if cfg.read_only {
        tracing::warn!("🔒 只读模式已启用（SAP_READ_ONLY）：/api/objects 写端点与 /api/adt 写方法返回 403；/api/rfc 不受影响");
    }
    // Prometheus 指标（GET /metrics）
    server::init_metrics();
    // 可选限流（按 IP；未设 SAP_RATE_LIMIT_RPS 则不限流）
    server::init_rate_limiter(cfg.rate_limit_rps);
    // ADT REST 代理（/api/adt/**；SAP_ADT_BASE_URL 为空串时禁用）
    adt::init(
        cfg.adt_base_url,
        &cfg.adt_user,
        &cfg.adt_passwd,
        cfg.request_timeout,
    );
    // 运行档位（SAP_MODE=runtime）：消费方运行模式——只留交付端口调用面
    server::init_runtime_mode(cfg.runtime_mode);
    if cfg.runtime_mode {
        tracing::warn!("🚀 运行档位（SAP_MODE=runtime）：仅保留 GET /api/registry 与 POST /api/invokes/**，其余 /api/* 返回 403 RUNTIME_MODE");
    }
    // API 注册表（v0.12：Agent 跨会话记忆；本地 JSON 文件，坏文件自动备份）
    registry::init(cfg.registry_file.clone());
    tracing::info!(
        path = %cfg.registry_file.display(),
        "API 注册表已启用（GET /api/registry）"
    );
    let shared: server::SharedPool = Arc::new(pool);

    // 注册表服务目录预热（v0.13）：后台 watcher 维护 published 条目的类型化
    // operation 缓存，公开的 /openapi.json 只读缓存——不被 SAP 健康状况绑架
    tokio::spawn(openapi::registry_ops_watcher(Arc::clone(&shared)));

    // 新版本检查（GitHub Releases，后台任务：启动即查 + 每 24h；失败静默）
    if cfg.update_check {
        tokio::spawn(version::update_checker_loop());
    } else {
        tracing::info!("新版本检查已禁用（SAP_UPDATE_CHECK=off）");
    }

    server::run(shared, &cfg.listen_addr, wait_shutdown_signal()).await?;
    Ok(())
}

/// server 模式：SAP server（被 SAP 回调 → webhook 转发）
async fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    let servers_path =
        std::env::var("SERVERS_CONFIG").unwrap_or_else(|_| "servers.toml".to_string());
    let cfg = server_config::load(&servers_path)
        .map_err(|e| format!("Server 配置加载失败 ({}): {}", servers_path, e))?;
    tracing::info!(path = %servers_path, funcs = cfg.functions.len(), "server 配置加载完成");

    // server_rfc::run 阻塞，放独立线程；主线程等停机信号
    let handle = std::thread::spawn(move || {
        if let Err(e) = server_rfc::run(&cfg) {
            tracing::error!(code = e.code, msg = %e.message, "server 运行失败");
        }
    });

    wait_shutdown_signal().await;
    tracing::info!("收到停机信号，等待 server 线程退出（gateway 断开后自动退出）");
    let _ = handle.join();
    Ok(())
}

/// server 模式的阻塞版本（both 模式内部用）
fn run_server_blocking() {
    let servers_path =
        std::env::var("SERVERS_CONFIG").unwrap_or_else(|_| "servers.toml".to_string());
    let cfg = match server_config::load(&servers_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Server 配置加载失败: {}", e);
            return;
        }
    };
    if let Err(e) = server_rfc::run(&cfg) {
        tracing::error!(code = e.code, msg = %e.message, "server 运行失败");
    }
}

/// 等待停机信号（Ctrl+C / SIGTERM）
async fn wait_shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("安装 ctrl-c 信号处理器失败");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("安装 SIGTERM 信号处理器失败")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("收到 Ctrl+C 信号"),
        _ = terminate => tracing::info!("收到 SIGTERM 信号"),
    }
}

/// 配置缺失时的友好引导。检测「无 .env 文件」这一典型场景，给出针对性步骤。
/// `dotenv_not_found` 为 true 表示项目根目录没有 .env 文件。
fn print_config_guide(err: &str, dotenv_not_found: bool) {
    eprintln!();
    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    eprintln!("  ❌ 配置加载失败: {}", err);
    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    eprintln!();
    if dotenv_not_found {
        eprintln!("  未检测到 .env 文件。请按以下步骤操作：");
        eprintln!();
        eprintln!("  1. 复制配置模板：");
        eprintln!("       cp .env.example .env        (Linux/macOS)");
        eprintln!("       copy .env.example .env      (Windows CMD)");
        eprintln!("       Copy-Item .env.example .env (PowerShell)");
        eprintln!();
        eprintln!("  2. 编辑 .env，填入 SAP 连接参数：");
        eprintln!("       SAP_ASHOST=<SAP 应用服务器地址>");
        eprintln!("       SAP_SYSNR=00");
        eprintln!("       SAP_CLIENT=001");
        eprintln!("       SAP_USER=<你的账号>");
        eprintln!("       SAP_PASSWD=<你的密码>");
        eprintln!();
        eprintln!("  3. 重新运行：cargo run --release  （或 ./start.sh / start.ps1）");
    } else {
        eprintln!("  .env 文件已存在，但缺少必填项或值无效。");
        eprintln!("  请检查 .env 中的以下变量是否都已填写：");
        eprintln!("    SAP_ASHOST / SAP_SYSNR / SAP_CLIENT / SAP_USER / SAP_PASSWD");
        eprintln!("  完整字段说明见 README.md §3 配置。");
    }
    eprintln!();
}

/// macOS 启动期 ICU 探测：dlopen 依次尝试 SDK 目录与系统搜索路径。
/// 任何一个能打开即通过（SDK 运行时会按同样顺序解析）。
#[cfg(target_os = "macos")]
fn probe_macos_icu() -> Result<(), String> {
    let libs = ["libicuuc57.dylib", "libicudata57.dylib", "libicui18n57.dylib"]
        .map(String::from);
    // 与 build.rs 同款目录约定：nwrfcsdk/lib/darwin-<arch>
    let arch = match std::env::var("SAP_SDK_DIR") {
        Ok(dir) => format!("{}/lib/darwin-{}", dir, std::env::consts::ARCH),
        Err(_) => format!("./nwrfcsdk/lib/darwin-{}", std::env::consts::ARCH),
    };
    for lib in &libs {
        let direct = format!("{}/{}", arch, lib);
        // 先试 SDK 目录，再试默认搜索（dlopen NULL 不需要，直接全名）
        let tried: Vec<std::ffi::CString> = [direct.clone(), lib.clone()]
            .iter()
            .map(|p| std::ffi::CString::new(p.as_str()).unwrap())
            .collect();
        // RTLD_LAZY | RTLD_LOCAL（2 | 256，macOS 常量）；dlopen 绑定在 unsafe 内完成
        let ok = tried.iter().any(|p| {
            let h = libc_dlopen(p.as_ptr(), 0x02 | 0x100);
            if !h.is_null() {
                libc_dlclose(h);
                true
            } else {
                false
            }
        });
        if !ok {
            return Err(format!("{} (tried: {:?})", lib, tried));
        }
    }
    Ok(())
}

/// dlopen 极简绑定（仅启动探测用；不引 libc crate）
#[cfg(target_os = "macos")]
fn libc_dlopen(path: *const std::ffi::c_char, mode: std::ffi::c_int) -> *mut std::ffi::c_void {
    extern "C" {
        fn dlopen(path: *const std::ffi::c_char, mode: std::ffi::c_int) -> *mut std::ffi::c_void;
    }
    unsafe { dlopen(path, mode) }
}

#[cfg(target_os = "macos")]
fn libc_dlclose(handle: *mut std::ffi::c_void) -> std::ffi::c_int {
    extern "C" {
        fn dlclose(handle: *mut std::ffi::c_void) -> std::ffi::c_int;
    }
    unsafe { dlclose(handle) }
}
