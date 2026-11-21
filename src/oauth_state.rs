//! OAuth authorization_code 流的 refresh_token 持久化（v0.23）。
//!
//! 存储 JSON 文件（`SAP_OAUTH_STATE_FILE`，默认 `./sap_oauth_state.json`）：
//! 系统名 → refresh_token + 授权元数据（用户 / 时间，仅展示用）。
//!
//! 为什么独立落盘而不是进 `sap_systems.toml`：toml 里的密钥值是 `${VAR}`
//! 引用（连接时插值），运行期获取的 refresh token 没有 toml 归处；信任级别
//! 与 `sap_gateway.toml`（明文 API key）一致——0600 + 原子写（tmp → rename）。
//!
//! 写入时机：① 授权完成（setup 层 `record_authorization`，带元数据）；
//! ② XSUAA 轮换 refresh token（adt.rs 换新钩子 `store_refresh`，保元数据）。
//! 读取时机：系统句柄构建（启动装配 / setup 热加载）水合进内存缓存。
//! 删除系统时 `drop_system` 同步清理。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static PATH: OnceLock<PathBuf> = OnceLock::new();
/// 文件读改写互斥（防两个并发授权/轮换丢更新，registry 同款）
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// 注册持久化文件路径（main 启动期调一次；测试可显式传入临时路径）。
pub fn init(path: PathBuf) {
    let _ = PATH.set(path);
}

fn path() -> PathBuf {
    PATH.get().cloned().unwrap_or_else(|| PathBuf::from("sap_oauth_state.json"))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SystemEntry {
    refresh_token: String,
    /// 授权用户（XSUAA token 的 `sub`，仅展示，不验签）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    /// 授权时间（ISO8601，仅展示）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    authorized_at: Option<String>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Store {
    version: u32,
    systems: BTreeMap<String, SystemEntry>,
}

/// 读整个文件（无文件/损坏 → None，损坏打 WARN 不致命——授权可重来）。
fn read_store() -> Option<Store> {
    let p = path();
    let raw = match std::fs::read_to_string(&p) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %p.display(), error = %e, "OAuth state 文件不可读（按未授权处理）");
            return None;
        }
    };
    match serde_json::from_str::<Store>(&raw) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(path = %p.display(), error = %e, "OAuth state 文件损坏（按未授权处理——重新授权即可）");
            None
        }
    }
}

/// 原子写（0600 tmp → rename，registry 同款）。
fn write_store(store: &Store) -> Result<(), String> {
    let p = path();
    let tmp = p.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    crate::config::write_private_file(&tmp, &json)?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("rename 失败 ({}): {}", p.display(), e))?;
    Ok(())
}

/// 写入 refresh token（保留既有元数据）。adt.rs 轮换钩子用。
pub fn store_refresh(system: &str, token: &str) -> Result<(), String> {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = read_store().unwrap_or_default();
    let entry = store.systems.entry(system.to_string()).or_insert_with(|| SystemEntry {
        refresh_token: String::new(),
        user: None,
        authorized_at: None,
    });
    entry.refresh_token = token.to_string();
    write_store(&store)
}

/// 授权完成落盘（refresh token + 用户元数据）。setup 层用。
pub fn record_authorization(system: &str, token: &str, user: Option<&str>) -> Result<(), String> {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = read_store().unwrap_or_default();
    store.systems.insert(
        system.to_string(),
        SystemEntry {
            refresh_token: token.to_string(),
            user: user.map(str::to_string),
            authorized_at: Some(crate::registry::now_iso8601_public()),
        },
    );
    write_store(&store)
}

/// 水合读：该系统已存的 refresh token（系统句柄构建时注入内存缓存）。
pub fn load_refresh(system: &str) -> Option<String> {
    read_store()?
        .systems
        .get(system)
        .map(|e| e.refresh_token.clone())
}

/// 授权状态元数据（`GET /api/setup/system/{name}` 的 oauth 展示）。
pub fn entry_info(system: &str) -> Option<(Option<String>, Option<String>)> {
    read_store()?.systems.get(system).map(|e| (e.user.clone(), e.authorized_at.clone()))
}

/// 系统删除时清理对应条目（与 toml 删除同节奏；无条目静默）。
pub fn drop_system(system: &str) {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = read_store().unwrap_or_default();
    if store.systems.remove(system).is_some() {
        if let Err(e) = write_store(&store) {
            tracing::warn!(system = %system, error = %e, "OAuth state 清理失败（残留条目无害）");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全局 PATH 是 OnceLock——本模块唯一 init 点，整个 roundtrip 在单测内
    /// 串行断言，避免与其他测试的初始化顺序耦合。
    #[test]
    fn state_file_roundtrip_rotation_and_corruption_tolerance() {
        let dir = std::env::temp_dir().join(format!(
            "sfa-oauth-state-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("state.json");
        let _ = std::fs::remove_file(&p);
        init(p.clone());

        // 未授权：读不到
        assert_eq!(load_refresh("sys1"), None);
        assert_eq!(entry_info("sys1"), None);

        // 授权落盘：token + 元数据
        record_authorization("sys1", "rt-1", Some("user@example.com")).unwrap();
        assert_eq!(load_refresh("sys1").as_deref(), Some("rt-1"));
        let (user, at) = entry_info("sys1").unwrap();
        assert_eq!(user.as_deref(), Some("user@example.com"));
        assert!(at.is_some(), "授权时间应记录");

        // 轮换只动 token，保留元数据（XSUAA refresh grant 可能轮换 token）
        store_refresh("sys1", "rt-2").unwrap();
        assert_eq!(load_refresh("sys1").as_deref(), Some("rt-2"));
        let (user2, _) = entry_info("sys1").unwrap();
        assert_eq!(user2.as_deref(), Some("user@example.com"));

        // 0600（unix）——refresh token 是密钥，同 sap_gateway.toml 信任级别
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "state 文件必须是 0600");
        }

        // drop 清理（系统删除同节奏）
        drop_system("sys1");
        assert_eq!(load_refresh("sys1"), None);
        assert_eq!(entry_info("sys1"), None);

        // 损坏文件 → 按未授权处理，不 panic、可恢复
        std::fs::write(&p, "{ broken").unwrap();
        assert_eq!(load_refresh("sys1"), None);
        record_authorization("sys1", "rt-3", None).unwrap();
        assert_eq!(load_refresh("sys1").as_deref(), Some("rt-3"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
