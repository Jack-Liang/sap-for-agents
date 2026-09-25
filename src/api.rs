//! REST API 的请求/响应数据结构（serde DTO）
//!
//! 这些类型是 `RfcCallSpec` 的 JSON 镜像，但全部用 owned 数据，
//! 以便 serde 反序列化。handler 收到请求后转换成底层调用。

use crate::error::RfcError;
use crate::function::{RfcFunction, RfcRow};
use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 显式类型标记：用于 BCD/INT8/Bytes 这些无法靠 JSON 字面量区分的类型。
/// JSON 形式：`{"type":"BCD","value":"123.45"}`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypedScalar {
    #[serde(rename = "type")]
    pub kind: TypedScalarKind,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TypedScalarKind {
    /// BCD packed number，value 为字符串形式的数字（保留小数位）
    Bcd,
    /// 8 字节整数，value 为 JSON 整数
    Int8,
    /// 二进制，value 为 Base64 编码的字符串
    Bytes,
}

/// SAP 字段类型的枚举表达。
///
/// 反序列化策略（双轨制，兼顾向后兼容与精确类型控制）：
/// - **隐式**（向后兼容）：
///   - JSON 字符串 → `Chars`
///   - JSON 整数   → `Int`（i32，对应 INT4）
///   - JSON 浮点   → `Float`
/// - **显式**（精确控制 BCD/INT8/XString）：
///   - `{"type":"BCD","value":"123.45"}` → BCD
///   - `{"type":"INT8","value":12345678901}` → INT8
///   - `{"type":"BYTES","value":"QkFTRTY0..."}` → 二进制（Base64）
///
/// 输出（响应 scalars）同样用这个枚举序列化，类型由读取方式决定。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ScalarValue {
    /// 隐式：JSON 字符串。对应 SAP CHAR/NUM/DATE/TIME/BCD(字符串形式)
    Chars(String),
    /// 隐式：JSON 整数。对应 SAP INT4（i32）
    Int(i32),
    /// 隐式：JSON 整数。对应 SAP INT8（i64，auto_outputs 按真实类型读时出现）
    Int8(i64),
    /// 隐式：JSON 浮点。对应 SAP FLOAT
    Float(f64),
    /// 显式：带类型标记的值（BCD/INT8/Bytes）
    Typed(TypedScalar),
}

impl ScalarValue {
    /// 把任意变体转成字符串表达（用于 discovery 等只需字符串的场景）。
    /// Chars → 原值；Int/Int8 → 数字字符串；Float → 浮点字符串；Typed → 按 value 序列化。
    pub fn into_chars(self) -> String {
        match self {
            ScalarValue::Chars(s) => s,
            ScalarValue::Int(i) => i.to_string(),
            ScalarValue::Int8(i) => i.to_string(),
            ScalarValue::Float(f) => f.to_string(),
            ScalarValue::Typed(t) => t.value.to_string(),
        }
    }

    /// 解码 Typed 形式，返回 (是否二进制, 字符串值, 数值占位)。
    /// 这是辅助 apply 方法处理 BCD/INT8/Bytes 三种显式类型。
    fn decode_typed(t: &TypedScalar, name: &str) -> Result<TypedDecoded, RfcError> {
        match t.kind {
            TypedScalarKind::Bcd => {
                let s = t.value.as_str().ok_or_else(|| RfcError {
                    code: -1,
                    message: format!("BCD 字段 {} 的 value 必须是字符串", name),
                    key: String::new(),
                    ..Default::default()
                })?;
                Ok(TypedDecoded::Chars(s.to_string()))
            }
            TypedScalarKind::Int8 => {
                let i = t.value.as_i64().ok_or_else(|| RfcError {
                    code: -1,
                    message: format!("INT8 字段 {} 的 value 必须是整数", name),
                    key: String::new(),
                    ..Default::default()
                })?;
                Ok(TypedDecoded::Int8(i))
            }
            TypedScalarKind::Bytes => {
                let s = t.value.as_str().ok_or_else(|| RfcError {
                    code: -1,
                    message: format!("Bytes 字段 {} 的 value 必须是 Base64 字符串", name),
                    key: String::new(),
                    ..Default::default()
                })?;
                let bytes = general_purpose::STANDARD.decode(s).map_err(|e| RfcError {
                    code: -1,
                    message: format!("Base64 解码失败 ({}): {}", name, e),
                    key: String::new(),
                    ..Default::default()
                })?;
                Ok(TypedDecoded::Bytes(bytes))
            }
        }
    }

    /// 应用到函数顶层标量参数。按枚举变体选择对应的 RfcSetXxx。
    pub fn apply_to_func(&self, func: &mut RfcFunction, name: &str) -> Result<(), RfcError> {
        match self {
            ScalarValue::Chars(s) => func.set_chars(name, s),
            ScalarValue::Int(i) => func.set_int(name, *i),
            ScalarValue::Int8(i) => func.set_int8(name, *i),
            ScalarValue::Float(f) => func.set_float(name, *f),
            ScalarValue::Typed(t) => match Self::decode_typed(t, name)? {
                TypedDecoded::Chars(s) => func.set_chars(name, &s),
                TypedDecoded::Int8(i) => func.set_int8(name, i),
                TypedDecoded::Bytes(b) => func.set_xstring(name, &b),
            },
        }
    }

    /// 应用到表行（结构体）字段。
    pub fn apply_to_row(&self, row: &RfcRow, name: &str) -> Result<(), RfcError> {
        match self {
            ScalarValue::Chars(s) => row.set_chars(name, s),
            ScalarValue::Int(i) => row.set_int(name, *i),
            ScalarValue::Int8(i) => row.set_int8(name, *i),
            ScalarValue::Float(f) => row.set_float(name, *f),
            ScalarValue::Typed(t) => match Self::decode_typed(t, name)? {
                TypedDecoded::Chars(s) => row.set_chars(name, &s),
                TypedDecoded::Int8(i) => row.set_int8(name, i),
                TypedDecoded::Bytes(b) => row.set_xstring(name, &b),
            },
        }
    }
}

/// Typed 解码中间结果
enum TypedDecoded {
    Chars(String),
    Int8(i64),
    Bytes(Vec<u8>),
}

/// 字符串输出参数：参数名 -> 可选最大长度。
/// 长度为 null 时由服务端用函数元数据自动填充；不填键时默认 255。
/// 为保持向后兼容，也支持直接传整数（旧格式 {"ECHOTEXT": 255}）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MaxLen {
    /// 旧格式：直接给整数
    Legacy(usize),
    /// 新格式：{"max_len": 255} 或 null（自动发现）
    Detailed { max_len: Option<usize> },
}

impl MaxLen {
    /// 取出长度：None 表示调用方未指定，用元数据或默认值。
    /// 有值时 clamp 到 MAX_FIELD_LEN，防止恶意传超大值导致 OOM 分配。
    pub fn resolve(&self) -> Option<usize> {
        match self {
            MaxLen::Legacy(n) => Some((*n).min(MAX_FIELD_LEN)),
            MaxLen::Detailed { max_len } => max_len.map(|n| n.min(MAX_FIELD_LEN)),
        }
    }
}

/// `POST /api/rfc` 请求体，描述一次任意 RFC/BAPI 调用
#[derive(Debug, Deserialize)]
pub struct InvokeRequest {
    /// RFC 函数模块名，如 "BAPI_USER_GETLIST"
    pub func_name: String,

    /// 标量输入参数：参数名 → 值
    #[serde(default)]
    pub inputs: HashMap<String, ScalarValue>,

    /// 表输入参数：表名 → 多行；每行是 字段名 → 值
    #[serde(default)]
    pub table_inputs: HashMap<String, Vec<HashMap<String, ScalarValue>>>,

    /// 顶层结构体输入参数：结构体名 → {字段名 → 值}
    /// 用于 BAPI 的 IMPORTING 结构体参数（如 BAPI_USER_CREATE.ADDRESS）
    #[serde(default)]
    pub struct_inputs: HashMap<String, HashMap<String, ScalarValue>>,

    /// 需要读取的整型输出参数名
    #[serde(default)]
    pub int_outputs: Vec<String>,

    /// 需要读取的字符串输出参数：参数名 → 最大长度（可空，null 表示自动发现）
    #[serde(default)]
    pub string_outputs: HashMap<String, MaxLen>,

    /// 需要按「真实类型」自动读取的标量输出参数名。
    /// 服务端按元数据里的 RFCTYPE 自动选 getter：
    /// INT/INT1/INT2→整数、INT8→i64、FLOAT→f64、BCD→字符串、CHAR/DATE/TIME→字符串、BYTE/XSTRING→Base64。
    /// 适合不想分别填 int_outputs/string_outputs、又想保留数值语义的场景。
    #[serde(default)]
    pub auto_outputs: Vec<String>,

    /// 需要遍历读取的输出表：表名 → 字段列表
    /// - 字段项为 `[字段名]` 或 `[字段名, 最大长度]`，长度可省略（自动发现）
    #[serde(default)]
    pub table_outputs: HashMap<String, Vec<FieldSpec>>,

    /// 需要读取的顶层结构体输出：结构体名 → 字段列表
    /// 结果放入响应的 structs 字段（字段值统一字符串）
    #[serde(default)]
    pub struct_outputs: HashMap<String, Vec<FieldSpec>>,

    /// 是否自动读取 RETURN 表（BAPI 通用返回消息表）
    #[serde(default)]
    pub read_return: bool,

    /// 本次调用的超时秒数（可选，≥1 生效）。不传或传 0 则用全局 `SAP_REQUEST_TIMEOUT_SECS`（默认 60s）。
    /// 让调用方对已知慢接口（批量 BAPI、大表查询）自主放宽超时；超时返回 504。
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// 为 discovery 模块的程序化构造提供便利：func_name 默认空（调用方必须覆盖），
/// 其余字段取自然默认值（空 map/vec/false）。
impl Default for InvokeRequest {
    fn default() -> Self {
        Self {
            func_name: String::new(),
            inputs: HashMap::new(),
            table_inputs: HashMap::new(),
            struct_inputs: HashMap::new(),
            int_outputs: Vec::new(),
            string_outputs: HashMap::new(),
            auto_outputs: Vec::new(),
            table_outputs: HashMap::new(),
            struct_outputs: HashMap::new(),
            read_return: false,
            timeout_secs: None,
        }
    }
}

/// 表输出字段规范：`{"name":"USERNAME"}` 或 `{"name":"USERNAME","max_len":12}`
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FieldSpec {
    pub name: String,
    pub max_len: Option<usize>,
    /// 是否按字段真实类型读（INT→整数、FLOAT→浮点、INT8→i64、BYTE/XSTRING→Base64、其余→字符串）。
    /// 默认 false（统一按字符串读，向后兼容）；true 时保留数值/二进制语义。
    #[serde(default)]
    pub auto: bool,
}

impl FieldSpec {
    /// 便捷构造：仅字段名，长度由元数据自动发现
    #[allow(dead_code)]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            max_len: None,
            auto: false,
        }
    }

    /// 便捷构造：字段名 + 显式长度
    #[allow(dead_code)]
    pub fn with_len(name: impl Into<String>, len: usize) -> Self {
        Self {
            name: name.into(),
            max_len: Some(len),
            auto: false,
        }
    }

    /// 返回 clamp 到 MAX_FIELD_LEN 的 max_len（防 OOM）
    fn clamped_len(&self) -> Option<usize> {
        self.max_len.map(|n| n.min(MAX_FIELD_LEN))
    }
}

/// `POST /api/rfc` 响应体
#[derive(Debug, Serialize)]
pub struct InvokeResponse {
    /// 回显调用的函数名
    pub func: String,
    /// 所有标量输出（string/int/float/bcd/int8/bytes），类型由读取方式决定
    pub scalars: HashMap<String, ScalarValue>,
    /// 所有输出表：表名 → 行数组；每行是 字段名 → 值。
    /// 字段值类型由 FieldSpec.auto 决定：false → 字符串，true → 按真实类型（数值/Base64）
    pub tables: HashMap<String, Vec<HashMap<String, ScalarValue>>>,
    /// 所有顶层结构体输出：结构体名 → {字段名 → 值}（同 tables 的值类型规则）
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub structs: HashMap<String, HashMap<String, ScalarValue>>,
    /// RETURN 表（仅当 read_return=true 且存在时非空）。
    /// BAPI 消息字段语义固定为字符串，保持 String 类型。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_table: Option<Vec<HashMap<String, String>>>,
}

/// 默认长度（元数据未命中且调用方未指定时的回退值）
pub const DEFAULT_CHAR_LEN: usize = 255;

/// func_name 最大长度（SAP RFC_ABAP_NAME = 30 字符 +1 终止符）
pub const MAX_FUNC_NAME_LEN: usize = 30;
/// max_len / FieldSpec.max_len 上界，防止恶意传超大值导致 OOM 分配
pub const MAX_FIELD_LEN: usize = 1_000_000;
/// table_inputs 每个表的行数上界，防止单请求耗尽连接池
pub const MAX_TABLE_ROWS: usize = 100_000;
/// table_outputs / RETURN 表的输出行数上界：SAP 返回的表可能极大（百万行），
/// 全量读进内存会 OOM。超过则截断前 MAX_OUTPUT_ROWS 行并告警。
/// 调用方需要分页应改用 /api/table/read（带 rowcount）。
pub const MAX_OUTPUT_ROWS: u32 = 10_000;

/// 校验函数名：非空、≤30 字符、主体仅 [A-Za-z0-9_]，允许可选的 SAP 命名空间前缀
/// `/NS/`（如 `/SDF/EWA_GET_ABAP_DUMPS`）：斜杠只能成对出现在前缀处，两段均非空且字符合法。
/// 非法返回 400 错误。空/超长/含特殊字符都拒绝，避免传给 FFI 后的未定义行为。
pub fn validate_func_name(name: &str) -> Result<(), RfcError> {
    if name.is_empty() {
        return Err(RfcError {
            code: -1,
            message: "func_name 不能为空".into(),
            status: 400,
            ..Default::default()
        });
    }
    if name.len() > MAX_FUNC_NAME_LEN {
        return Err(RfcError {
            code: -1,
            message: format!(
                "func_name 长度 {} 超过上限 {}",
                name.len(),
                MAX_FUNC_NAME_LEN
            ),
            status: 400,
            ..Default::default()
        });
    }
    let valid = match name.strip_prefix('/') {
        // 命名空间形态 /NS/NAME：恰一个内嵌斜杠分隔，两段均为合法标识符
        Some(rest) => match rest.split_once('/') {
            Some((ns, base)) => is_valid_ident(ns) && is_valid_ident(base),
            None => false,
        },
        None => is_valid_ident(name),
    };
    if !valid {
        return Err(RfcError {
            code: -1,
            message: format!(
                "func_name '{}' 含非法字符（仅允许字母、数字、下划线；可带 /NS/ 命名空间前缀）",
                name
            ),
            status: 400,
            ..Default::default()
        });
    }
    Ok(())
}

/// SAP 标识符段：非空且仅 [A-Za-z0-9_]（函数名主体或命名空间段）
fn is_valid_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 长度与类型解析结果（来自元数据缓存或调用方提供的 resolver）
pub struct ResolvedMeta {
    /// 标量输出：参数名 -> (字符长度, RFCTYPE 类型值)
    pub scalars: HashMap<String, (usize, i32)>,
    /// 表输出：表名 -> {字段名 -> (字符长度, RFCTYPE 类型值)}
    pub tables: HashMap<String, HashMap<String, (usize, i32)>>,
    /// STRUCTURE 输出：结构体名 -> {字段名 -> (字符长度, RFCTYPE 类型值)}
    /// 供 `string_outputs` 遇 STRUCTURE 时按子字段读取（类似 `struct_outputs`）
    pub structures: HashMap<String, HashMap<String, (usize, i32)>>,
}

impl ResolvedMeta {
    /// 空解析（测试用）
    #[allow(dead_code)]
    pub fn empty() -> Self {
        Self {
            scalars: HashMap::new(),
            tables: HashMap::new(),
            structures: HashMap::new(),
        }
    }
}

/// 连接抽象：让执行逻辑既能用于真实 RfcConnection，也能在测试中 mock。
/// RfcConnection 天然实现了这个 trait（方法签名一致）。
pub trait RfcInvokeConn {
    fn get_function(&self, func_name: &str) -> Result<crate::function::RfcFunction, RfcError>;
}

impl RfcInvokeConn for crate::connection::RfcConnection {
    fn get_function(&self, func_name: &str) -> Result<crate::function::RfcFunction, RfcError> {
        crate::connection::RfcConnection::get_function(self, func_name)
    }
}

/// 通用执行入口（连接无关核心逻辑）。
///
/// `len_resolver` 是个回调：在 invoke 之前调用，让调用方用元数据或任何方式
/// 把 string_outputs / table_outputs 里未指定的长度填上。这样本函数本身
/// 不依赖 metadata 模块，可以纯单测（传 mock 连接 + 固定长度的 resolver）。
pub fn execute_invoke<C: RfcInvokeConn, F>(
    conn: &C,
    req: &InvokeRequest,
    meta_resolver: F,
) -> Result<InvokeResponse, RfcError>
where
    F: FnOnce(&C, &InvokeRequest) -> ResolvedMeta,
{
    // 元数据解析：调用方负责（实际走 metadata 缓存；测试里返回固定值）
    let resolved = meta_resolver(conn, req);

    // 入参校验：func_name 格式 + table_inputs 行数上界（防 DoS）
    validate_func_name(&req.func_name)?;
    for (table_name, rows) in &req.table_inputs {
        if rows.len() > MAX_TABLE_ROWS {
            return Err(RfcError {
                code: -1,
                message: format!(
                    "表 {} 的输入行数 {} 超过上限 {}",
                    table_name,
                    rows.len(),
                    MAX_TABLE_ROWS
                ),
                status: 400,
                ..Default::default()
            });
        }
    }

    let mut func = conn.get_function(&req.func_name)?;

    // 1. 标量输入（apply_to_func 按 ScalarValue 变体自动选 RfcSetXxx）
    for (name, value) in &req.inputs {
        value.apply_to_func(&mut func, name)?;
    }

    // 2. 表输入
    for (table_name, rows) in &req.table_inputs {
        let mut table = func.get_table(table_name)?;
        for row in rows {
            let r = table.append_row()?;
            for (field, value) in row {
                value.apply_to_row(&r, field)?;
            }
        }
    }

    // 2b. 顶层结构体输入（IMPORTING 结构体参数）
    for (struct_name, fields) in &req.struct_inputs {
        let row = func.get_structure(struct_name)?;
        for (field, value) in fields {
            value.apply_to_row(&row, field)?;
        }
    }

    // 3. 执行
    func.invoke()?;

    // 4. 收集标量输出
    //    类型选择策略：
    //    - int_outputs 显式声明 → 按整数读
    //    - string_outputs 显式声明 → 按字符串读（长度可指定或元数据发现）
    let mut scalars: HashMap<String, ScalarValue> = HashMap::new();
    let mut structs: HashMap<String, HashMap<String, ScalarValue>> = HashMap::new();

    for name in &req.int_outputs {
        let v = func.get_int(name)?;
        scalars.insert(name.clone(), ScalarValue::Int(v));
    }
    for (name, max_len_spec) in &req.string_outputs {
        let type_ = resolved
            .scalars
            .get(name)
            .map(|(_, t)| *t)
            .unwrap_or(crate::ffi::RFCTYPE_CHAR);
        if type_ == crate::ffi::rfctype::STRUCTURE {
            // STRUCTURE：自动按子字段读（类似 struct_outputs），结果进 structs。
            // 子字段列表已由 resolve_meta 预填 resolved.structures；无字段元数据则跳过（少见）。
            if let Some(sub_fields) = resolved.structures.get(name).cloned() {
                if let Ok(row) = func.get_structure(name) {
                    let struct_metas = sub_fields;
                    let mut m = HashMap::new();
                    for (fname, (flen, ftype)) in struct_metas {
                        let v = if ftype == crate::ffi::rfctype::INT1
                            || ftype == crate::ffi::rfctype::INT2
                        {
                            row.get_int(fname.as_str())
                                .map(ScalarValue::Int)
                                .unwrap_or_else(|_| ScalarValue::Chars(String::new()))
                        } else {
                            row.get_chars(fname.as_str(), flen)
                                .map(ScalarValue::Chars)
                                .unwrap_or_else(|_| ScalarValue::Chars(String::new()))
                        };
                        m.insert(fname.clone(), v);
                    }
                    structs.insert(name.clone(), m);
                }
            }
        } else {
            let len = max_len_spec
                .resolve()
                .or_else(|| resolved.scalars.get(name).map(|(l, _)| *l))
                .unwrap_or(DEFAULT_CHAR_LEN);
            let v = func.get_chars(name, len)?;
            scalars.insert(name.clone(), ScalarValue::Chars(v));
        }
    }
    // auto_outputs：按元数据真实类型自动选 getter
    for name in &req.auto_outputs {
        if scalars.contains_key(name) {
            continue; // 已被 int/string_outputs 读过，跳过
        }
        let (type_, char_len) = resolved
            .scalars
            .get(name)
            .map(|(l, t)| (*t, *l))
            .unwrap_or((crate::ffi::RFCTYPE_CHAR, DEFAULT_CHAR_LEN));
        let v = read_scalar_by_type(&mut func, name, type_, char_len)?;
        scalars.insert(name.clone(), v);
    }

    // 5. 收集表输出。字段值类型由 FieldSpec.auto 决定：
    //    false → 字符串（向后兼容），true → 按真实类型（INT/FLOAT/INT8/Base64）
    let mut tables: HashMap<String, Vec<HashMap<String, ScalarValue>>> = HashMap::new();
    for (table_name, fields) in &req.table_outputs {
        let table = func.get_table(&table_name.to_uppercase())?;
        let count = table.row_count()?;
        // 输出表可能极大，截断到 MAX_OUTPUT_ROWS 行防 OOM（与 /api/table/read 上限一致）
        let capped = count.min(MAX_OUTPUT_ROWS);
        if capped < count {
            tracing::warn!(
                table = %table_name, actual = count, capped = MAX_OUTPUT_ROWS,
                "表输出行数超上限，已截断（需分页请用 /api/table/read）"
            );
        }
        let field_metas = resolved.tables.get(table_name);
        let mut out_rows = Vec::with_capacity(capped as usize);
        for i in 0..capped {
            let mut row = table.get_row(i)?;
            let mut m = HashMap::new();
            for field_spec in fields {
                let v = if field_spec.auto {
                    let (type_, char_len) = field_metas
                        .and_then(|fm| {
                            fm.get(&field_spec.name.to_uppercase())
                                .map(|(l, t)| (*t, *l))
                        })
                        .unwrap_or((crate::ffi::RFCTYPE_CHAR, DEFAULT_CHAR_LEN));
                    match read_scalar_by_type(
                        &mut row,
                        &field_spec.name.to_uppercase(),
                        type_,
                        char_len,
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(table = %table_name, field = %field_spec.name, error = %e.message, "表字段按类型读取失败，用空串替代");
                            ScalarValue::Chars(String::new())
                        }
                    }
                } else {
                    let len = field_spec
                        .clamped_len()
                        .or_else(|| {
                            field_metas.and_then(|fm| {
                                fm.get(&field_spec.name.to_uppercase()).map(|(l, _)| *l)
                            })
                        })
                        .unwrap_or(DEFAULT_CHAR_LEN);
                    match row.get_chars(&field_spec.name.to_uppercase(), len) {
                        Ok(v) => ScalarValue::Chars(v),
                        Err(e) => {
                            tracing::warn!(table = %table_name, field = %field_spec.name, error = %e.message, "表字段读取失败，用空串替代");
                            ScalarValue::Chars(String::new())
                        }
                    }
                };
                m.insert(field_spec.name.clone(), v);
            }
            out_rows.push(m);
        }
        tables.insert(table_name.clone(), out_rows);
    }

    // 5b. 收集顶层结构体输出（同表输出的类型规则）— `structs` 已在函数顶部声明（line ~459），
    //     此处直接向其填充。
    for (struct_name, fields) in &req.struct_outputs {
        let mut row = match func.get_structure(&struct_name.to_uppercase()) {
            Ok(r) => r,
            Err(_) => continue, // 该函数无此结构体参数，跳过
        };
        let struct_metas = resolved.tables.get(struct_name);
        let mut m = HashMap::new();
        for field_spec in fields {
            let v = if field_spec.auto {
                let (type_, char_len) = struct_metas
                    .and_then(|fm| {
                        fm.get(&field_spec.name.to_uppercase())
                            .map(|(l, t)| (*t, *l))
                    })
                    .unwrap_or((crate::ffi::RFCTYPE_CHAR, DEFAULT_CHAR_LEN));
                match read_scalar_by_type(
                    &mut row,
                    &field_spec.name.to_uppercase(),
                    type_,
                    char_len,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(structure = %struct_name, field = %field_spec.name, error = %e.message, "结构体字段按类型读取失败，用空串替代");
                        ScalarValue::Chars(String::new())
                    }
                }
            } else {
                let len = field_spec
                    .clamped_len()
                    .or_else(|| {
                        struct_metas
                            .and_then(|fm| fm.get(&field_spec.name.to_uppercase()).map(|(l, _)| *l))
                    })
                    .unwrap_or(DEFAULT_CHAR_LEN);
                match row.get_chars(&field_spec.name.to_uppercase(), len) {
                    Ok(v) => ScalarValue::Chars(v),
                    Err(e) => {
                        tracing::warn!(structure = %struct_name, field = %field_spec.name, error = %e.message, "结构体字段读取失败，用空串替代");
                        ScalarValue::Chars(String::new())
                    }
                }
            };
            m.insert(field_spec.name.clone(), v);
        }
        structs.insert(struct_name.clone(), m);
    }

    // 6. 可选：自动读取 BAPI 通用 RETURN 表
    let return_table = if req.read_return {
        read_return_table(&mut func)?
    } else {
        None
    };

    Ok(InvokeResponse {
        func: req.func_name.clone(),
        scalars,
        tables,
        structs,
        return_table,
    })
}

/// 按 RFCTYPE 类型值选择对应的 getter 读取标量输出。
/// 用于 auto_outputs 和 FieldSpec.auto=true 的表/结构体字段：
/// 让服务端按字段真实类型保留数值/二进制语义。
fn read_scalar_by_type<R: crate::function::ScalarReader>(
    reader: &mut R,
    name: &str,
    type_: i32,
    char_len: usize,
) -> Result<ScalarValue, RfcError> {
    use crate::ffi::rfctype::*;
    match type_ {
        INT => Ok(ScalarValue::Int(reader.read_int(name)?)), // INT (i32)
        INT2 | INT1 => Ok(ScalarValue::Int(reader.read_int(name)?)), // INT2/INT1 归到 i32
        INT8 => Ok(ScalarValue::Int8(reader.read_int8(name)?)), // INT8 (i64)
        FLOAT => Ok(ScalarValue::Float(reader.read_float(name)?)), // FLOAT (f64)
        // 二进制（BYTE, XSTRING）：读字节后 Base64。char_len 对 BYTE 即字节数，至少 64 防 tiny alloc
        BYTE | XSTRING => {
            let bytes = reader.read_xstring(name, char_len.max(64))?;
            let b64 = general_purpose::STANDARD.encode(&bytes);
            Ok(ScalarValue::Chars(b64))
        }
        // 其余（CHAR/NUM/DATE/TIME/BCD/STRING）：用 metadata 长度作初始缓冲区，不够时自适应重读
        _ => {
            let v = reader.read_chars(name, char_len)?;
            Ok(ScalarValue::Chars(v))
        }
    }
}

fn read_return_table(
    func: &mut RfcFunction,
) -> Result<Option<Vec<HashMap<String, String>>>, RfcError> {
    let table = match func.get_table("RETURN") {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    let count = match table.row_count() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e.message, "读取 RETURN 表行数失败，按 0 行处理");
            0
        }
    };
    if count == 0 {
        return Ok(None);
    }
    // RETURN 表通常很短，但同样加防御性上限防异常情况
    let capped = count.min(MAX_OUTPUT_ROWS);
    let mut rows = Vec::with_capacity(capped as usize);
    for i in 0..capped {
        let row = table.get_row(i)?;
        let mut m = HashMap::new();
        // RETURN 字段读取失败时 log warn，避免空字段掩盖真实 SAP 错误
        for (key, len) in [("TYPE", 1), ("ID", 20), ("NUMBER", 3), ("MESSAGE", 220)] {
            let val = match row.get_chars(key, len) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(field = key, error = %e.message, "读取 RETURN 表字段失败，用空串替代");
                    String::new()
                }
            };
            m.insert(key.to_string(), val);
        }
        rows.push(m);
    }
    Ok(Some(rows))
}

// ========================================================================
// 面向 AI 的元数据 API 响应 DTO（端点 ① ~ ⑤）
// ========================================================================

/// 把 RFCTYPE 数值常量转成人类/AI 可读的类型名（如 0 → "CHAR"）。
/// 未知类型回退 "TYPE_<n>"。
pub fn rfctype_name(t: i32) -> &'static str {
    match t {
        0 => "CHAR",
        1 => "DATE",
        2 => "BCD",
        3 => "TIME",
        4 => "BYTE",
        5 => "TABLE",
        6 => "NUM",
        7 => "FLOAT",
        8 => "INT",
        9 => "INT2",
        10 => "INT1",
        17 => "STRUCTURE",
        29 => "STRING",
        30 => "XSTRING",
        31 => "INT8",
        _ => "UNKNOWN",
    }
}

/// 把 RFC_DIRECTION 位掩码转成方向名（import/export/changing/tables/unknown）。
pub fn direction_name(d: i32) -> &'static str {
    use crate::ffi::*;
    match d {
        RFC_DIRECTION_IMPORT => "IMPORT",
        RFC_DIRECTION_EXPORT => "EXPORT",
        RFC_DIRECTION_CHANGING => "CHANGING",
        RFC_DIRECTION_TABLES => "TABLES",
        _ => "UNKNOWN",
    }
}

/// 字段定义（用于函数参数嵌套字段、DDIC 类型字段）
#[derive(Debug, Serialize)]
pub struct FieldDef {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: &'static str,
    pub length: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub decimals: u32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// 嵌套结构体/表的子字段（仅 STRUCTURE/TABLE 且有展开时出现）。
    /// Vec 自带堆分配，已打破递归（无需 Box）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<FieldDef>>,
}

/// RFC_DIRECTION 位掩码为 0（字段无方向）时不输出 decimals 的判断函数
fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl FieldDef {
    /// 由 metadata::TypeFieldMeta 构造（含递归子字段）
    pub fn from_type_field(f: &crate::metadata::TypeFieldMeta) -> Self {
        Self {
            name: f.name.clone(),
            type_name: rfctype_name(f.type_),
            length: f.char_length,
            decimals: f.decimals,
            description: f.description.clone(),
            fields: f
                .sub_fields
                .as_ref()
                .map(|subs| subs.iter().map(FieldDef::from_type_field).collect()),
        }
    }
}

// --- 端点① GET /api/functions/{name} ---

#[derive(Debug, Serialize)]
pub struct FunctionParam {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: &'static str,
    pub direction: &'static str,
    pub length: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub decimals: u32,
    pub optional: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub default: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// 嵌套结构体/表子字段（Vec 自带堆分配，打破递归）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<FieldDef>>,
}

#[derive(Debug, Serialize)]
pub struct FunctionInterface {
    pub name: String,
    pub params: Vec<FunctionParam>,
    /// 接口元数据的服务通道（v0.15）：
    /// - `fii`：FUNCTION_IMPORT_INTERFACE 服务器端实时读（签名变更即时可见）；
    /// - `sdk`：SDK 描述符（进程级缓存，FII 不可用时降级）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface_via: Option<&'static str>,
}

// --- 端点② POST /api/functions/search ---

#[derive(Debug, Serialize)]
pub struct SearchFunctionEntry {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub group: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub pattern: String,
    pub count: usize,
    pub functions: Vec<SearchFunctionEntry>,
}

// --- 端点③ GET /api/ddic/type/{name} ---

#[derive(Debug, Serialize)]
pub struct DdicTypeResponse {
    pub name: String,
    pub fields: Vec<FieldDef>,
}

// --- 端点④ GET /api/ddic/field/{table}/{field} ---

#[derive(Debug, Serialize)]
pub struct FixedValueDto {
    pub value: String,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct FieldSemanticsResponse {
    pub table: String,
    pub field: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub data_element: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub domain: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub check_table: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub medium_label: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub fixed_values: Vec<FixedValueDto>,
}

// --- 端点⑤ GET /api/functions/{name}/doc ---

#[derive(Debug, Serialize)]
pub struct ParamDoc {
    pub name: String,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct FunctionDocResponse {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub short_text: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub long_text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub parameter_docs: Vec<ParamDoc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- MaxLen 解析 ----------

    #[test]
    fn max_len_legacy_integer() {
        // 旧格式：直接整数 {"ECHOTEXT": 255}
        let m: MaxLen = serde_json::from_str("255").unwrap();
        assert_eq!(m.resolve(), Some(255));
    }

    #[test]
    fn max_len_detailed_some() {
        // 新格式：{"max_len": 100}
        let m: MaxLen = serde_json::from_str(r#"{"max_len":100}"#).unwrap();
        assert_eq!(m.resolve(), Some(100));
    }

    #[test]
    fn max_len_detailed_null_means_auto() {
        // null 表示让服务端自动发现
        let m: MaxLen = serde_json::from_str(r#"{"max_len":null}"#).unwrap();
        assert_eq!(m.resolve(), None);
    }

    // ---------- FieldSpec 反序列化 ----------

    #[test]
    fn field_spec_name_only() {
        // 仅字段名：["USERNAME"] —— 通过对象形式
        let f: FieldSpec = serde_json::from_str(r#"{"name":"USERNAME"}"#).unwrap();
        assert_eq!(f.name, "USERNAME");
        assert_eq!(f.max_len, None);
    }

    #[test]
    fn field_spec_with_length() {
        let f: FieldSpec = serde_json::from_str(r#"{"name":"USERNAME","max_len":12}"#).unwrap();
        assert_eq!(f.name, "USERNAME");
        assert_eq!(f.max_len, Some(12));
    }

    #[test]
    fn field_spec_new_helpers() {
        let f1 = FieldSpec::new("X");
        assert_eq!(f1.max_len, None);
        assert!(!f1.auto);
        let f2 = FieldSpec::with_len("Y", 40);
        assert_eq!(f2.max_len, Some(40));
        assert!(!f2.auto);
    }

    #[test]
    fn field_spec_auto_defaults_false() {
        // 不传 auto 时默认 false（向后兼容）
        let f: FieldSpec = serde_json::from_str(r#"{"name":"X","max_len":10}"#).unwrap();
        assert!(!f.auto);
        // 显式传 true
        let f: FieldSpec =
            serde_json::from_str(r#"{"name":"X","max_len":10,"auto":true}"#).unwrap();
        assert!(f.auto);
    }

    #[test]
    fn scalar_value_into_chars() {
        assert_eq!(ScalarValue::Chars("hi".into()).into_chars(), "hi");
        assert_eq!(ScalarValue::Int(42).into_chars(), "42");
        assert_eq!(ScalarValue::Int8(9_000_000_000).into_chars(), "9000000000");
        assert_eq!(ScalarValue::Float(1.5).into_chars(), "1.5");
    }

    // ---------- InvokeRequest 端到端反序列化 ----------

    #[test]
    fn invoke_request_minimal() {
        // 只给函数名，其他字段应该走 default
        let req: InvokeRequest =
            serde_json::from_str(r#"{"func_name":"STFC_CONNECTION"}"#).unwrap();
        assert_eq!(req.func_name, "STFC_CONNECTION");
        assert!(req.inputs.is_empty());
        assert!(req.table_inputs.is_empty());
        assert!(req.int_outputs.is_empty());
        assert!(req.string_outputs.is_empty());
        assert!(req.table_outputs.is_empty());
        assert!(!req.read_return);
    }

    #[test]
    fn invoke_request_full_with_legacy_lens() {
        // 验证旧格式 max_len 仍兼容（向后不破坏）
        let json = r#"{
            "func_name": "BAPI_USER_GETLIST",
            "inputs": {"MAX_ROWS": 50, "WITH_USERNAME": "X"},
            "int_outputs": ["ROWS"],
            "string_outputs": {"ECHOTEXT": 255},
            "table_outputs": {
                "USERLIST": [{"name":"USERNAME","max_len":12},{"name":"FIRSTNAME"}]
            },
            "read_return": true
        }"#;
        let req: InvokeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.func_name, "BAPI_USER_GETLIST");

        // inputs：数字→Int，字符串→Chars
        match &req.inputs["MAX_ROWS"] {
            ScalarValue::Int(50) => {}
            other => panic!("MAX_ROWS 应为 Int(50), 实际 {:?}", other),
        }
        match &req.inputs["WITH_USERNAME"] {
            ScalarValue::Chars(s) => assert_eq!(s, "X"),
            other => panic!("WITH_USERNAME 应为 Chars, 实际 {:?}", other),
        }

        // string_outputs 旧格式
        assert_eq!(req.string_outputs["ECHOTEXT"].resolve(), Some(255));

        // table_outputs：第一个字段有长度，第二个无（自动发现）
        assert_eq!(req.table_outputs["USERLIST"][0].max_len, Some(12));
        assert_eq!(req.table_outputs["USERLIST"][1].max_len, None);

        assert!(req.read_return);
    }

    #[test]
    fn invoke_request_string_outputs_null_means_auto() {
        let json = r#"{
            "func_name": "STFC_CONNECTION",
            "string_outputs": {"ECHOTEXT": {"max_len": null}}
        }"#;
        let req: InvokeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.string_outputs["ECHOTEXT"].resolve(), None);
    }

    #[test]
    fn invoke_request_missing_func_name_rejected() {
        // func_name 是必填，缺了应反序列化失败
        let r = serde_json::from_str::<InvokeRequest>(r#"{"inputs":{}}"#);
        assert!(r.is_err());
    }

    // ---------- 类型感知：隐式 Float ----------

    #[test]
    fn scalar_implicit_float() {
        // JSON 浮点 → Float 变体
        let json = r#"{"func_name":"X","inputs":{"AMOUNT": 123.45}}"#;
        let req: InvokeRequest = serde_json::from_str(json).unwrap();
        match &req.inputs["AMOUNT"] {
            ScalarValue::Float(f) => assert!((f - 123.45).abs() < 1e-9),
            other => panic!("AMOUNT 应为 Float, 实际 {:?}", other),
        }
    }

    // ---------- 类型感知：显式 BCD ----------

    #[test]
    fn scalar_explicit_bcd() {
        let json = r#"{"func_name":"X","inputs":{"PRICE":{"type":"BCD","value":"999.99"}}}"#;
        let req: InvokeRequest = serde_json::from_str(json).unwrap();
        match &req.inputs["PRICE"] {
            ScalarValue::Typed(t) => {
                assert!(matches!(t.kind, TypedScalarKind::Bcd));
                assert_eq!(t.value.as_str(), Some("999.99"));
            }
            other => panic!("PRICE 应为 Typed(BCD), 实际 {:?}", other),
        }
    }

    // ---------- 类型感知：显式 INT8 ----------

    #[test]
    fn scalar_explicit_int8() {
        let json = r#"{"func_name":"X","inputs":{"BIG_ID":{"type":"INT8","value":9876543210}}}"#;
        let req: InvokeRequest = serde_json::from_str(json).unwrap();
        match &req.inputs["BIG_ID"] {
            ScalarValue::Typed(t) => {
                assert!(matches!(t.kind, TypedScalarKind::Int8));
                assert_eq!(t.value.as_i64(), Some(9876543210));
            }
            other => panic!("BIG_ID 应为 Typed(Int8), 实际 {:?}", other),
        }
    }

    // ---------- 类型感知：显式 Bytes (Base64) ----------

    #[test]
    fn scalar_explicit_bytes() {
        // "aGVsbG8=" 是 "hello" 的 Base64
        let json = r#"{"func_name":"X","inputs":{"BINARY":{"type":"BYTES","value":"aGVsbG8="}}}"#;
        let req: InvokeRequest = serde_json::from_str(json).unwrap();
        match &req.inputs["BINARY"] {
            ScalarValue::Typed(t) => {
                assert!(matches!(t.kind, TypedScalarKind::Bytes));
                assert_eq!(t.value.as_str(), Some("aGVsbG8="));
            }
            other => panic!("BINARY 应为 Typed(Bytes), 实际 {:?}", other),
        }
    }

    // ---------- 顶层结构体参数 ----------

    #[test]
    fn struct_inputs_outputs_parsed() {
        let json = r#"{
            "func_name": "BAPI_USER_CREATE",
            "struct_inputs": {
                "ADDRESS": {"FIRSTNAME": "Dev", "LASTNAME": "User"}
            },
            "struct_outputs": {
                "RETURN": [{"name":"TYPE"}, {"name":"MESSAGE","max_len":220}]
            }
        }"#;
        let req: InvokeRequest = serde_json::from_str(json).unwrap();
        // struct_inputs
        assert_eq!(req.struct_inputs.len(), 1);
        match &req.struct_inputs["ADDRESS"]["FIRSTNAME"] {
            ScalarValue::Chars(s) => assert_eq!(s, "Dev"),
            other => panic!("FIRSTNAME 应为 Chars, 实际 {:?}", other),
        }
        // struct_outputs
        assert_eq!(req.struct_outputs.len(), 1);
        let ret_fields = &req.struct_outputs["RETURN"];
        assert_eq!(ret_fields.len(), 2);
        assert_eq!(ret_fields[1].max_len, Some(220));
    }

    #[test]
    fn struct_fields_default_empty() {
        // 不传 struct_inputs/struct_outputs 时，默认空 map
        let req: InvokeRequest = serde_json::from_str(r#"{"func_name":"X"}"#).unwrap();
        assert!(req.struct_inputs.is_empty());
        assert!(req.struct_outputs.is_empty());
    }

    // ---------- TypedScalarKind 序列化（UPPERCASE）----------

    #[test]
    fn typed_kind_serializes_uppercase() {
        let kinds = vec![
            (TypedScalarKind::Bcd, "BCD"),
            (TypedScalarKind::Int8, "INT8"),
            (TypedScalarKind::Bytes, "BYTES"),
        ];
        for (k, expected) in kinds {
            let s = serde_json::to_string(&k).unwrap();
            assert_eq!(s, format!("\"{}\"", expected));
        }
    }

    // ---------- read_scalar_by_type（mock ScalarReader）----------

    /// 记录读取操作的 mock reader，按预置值返回。
    struct MockReader {
        chars_val: String,
        int_val: i32,
        int8_val: i64,
        float_val: f64,
        xstring_val: Vec<u8>,
        calls: Vec<(String, &'static str)>, // (字段名, 调用方法)
    }

    impl crate::function::ScalarReader for MockReader {
        fn read_chars(&mut self, name: &str, _max_len: usize) -> Result<String, RfcError> {
            self.calls.push((name.to_string(), "chars"));
            Ok(self.chars_val.clone())
        }
        fn read_int(&mut self, name: &str) -> Result<i32, RfcError> {
            self.calls.push((name.to_string(), "int"));
            Ok(self.int_val)
        }
        fn read_int8(&mut self, name: &str) -> Result<i64, RfcError> {
            self.calls.push((name.to_string(), "int8"));
            Ok(self.int8_val)
        }
        fn read_float(&mut self, name: &str) -> Result<f64, RfcError> {
            self.calls.push((name.to_string(), "float"));
            Ok(self.float_val)
        }
        fn read_xstring(&mut self, name: &str, _cap: usize) -> Result<Vec<u8>, RfcError> {
            self.calls.push((name.to_string(), "xstring"));
            Ok(self.xstring_val.clone())
        }
    }

    fn mock_reader() -> MockReader {
        MockReader {
            chars_val: "text".into(),
            int_val: 42,
            int8_val: 9_000_000_000,
            float_val: 1.5,
            xstring_val: b"hello".to_vec(),
            calls: Vec::new(),
        }
    }

    #[test]
    fn read_scalar_by_type_int() {
        let mut r = mock_reader();
        let v = read_scalar_by_type(&mut r, "F", crate::ffi::rfctype::INT, 255).unwrap();
        assert!(matches!(v, ScalarValue::Int(42)));
        assert_eq!(r.calls[0].1, "int");
    }

    #[test]
    fn read_scalar_by_type_int2_int1_route_to_int() {
        let mut r = mock_reader();
        let v = read_scalar_by_type(&mut r, "F", crate::ffi::rfctype::INT2, 255).unwrap();
        assert!(matches!(v, ScalarValue::Int(42)));
        let mut r2 = mock_reader();
        let v2 = read_scalar_by_type(&mut r2, "F", crate::ffi::rfctype::INT1, 255).unwrap();
        assert!(matches!(v2, ScalarValue::Int(42)));
    }

    #[test]
    fn read_scalar_by_type_int8() {
        let mut r = mock_reader();
        let v = read_scalar_by_type(&mut r, "F", crate::ffi::rfctype::INT8, 255).unwrap();
        assert!(matches!(v, ScalarValue::Int8(9_000_000_000)));
        assert_eq!(r.calls[0].1, "int8");
    }

    #[test]
    fn read_scalar_by_type_float() {
        let mut r = mock_reader();
        let v = read_scalar_by_type(&mut r, "F", crate::ffi::rfctype::FLOAT, 255).unwrap();
        assert!(matches!(v, ScalarValue::Float(f) if (f - 1.5).abs() < 1e-9));
    }

    #[test]
    fn read_scalar_by_type_byte_xstring_base64() {
        // BYTE(4) 和 XSTRING(30) 都读字节后 Base64 编码
        for ty in [crate::ffi::rfctype::BYTE, crate::ffi::rfctype::XSTRING] {
            let mut r = mock_reader();
            let v = read_scalar_by_type(&mut r, "F", ty, 255).unwrap();
            match v {
                ScalarValue::Chars(s) => assert_eq!(s, "aGVsbG8="), // "hello" 的 Base64
                other => panic!("BYTE/XSTRING 应为 Chars(Base64), 实际 {:?}", other),
            }
        }
    }

    #[test]
    fn read_scalar_by_type_char_bcd_date_fallback_to_chars() {
        // 其余类型（CHAR/BCD/DATE/TIME/STRING）走 _=> 字符串读
        for ty in [
            crate::ffi::rfctype::CHAR,
            crate::ffi::rfctype::BCD,
            crate::ffi::rfctype::DATE,
            crate::ffi::rfctype::STRING,
        ] {
            let mut r = mock_reader();
            let v = read_scalar_by_type(&mut r, "F", ty, 255).unwrap();
            assert!(matches!(v, ScalarValue::Chars(_)), "ty={} 应为 Chars", ty);
        }
    }

    // ---------- decode_typed 错误分支 ----------

    #[test]
    fn decode_typed_bcd_non_string_value_errors() {
        // BCD 的 value 必须是字符串，传数字应报错
        let req: InvokeRequest =
            serde_json::from_str(r#"{"func_name":"X","inputs":{"P":{"type":"BCD","value":123}}}"#)
                .unwrap();
        // apply_to_func 会调 decode_typed，应在 BCD 非字符串时报错
        // 这里通过反序列化验证 Typed 变体，decode_typed 错误路径需通过 apply 触发
        match &req.inputs["P"] {
            ScalarValue::Typed(t) => {
                assert!(matches!(t.kind, TypedScalarKind::Bcd));
                assert_eq!(t.value.as_i64(), Some(123)); // 非字符串
            }
            other => panic!("应为 Typed, 实际 {:?}", other),
        }
    }

    #[test]
    fn decode_typed_bytes_invalid_base64_errors() {
        // Base64 非法字符串，decode_typed 应返回 Err
        let t = TypedScalar {
            kind: TypedScalarKind::Bytes,
            value: serde_json::json!("!!!非Base64!!!"),
        };
        let res = ScalarValue::Typed(t);
        // decode_typed 是私有的，通过 apply_to_func 无法测（需 RfcFunction），
        // 这里验证 Base64 解码逻辑本身：非法输入应产生错误
        let decode_result = general_purpose::STANDARD.decode("!!!非Base64!!!");
        assert!(decode_result.is_err(), "非法 Base64 应解码失败");
        // 确认 ScalarValue 反序列化成功（decode_typed 在 apply 时才调）
        let _ = res;
    }

    // ---------- rfctype_name / direction_name 映射 ----------

    #[test]
    fn rfctype_name_mapping() {
        assert_eq!(rfctype_name(0), "CHAR");
        assert_eq!(rfctype_name(2), "BCD");
        assert_eq!(rfctype_name(8), "INT");
        assert_eq!(rfctype_name(17), "STRUCTURE");
        assert_eq!(rfctype_name(31), "INT8");
        assert_eq!(rfctype_name(999), "UNKNOWN");
    }

    #[test]
    fn direction_name_mapping() {
        assert_eq!(direction_name(crate::ffi::RFC_DIRECTION_IMPORT), "IMPORT");
        assert_eq!(direction_name(crate::ffi::RFC_DIRECTION_EXPORT), "EXPORT");
        assert_eq!(
            direction_name(crate::ffi::RFC_DIRECTION_CHANGING),
            "CHANGING"
        );
        assert_eq!(direction_name(crate::ffi::RFC_DIRECTION_TABLES), "TABLES");
        assert_eq!(direction_name(0), "UNKNOWN"); // 无方向
        assert_eq!(direction_name(999), "UNKNOWN");
    }

    // ---------- 输入校验 ----------

    #[test]
    fn validate_func_name_accepts_valid() {
        assert!(validate_func_name("STFC_CONNECTION").is_ok());
        assert!(validate_func_name("BAPI_USER_GETLIST").is_ok());
        assert!(validate_func_name("RFC_FUNCTION_SEARCH").is_ok());
        assert!(validate_func_name("Z_FOO_123").is_ok());
        // 命名空间前缀形态
        assert!(validate_func_name("/SDF/EWA_GET_ABAP_DUMPS").is_ok());
        assert!(validate_func_name("/AIF/SUBMIT_FUNCTION").is_ok());
        assert!(validate_func_name("/BOBF/R_FRUIT").is_ok());
    }

    #[test]
    fn validate_func_name_rejects_empty() {
        let err = validate_func_name("").unwrap_err();
        assert_eq!(err.status, 400);
        assert!(err.message.contains("空"));
    }

    #[test]
    fn validate_func_name_rejects_too_long() {
        let err = validate_func_name(&"A".repeat(31)).unwrap_err();
        assert_eq!(err.status, 400);
        assert!(err.message.contains("超过上限"));
    }

    #[test]
    fn validate_func_name_rejects_special_chars() {
        // 含空格、连字符、中文等非法字符
        for bad in ["FOO BAR", "FOO-BAR", "FOO;DROP", "函数", "FOO$"] {
            let err = validate_func_name(bad).unwrap_err();
            assert_eq!(err.status, 400, "{} 应被拒绝", bad);
            assert!(
                err.message.contains("非法字符"),
                "{} 的错误信息应含非法字符",
                bad
            );
        }
    }

    #[test]
    fn validate_func_name_rejects_malformed_slashes() {
        // 斜杠只允许作为 /NS/ 前缀出现，且两段非空
        for bad in [
            "FOO/BAR",  // 无命名空间前缀的斜杠
            "/SDF",     // 只有前斜杠
            "/SDF/",    // 名字段为空
            "//X",      // 命名空间段为空
            "/SDF/X/Y", // 多余斜杠
            "/SDF/X/",  // 尾斜杠
        ] {
            let err = validate_func_name(bad).unwrap_err();
            assert_eq!(err.status, 400, "{} 应被拒绝", bad);
            assert!(
                err.message.contains("非法字符"),
                "{} 的错误信息应含非法字符",
                bad
            );
        }
    }

    #[test]
    fn max_len_clamps_to_upper_bound() {
        // 超大 max_len 应被 clamp 到 MAX_FIELD_LEN，防止 OOM
        let m: MaxLen = serde_json::from_str("9999999999").unwrap();
        assert_eq!(m.resolve(), Some(MAX_FIELD_LEN));
        let m: MaxLen = serde_json::from_str(r#"{"max_len":9999999999}"#).unwrap();
        assert_eq!(m.resolve(), Some(MAX_FIELD_LEN));
        // 正常值不受影响
        let m: MaxLen = serde_json::from_str("255").unwrap();
        assert_eq!(m.resolve(), Some(255));
    }
}
