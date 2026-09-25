//! 通用 RFC 执行器：根据 `InvokeRequest` 完成
//! "取函数 → 填参数 → invoke → 读结果" 全过程，返回结构化 `InvokeResponse`。
//!
//! 字段长度与类型自动发现：当请求中长度未指定时，通过 metadata 模块从 SAP
//! 函数描述查询；输入按调用方给出的 ScalarValue 变体选对应 RfcSetXxx。

use crate::api::{execute_invoke, InvokeRequest, InvokeResponse};
use crate::connection::RfcConnection;
use crate::error::RfcError;

/// 通用执行入口。委托给 api::execute_invoke，并注入元数据解析函数。
pub fn execute_collect(
    conn: &RfcConnection,
    req: &InvokeRequest,
) -> Result<InvokeResponse, RfcError> {
    execute_invoke(conn, req, resolve_meta)
}

/// 元数据解析回调：对 string_outputs 查标量参数元数据，对 table_outputs 查表字段元数据。
/// 任何元数据查询失败都静默回退到 DEFAULT_CHAR_LEN（不阻断主流程），
/// 因为 get_chars 本身已支持自适应重试。
fn resolve_meta(conn: &RfcConnection, req: &InvokeRequest) -> crate::api::ResolvedMeta {
    use std::collections::HashMap;

    // 标量输出：参数名 -> (字符长度, 类型)
    let mut scalars: HashMap<String, (usize, i32)> = HashMap::new();
    // string_outputs 和 auto_outputs 都需要元数据（类型+长度）
    for name in req.string_outputs.keys().chain(req.auto_outputs.iter()) {
        let meta = crate::metadata::param_meta(conn, &req.func_name, name);
        let entry = match meta {
            Some(m) => (m.char_length, m.type_),
            None => (crate::api::DEFAULT_CHAR_LEN, crate::ffi::RFCTYPE_CHAR),
        };
        scalars.insert(name.clone(), entry);
    }

    // 表输出：表名 -> {字段名 -> (字符长度, 类型)}
    let mut tables: HashMap<String, HashMap<String, (usize, i32)>> = HashMap::new();
    for table_name in req.table_outputs.keys() {
        let field_map = crate::metadata::table_field_metas(conn, &req.func_name, table_name)
            .unwrap_or_default()
            .into_iter()
            .map(|(k, m)| (k, (m.char_length, m.type_)))
            .collect();
        tables.insert(table_name.clone(), field_map);
    }

    // STRUCTURE 输出：结构体名 -> {字段名 -> (字符长度, 类型)}
    // 供 string_outputs/auto_outputs 遇 STRUCTURE 时按子字段读；也供 struct_outputs 复用（与 table_outputs 同样用 table_field_metas）
    let mut structures: HashMap<String, HashMap<String, (usize, i32)>> = HashMap::new();
    // 收集需要预查 STRUCTURE 子字段的参数名（去重）
    let mut need_struct_meta: std::collections::HashSet<String> = std::collections::HashSet::new();
    // struct_outputs 明示要的
    for struct_name in req.struct_outputs.keys() {
        // struct_outputs 之前也走 tables（见下句，兼容旧逻辑）；同时预查 structures（供 string_outputs/auto_outputs 用）
        need_struct_meta.insert(struct_name.clone());
    }
    // string_outputs / auto_outputs 中类型为 STRUCTURE 的（避免重复查）
    for name in req.string_outputs.keys().chain(req.auto_outputs.iter()) {
        if let Some((_, t)) = scalars.get(name) {
            if *t == crate::ffi::RFCTYPE_STRUCTURE {
                need_struct_meta.insert(name.clone());
            }
        }
    }
    for struct_name in &need_struct_meta {
        let field_map = crate::metadata::table_field_metas(conn, &req.func_name, struct_name)
            .unwrap_or_default()
            .into_iter()
            .map(|(k, m)| (k, (m.char_length, m.type_)))
            .collect();
        structures.insert(struct_name.clone(), field_map);
    }

    crate::api::ResolvedMeta {
        scalars,
        tables,
        structures,
    }
}
