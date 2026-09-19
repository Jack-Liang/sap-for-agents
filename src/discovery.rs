//! 面向 AI 的 SAP 元数据发现：封装对 ABAP 标准 RFC 的语义化调用。
//!
//! 与 `metadata.rs`（走 C API）不同，本模块的功能靠调用 ABAP 端的标准 RFC 实现：
//! - 搜索函数：`RFC_FUNCTION_SEARCH`
//! - DDIC 字段语义：`DDIF_FIELDINFO_GET`
//! - 函数长文档：`RFC_READ_TEXT`
//!
//! 这些都是普通 RFC，复用 `api::execute_collect` 即可，无需新 FFI。
//! 每个封装函数把原始表/标量结果映射成语义化的 Rust 结构体。

use crate::api::{FieldSpec, InvokeRequest, ScalarValue};
use crate::connection::RfcConnection;
use serde::Serialize;
use crate::error::RfcError;
use crate::executor::execute_collect;
use std::collections::HashMap;

/// 搜索结果条目：一个可远程调用的函数模块
#[derive(Debug, Clone, Serialize)]
pub struct FunctionEntry {
    pub name: String,
    /// 函数组（可能为空）
    pub group: String,
    /// 短文本描述
    pub description: String,
}

/// 搜索可远程调用的函数模块。
/// 内部调用 ABAP RFC `RFC_FUNCTION_SEARCH`。
///
/// - `pattern`: 函数名通配符，如 `BAPI_USER_*`（空表示 `*`）
/// - `group`: 函数组过滤（空表示不限制）
/// - `max_results`: 最多返回条数（防巨型结果集，默认 50）
pub fn search_functions(
    conn: &RfcConnection,
    pattern: &str,
    group: &str,
    max_results: usize,
) -> Result<Vec<FunctionEntry>, RfcError> {
    let req = InvokeRequest {
        func_name: "RFC_FUNCTION_SEARCH".to_string(),
        inputs: HashMap::from([
            (
                "FUNCNAME".to_string(),
                ScalarValue::Chars(if pattern.is_empty() {
                    "*".to_string()
                } else {
                    pattern.to_uppercase()
                }),
            ),
            (
                "GROUPNAME".to_string(),
                ScalarValue::Chars(group.to_uppercase()),
            ),
        ]),
        table_outputs: HashMap::from([("FUNCTIONS".to_string(), function_table_spec())]),
        ..Default::default()
    };

    // 无匹配时 SAP 抛 ABAP_EXCEPTION(NO_FUNCTION_FOUND)——按搜索 API 惯例视为"空结果"而非错误。
    // （且避免 error_info.message 残留上次失败调用信息，误导调用方 / 串扰多用户）
    let resp = match execute_collect(conn, &req) {
        Ok(r) => r,
        Err(e) if e.key == "NO_FUNCTION_FOUND" => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let table = resp.tables.get("FUNCTIONS").cloned().unwrap_or_default();

    let mut out = Vec::new();
    for row in table.into_iter().take(max_results) {
        out.push(FunctionEntry {
            name: row.get("FUNCNAME").map(|v| v.clone().into_chars()).unwrap_or_default(),
            group: row.get("GROUPNAME").map(|v| v.clone().into_chars()).unwrap_or_default(),
            description: row.get("STEXT").map(|v| v.clone().into_chars()).unwrap_or_default(),
        });
    }
    Ok(out)
}

/// RFC_FUNCTION_SEARCH 结果表的字段规格。
/// SAP 不同版本字段可能略有差异，对缺失字段容错（unwrap_or_default）。
fn function_table_spec() -> Vec<FieldSpec> {
    vec![
        FieldSpec {
            name: "FUNCNAME".to_string(),
            max_len: Some(30),
            auto: false,
        },
        FieldSpec {
            name: "GROUPNAME".to_string(),
            max_len: Some(18),
            auto: false,
        },
        FieldSpec {
            name: "STEXT".to_string(),
            max_len: Some(79),
            auto: false,
        },
    ]
}

/// DDIC 字段的固定值（域的值范围，如状态码 → 描述）
#[derive(Debug, Clone, Serialize)]
pub struct FixedValue {
    pub value: String,
    pub text: String,
}

/// 单个 DDIC 字段的语义元数据（来自 DDIF_FIELDINFO_GET 的 DFIES 结构）
#[derive(Debug, Clone, Default, Serialize)]
pub struct FieldSemantics {
    pub field: String,
    /// 数据元素（Roll Name）
    pub data_element: String,
    /// 域（Domain）
    pub domain: String,
    /// 检查表（Check Table）
    pub check_table: String,
    /// 字段描述（短文本）
    pub description: String,
    /// 字段文本（中等长度标签）
    pub medium_label: String,
    /// 固定值列表（域的固定值范围）
    pub fixed_values: Vec<FixedValue>,
}

/// 查询单个 DDIC 字段的语义元数据。
/// 内部调用 ABAP RFC `DDIF_FIELDINFO_GET`，返回 DFIES 结构 + 固定值表。
///
/// - `table`: 表/结构/视图名（如 MARA）
/// - `field`: 字段名（如 MATNR）
/// - `lang`: 语言（如 ZH/EN，影响文本标签语言）
pub fn read_ddic_field_info(
    conn: &RfcConnection,
    table: &str,
    field: &str,
    lang: &str,
) -> Result<FieldSemantics, RfcError> {
    let req = InvokeRequest {
        func_name: "DDIF_FIELDINFO_GET".to_string(),
        inputs: HashMap::from([
            ("TABNAME".to_string(), ScalarValue::Chars(table.to_uppercase())),
            // 注意：查结构单字段必须用 LFIELDNAME 而非 FIELDNAME（后者对结构无效）
            ("LFIELDNAME".to_string(), ScalarValue::Chars(field.to_uppercase())),
            ("LANGU".to_string(), ScalarValue::Chars(lang.to_string())),
            ("ALL_TYPES".to_string(), ScalarValue::Chars("X".to_string())),
        ]),
        // DFIES_WA 是 EXPORT 结构体（单字段语义），FIXED_VALUES 是 TABLES（固定值列表）
        struct_outputs: HashMap::from([("DFIES_WA".to_string(), dfies_field_spec())]),
        table_outputs: HashMap::from([(
            "FIXED_VALUES".to_string(),
            fixed_values_spec(),
        )]),
        ..Default::default()
    };

    let resp = execute_collect(conn, &req)?;
    let dfies = resp.structs.get("DFIES_WA").cloned().unwrap_or_default();

    let fixed_values = resp
        .tables
        .get("FIXED_VALUES")
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|row| FixedValue {
            value: row.get("LOW").map(|v| v.clone().into_chars()).unwrap_or_default(),
            text: row.get("DDTEXT").map(|v| v.clone().into_chars()).unwrap_or_default(),
        })
        .collect();

    Ok(FieldSemantics {
        field: field.to_uppercase(),
        data_element: dfies.get("ROLLNAME").map(|v| v.clone().into_chars()).unwrap_or_default(),
        domain: dfies.get("DOMNAME").map(|v| v.clone().into_chars()).unwrap_or_default(),
        check_table: dfies.get("CHECKTABLE").map(|v| v.clone().into_chars()).unwrap_or_default(),
        description: dfies.get("FIELDTEXT").map(|v| v.clone().into_chars()).unwrap_or_default(),
        medium_label: dfies.get("SCRTEXT_M").map(|v| v.clone().into_chars()).unwrap_or_default(),
        fixed_values,
    })
}

/// DFIES 结构体输出的字段规格（DDIF_FIELDINFO_GET 的 FIELDINFO 参数）。
/// 字段名取自 SAP DFIES 结构定义；对可能缺失的字段容错。
fn dfies_field_spec() -> Vec<FieldSpec> {
    [
        "ROLLNAME",  // 数据元素
        "DOMNAME",   // 域
        "CHECKTABLE", // 检查表
        "FIELDTEXT", // 字段描述
        "SCRTEXT_M", // 中等标签
        "SCRTEXT_L", // 长标签
        "SCRTEXT_S", // 短标签
    ]
    .into_iter()
    .map(|name| FieldSpec {
        name: name.to_string(),
        max_len: Some(79),
        auto: false,
    })
    .collect()
}

/// 固定值表（DD07V）的字段规格
fn fixed_values_spec() -> Vec<FieldSpec> {
    vec![
        FieldSpec {
            name: "LOW".to_string(),
            max_len: Some(10),
            auto: false,
        },
        FieldSpec {
            name: "DDTEXT".to_string(),
            max_len: Some(60),
            auto: false,
        },
    ]
}

/// 函数模块的文档（短文本 + SE37 长文本）
#[derive(Debug, Clone, Default, Serialize)]
pub struct FunctionDoc {
    #[allow(dead_code)]
    pub name: String,
    /// 短文本（函数模块的 STEXT，来自 FUNCTION_SEARCH 或元数据）
    pub short_text: String,
    /// SE37 长文档（来自 RFC_READ_TEXT，文本对象 FUNC，ID u）
    pub long_text: String,
    /// 读取过程中是否遇到警告（如文档对象不存在）
    pub warning: Option<String>,
}

/// 读取函数模块的 SE37 长文档。
/// 内部调用 ABAP 函数 `DOCU_GET`（SAP 标准文档读取，组 SDOC）。
/// 文档对象约定：
///   - OBJECT = 函数名
///   - ID = "FU"（Function Module 文档类）
///   - LANGU = lang
///
/// 并非所有函数都有文档；无文档时返回空 long_text（不报错）。
pub fn read_function_doc(
    conn: &RfcConnection,
    func_name: &str,
    lang: &str,
    short_text: &str,
) -> Result<FunctionDoc, RfcError> {
    let req = InvokeRequest {
        func_name: "DOCU_GET".to_string(),
        inputs: HashMap::from([
            ("OBJECT".to_string(), ScalarValue::Chars(func_name.to_uppercase())),
            ("ID".to_string(), ScalarValue::Chars("FU".to_string())),
            ("LANGU".to_string(), ScalarValue::Chars(lang.to_string())),
        ]),
        table_outputs: HashMap::from([("LINE".to_string(), text_lines_spec())]),
        ..Default::default()
    };

    match execute_collect(conn, &req) {
        Ok(resp) => {
            let lines = resp.tables.get("LINE").cloned().unwrap_or_default();
            // TLINE 结构：TDFORMAT(2) + TDLINE(132)，拼接所有 TDLINE 成完整文档
            let long_text = lines
                .iter()
                .map(|row| {
                    row.get("TDLINE").map(|v| v.clone().into_chars()).unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(FunctionDoc {
                name: func_name.to_string(),
                short_text: short_text.to_string(),
                long_text,
                warning: None,
            })
        }
        Err(e) => {
            // 文档读取失败不阻断——返回空文档 + 警告
            Ok(FunctionDoc {
                name: func_name.to_string(),
                short_text: short_text.to_string(),
                long_text: String::new(),
                warning: Some(format!("读取长文档失败（可能无文档或 DOCU_GET 不可用）: {}", e.message)),
            })
        }
    }
}

/// DOCU_GET 输出 LINE 表（TLINE 结构）的字段规格
fn text_lines_spec() -> Vec<FieldSpec> {
    vec![
        FieldSpec {
            name: "TDFORMAT".to_string(),
            max_len: Some(2),
            auto: false,
        },
        FieldSpec {
            name: "TDLINE".to_string(),
            max_len: Some(132),
            auto: false,
        },
    ]
}

/// 读函数模块的 ABAP 源代码（内部调 `RPY_FUNCTIONMODULE_READ`，源码在 `SOURCE` 表）。
///
/// 注意：该 FM **只有窄表 SOURCE（CHAR72），没有 SOURCE_EXTENDED**——那是
/// `RPY_PROGRAM_READ` 的参数（实测接口：向本 FM 传 SOURCE_EXTENDED 会得到
/// RFC_INVALID_PARAMETER「field not found」）。因此源码行宽超 72 时 SAP 直接
/// 报 FL 180「Source wider than 72 char」，本函数原样返回错误，由调用方
/// （server 层）走 ADT 降级通道兜底——宽表方案对这个 FM 不存在。
/// 返回源码行列表（每行一个字符串）。
pub fn read_function_source(
    conn: &RfcConnection,
    func_name: &str,
) -> Result<Vec<String>, RfcError> {
    let req = InvokeRequest {
        func_name: "RPY_FUNCTIONMODULE_READ".to_string(),
        inputs: HashMap::from([(
            "FUNCTIONNAME".to_string(),
            ScalarValue::Chars(func_name.to_uppercase()),
        )]),
        table_outputs: HashMap::from([("SOURCE".to_string(), source_line_spec())]),
        ..Default::default()
    };
    let resp = execute_collect(conn, &req)?;
    Ok(resp
        .tables
        .get("SOURCE")
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|row| {
            row.get("LINE")
                .map(|v| v.clone().into_chars())
                .unwrap_or_default()
        })
        .collect())
}

/// 读 ABAP 程序源代码（内部调 `RPY_PROGRAM_READ`，源码在 `SOURCE_EXTENDED` 表）。
/// 程序不存在时 SAP 返回错误（透传 → 404）；存在但无源码 → 空 Vec。
pub fn read_program_source(
    conn: &RfcConnection,
    prog_name: &str,
) -> Result<Vec<String>, RfcError> {
    let req = InvokeRequest {
        func_name: "RPY_PROGRAM_READ".to_string(),
        inputs: HashMap::from([(
            "PROGRAM_NAME".to_string(),
            ScalarValue::Chars(prog_name.to_uppercase()),
        )]),
        table_outputs: HashMap::from([("SOURCE_EXTENDED".to_string(), source_line_spec())]),
        ..Default::default()
    };
    let resp = execute_collect(conn, &req)?;
    Ok(resp
        .tables
        .get("SOURCE_EXTENDED")
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|row| {
            row.get("LINE")
                .map(|v| v.clone().into_chars())
                .unwrap_or_default()
        })
        .collect())
}

/// 源码行表字段规格（`LINE`: CHAR，源码一行）。
fn source_line_spec() -> Vec<FieldSpec> {
    vec![FieldSpec {
        name: "LINE".to_string(),
        max_len: Some(255),
        auto: false,
    }]
}

/// 读 SAP 透明表数据（内部调 `RFC_READ_TABLE`，用 `ET_DATA` 返回避免 512 截断）。
///
/// - `table`: 表名（如 `T000`、`USR01`）
/// - `fields`: 要查的字段名（**必须非空**，决定返回列顺序 + 字段名映射）
/// - `where_clauses`: WHERE 条件（ABAP Open SQL 片段，每段一行；空 = 无过滤）
/// - `rowcount`: 最多返回行数（防全表）
/// - `delimiter`: 字段分隔符（解析 `ET_DATA.LINE` 用；建议罕见字符如 `\u0001` 避免值冲突）
///
/// 返回行列表，每行是 `{字段名: 值}`（按 `fields` 顺序对齐）。
///
/// > 安全：`where_clauses` 是 ABAP SQL 片段，调用方可传任意 WHERE——
/// > 读权限边界靠 SAP 侧（账号 `S_TABU_DIS` 表权限，见 `docs/SAP_PERMISSIONS.md`）。
pub fn read_table(
    conn: &RfcConnection,
    table: &str,
    fields: &[String],
    where_clauses: &[String],
    rowcount: u32,
    delimiter: char,
) -> Result<Vec<HashMap<String, String>>, RfcError> {
    let inputs = HashMap::from([
        (
            "QUERY_TABLE".to_string(),
            ScalarValue::Chars(table.to_uppercase()),
        ),
        (
            "DELIMITER".to_string(),
            ScalarValue::Chars(delimiter.to_string()),
        ),
        ("ROWCOUNT".to_string(), ScalarValue::Int(rowcount as i32)),
        // 用 ET_DATA（STRING，无 512 截断）而非传统 DATA（CHAR 512）
        (
            "USE_ET_DATA_4_RETURN".to_string(),
            ScalarValue::Chars("X".to_string()),
        ),
    ]);
    let mut table_inputs = HashMap::new();
    // FIELDS：字段过滤（每行 FIELDNAME）
    table_inputs.insert(
        "FIELDS".to_string(),
        fields
            .iter()
            .map(|f| {
                HashMap::from([(
                    "FIELDNAME".to_string(),
                    ScalarValue::Chars(f.to_uppercase()),
                )])
            })
            .collect::<Vec<_>>(),
    );
    // OPTIONS：WHERE 条件（每行 TEXT）
    if !where_clauses.is_empty() {
        table_inputs.insert(
            "OPTIONS".to_string(),
            where_clauses
                .iter()
                .map(|w| HashMap::from([("TEXT".to_string(), ScalarValue::Chars(w.clone()))]))
                .collect::<Vec<_>>(),
        );
    }
    let req = InvokeRequest {
        func_name: "RFC_READ_TABLE".to_string(),
        inputs,
        table_inputs,
        table_outputs: HashMap::from([(
            "ET_DATA".to_string(),
            vec![FieldSpec {
                name: "LINE".to_string(),
                max_len: Some(1000),
                auto: false,
            }],
        )]),
        ..Default::default()
    };
    let resp = execute_collect(conn, &req)?;
    // 解析 ET_DATA.LINE：按 delimiter 分隔 → 按 fields 顺序映射成 {字段名: 值}
    let rows = resp
        .tables
        .get("ET_DATA")
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|row| {
            let line = row
                .get("LINE")
                .map(|v| v.clone().into_chars())
                .unwrap_or_default();
            let values: Vec<&str> = line.split(delimiter).collect();
            let mut m = HashMap::new();
            for (i, f) in fields.iter().enumerate() {
                m.insert(
                    f.clone(),
                    values.get(i).map(|s| s.to_string()).unwrap_or_default(),
                );
            }
            m
        })
        .collect();
    Ok(rows)
}

// ========================================================================
// 依赖签名前言（prologue）：扫 CALL FUNCTION → 内联被调函数的紧凑接口
// ========================================================================

/// prologue 构建结果。
#[derive(serde::Serialize)]
pub struct PrologueSummary {
    /// 成功取到接口的
    pub resolved: usize,
    /// 读取失败的（文本中保留占位行，缺口可见而非静默丢弃）
    pub failed: usize,
    /// ABAP 注释风格的签名块，可直接粘贴到源码上方
    pub text: String,
}

/// where-used 单条使用关系（REPOSITORY_ENVIRONMENT_SET_RFC 的 ENVIRONMENT 行）。
#[derive(Debug, Clone, Serialize, Default)]
pub struct WhereUsedEntry {
    /// 使用者类型（PROG / FUGR / FUNC / ...）
    #[serde(rename = "type")]
    pub usage_type: String,
    /// 使用者对象名
    pub object: String,
    /// 使用者的宿主对象（如函数组主程序）
    pub enclosing_object: String,
    /// 开发包
    pub devclass: String,
    /// 调用类型（如 PERFORM / CALL FUNCTION / 3 = 动态调用等，语义依 SAP 版本）
    pub call_type: String,
}

/// 查询函数模块被谁使用（where-used list）。
///
/// 基于 `REPOSITORY_ENVIRONMENT_SET_RFC`（经典环境引擎的 RFC 化变体；
/// `RS_EU_CROSSREF` 在 ABAP Cloud Trial 等精简系统上不存在）。结果依赖
/// SAP 端使用索引（WBCROSSGT）——索引未建立的系统（如试用版）会返回空，
/// 由调用方负责向用户解释（响应附 note）。
///
/// OBJ_TYPE 的正确取值（'FUNC' vs 'FF'）在不同版本存在分歧：先按 'FUNC'
/// 查，空结果再按 'FF' 兜底一次（有索引的系统第一次即命中，代价为零）。
pub fn read_where_used(
    conn: &RfcConnection,
    func_name: &str,
    max_results: usize,
) -> Result<Vec<WhereUsedEntry>, RfcError> {
    fn query(conn: &RfcConnection, name: &str, obj_type: &str, max: usize) -> Result<Vec<WhereUsedEntry>, RfcError> {
        let req = InvokeRequest {
            func_name: "REPOSITORY_ENVIRONMENT_SET_RFC".to_string(),
            inputs: HashMap::from([
                ("OBJECT_NAME".to_string(), ScalarValue::Chars(name.to_uppercase())),
                ("OBJ_TYPE".to_string(), ScalarValue::Chars(obj_type.to_string())),
            ]),
            // 只关心「谁在用」：程序与函数组（类在环境引擎里也归 PROG）
            struct_inputs: HashMap::from([(
                "ENVIRONMENT_TYPES".to_string(),
                HashMap::from([
                    ("PROG".to_string(), ScalarValue::Chars("X".to_string())),
                    ("FUGR".to_string(), ScalarValue::Chars("X".to_string())),
                ]),
            )]),
            table_outputs: HashMap::from([(
                "ENVIRONMENT".to_string(),
                vec![
                    crate::api::FieldSpec { name: "TYPE".to_string(), max_len: Some(15), auto: false },
                    crate::api::FieldSpec { name: "OBJECT".to_string(), max_len: Some(180), auto: false },
                    crate::api::FieldSpec { name: "ENCL_OBJ".to_string(), max_len: Some(40), auto: false },
                    crate::api::FieldSpec { name: "DEVCLASS".to_string(), max_len: Some(30), auto: false },
                    crate::api::FieldSpec { name: "CALL_TYPE".to_string(), max_len: Some(15), auto: false },
                ],
            )]),
            ..Default::default()
        };
        let resp = crate::executor::execute_collect(conn, &req)?;
        let rows = resp.tables.get("ENVIRONMENT").cloned().unwrap_or_default();
        Ok(rows
            .into_iter()
            .take(max)
            .map(|row| {
                fn sv(v: Option<&crate::api::ScalarValue>) -> String {
                    match v {
                        Some(crate::api::ScalarValue::Chars(s)) => s.clone(),
                        _ => String::new(),
                    }
                }
                WhereUsedEntry {
                    usage_type: sv(row.get("TYPE")),
                    object: sv(row.get("OBJECT")),
                    enclosing_object: sv(row.get("ENCL_OBJ")),
                    devclass: sv(row.get("DEVCLASS")),
                    call_type: sv(row.get("CALL_TYPE")),
                }
            })
            .collect())
    }

    let out = query(conn, func_name, "FUNC", max_results)?;
    if !out.is_empty() {
        return Ok(out);
    }
    query(conn, func_name, "FF", max_results)
}

/// 扫描 ABAP 源码行里的 `CALL FUNCTION 'X'` 目标（FM 依赖）。
///
/// - 跳过整行注释（行首 `*` 或 `"`）并截断 `"` 起的行内注释；
/// - 名字取单引号内内容，转大写、去重、保持首次出现顺序；
/// - `skip`（通常是函数自身）与 cap 截断防递归自引用/病态源码；
/// - 动态调用（`CALL FUNCTION lv_name`）引号缺失，自然跳过；
/// - `CALL` 与 `FUNCTION` 跨行的写法不识别（罕见，见端点文档）。
pub fn scan_called_functions(lines: &[String], skip: &str, cap: usize) -> Vec<String> {
    let mut seen: Vec<String> = Vec::with_capacity(cap.min(16));
    let skip_upper = skip.to_uppercase();
    'outer: for raw in lines {
        let trimmed = raw.trim_start();
        // 整行注释
        if trimmed.starts_with('*') || trimmed.starts_with('"') {
            continue;
        }
        // 截断行内注释
        let code = match raw.find('"') {
            Some(i) => &raw[..i],
            None => raw.as_str(),
        };
        let up = code.to_uppercase();
        let mut search_from = 0usize;
        while let Some(rel) = up[search_from..].find("CALL FUNCTION") {
            let mut rest = &code[search_from + rel + "CALL FUNCTION".len()..];
            // 关键字与引号之间只有空白
            let rest_trim = rest.trim_start();
            if !rest_trim.starts_with('\'') {
                // 动态调用（变量名）或跨行：跳过本次出现
                search_from += rel + "CALL FUNCTION".len();
                continue;
            }
            rest = rest_trim;
            let name_part = &rest[1..];
            if let Some(end) = name_part.find('\'') {
                let name = name_part[..end].trim().to_uppercase();
                if !name.is_empty()
                    && name != skip_upper
                    && crate::api::validate_func_name(&name).is_ok()
                    && !seen.contains(&name)
                {
                    if seen.len() >= cap {
                        break 'outer;
                    }
                    seen.push(name);
                }
            }
            search_from += rel + "CALL FUNCTION".len();
        }
    }
    seen
}

/// 按字符数截断（不切多字节字符）。
fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// 单个依赖的紧凑签名块（纯格式化，可测）。
///
/// 形如：
/// ```text
/// FUNCTION BAPI_TRANSACTION_COMMIT.
///   IMPORT   WAIT        CHAR(1) 可选
///   EXPORT   RETURN      STRUCT(TYPE,ID,NUMBER,MESSAGE)
/// ```
fn format_prologue_dep(
    name: &str,
    params: &[(String, String, String, usize, bool, String)], // (方向, 名, 类型表达, 长度, 可选, 描述)
) -> Vec<String> {
    let mut out = vec![format!("FUNCTION {}.", name)];
    for (dir, pname, type_repr, _len, optional, desc) in params {
        let mut line = format!("  {:<8}{:<31}{}", dir, pname, type_repr);
        if *optional {
            line.push_str(" 可选");
        }
        if !desc.is_empty() {
            line.push_str(" -- ");
            line.push_str(truncate_chars(desc, 40));
        }
        out.push(line);
    }
    out
}

/// 取一个函数参数的紧凑类型表达：
/// 标量 `CHAR(12)` / `INT`；STRUCTURE/TABLE 带一层子字段名 `STRUCT(TYPE,ID,…)`（上限 15 个）。
fn param_type_repr(p: &crate::connection::ParamInfo) -> String {
    let ty = crate::api::rfctype_name(p.type_);
    if ty != "STRUCTURE" && ty != "TABLE" {
        return if p.char_length > 0 {
            format!("{}({})", ty, p.char_length)
        } else {
            ty.to_string()
        };
    }
    let Some(handle) = p.type_desc_handle else {
        return ty.to_string();
    };
    // SAFETY: handle 来自刚拉取的接口元数据，连接仍有效
    let subs = match unsafe { crate::connection::get_field_infos(handle) } {
        Ok(s) => s,
        Err(_) => return ty.to_string(),
    };
    if subs.is_empty() {
        return ty.to_string();
    }
    let mut names: Vec<&str> = subs.iter().map(|sf| sf.name.as_str()).collect();
    let more = names.len() > 15;
    names.truncate(15);
    let mut s = format!("{}({}", ty, names.join(","));
    if more {
        s.push_str(",…");
    }
    s.push(')');
    s
}

/// 为依赖列表构建 prologue 文本：逐个读接口，失败保留占位行（缺口可见）。
pub fn build_function_prologue(conn: &RfcConnection, deps: &[String]) -> PrologueSummary {
    let mut text = String::new();
    let mut resolved = 0usize;
    let mut failed = 0usize;
    for d in deps {
        match conn.get_param_infos(d) {
            Ok(infos) => {
                resolved += 1;
                let params: Vec<(String, String, String, usize, bool, String)> = infos
                    .iter()
                    .map(|p| {
                        (
                            crate::api::direction_name(p.direction).to_string(),
                            p.name.clone(),
                            param_type_repr(p),
                            p.char_length,
                            p.optional,
                            p.parameter_text.clone(),
                        )
                    })
                    .collect();
                for line in format_prologue_dep(d, &params) {
                    text.push_str("\" ");
                    text.push_str(&line);
                    text.push('\n');
                }
            }
            Err(e) => {
                failed += 1;
                text.push_str(&format!("\" FUNCTION {} -- 接口读取失败: {}\n", d, e.message));
            }
        }
    }
    PrologueSummary {
        resolved,
        failed,
        text,
    }
}

// ========================================================================
// 源码读取的 ADT 降级（RPY 系 RFC 在部分系统上普遍失败，如 FL 180
// 「Source wider than 72 char」——现代 ABAP 源码行宽超 72 即中招；
// ADT 通道返回原始行，无此限制）
// ========================================================================

/// 判定 RPY 源码读取错误是否值得走 ADT 降级。
/// NOT_FOUND 类不降级——那是「对象不存在」的语义，降级只会得到另一个 404，
/// 徒增一次往返还可能盖掉更具体的错误信息。
pub fn should_fallback_to_adt(err: &RfcError) -> bool {
    !(err.key.contains("NOT_FOUND") || err.status == 404)
}

/// 反解函数所属的函数组（ADT 的 FM 源码 URL 需要两级名字）。
/// FM 名在 SAP 全局唯一，但其资源嵌在组下；这里用 `RFC_FUNCTION_SEARCH`
/// 精确名搜索拿组名（vsp 的镜像做法：它用 ADT 搜索从结果 URI 里反解组名）。
pub fn resolve_function_group(conn: &RfcConnection, func_name: &str) -> Result<String, RfcError> {
    let target = func_name.trim().to_uppercase();
    // 多取几条防模糊命中（如 Z_FOO 与 Z_FOO_X），本地精确匹配
    let hits = search_functions(conn, &target, "", 10)?;
    for h in &hits {
        if h.name == target && !h.group.trim().is_empty() {
            return Ok(h.group.trim().to_string());
        }
    }
    Err(RfcError {
        code: -1,
        status: 404,
        message: format!("无法反解函数 {} 的所属函数组（ADT 降级读需要组名）", func_name),
        key: "FUNCTION_GROUP_NOT_FOUND".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_decision_preserves_not_found_semantics() {
        // NOT_FOUND 类（key 或 404 状态）不降级
        assert!(!should_fallback_to_adt(&RfcError {
            code: 5,
            status: 404,
            key: "FU_NOT_FOUND".into(),
            message: String::new(),
        }));
        assert!(!should_fallback_to_adt(&RfcError {
            code: 17,
            status: 404,
            key: "RFC_NOT_FOUND".into(),
            message: String::new(),
        }));
        // 404 状态即使 key 不含 NOT_FOUND 也不降级
        assert!(!should_fallback_to_adt(&RfcError {
            code: 4,
            status: 404,
            key: "ERROR_MESSAGE".into(),
            message: String::new(),
        }));
        // FL 180 等业务错误（key=ERROR_MESSAGE, 400）→ 降级
        assert!(should_fallback_to_adt(&RfcError {
            code: 4,
            status: 400,
            key: "ERROR_MESSAGE".into(),
            message: "ID:FL Type:E Number:180 Source wider than 72 char".into(),
        }));
        // 5xx 也降级（SAP 端 RPY 异常等）
        assert!(should_fallback_to_adt(&RfcError {
            code: 3,
            status: 500,
            key: "RFC_ABAP_RUNTIME_FAILURE".into(),
            message: String::new(),
        }));
    }

    #[test]
    fn scan_finds_quoted_call_function_targets() {
        let lines: Vec<String> = vec![
            "  CALL FUNCTION 'BAPI_TRANSACTION_COMMIT'".into(),
            "    call function 'z_foo'".into(),
            "  CALL FUNCTION '/SDF/EWA_GET_ABAP_DUMPS' DESTINATION 'NONE'.".into(),
            "  PERFORM do_something.".into(),
        ];
        assert_eq!(
            scan_called_functions(&lines, "", 30),
            vec!["BAPI_TRANSACTION_COMMIT", "Z_FOO", "/SDF/EWA_GET_ABAP_DUMPS"]
        );
    }

    #[test]
    fn scan_skips_comments_dynamics_duplicates_and_self() {
        let lines: Vec<String> = vec![
            "* CALL FUNCTION 'COMMENTED_OUT'".into(),
            "  X = 1. \" CALL FUNCTION 'INLINE_COMMENT'".into(),
            "  CALL FUNCTION lv_dynamic.".into(),
            "  CALL FUNCTION 'DUP'.".into(),
            "  CALL FUNCTION 'dup'.".into(),
            "  CALL FUNCTION 'SELF'.".into(),
            "  CALL FUNCTION 'KEPT'.".into(),
        ];
        assert_eq!(scan_called_functions(&lines, "SELF", 30), vec!["DUP", "KEPT"]);
    }

    #[test]
    fn scan_respects_cap_in_first_seen_order() {
        let lines: Vec<String> = (1..=5)
            .map(|i| format!("  CALL FUNCTION 'FM_{}'.", i))
            .collect();
        assert_eq!(
            scan_called_functions(&lines, "", 3),
            vec!["FM_1", "FM_2", "FM_3"]
        );
    }

    #[test]
    fn prologue_formats_scalars_structs_and_optionals() {
        let params = vec![
            (
                "IMPORT".into(),
                "WAIT".into(),
                "CHAR(1)".into(),
                1,
                true,
                "等待更新结束".into(),
            ),
            (
                "EXPORT".into(),
                "RETURN".into(),
                "STRUCT(TYPE,ID,NUMBER,MESSAGE)".into(),
                0,
                false,
                String::new(),
            ),
        ];
        let out = format_prologue_dep("BAPI_TRANSACTION_COMMIT", &params);
        assert_eq!(out[0], "FUNCTION BAPI_TRANSACTION_COMMIT.");
        assert!(out[1].starts_with("  IMPORT  WAIT"), "got: {}", out[1]);
        assert!(out[1].ends_with("CHAR(1) 可选 -- 等待更新结束"), "got: {}", out[1]);
        assert!(out[2].starts_with("  EXPORT  RETURN"), "got: {}", out[2]);
        assert!(out[2].ends_with("STRUCT(TYPE,ID,NUMBER,MESSAGE)"), "got: {}", out[2]);
        // 列对齐：方向列 8 字符、名字列 31 字符，两行的类型表达从同一列开始
        let type_col = |l: &str| l.find("CHAR(1)").or_else(|| l.find("STRUCT("));
        assert_eq!(type_col(&out[1]), type_col(&out[2]));
    }
}
