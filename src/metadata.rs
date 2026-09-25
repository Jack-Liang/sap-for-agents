//! 函数与 DDIC 类型元数据缓存：首次查询时拉取描述，后续复用。
//!
//! 目的：
//! 1. 让 REST 调用方不必手填 string_outputs 的 max_len——服务端用 SAP 真实字段长度填充。
//! 2. 输出读取时按字段真实类型（INT/FLOAT/BCD/CHAR...）自动选择对应 RfcGetXxx，
//!    保留数值/二进制语义。
//! 3. 面向 AI 的元数据端点复用本缓存的函数接口/DDIC 字段查询。
//!
//! 缓存设计：只存 Rust 化的纯数据（不含 SAP 句柄），保证可跨线程共享。
//! 表/结构体参数的字段信息在拉取时一次性递归解析好。

use crate::connection::{get_field_infos, RfcConnection};
use crate::error::RfcError;
use crate::ffi::{RFCTYPE_STRUCTURE, RFCTYPE_TABLE};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// 嵌套结构体递归深度上限，防止 ABAP 深层自引用导致栈溢出。
const MAX_RECURSION_DEPTH: usize = 5;

/// 单个字段的元数据（精简版，用于执行期类型派发）
#[derive(Debug, Clone, Copy)]
pub struct FieldMeta {
    /// 字符长度（ucLength/2，对 CHAR/NUM/DATE/TIME 有意义）
    pub char_length: usize,
    /// RFCTYPE_* 常量值（见 ffi.rs）
    pub type_: i32,
}

/// DDIC 类型单个字段的元数据（完整版，含描述，面向 AI 端点）
#[derive(Debug, Clone)]
pub struct TypeFieldMeta {
    pub name: String,
    /// RFCTYPE_* 常量值
    pub type_: i32,
    /// 字符长度
    pub char_length: usize,
    /// 小数位（BCD/FLOAT）
    pub decimals: u32,
    /// 参数/字段描述文本（parameterText，可能为空）
    pub description: String,
    /// 若为结构体/表，递归解析的子字段（None 表示标量或达深度上限）
    pub sub_fields: Option<Vec<TypeFieldMeta>>,
}

/// 单个函数的元数据（纯数据，无裸指针，可跨线程共享）
#[derive(Debug, Clone, Default)]
pub struct FuncMetadata {
    /// 标量参数：参数名 -> 字段元数据
    pub scalars: HashMap<String, FieldMeta>,
    /// 表参数：表名 -> {字段名 -> 字段元数据}
    pub tables: HashMap<String, HashMap<String, FieldMeta>>,
}

/// 函数元数据缓存：函数名 -> 元数据
static FUNC_CACHE: OnceLock<RwLock<HashMap<String, FuncMetadata>>> = OnceLock::new();
/// DDIC 类型字段缓存：类型名 -> 字段列表
static TYPE_CACHE: OnceLock<RwLock<HashMap<String, Vec<TypeFieldMeta>>>> = OnceLock::new();

fn func_cache() -> &'static RwLock<HashMap<String, FuncMetadata>> {
    FUNC_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn type_cache() -> &'static RwLock<HashMap<String, Vec<TypeFieldMeta>>> {
    TYPE_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 写钩子调用：函数对象写入/删除成功后清除其元数据缓存，
/// 让下一次接口自省/auto_outputs 拿到新签名（否则读到的是写入前的快照）。
/// 返回是否确有条目被清除（供日志判断）。
pub fn invalidate_function(func_name: &str) -> bool {
    match func_cache().write() {
        Ok(mut map) => map.remove(&func_name.to_uppercase()).is_some(),
        Err(_) => false,
    }
}

/// 写钩子调用：DDIC 表/结构（tabl/stru）写入或删除成功后清除其字段缓存，
/// 让下一次 `/api/ddic/type/{name}` 及结构/表参数的字段展开拿到新定义。
/// 返回是否确有条目被清除（供日志判断）。
pub fn invalidate_type(type_name: &str) -> bool {
    match type_cache().write() {
        Ok(mut map) => map.remove(&type_name.to_uppercase()).is_some(),
        Err(_) => false,
    }
}

/// 把 type_desc_handle 递归解析成 TypeFieldMeta 列表。
/// SAFETY: type_handle 必须是有效的 DDIC 类型描述符句柄。
unsafe fn resolve_fields_recursive(
    type_handle: crate::ffi::RFC_TYPE_DESC_HANDLE,
    depth: usize,
) -> Result<Vec<TypeFieldMeta>, RfcError> {
    let fields = get_field_infos(type_handle)?;
    let mut out = Vec::with_capacity(fields.len());
    for f in fields {
        // 仅 STRUCTURE/TABLE 类型且未达深度上限时递归下钻
        let sub_fields = if depth < MAX_RECURSION_DEPTH
            && (f.type_ == RFCTYPE_TABLE || f.type_ == RFCTYPE_STRUCTURE)
        {
            if let Some(sub_handle) = f.type_desc_handle {
                // SAFETY: sub_handle 来自刚解析的有效字段元数据
                match unsafe { resolve_fields_recursive(sub_handle, depth + 1) } {
                    Ok(sub) if !sub.is_empty() => Some(sub),
                    _ => None,
                }
            } else {
                None
            }
        } else {
            None
        };
        out.push(TypeFieldMeta {
            name: f.name.clone(),
            type_: f.type_,
            char_length: f.char_length,
            decimals: f.decimals,
            description: f.parameter_text.clone(),
            sub_fields,
        });
    }
    Ok(out)
}

/// 拉取并组装某函数的完整元数据（含表字段类型递归解析）。
fn fetch_metadata(conn: &RfcConnection, func_name: &str) -> Result<FuncMetadata, RfcError> {
    let param_infos = conn.get_param_infos(func_name)?;
    let mut meta = FuncMetadata::default();

    for p in &param_infos {
        let fm = FieldMeta {
            char_length: p.char_length,
            type_: p.type_,
        };
        // 表参数和结构体参数：递归读其字段元数据
        // （表和结构体的行类型都用 type_desc_handle 描述，处理方式相同）
        if p.type_ == RFCTYPE_TABLE || p.type_ == RFCTYPE_STRUCTURE {
            if let Some(type_handle) = p.type_desc_handle {
                // SAFETY: type_handle 来自刚拉取的有效元数据，连接仍有效
                let fields = unsafe { get_field_infos(type_handle) }?;
                let field_map: HashMap<String, FieldMeta> = fields
                    .iter()
                    .map(|f| {
                        (
                            f.name.to_uppercase(),
                            FieldMeta {
                                char_length: f.char_length,
                                type_: f.type_,
                            },
                        )
                    })
                    .collect();
                meta.tables.insert(p.name.to_uppercase(), field_map);
            }
        }
        meta.scalars.insert(p.name.to_uppercase(), fm);
    }

    Ok(meta)
}

/// 获取某函数的元数据。命中缓存直接返回；未命中则从 SAP 拉取并缓存。
pub fn get_metadata(conn: &RfcConnection, func_name: &str) -> Result<FuncMetadata, RfcError> {
    // 先查缓存（快路径，读锁）
    {
        let map = func_cache().read().map_err(|e| RfcError {
            code: -1,
            message: format!("元数据缓存锁被毒化: {}", e),
            key: String::new(),
            ..Default::default()
        })?;
        if let Some(meta) = map.get(func_name) {
            return Ok(meta.clone());
        }
    }

    // 未命中：从 SAP 拉取
    let meta = fetch_metadata(conn, func_name)?;
    let mut map = func_cache().write().map_err(|e| RfcError {
        code: -1,
        message: format!("元数据缓存锁被毒化: {}", e),
        key: String::new(),
        ..Default::default()
    })?;
    map.insert(func_name.to_string(), meta.clone());
    tracing::debug!(
        func = func_name,
        scalars = meta.scalars.len(),
        tables = meta.tables.len(),
        "元数据已缓存"
    );
    Ok(meta)
}

/// 查询某标量参数的元数据。
pub fn param_meta(conn: &RfcConnection, func_name: &str, param_name: &str) -> Option<FieldMeta> {
    let meta = get_metadata(conn, func_name).ok()?;
    meta.scalars.get(&param_name.to_uppercase()).copied()
}

/// 查询某表参数的字段元数据映射（field_name -> FieldMeta）。
pub fn table_field_metas(
    conn: &RfcConnection,
    func_name: &str,
    table_param: &str,
) -> Option<HashMap<String, FieldMeta>> {
    let meta = get_metadata(conn, func_name).ok()?;
    meta.tables.get(&table_param.to_uppercase()).cloned()
}

/// 获取某 DDIC 类型（结构/表，如 MARA、BAPIRETURN）的字段元数据。
/// 命中缓存直接返回；未命中则用 RfcGetTypeDesc 拉取并递归解析后缓存。
/// 与函数元数据不同，这里返回完整的 TypeFieldMeta（含描述/小数位/嵌套子字段）。
pub fn get_type_fields(
    conn: &RfcConnection,
    type_name: &str,
) -> Result<Vec<TypeFieldMeta>, RfcError> {
    let upper = type_name.to_uppercase();
    // 先查缓存（快路径，读锁）
    {
        let map = type_cache().read().map_err(|e| RfcError {
            code: -1,
            message: format!("类型缓存锁被毒化: {}", e),
            key: String::new(),
            ..Default::default()
        })?;
        if let Some(fields) = map.get(&upper) {
            return Ok(fields.clone());
        }
    }

    // 未命中：用 RfcGetTypeDesc 取类型描述符，递归解析字段
    let type_handle = conn.get_type_desc(&upper)?;
    // SAFETY: type_handle 来自刚获取的有效类型描述符，连接仍有效
    let fields = unsafe { resolve_fields_recursive(type_handle, 0) }?;

    let mut map = type_cache().write().map_err(|e| RfcError {
        code: -1,
        message: format!("类型缓存锁被毒化: {}", e),
        key: String::new(),
        ..Default::default()
    })?;
    map.insert(upper.clone(), fields.clone());
    tracing::debug!(r#type = %upper, fields = fields.len(), "DDIC 类型字段已缓存");
    Ok(fields)
}
