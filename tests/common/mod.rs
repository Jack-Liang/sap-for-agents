//! 真实 SAP 集成测试的公共辅助。
//!
//! 这些测试需要真实 SAP 连接（localhost:00），用 #[ignore] 标记，
//! 默认 `cargo test` 跳过，用 `cargo test -- --ignored` 按需运行。
//!
//! 策略：启动真实 HTTP server（复用生产 binary 的 server::app），
//! 通过 HTTP 客户端测端到端行为——测的就是用户真实接口。

use std::sync::atomic::{AtomicU16, Ordering};

/// 每个测试分配一个唯一的监听端口，避免并行测试端口冲突。
/// 从 13600 开始递增（避开常用端口）。
static NEXT_PORT: AtomicU16 = AtomicU16::new(13600);

/// 检查 SAP 环境变量是否就绪，未就绪则跳过测试。
pub fn ensure_sap_env() {
    // 集成测试需要 SAP 连接。.env 在 cargo test 运行时不会被自动加载，
    // 这里手动加载 + 检查关键变量。
    let _ = dotenvy::dotenv();
    for key in [
        "SAP_ASHOST",
        "SAP_SYSNR",
        "SAP_CLIENT",
        "SAP_USER",
        "SAP_PASSWD",
    ] {
        if std::env::var(key).is_err() {
            eprintln!("跳过 SAP 集成测试：缺少环境变量 {}", key);
            std::process::exit(0);
        }
    }
}

/// 分配一个唯一端口。
pub fn alloc_port() -> u16 {
    NEXT_PORT.fetch_add(1, Ordering::SeqCst)
}
