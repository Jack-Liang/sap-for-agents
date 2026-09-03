//! SAP 连接管理：多连接池 + 失败自动重连。
//!
//! 现实中 SAP 连接会因为网络抖动、系统重启、会话超时而失效。
//! 本模块维护一组可复用的连接，每个连接的 FFI 调用各自串行，
//! 但不同连接之间可并行——配合 tokio 的 `spawn_blocking`，handler
//! 并发请求能拿到不同连接并行执行。
//!
//! 连接生命周期：首次按需创建 → 复用 → 遇通信错误丢弃 → 池满后新建补充。
//! 对调用方完全透明（`with_connection` 接口与旧单连接版一致）。

use crate::connection::RfcConnection;
use crate::error::RfcError;
use crate::ffi::{
    RFC_ABAP_RUNTIME_FAILURE, RFC_CLOSED, RFC_COMMUNICATION_FAILURE, RFC_INVALID_HANDLE,
    RFC_LOGON_FAILURE, RFC_RC,
};
use std::sync::{Condvar, Mutex};

/// 触发「丢弃该连接」的 SAP 错误码集合。
///
/// 这些都是「连接已不可用 / 状态不可靠」类错误，复用废连接无意义，应销毁后新建。
/// 用 ffi 命名常量（值见 nwrfcsdk/include/sapnwrfc.h 的 _RFC_RC 枚举），不再写魔法数字，
/// 避免此前把 CONVERSION_FAILURE(22) 误当 CLOSED 的错误。
///
/// ## 为什么 RFC_INVALID_HANDLE(13) 也在里面
///
/// SDK 文档定义：「`RFC_INVALID_HANDLE, if the given rfcHandle is not connected`」
/// （sapnwrfc.h L1582）——只要句柄处于"非连接"状态就报这个，不仅仅是"已 close"。
///
/// 实证（dev_rfc.log 复现路径）：
/// 1. 一次 ABAP 短转储（code 3，如 `DATA_OFFSET_LENGTH_TOO_LARGE`）
///    → 池丢弃旧 conn、新 conn 入池
/// 2. 新 conn 在做下一次 FFI 调用时，SDK 检测该句柄在 SAP 端 CPIC 会话已死
///    → 返回 code 13（不是 code 1 的「no conversation found」——那次是
///    CPIC 层，下一次 FFI 时 SDK 看到句柄不可达就报 13）
/// 3. 之前 13 不在丢弃名单 → should_discard=false → conn 被 release 入池
/// 4. 后续请求 pop 到这个毒连接 → 又是 code 13 → 永久全挂，
///    直到 SAP NWRFC DLL 自身状态机崩溃（`process exit code: 1`）
///
/// 加入 13 后：code 13 一出现就立刻丢弃 + 新建，断开毒连接的死循环。
const RECONNECT_RC: [RFC_RC; 5] = [
    RFC_COMMUNICATION_FAILURE, // 1：网络/通信层失败（含 CPIC 「no conversation found」）
    RFC_LOGON_FAILURE,         // 2：登录/会话失效，重连可能恢复
    RFC_ABAP_RUNTIME_FAILURE,  // 3：SYSTEM_FAILURE(shortdump) 后连接状态不可靠
    RFC_CLOSED,                // 6：连接被对端/gateway 关闭
    RFC_INVALID_HANDLE,        // 13：句柄"非连接"——SAP 端会话已死后 SDK 的状态机错误
];

fn should_discard(err: &RfcError) -> bool {
    RECONNECT_RC.contains(&err.code)
}

/// 单次 wait_timeout 的等待时长（让循环定期醒来检查池状态）。
const WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// acquire 的总等待上限：超过则返回错误，避免池耗尽时调用方永久挂起。
const ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// 池的内部可变状态：空闲连接栈 + 当前总连接数。
struct PoolInner {
    idle: Vec<RfcConnection>,
    /// 当前已创建的连接总数（空闲 + 借出中）
    total: usize,
}

/// 多连接池：维护一组可复用的 SAP 连接。
///
/// - 空闲时连接留在池里复用（避免每次调用都握手）
/// - 借出时从空闲栈 pop；无空闲且未达 `max_size` 则新建
/// - 无空闲且已达上限则阻塞等待（Condvar），直到有连接归还
/// - 通信类错误归还时丢弃该连接（不回池），自然降总量
pub struct RfcConnectionPool {
    /// 连接参数：键为 'static 字面量，值为 owned String（新建连接时复用）
    params: Vec<(&'static str, String)>,
    /// 池上限（含空闲 + 借出）。超过则等待，不无限增长。
    max_size: usize,
    inner: Mutex<PoolInner>,
    /// 空闲连接可用时唤醒等待者
    cv: Condvar,
}

impl RfcConnectionPool {
    /// 创建池（默认上限 8）：立即建立首次连接，其余按需创建。
    #[allow(dead_code)]
    pub fn new(params: Vec<(&'static str, String)>) -> Result<Self, RfcError> {
        Self::with_max_size(params, 8)
    }

    /// 指定池上限创建。
    pub fn with_max_size(
        params: Vec<(&'static str, String)>,
        max_size: usize,
    ) -> Result<Self, RfcError> {
        let borrowed: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let conn = RfcConnection::new(&borrowed)?;
        let max_size = max_size.max(1); // 至少 1
        tracing::info!(max_size, "SAP 连接池已创建");
        Ok(Self {
            params,
            max_size,
            inner: Mutex::new(PoolInner {
                idle: vec![conn],
                total: 1,
            }),
            cv: Condvar::new(),
        })
    }

    /// 用一个连接执行闭包。接口与旧单连接版完全一致，调用方无需改动。
    ///
    /// - 闭包成功 → 归还连接，返回结果
    /// - 闭包失败但属通信类 → 丢弃连接，新建一个重试一次
    /// - 闭包失败且非通信类 → 归还连接（连接仍健康），返回错误
    ///
    /// 无空闲连接且未达上限时新建；已达上限则阻塞等待他人归还。
    pub fn with_connection<R, F>(&self, mut f: F) -> Result<R, RfcError>
    where
        F: FnMut(&RfcConnection) -> Result<R, RfcError>,
    {
        // 1. 借出一个连接
        let conn = self.acquire()?;

        // 2. 执行
        let result = f(&conn);

        match result {
            Ok(r) => {
                self.release(conn);
                Ok(r)
            }
            Err(e) if should_discard(&e) => {
                // 通信类错误：丢弃废连接，新建一个重试
                tracing::warn!(code = e.code, key = %e.key, "SAP 连接失败，丢弃并重试");
                self.discard_and_replenish(conn)?;
                // 重新借一个（此时池里至少有刚新建的那个）
                let conn2 = self.acquire()?;
                let r = f(&conn2);
                // 重试失败时也要按错误码决定丢弃/归还：
                // 若仍属"应丢弃"类（含 RFC_INVALID_HANDLE）→ conn2 也丢，避免把毒连接塞回池里
                // 否则（业务类错误，比如参数错）→ 归还，连接本身仍健康
                match &r {
                    Err(e2) if should_discard(e2) => {
                        tracing::warn!(code = e2.code, key = %e2.key, "SAP 重试连接也失败，丢弃（不归还）");
                        // 显式 drop conn2 触发 RfcCloseConnection；total 由 create_connection 加回去，
                        // 这里同步减回去，保持计数平衡。
                        drop(conn2);
                        if let Ok(mut guard) = self.inner.lock() {
                            guard.total = guard.total.saturating_sub(1);
                            self.cv.notify_one();
                        }
                    }
                    _ => self.release(conn2),
                }
                r
            }
            Err(e) => {
                // 非通信错误（参数错/ABAP 业务异常）：连接仍健康，归还
                self.release(conn);
                Err(e)
            }
        }
    }

    /// 借出一个连接：优先 pop 空闲；无空闲且未达上限则新建；达上限则等待。
    ///
    /// 等待有总超时上限（ACQUIRE_TIMEOUT），避免池耗尽时调用方永久挂起。
    fn acquire(&self) -> Result<RfcConnection, RfcError> {
        let mut guard = self.inner.lock().map_err(|e| poison_err("连接池锁", e))?;
        let deadline = std::time::Instant::now() + ACQUIRE_TIMEOUT;
        loop {
            // 有空闲：直接 pop
            if let Some(conn) = guard.idle.pop() {
                return Ok(conn);
            }
            // 无空闲但未达上限：新建（锁内不建连接——握手慢，会阻塞他人）
            if guard.total < self.max_size {
                guard.total += 1;
                // 释放锁后再建连接（避免持锁期间长时间阻塞其他 acquire）
                drop(guard);
                match self.create_connection() {
                    Ok(c) => return Ok(c),
                    Err(e) => {
                        // 新建失败：回滚计数
                        let mut g = self.inner.lock().map_err(|e2| poison_err("连接池锁", e2))?;
                        g.total -= 1;
                        // 新建失败可能意味着 SAP 挂了，唤醒等待者让他们也重试/失败
                        self.cv.notify_one();
                        return Err(e);
                    }
                }
            }
            // 已达上限且无空闲：等待归还（累计不超过 ACQUIRE_TIMEOUT）
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(RfcError {
                    code: -1,
                    message: format!(
                        "从连接池获取连接超时（等待 {:?}，池上限 {} 已耗尽）",
                        ACQUIRE_TIMEOUT, self.max_size
                    ),
                    ..Default::default()
                });
            }
            let wait_slice = deadline.saturating_duration_since(now).min(WAIT_TIMEOUT);
            guard = self
                .cv
                .wait_timeout(guard, wait_slice)
                .map_err(|e| poison_err("连接池等待", e))?
                .0;
        }
    }

    /// 归还健康连接到空闲栈，唤醒一个等待者。
    fn release(&self, conn: RfcConnection) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.idle.push(conn);
        }
        // 无论锁成功与否都唤醒（锁毒化时等待者会自行报错）
        self.cv.notify_one();
    }

    /// 丢弃废连接并补充一个新建连接（保持池容量）。
    /// 旧连接 drop 时自动 RfcCloseConnection。
    fn discard_and_replenish(&self, _discarded: RfcConnection) -> Result<(), RfcError> {
        // _discarded 在函数结束时 drop，自动关闭。
        // 先减计数（旧连接即将销毁），再建新的（计数加回）。
        {
            let mut guard = self.inner.lock().map_err(|e| poison_err("连接池锁", e))?;
            guard.total -= 1; // 旧连接销毁，腾出配额
        }
        // 新建并放回空闲栈
        let new_conn = match self.create_connection() {
            Ok(c) => c,
            Err(e) => {
                // 新建失败：把 total 加回，避免池容量永久缩水
                if let Ok(mut guard) = self.inner.lock() {
                    guard.total += 1;
                }
                // 唤醒等待者让他们也重试/失败
                self.cv.notify_one();
                return Err(e);
            }
        };
        {
            let mut guard = self.inner.lock().map_err(|e| poison_err("连接池锁", e))?;
            guard.total += 1;
            guard.idle.push(new_conn);
        }
        self.cv.notify_one();
        tracing::info!("SAP 连接已重建（池内补充）");
        Ok(())
    }

    /// 用保存的参数新建一个连接（无锁操作，调用方负责计数管理）。
    fn create_connection(&self) -> Result<RfcConnection, RfcError> {
        let borrowed: Vec<(&str, &str)> =
            self.params.iter().map(|(k, v)| (*k, v.as_str())).collect();
        RfcConnection::new(&borrowed)
    }

    /// 当前池状态快照（空闲 / 已建总数 / 上限），供 `/metrics` 采集。
    /// 锁毒化时返回 0（不阻塞调用方）。
    pub fn stats(&self) -> PoolStats {
        match self.inner.lock() {
            Ok(g) => PoolStats {
                idle: g.idle.len(),
                total: g.total,
                max: self.max_size,
            },
            Err(_) => PoolStats {
                idle: 0,
                total: 0,
                max: self.max_size,
            },
        }
    }
}

/// 连接池状态快照（供 `/metrics`）。
pub struct PoolStats {
    /// 空闲连接数（可立即复用）
    pub idle: usize,
    /// 已创建连接总数（空闲 + 借出中）
    pub total: usize,
    /// 池上限
    pub max: usize,
}

fn poison_err<T>(ctx: &str, _e: T) -> RfcError {
    RfcError {
        code: -1,
        message: format!("{}被毒化", ctx),
        key: String::new(),
        ..Default::default()
    }
}

// SAFETY: 与 RfcConnection 同理。池内部用 Mutex 串行化对连接 Vec 的访问，
// 每个借出的 RfcConnection 由调用方独占使用（spawn_blocking 闭包内不跨线程共享）。
// NWRFC SDK 允许不同连接对象在不同线程并发使用。
unsafe impl Send for RfcConnectionPool {}
unsafe impl Sync for RfcConnectionPool {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_discard_for_communication_errors() {
        // RECONNECT_RC = [COMMUNICATION_FAILURE(1), LOGON_FAILURE(2),
        //                 ABAP_RUNTIME_FAILURE(3), CLOSED(6)]
        for rc in [1i32, 2, 3, 6] {
            let err = RfcError {
                code: rc,
                ..Default::default()
            };
            assert!(should_discard(&err), "code={} 应触发丢弃重连", rc);
        }
    }

    #[test]
    fn should_not_discard_for_non_communication_errors() {
        // 非 RECONNECT_RC 的码不丢弃（连接仍健康）。
        // 注意：CONVERSION_FAILURE(22)、BUFFER_TOO_SMALL(23) 是数据/缓冲区问题，
        //       连接本身健康，复用即可——此前误把 22 当 CLOSED 导致无谓重连。
        for rc in [0i32, 5, 7, 9, 17, 20, 22, 23, 25, -1] {
            let err = RfcError {
                code: rc,
                ..Default::default()
            };
            assert!(!should_discard(&err), "code={} 不应触发丢弃", rc);
        }
    }
}
