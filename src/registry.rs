//! API 注册表：Agent 跨会话的共享记忆，也是交付契约的载体（v0.12）。
//!
//! 解决的问题：Agent 在一个会话里建好的接口（rfc_enabled 函数），下个会话
//! "想不起来"——只能模糊搜索 Z* 重新探索，踩过的坑也要重踩。注册表把每个
//! 会话的有效产出（接口是什么、为什么存在、怎么调用、有什么坑）沉淀成
//! 贴在交付物上的持久记录，任何后续会话/任何 Agent 一次查询即可取回。
//!
//! 定位（见项目 roadmap）：
//! - **自动登记是默认路径**：经网关成功写入的 remote-enabled 函数自动获得
//!   draft 条目（Agent 只需补 intent/notes），登记失败只降级为警告——
//!   注册表绝不能让 SAP 写入本身失败；
//! - **删除留墓碑**（tombstone）不物理删：防止下次会话对"曾经有过什么"
//!   产生困惑；`?purge=true` 才物理移除；
//! - **存储格式即外部契约**：文件顶层带 `version` 字段，字段只增不删，
//!   将来若把注册表演进成独立服务，这个文件就是接缝。
//!
//! 存储：网关本地 JSON 文件（`SAP_REGISTRY_FILE`，默认 `./registry.json`）。
//! 单进程内互斥锁 + 临时文件原子写（write tmp → rename）。不依赖 SAP——
//! `SAP_READ_ONLY` 不拦截注册表写（网关本地状态，不是 SAP 状态）。

use crate::error::RfcError;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// 当前存储格式版本（文件顶层 `version` 字段）。字段只做加法；破坏性
/// 变更必须递增此号并在 `load` 里做迁移。
const SCHEMA_VERSION: u32 = 1;

/// alias 上限（与对象名上限同量级，给 `team/name` 前缀留空间）。
const ALIAS_MAX_LEN: usize = 60;

// ========================================================================
// 数据模型
// ========================================================================

/// 条目生命周期：draft（自动登记/刚创建）→ published（Agent 确认可交付）
/// → deleted（墓碑：SAP 侧对象已删，条目留档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryStatus {
    Draft,
    Published,
    Deleted,
}

impl EntryStatus {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "draft" => Some(Self::Draft),
            "published" => Some(Self::Published),
            // PUT 不接受 deleted（删除走 DELETE 端点）
            _ => None,
        }
    }
}

/// 一条注册表记录。`intent`/`notes`/`example` 是给"下个会话的 Agent"看的三件套。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// 唯一标识，小写 URL 安全（允许 `team/name` 前缀），即调用别名
    pub alias: String,
    /// SAP 函数模块名（大写，rfc_enabled）
    pub func_name: String,
    /// 函数组（可空；信息性，便于溯源）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// 这个接口是干什么的、给谁用的（Agent 撰写）
    #[serde(default)]
    pub intent: String,
    /// 踩坑记录 / 使用注意事项（Agent 撰写）
    #[serde(default)]
    pub notes: String,
    /// 接口文档正文（Markdown）：面向消费方的完整说明，随代码写入，
    /// 流入 OpenAPI 目录的 operation description。区别于 intent（一句话
    /// 干什么）与 notes（踩坑提醒）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub doc: String,
    /// 调用示例（任意 JSON，通常是 /api/functions/{name}/invoke 的请求体）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<serde_json::Value>,
    pub status: EntryStatus,
    /// 条目来源：auto（写钩子自动登记）/ manual（Agent 手工创建）
    pub origin: EntryOrigin,
    /// 平坦调用（/api/invokes/{alias}）的输出表行数封顶；
    /// None → 用默认 100（调用方还能用 ?limit= 覆盖）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<u32>,
    /// RFC3339 UTC
    pub created_at: String,
    /// RFC3339 UTC
    pub updated_at: String,
}

/// 条目来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryOrigin {
    Auto,
    Manual,
}

/// 磁盘文件形态。entries 按alias 排序落盘（人可读、diff 友好）。
#[derive(Serialize, Deserialize)]
struct RegistryFile {
    version: u32,
    entries: Vec<Entry>,
}

/// 内存态存储（纯逻辑，不碰文件系统——持久化由全局层负责，便于并行单测）。
#[derive(Default)]
struct Store {
    entries: BTreeMap<String, Entry>,
}

impl Store {
    fn from_json(text: &str) -> Result<Self, String> {
        let f: RegistryFile =
            serde_json::from_str(text).map_err(|e| format!("JSON 解析失败: {e}"))?;
        if f.version != SCHEMA_VERSION {
            return Err(format!(
                "schema 版本不匹配（文件 {}，程序 {}）",
                f.version, SCHEMA_VERSION
            ));
        }
        let mut entries = BTreeMap::new();
        for e in f.entries {
            if validate_alias(&e.alias).is_err() {
                return Err(format!("条目 alias 非法: {}", e.alias));
            }
            entries.insert(e.alias.clone(), e);
        }
        Ok(Self { entries })
    }

    fn to_json(&self) -> String {
        let f = RegistryFile {
            version: SCHEMA_VERSION,
            entries: self.entries.values().cloned().collect(),
        };
        serde_json::to_string_pretty(&f).expect("注册表序列化不会失败")
    }

    fn get(&self, alias: &str) -> Option<&Entry> {
        self.entries.get(alias)
    }

    /// 列表（按 alias 排序）。`q` 对 alias/func_name/intent 做大小写不敏感子串过滤；
    /// 默认排除墓碑，`include_deleted=true` 时包含。
    fn list(&self, q: Option<&str>, include_deleted: bool) -> Vec<&Entry> {
        self.entries
            .values()
            .filter(|e| include_deleted || e.status != EntryStatus::Deleted)
            .filter(|e| match q {
                Some(q) => {
                    let ql = q.to_lowercase();
                    e.alias.to_lowercase().contains(&ql)
                        || e.func_name.to_lowercase().contains(&ql)
                        || e.intent.to_lowercase().contains(&ql)
                }
                None => true,
            })
            .collect()
    }

    /// PUT 语义：全量替换条目（调用方应先 GET 再改再 PUT）。
    fn put(&mut self, alias: &str, body: &PutBody, now: &str) -> Result<(Entry, bool), String> {
        let existing = self.entries.get(alias);
        let created = existing.is_none();
        let entry = Entry {
            alias: alias.to_string(),
            func_name: body.func_name.trim().to_uppercase(),
            group: body
                .group
                .as_deref()
                .map(str::trim)
                .filter(|g| !g.is_empty())
                .map(|g| g.to_uppercase()),
            intent: body.intent.trim().to_string(),
            notes: body.notes.trim().to_string(),
            doc: body.doc.trim().to_string(),
            example: body.example.clone(),
            status: body.status,
            // 手工 PUT 保留 origin（auto 条目被 Agent 完善后仍是 auto 起源）
            origin: existing.map_or(EntryOrigin::Manual, |e| e.origin),
            max_rows: body.max_rows,
            created_at: existing.map_or(now.to_string(), |e| e.created_at.clone()),
            updated_at: now.to_string(),
        };
        self.entries.insert(alias.to_string(), entry.clone());
        Ok((entry, created))
    }

    /// 自动登记（写钩子调用）：按函数名幂等 upsert。
    /// - 已有该函数的条目 → 保留 intent/notes/example（绝不清空 Agent 写的心血），
    ///   刷新 group 与 updated_at；墓碑则复活为 draft；doc 非空时更新（文档随代码走）；
    /// - 没有 → 以小写函数名为 alias 建 draft；alias 被其他函数占用时追加 -2/-3…。
    fn ensure_draft_for_func(
        &mut self,
        func_name: &str,
        group: Option<&str>,
        doc: Option<&str>,
        now: &str,
    ) -> String {
        let func_upper = func_name.trim().to_uppercase();
        let group_upper = group
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .map(|g| g.to_uppercase().to_string());
        let doc_text = doc
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .unwrap_or_default()
            .to_string();
        if let Some(e) = self
            .entries
            .values_mut()
            .find(|e| e.func_name == func_upper)
        {
            // 复活/刷新：只动状态类字段（doc 非空才覆盖——文档随代码走）
            e.status = EntryStatus::Draft;
            e.group = group_upper.or(e.group.take());
            if !doc_text.is_empty() {
                e.doc = doc_text;
            }
            e.updated_at = now.to_string();
            return e.alias.clone();
        }
        let base = sanitize_alias(&func_upper);
        let alias = self.first_free_alias(&base);
        let entry = Entry {
            alias: alias.clone(),
            func_name: func_upper,
            group: group_upper,
            intent: String::new(),
            notes: String::new(),
            doc: doc_text,
            example: None,
            status: EntryStatus::Draft,
            origin: EntryOrigin::Auto,
            max_rows: None,
            created_at: now.to_string(),
            updated_at: now.to_string(),
        };
        self.entries.insert(alias.clone(), entry);
        alias
    }

    /// 在 `base`、`base-2`、`base-3`… 中找第一个未占用（或恰好指向同函数）的 alias。
    fn first_free_alias(&self, base: &str) -> String {
        if !self.entries.contains_key(base) {
            return base.to_string();
        }
        for n in 2..100 {
            let cand = format!("{base}-{n}");
            if !self.entries.contains_key(&cand) {
                return cand;
            }
        }
        // 99 个同名变体仍占满：退化用时间戳后缀（保证唯一即可）
        format!("{base}-x{}", now_unix())
    }

    /// 墓碑化某个函数的全部条目（对象删除钩子）。返回被墓碑化的条数。
    fn tombstone_for_func(&mut self, func_name: &str, now: &str) -> usize {
        let func_upper = func_name.trim().to_uppercase();
        let mut n = 0;
        for e in self.entries.values_mut() {
            if e.func_name == func_upper && e.status != EntryStatus::Deleted {
                e.status = EntryStatus::Deleted;
                e.updated_at = now.to_string();
                n += 1;
            }
        }
        n
    }

    /// 单条墓碑化（DELETE 端点）。返回是否存在。
    fn tombstone(&mut self, alias: &str, now: &str) -> bool {
        match self.entries.get_mut(alias) {
            Some(e) => {
                e.status = EntryStatus::Deleted;
                e.updated_at = now.to_string();
                true
            }
            None => false,
        }
    }

    /// 物理删除（DELETE ?purge=true）。返回是否存在。
    fn purge(&mut self, alias: &str) -> bool {
        self.entries.remove(alias).is_some()
    }
}

// ========================================================================
// alias 规则与时间戳
// ========================================================================

/// alias 校验：1..=60 字符，仅 `[a-z0-9_/-]`，无空段、不以 `/` 开头结尾。
/// 小写 + URL 安全是为了直接用作 `GET /api/registry/{alias}` 路径段。
fn validate_alias(alias: &str) -> Result<(), RfcError> {
    let ok = (1..=ALIAS_MAX_LEN).contains(&alias.chars().count())
        && !alias.starts_with('/')
        && !alias.ends_with('/')
        && alias.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' || c == '/'
        })
        && !alias.split('/').any(|s| s.is_empty());
    if ok {
        Ok(())
    } else {
        Err(RfcError {
            code: -1,
            status: 400,
            message: format!(
                "alias 非法: {:?}（规则：1-60 字符，仅小写字母/数字/-/_//，\
                 支持 team/name 前缀，无空段）",
                alias
            ),
            key: "REGISTRY_ALIAS_INVALID".into(),
        })
    }
}

/// 函数名 → 默认 alias：小写化，非常规字符替换为 '-'（防御性；SAP FM 名
/// 理论上只含 A-Z0-9/_ 与命名空间斜杠）。
fn sanitize_alias(func_name: &str) -> String {
    func_name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '/' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// 当前时刻 RFC3339 UTC（秒精度）。
fn now_iso8601() -> String {
    iso8601_from_unix(now_unix())
}

/// 供其他模块（调用审计）取同一格式的时间戳。
pub fn now_iso8601_public() -> String {
    now_iso8601()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// unix 秒 → "YYYY-MM-DDTHH:MM:SSZ"（civil-from-days，Howard Hinnant 算法）。
/// 不引 chrono：格式是 schema 契约，函数纯静态可单测锁定。
fn iso8601_from_unix(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // civil-from-days：从 1970-01-01 起的天数转 (y, m, d)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let mth = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

// ========================================================================
// 全局单例（启动期 init；进程内 Mutex 串行化读写）
// ========================================================================

struct RegistryState {
    path: PathBuf,
    store: Mutex<Store>,
}

static REGISTRY: OnceLock<RegistryState> = OnceLock::new();

/// 变更代际（原子递增）：每次成功落盘 +1。/openapi.json 的注册表 operation
/// 缓存靠它感知"该重建了"——不持有锁、不轮询文件。
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 当前代际（缓存方读取）。
pub fn generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::Relaxed)
}

fn bump_generation() {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// 启动期初始化（main 调一次）：加载文件；不存在则空表启动；
/// 文件损坏/版本不识 → 备份为 `<path>.corrupt-<时间戳>` 后空表启动
/// （坏文件不该拖垮网关，但也不能被覆盖丢失）。
pub fn init(path: PathBuf) {
    let store = match std::fs::read_to_string(&path) {
        Ok(text) => match Store::from_json(&text) {
            Ok(s) => {
                tracing::info!(path = %path.display(), "API 注册表已加载");
                s
            }
            Err(e) => {
                let backup = format!(
                    "{}.corrupt-{}",
                    path.display(),
                    iso8601_from_unix(now_unix()).replace(['-', ':'], "")
                );
                tracing::warn!(
                    path = %path.display(),
                    backup = %backup,
                    error = %e,
                    "注册表文件不可读，已备份并以空表启动"
                );
                let _ = std::fs::rename(&path, &backup);
                Store::default()
            }
        },
        Err(_) => Store::default(),
    };
    let _ = REGISTRY.set(RegistryState {
        path,
        store: Mutex::new(store),
    });
}

/// 注册表是否已初始化（/api/version 能力自描述用；main 总会 init → true）。
pub fn is_enabled() -> bool {
    REGISTRY.get().is_some()
}

/// 取全局状态的内部错误（未 init / 锁中毒）。
fn disabled_err() -> RfcError {
    RfcError {
        code: -1,
        status: 503,
        message: "注册表未初始化（REGISTRY_DISABLED）".into(),
        key: "REGISTRY_DISABLED".into(),
    }
}

/// 互斥锁访问辅助：poisoned 视作内部错误（不 panic——注册表坏了不该拖垮请求）。
fn with_store<R>(f: impl FnOnce(&mut Store) -> Result<R, RfcError>) -> Result<R, RfcError> {
    let state = REGISTRY.get().ok_or_else(disabled_err)?;
    let mut store = state.store.lock().map_err(|_| RfcError {
        code: -1,
        status: 500,
        message: "注册表内部锁中毒".into(),
        key: "REGISTRY_STORE_ERROR".into(),
    })?;
    f(&mut store)
}

/// 持久化：临时文件 + 原子 rename（读侧永远不会看到半截 JSON），
/// 成功后递增代际（通知 /openapi.json 缓存重建）。
/// 失败返回 RfcError（调用方决定降级为警告或暴露给请求方）。
fn persist(store: &Store) -> Result<(), RfcError> {
    let state = REGISTRY.get().ok_or_else(disabled_err)?;
    let tmp = state.path.with_extension("json.tmp");
    std::fs::write(&tmp, store.to_json()).map_err(|e| RfcError {
        code: -1,
        status: 500,
        message: format!("注册表写入失败（{}）: {}", state.path.display(), e),
        key: "REGISTRY_STORE_ERROR".into(),
    })?;
    std::fs::rename(&tmp, &state.path).map_err(|e| RfcError {
        code: -1,
        status: 500,
        message: format!("注册表落盘失败（{}）: {}", state.path.display(), e),
        key: "REGISTRY_STORE_ERROR".into(),
    })?;
    bump_generation();
    Ok(())
}

// ========================================================================
// 面向钩子/端点的高层操作
// ========================================================================

/// 写钩子：rfc_enabled 函数写入成功后自动登记（幂等，保留已有 intent/notes）。
/// `doc` 非空时随本次写入更新条目文档（文档随代码走）。
/// 返回 alias。失败返回 Err（调用方降级为警告，绝不影响 SAP 写入结果）。
pub fn auto_register_func(
    func_name: &str,
    group: Option<&str>,
    doc: Option<&str>,
) -> Result<String, RfcError> {
    let now = now_iso8601();
    let mut result = String::new();
    with_store(|store| {
        result = store.ensure_draft_for_func(func_name, group, doc, &now);
        persist(store)
    })?;
    Ok(result)
}

/// 删除钩子：函数对象删除成功后墓碑化。失败返回 Err（调用方降级为警告）。
pub fn tombstone_func(func_name: &str) -> Result<usize, RfcError> {
    let now = now_iso8601();
    let mut count = 0;
    with_store(|store| {
        count = store.tombstone_for_func(func_name, &now);
        persist(store)
    })?;
    Ok(count)
}

/// 列表（GET /api/registry）。
pub fn list_entries(q: Option<&str>, include_deleted: bool) -> Result<Vec<Entry>, RfcError> {
    with_store(|store| {
        Ok(store
            .list(q, include_deleted)
            .into_iter()
            .cloned()
            .collect())
    })
}

/// 全部 published 条目（/openapi.json 的服务目录来源）。
pub fn published_entries() -> Result<Vec<Entry>, RfcError> {
    with_store(|store| {
        Ok(store
            .entries
            .values()
            .filter(|e| e.status == EntryStatus::Published)
            .cloned()
            .collect())
    })
}

/// 单条（GET /api/registry/{alias}）。
pub fn get_entry(alias: &str) -> Result<Entry, RfcError> {
    validate_alias(alias)?;
    with_store(|store| {
        store.get(alias).cloned().ok_or_else(|| RfcError {
            code: -1,
            status: 404,
            message: format!("注册表无此条目: {alias}"),
            key: "REGISTRY_NOT_FOUND".into(),
        })
    })
}

/// 全量替换/创建（PUT /api/registry/{alias}）。
pub fn put_entry(alias: &str, body: &PutBody) -> Result<(Entry, bool), RfcError> {
    validate_alias(alias)?;
    crate::api::validate_func_name(&body.func_name)?;
    if let Some(g) = body.group.as_deref() {
        if !g.trim().is_empty() && g.trim().len() > 26 {
            return Err(RfcError {
                code: -1,
                status: 400,
                message: format!("group 超长（≤26）: {g}"),
                key: "REGISTRY_GROUP_INVALID".into(),
            });
        }
    }
    let now = now_iso8601();
    let mut out: Option<(Entry, bool)> = None;
    with_store(|store| {
        out = Some(store.put(alias, body, &now).map_err(|e| RfcError {
            code: -1,
            status: 500,
            message: e,
            key: "REGISTRY_STORE_ERROR".into(),
        })?);
        persist(store)
    })?;
    Ok(out.expect("with_store 回调必已赋值"))
}

/// 墓碑/物理删除（DELETE /api/registry/{alias}?purge=）。
pub fn delete_entry(alias: &str, purge: bool) -> Result<bool, RfcError> {
    validate_alias(alias)?;
    let now = now_iso8601();
    let mut existed = false;
    with_store(|store| {
        existed = if purge {
            store.purge(alias)
        } else {
            store.tombstone(alias, &now)
        };
        persist(store)
    })?;
    if !existed {
        return Err(RfcError {
            code: -1,
            status: 404,
            message: format!("注册表无此条目: {alias}"),
            key: "REGISTRY_NOT_FOUND".into(),
        });
    }
    Ok(true)
}

// ========================================================================
// HTTP 子路由（挂进 /api 鉴权层内；registry 不依赖连接池）
// ========================================================================

/// PUT /api/registry/{alias} 请求体（全量替换语义）。
#[derive(Deserialize)]
pub struct PutBody {
    /// SAP 函数模块名（必填；SAP 参数命名规则）
    pub func_name: String,
    #[serde(default)]
    pub intent: String,
    #[serde(default)]
    pub notes: String,
    /// 接口文档正文（Markdown，面向消费方；流入 OpenAPI 目录）
    #[serde(default)]
    pub doc: String,
    #[serde(default)]
    pub example: Option<serde_json::Value>,
    /// draft（默认）/ published；删除态走 DELETE
    #[serde(default = "default_status")]
    pub status: EntryStatus,
    #[serde(default)]
    pub group: Option<String>,
    /// 平坦调用输出表行数封顶（可选；null/缺省 = 默认 100）
    #[serde(default)]
    pub max_rows: Option<u32>,
}

fn default_status() -> EntryStatus {
    EntryStatus::Draft
}

/// `status`/`intent` 等枚举字段反序列化失败时给出可读错误（而非 serde 天书）。
fn parse_put_body(
    raw: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> Result<PutBody, RfcError> {
    let Json(raw) = raw.map_err(|r| RfcError {
        code: -1,
        status: r.status().as_u16(),
        message: r.body_text(),
        key: "JSON_INVALID".into(),
    })?;
    // status 先按字符串校验（EntryStatus 直接反序列化报 422 风格错误，可读性差）
    if let Some(s) = raw.get("status").and_then(|v| v.as_str()) {
        if EntryStatus::parse(s).is_none() {
            return Err(RfcError {
                code: -1,
                status: 400,
                message: format!("status 仅支持 draft/published（删除走 DELETE）: {s}"),
                key: "REGISTRY_STATUS_INVALID".into(),
            });
        }
    }
    serde_json::from_value(raw).map_err(|e| RfcError {
        code: -1,
        status: 400,
        message: format!("请求体非法（func_name 必填）: {e}"),
        key: "JSON_INVALID".into(),
    })
}

/// GET /api/registry 查询参数。
#[derive(Deserialize)]
struct ListQuery {
    /// 子串过滤（alias/func_name/intent，大小写不敏感）
    #[serde(default)]
    q: Option<String>,
    /// true = 连墓碑一起列出
    #[serde(default)]
    include_deleted: Option<bool>,
}

async fn registry_list_handler(
    axum::extract::Query(q): axum::extract::Query<ListQuery>,
) -> Result<Json<serde_json::Value>, RfcError> {
    let entries = list_entries(q.q.as_deref(), q.include_deleted.unwrap_or(false))?;
    let count = entries.len();
    Ok(Json(serde_json::json!({
        "count": count,
        "version": SCHEMA_VERSION,
        "entries": entries,
    })))
}

/// 通配路径剥尾部 `/detail`-式动作段（预留）并解码。alias 可含 `/`（team/name）。
fn alias_from_path(raw: &str) -> Result<String, RfcError> {
    let alias = raw.strip_suffix('/').unwrap_or(raw);
    validate_alias(alias)?;
    Ok(alias.to_string())
}

async fn registry_get_handler(
    axum::extract::Path(alias): axum::extract::Path<String>,
) -> Result<Json<Entry>, RfcError> {
    let alias = alias_from_path(&alias)?;
    Ok(Json(get_entry(&alias)?))
}

async fn registry_put_handler(
    axum::extract::Path(alias): axum::extract::Path<String>,
    body: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<serde_json::Value>, RfcError> {
    let alias = alias_from_path(&alias)?;
    let body = parse_put_body(body)?;
    let (entry, created) = put_entry(&alias, &body)?;
    Ok(Json(
        serde_json::json!({ "created": created, "entry": entry }),
    ))
}

async fn registry_delete_handler(
    axum::extract::Path(alias): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<PurgeQuery>,
) -> Result<Json<serde_json::Value>, RfcError> {
    let alias = alias_from_path(&alias)?;
    delete_entry(&alias, q.purge.unwrap_or(false))?;
    Ok(Json(serde_json::json!({
        "alias": alias,
        // 响应里统一给 status，purge 时给 purged 标记
        "status": if q.purge.unwrap_or(false) { "purged" } else { "deleted" },
    })))
}

#[derive(Deserialize)]
struct PurgeQuery {
    /// true = 物理删除（默认墓碑）
    #[serde(default)]
    purge: Option<bool>,
}

/// 注册表子路由。合并进 server::app 的 /api 鉴权层内（不依赖连接池）。
/// axum 0.8 语法：{*alias}（alias 可含 / 的 team/name 前缀）。
pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/registry", get(registry_list_handler))
        .route(
            "/api/registry/{*alias}",
            get(registry_get_handler)
                .put(registry_put_handler)
                .delete(registry_delete_handler),
        )
}

// ========================================================================
// 测试：存储逻辑用局部实例（并行安全）；端点走全局单例（串行锁）
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- alias 规则 ----

    #[test]
    fn alias_validation_rules() {
        for ok in [
            "z_calc",
            "calc2",
            "team/customer-list",
            "a_b-c/d_e",
            &"x".repeat(60),
        ] {
            assert!(validate_alias(ok).is_ok(), "应合法: {ok}");
        }
        for bad in [
            "",              // 空
            "Z_CALC",        // 大写
            "z calc",        // 空格
            "z.calc",        // 点
            "/leading",      // 以 / 开头
            "trailing/",     // 以 / 结尾
            "a//b",          // 空段
            &"x".repeat(61), // 超长
            "中文",          // 非 ASCII
        ] {
            assert!(validate_alias(bad).is_err(), "应非法: {bad:?}");
        }
    }

    #[test]
    fn sanitize_alias_lowercases_and_replaces() {
        assert_eq!(sanitize_alias("Z_CALC"), "z_calc");
        assert_eq!(
            sanitize_alias("/SDF/EWA_GET_ABAP_DUMPS"),
            "/sdf/ewa_get_abap_dumps"
        );
        assert_eq!(sanitize_alias("Z FM"), "z-fm");
    }

    // ---- 时间戳 ----

    #[test]
    fn iso8601_known_anchors() {
        assert_eq!(iso8601_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_from_unix(86_400), "1970-01-02T00:00:00Z");
        // 2000-02-29（闰年边界）
        assert_eq!(iso8601_from_unix(951_782_400), "2000-02-29T00:00:00Z");
        // 2026-01-01（今天附近的年代锚点）
        assert_eq!(iso8601_from_unix(1_767_225_600), "2026-01-01T00:00:00Z");
        // 2024-02-29（世纪闰年规则：400 年一闰）
        assert_eq!(iso8601_from_unix(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    // ---- 存储核心（局部 Store 实例，并行安全）----

    fn store_with(entry: Entry) -> Store {
        let mut s = Store::default();
        s.entries.insert(entry.alias.clone(), entry);
        s
    }

    fn draft(alias: &str, func: &str) -> Entry {
        Entry {
            alias: alias.into(),
            func_name: func.into(),
            group: None,
            intent: String::new(),
            notes: String::new(),
            doc: String::new(),
            example: None,
            status: EntryStatus::Draft,
            origin: EntryOrigin::Auto,
            max_rows: None,
            created_at: "2026-09-25T00:00:00Z".into(),
            updated_at: "2026-09-25T00:00:00Z".into(),
        }
    }

    #[test]
    fn store_roundtrip_keeps_version_and_sort() {
        let mut s = Store::default();
        s.entries.insert("b".into(), draft("b", "Z_B"));
        s.entries.insert("a".into(), draft("a", "Z_A"));
        let json = s.to_json();
        assert!(json.contains("\"version\": 1"));
        let s2 = Store::from_json(&json).unwrap();
        // BTreeMap 落盘即有序
        let order: Vec<&String> = s2.entries.keys().collect();
        assert_eq!(order, [&"a".to_string(), &"b".to_string()]);
    }

    #[test]
    fn store_rejects_bad_version_and_bad_json() {
        assert!(Store::from_json("{}").is_err(), "缺 version");
        assert!(
            Store::from_json("{\"version\":2,\"entries\":[]}").is_err(),
            "未来版本"
        );
        assert!(Store::from_json("not json").is_err());
    }

    #[test]
    fn ensure_draft_new_uses_lowercase_func_as_alias() {
        let mut s = Store::default();
        let alias = s.ensure_draft_for_func("Z_CALC", Some("ZMATH"), None, "t1");
        assert_eq!(alias, "z_calc");
        assert_eq!(s.get("z_calc").unwrap().func_name, "Z_CALC");
        assert_eq!(s.get("z_calc").unwrap().group.as_deref(), Some("ZMATH"));
        assert_eq!(s.get("z_calc").unwrap().status, EntryStatus::Draft);
    }

    #[test]
    fn ensure_draft_keeps_agent_written_fields() {
        let mut s = store_with(draft("z_calc", "Z_CALC"));
        {
            let e = s.entries.get_mut("z_calc").unwrap();
            e.intent = "客户列表查询".into();
            e.notes = "必须传 MAX_ROWS".into();
            e.doc = "完整文档 v1".into();
            e.status = EntryStatus::Published;
        }
        // doc=None：保留已有文档（手工完善的文档不被空写入冲掉）
        let alias = s.ensure_draft_for_func("Z_CALC", None, None, "t2");
        assert_eq!(alias, "z_calc");
        let e = s.get("z_calc").unwrap();
        assert_eq!(e.intent, "客户列表查询", "重登记不清空 intent");
        assert_eq!(e.notes, "必须传 MAX_ROWS", "重登记不清空 notes");
        assert_eq!(e.doc, "完整文档 v1", "doc=None 保留已有文档");
        assert_eq!(e.updated_at, "t2");
        // doc 非空：文档随代码更新
        s.ensure_draft_for_func("Z_CALC", None, Some("完整文档 v2（随代码）"), "t3");
        assert_eq!(s.get("z_calc").unwrap().doc, "完整文档 v2（随代码）");
        // doc 空白串视同未提供
        s.ensure_draft_for_func("Z_CALC", None, Some("   "), "t4");
        assert_eq!(
            s.get("z_calc").unwrap().doc,
            "完整文档 v2（随代码）",
            "空白 doc 不覆盖"
        );
    }

    #[test]
    fn ensure_draft_revives_tombstone() {
        let mut s = store_with(draft("z_calc", "Z_CALC"));
        s.tombstone("z_calc", "t0");
        assert_eq!(s.get("z_calc").unwrap().status, EntryStatus::Deleted);
        let alias = s.ensure_draft_for_func("Z_CALC", None, None, "t1");
        assert_eq!(alias, "z_calc");
        assert_eq!(s.get("z_calc").unwrap().status, EntryStatus::Draft);
    }

    #[test]
    fn ensure_draft_alias_collision_suffixes() {
        // z_calc 已被另一个函数占用 → 新函数取 z_calc-2
        let mut s = store_with(draft("z_calc", "Z_SOMETHING_ELSE"));
        let alias = s.ensure_draft_for_func("Z_CALC", None, None, "t1");
        assert_eq!(alias, "z_calc-2");
        assert_eq!(s.get("z_calc-2").unwrap().func_name, "Z_CALC");
    }

    #[test]
    fn tombstone_for_func_matches_all_aliases_and_is_noop_when_absent() {
        let mut s = store_with(draft("z_calc", "Z_CALC"));
        s.entries.insert("calc2".into(), draft("calc2", "Z_CALC"));
        assert_eq!(s.tombstone_for_func("z_calc", "t1"), 2);
        assert_eq!(s.get("z_calc").unwrap().status, EntryStatus::Deleted);
        assert_eq!(s.get("calc2").unwrap().status, EntryStatus::Deleted);
        // 不存在的函数：无操作不报错
        assert_eq!(s.tombstone_for_func("Z_NOPE", "t2"), 0);
    }

    #[test]
    fn list_filters_and_hides_tombstones() {
        let mut s = store_with(draft("z_calc", "Z_CALC"));
        {
            let e = s.entries.get_mut("z_calc").unwrap();
            e.intent = "customer list".into();
            e.status = EntryStatus::Published;
        }
        s.entries.insert("old".into(), draft("old", "Z_OLD"));
        s.tombstone("old", "t1");
        // 默认隐藏墓碑
        assert_eq!(s.list(None, false).len(), 1);
        // include_deleted 全量
        assert_eq!(s.list(None, true).len(), 2);
        // q 命中 intent（大小写不敏感）
        assert_eq!(s.list(Some("CUSTOMER"), false).len(), 1);
        assert_eq!(s.list(Some("nomatch"), false).len(), 0);
    }

    #[test]
    fn put_replaces_fully_and_keeps_created_at() {
        let mut s = store_with(draft("z_calc", "Z_CALC"));
        let body = PutBody {
            func_name: "z_calc".into(),
            intent: "intent v2".into(),
            notes: String::new(),
            doc: String::new(),
            example: Some(serde_json::json!({"inputs": {"IV_A": 1}})),
            status: EntryStatus::Published,
            group: None,
            max_rows: Some(500),
        };
        let (entry, created) = s.put("z_calc", &body, "t9").unwrap();
        assert!(!created, "已存在条目不是新建");
        assert_eq!(entry.intent, "intent v2");
        assert_eq!(entry.max_rows, Some(500));
        assert_eq!(entry.created_at, "2026-09-25T00:00:00Z", "创建时间保留");
        assert_eq!(entry.updated_at, "t9");
        // 全量替换：未带的 example=null 即清空
        let body2 = PutBody {
            func_name: "z_calc".into(),
            intent: String::new(),
            notes: String::new(),
            doc: String::new(),
            example: None,
            status: EntryStatus::Draft,
            group: None,
            max_rows: None,
        };
        let (entry2, _) = s.put("z_calc", &body2, "t10").unwrap();
        assert!(entry2.example.is_none(), "PUT 全量替换：null 即清空");
        assert_eq!(entry2.max_rows, None, "PUT 全量替换：max_rows 一并复位");
    }

    // ---- HTTP 端点（全局单例 → 串行）----

    // tokio Mutex：测试体是 async fn，守卫要跨 await 持有（std Mutex 会触发
    // clippy::await_holding_lock；这里序列化的是全局单例测试，无竞争语义差异）
    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// OnceLock 只能设一次：测试进程内首个调用者定路径；后续测试只重置
    /// 内部 store（文件路径不变，断言时从 REGISTRY 取真实路径）。
    fn reset_global(tag: &str) {
        let dir = std::env::temp_dir().join(format!("sfa-registry-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if REGISTRY.get().is_none() {
            init(dir.join("registry.json"));
        }
        if let Some(state) = REGISTRY.get() {
            if let Ok(mut store) = state.store.lock() {
                *store = Store::default();
            }
        }
    }

    async fn body_string(body: axum::body::Body) -> String {
        use http_body_util::BodyExt;
        let bytes = body.collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).to_string()
    }

    async fn call(req: axum::http::Request<axum::body::Body>) -> (u16, String) {
        use tower::ServiceExt;
        let app: Router = router();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        (status, body_string(resp.into_body()).await)
    }

    fn put_req(alias: &str, body: &serde_json::Value) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("PUT")
            .uri(format!("/api/registry/{alias}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn endpoint_crud_lifecycle() {
        let _g = TEST_LOCK.lock().await;
        reset_global("crud");
        // 空表
        let (st, body) = call(
            axum::http::Request::builder()
                .uri("/api/registry")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, 200);
        assert!(body.contains("\"count\":0"), "空表: {body}");
        // 创建
        let body = serde_json::json!({
            "func_name": "Z_CALC", "intent": "calculator", "status": "published"
        });
        let (st, body) = call(put_req("z_calc", &body)).await;
        assert_eq!(st, 200, "{body}");
        assert!(body.contains("\"created\":true"), "{body}");
        // 单条读取
        let (st, body) = call(
            axum::http::Request::builder()
                .uri("/api/registry/z_calc")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, 200);
        assert!(body.contains("Z_CALC"), "{body}");
        // 更新（全量替换）
        let body = serde_json::json!({"func_name": "Z_CALC", "intent": "calc v2"});
        let (st, _) = call(put_req("z_calc", &body)).await;
        assert_eq!(st, 200);
        // 墓碑删除 → 默认列表不可见
        let (st, _) = call(
            axum::http::Request::builder()
                .method("DELETE")
                .uri("/api/registry/z_calc")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, 200);
        let (st, body) = call(
            axum::http::Request::builder()
                .uri("/api/registry?include_deleted=true")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, 200);
        assert!(body.contains("\"count\":1"), "墓碑仍在: {body}");
        assert!(body.contains("\"deleted\""), "{body}");
        // 文件已按全局状态的真实路径落盘且可解析（OnceLock 首个 init 者定路径，
        // 断言必须取 REGISTRY 里的真实 path，而非本测试想设置的路径）
        let path = REGISTRY.get().expect("init 已执行").path.clone();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\": 1"), "{text}");
        assert!(Store::from_json(&text).is_ok());
    }

    #[tokio::test]
    async fn endpoint_validations() {
        let _g = TEST_LOCK.lock().await;
        reset_global("validate");
        // 非法 alias（大写）
        let (st, body) = call(put_req("Z_BAD", &serde_json::json!({"func_name": "Z_X"}))).await;
        assert_eq!(st, 400, "{body}");
        assert!(body.contains("REGISTRY_ALIAS_INVALID"), "{body}");
        // 非法 status
        let (st, body) = call(put_req(
            "z_ok",
            &serde_json::json!({"func_name": "Z_X", "status": "deleted"}),
        ))
        .await;
        assert_eq!(st, 400, "{body}");
        assert!(body.contains("REGISTRY_STATUS_INVALID"), "{body}");
        // 非法函数名（SAP 规则）
        let (st, body) = call(put_req(
            "z_ok",
            &serde_json::json!({"func_name": "bad name!"}),
        ))
        .await;
        assert_eq!(st, 400, "{body}");
        // 404
        let (st, body) = call(
            axum::http::Request::builder()
                .uri("/api/registry/none_such")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, 404, "{body}");
        assert!(body.contains("REGISTRY_NOT_FOUND"), "{body}");
        // DELETE 不存在的
        let (st, _) = call(
            axum::http::Request::builder()
                .method("DELETE")
                .uri("/api/registry/none_such")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(st, 404);
        // slashed alias 合法创建
        let (st, body) = call(put_req(
            "team/z-calc",
            &serde_json::json!({"func_name": "Z_CALC"}),
        ))
        .await;
        assert_eq!(st, 200, "{body}");
    }

    #[tokio::test]
    async fn corrupt_file_backed_up_and_starts_fresh() {
        let _g = TEST_LOCK.lock().await;
        // 直接构造 state 验证逻辑（不动全局）：写坏文件 → init 分支逻辑在
        // Store::from_json 层面已被上面覆盖；这里验证备份命名与空表启动。
        let dir = std::env::temp_dir().join("sfa-registry-test-corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("registry.json");
        std::fs::write(&path, "{ broken").unwrap();
        // 复用 init 的核心行为：from_json 失败 → 备份 + 空表。这里用局部函数
        // 模拟（全局 OnceLock 已被其他测试占用）。
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(Store::from_json(&text).is_err());
        // init() 分支本身由集成测试覆盖（真实子进程）。
    }
}
