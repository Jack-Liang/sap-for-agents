//! 交付端口：`POST /api/invokes/{alias}` 平坦化调用（v0.13）。
//!
//! 注册表条目的**消费方面**：外部系统（前端/其他语言/其他工具）不学 SAP 方言
//! （大写参数名、table_outputs 声明、max_len……），直接对着别名发扁平 JSON：
//!
//! ```text
//! POST /api/invokes/z_calc        {"iv_a": 20, "iv_b": 22}
//!                              →  {"EV_SUM": 42}
//! ```
//!
//! 契约**按需派生**而非注册时快照：每次调用从 SAP 元数据现推输入/输出分类，
//! 接口签名漂移自动跟随，注册表里只存覆盖项（如 `max_rows`）。三类输出全部
//! 按真实类型序列化（复用 auto 机制：INT→整数、FLOAT→浮点、BYTE→Base64），
//! 输出表封顶 `?limit=` > 条目 `max_rows` > 默认 100 行（截断时响应带
//! `_truncated` 列表）。
//!
//! 与 `/api/rfc` 的关系：同一执行引擎（超时/重试/指标/审计完全一致），只是
//! 把"怎么读输出"的知识从调用方挪到了提供方。

use crate::api::{FieldSpec, FunctionParam, InvokeRequest, InvokeResponse, ScalarValue};
use crate::error::RfcError;
use crate::server::SharedPool;
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// 输出表默认行数封顶（防大表全量读；可用 `?limit=` 或条目 `max_rows` 覆盖）。
pub const DEFAULT_TABLE_CAP: u32 = 100;

/// 封顶上限（与全局 MAX_OUTPUT_ROWS 对齐）。
const MAX_TABLE_CAP: u32 = 10_000;

// ========================================================================
// 契约派生（纯函数，单测锁定）
// ========================================================================

/// 从函数接口元数据派生的平坦调用契约。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlatContract {
    /// 允许出现在请求体里的参数（IMPORT/CHANGING），大写规范名
    pub inputs: Vec<ContractParam>,
    /// 会被读回的输出参数（EXPORT/CHANGING/TABLES）
    pub outputs: Vec<ContractParam>,
}

/// 契约里的单个参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractParam {
    /// SAP 规范大写名
    pub name: String,
    /// 标量 / 结构体 / 表
    pub kind: ParamKind,
    /// 是否可省略（OpenAPI required 用）
    pub optional: bool,
    /// 参数描述
    pub description: String,
    /// STRUCTURE/TABLE 的子字段名（大写）
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    Scalar,
    Struct,
    Table,
}

/// 按 RFCTYPE 名分类参数形态。
fn kind_of(type_name: &str) -> ParamKind {
    match type_name {
        "STRUCTURE" => ParamKind::Struct,
        "TABLE" => ParamKind::Table,
        _ => ParamKind::Scalar,
    }
}

/// 方向是否接受输入 / 是否产生输出。TABLES 双向：既能作为输入传行
/// （BAPI 选择表的惯例），也是读回的输出。
fn is_input_dir(direction: &str) -> bool {
    matches!(direction, "IMPORT" | "CHANGING" | "TABLES")
}

fn is_output_dir(direction: &str) -> bool {
    matches!(direction, "EXPORT" | "CHANGING" | "TABLES")
}

/// 从 `/api/functions/{name}` 同源的接口元数据派生契约。
pub fn derive_contract(params: &[FunctionParam]) -> FlatContract {
    let conv = |p: &FunctionParam| ContractParam {
        name: p.name.to_uppercase(),
        kind: kind_of(p.type_name),
        optional: p.optional,
        description: p.description.clone(),
        fields: p
            .fields
            .as_ref()
            .map(|fs| fs.iter().map(|f| f.name.to_uppercase()).collect())
            .unwrap_or_default(),
    };
    FlatContract {
        inputs: params
            .iter()
            .filter(|p| is_input_dir(p.direction))
            .map(conv)
            .collect(),
        outputs: params
            .iter()
            .filter(|p| is_output_dir(p.direction))
            .map(conv)
            .collect(),
    }
}

// ========================================================================
// 请求体分类（扁平 JSON → InvokeRequest 的输入侧）
// ========================================================================

/// JSON 值 → ScalarValue（复用 untagged 反序列化：string→Chars、整数→Int/Int8、
/// 浮点→Float、{"type":"BCD",...}→Typed）。bool/null 不被接受。
fn to_scalar(key: &str, v: &Value) -> Result<ScalarValue, RfcError> {
    serde_json::from_value(v.clone()).map_err(|e| RfcError {
        code: -1,
        status: 400,
        message: format!("参数 {key} 的值类型不被支持（标量传 string/number）: {e}"),
        key: "INVOKE_VALUE_INVALID".into(),
    })
}

/// 结构体/表行字段的键也做大小写归一（值必须是标量）。
fn uppercase_row_keys(key: &str, obj: &Map<String, Value>) -> Result<HashMap<String, ScalarValue>, RfcError> {
    let mut out = HashMap::new();
    for (k, v) in obj {
        out.insert(k.to_uppercase(), to_scalar(&format!("{key}.{k}"), v)?);
    }
    Ok(out)
}

/// 把扁平请求体按契约分类成 InvokeRequest 的输入侧。
/// 键大小写不敏感；未知键 → 400（带合法键清单，防拼写错误静默吞掉）。
pub fn classify_body(
    contract: &FlatContract,
    func_name: &str,
    body: &Value,
    timeout_secs: Option<u64>,
) -> Result<InvokeRequest, RfcError> {
    let obj = body.as_object().ok_or_else(|| RfcError {
        code: -1,
        status: 400,
        message: "请求体必须是 JSON 对象（{参数名: 值}）".into(),
        key: "INVOKE_BODY_INVALID".into(),
    })?;
    // 大小写不敏感匹配表：upper(key) → (规范名, 契约参数)
    let mut lookup: HashMap<String, &ContractParam> = HashMap::new();
    for p in &contract.inputs {
        lookup.insert(p.name.clone(), p);
    }
    let mut req = InvokeRequest {
        func_name: func_name.to_string(),
        timeout_secs,
        ..Default::default()
    };
    let mut unknown = Vec::new();
    for (k, v) in obj {
        let Some(p) = lookup.get(&k.to_uppercase()) else {
            unknown.push(k.clone());
            continue;
        };
        match p.kind {
            ParamKind::Scalar => {
                req.inputs.insert(p.name.clone(), to_scalar(k, v)?);
            }
            ParamKind::Struct => {
                let o = v.as_object().ok_or_else(|| RfcError {
                    code: -1,
                    status: 400,
                    message: format!("参数 {} 是结构体，值须为 JSON 对象 {{字段: 值}}", p.name),
                    key: "INVOKE_VALUE_INVALID".into(),
                })?;
                let m = uppercase_row_keys(k, o)?;
                req.struct_inputs.insert(p.name.clone(), m);
            }
            ParamKind::Table => {
                let arr = v.as_array().ok_or_else(|| RfcError {
                    code: -1,
                    status: 400,
                    message: format!("参数 {} 是表，值须为 JSON 数组 [{{字段: 值}}, ...]", p.name),
                    key: "INVOKE_VALUE_INVALID".into(),
                })?;
                let mut rows = Vec::with_capacity(arr.len());
                for (i, rv) in arr.iter().enumerate() {
                    let ro = rv.as_object().ok_or_else(|| RfcError {
                        code: -1,
                        status: 400,
                        message: format!("参数 {} 第 {} 行须为 JSON 对象", p.name, i + 1),
                        key: "INVOKE_VALUE_INVALID".into(),
                    })?;
                    rows.push(uppercase_row_keys(&format!("{}[{}]", k, i), ro)?);
                }
                req.table_inputs.insert(p.name.clone(), rows);
            }
        }
    }
    if !unknown.is_empty() {
        let allowed: Vec<&str> = contract.inputs.iter().map(|p| p.name.as_str()).collect();
        return Err(RfcError {
            code: -1,
            status: 400,
            message: format!(
                "未知参数 {:?}；该接口接受的参数: {:?}（大小写不敏感）",
                unknown, allowed
            ),
            key: "INVOKE_PARAM_UNKNOWN".into(),
        });
    }
    Ok(req)
}

/// 按契约填 InvokeRequest 的输出侧：标量→auto_outputs（真实类型），
/// 结构体/表→字段全开 auto。无子字段元数据的输出跳过（无法遍历）。
pub fn apply_output_specs(contract: &FlatContract, req: &mut InvokeRequest) {
    for p in &contract.outputs {
        let fields: Vec<FieldSpec> = p
            .fields
            .iter()
            .map(|f| FieldSpec {
                name: f.clone(),
                max_len: None,
                auto: true,
            })
            .collect();
        match p.kind {
            ParamKind::Scalar => {
                req.auto_outputs.push(p.name.clone());
            }
            ParamKind::Struct if !fields.is_empty() => {
                req.struct_outputs.insert(p.name.clone(), fields);
            }
            ParamKind::Table if !fields.is_empty() => {
                req.table_outputs.insert(p.name.clone(), fields);
            }
            _ => {}
        }
    }
}

// ========================================================================
// 响应扁平化
// ========================================================================

/// ScalarValue → JSON（untagged 序列化：类型语义保留在 JSON 值本身里）。
fn scalar_to_value(s: &ScalarValue) -> Value {
    serde_json::to_value(s).unwrap_or(Value::Null)
}

/// InvokeResponse → 扁平 JSON：标量/结构体/表都以参数名为键平铺；
/// 表超过封顶被截断时，响应附加 `"_truncated": [表名...]`。
pub fn flatten_response(resp: &InvokeResponse, table_cap: usize) -> Value {
    let mut out = Map::new();
    for (k, v) in &resp.scalars {
        out.insert(k.clone(), scalar_to_value(v));
    }
    for (k, fields) in &resp.structs {
        let mut o = Map::new();
        for (f, v) in fields {
            o.insert(f.clone(), scalar_to_value(v));
        }
        out.insert(k.clone(), Value::Object(o));
    }
    let mut truncated = Vec::new();
    for (k, rows) in &resp.tables {
        let cap = table_cap.min(rows.len());
        if cap < rows.len() {
            truncated.push(k.clone());
        }
        let arr: Vec<Value> = rows[..cap]
            .iter()
            .map(|row| {
                let mut o = Map::new();
                for (f, v) in row {
                    o.insert(f.clone(), scalar_to_value(v));
                }
                Value::Object(o)
            })
            .collect();
        out.insert(k.clone(), Value::Array(arr));
    }
    let mut v = Value::Object(out);
    if !truncated.is_empty() {
        v["_truncated"] = json!(truncated);
    }
    v
}

// ========================================================================
// HTTP handler
// ========================================================================

/// `POST /api/invokes/{alias}?limit=&timeout_secs=` 的查询参数。
#[derive(serde::Deserialize)]
struct InvokeQuery {
    /// 输出表行数封顶（覆盖条目 max_rows 与默认值；上限 10000）
    #[serde(default)]
    limit: Option<u32>,
    /// 本次调用超时秒数（同 /api/rfc 的 timeout_secs）
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// 有效封顶：`?limit=` > 条目 `max_rows` > 默认 100，clamp 到 [1, 10000]。
fn effective_cap(limit: Option<u32>, entry_max_rows: Option<u32>) -> usize {
    limit
        .or(entry_max_rows)
        .unwrap_or(DEFAULT_TABLE_CAP)
        .clamp(1, MAX_TABLE_CAP) as usize
}

/// 平坦化调用 handler：查条目 → 拉接口元数据 → 派生契约 → 分类请求体 →
/// 复用 /api/rfc 执行引擎 → 扁平化响应。
async fn flat_invoke_handler(
    axum::extract::State(pool): axum::extract::State<SharedPool>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    axum::extract::Path(alias): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<InvokeQuery>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, RfcError> {
    let alias = alias.strip_suffix('/').unwrap_or(&alias).to_string();
    // 404 语义：条目不存在 / 已墓碑（对象已删）。墓碑给出明确提示而非裸 404。
    let entry = crate::registry::get_entry(&alias)?;
    if entry.status == crate::registry::EntryStatus::Deleted {
        return Err(RfcError {
            code: -1,
            status: 404,
            message: format!(
                "别名 {alias} 的条目已墓碑（SAP 对象 {} 已删除）",
                entry.func_name
            ),
            key: "REGISTRY_NOT_FOUND".into(),
        });
    }
    let Json(body) = body.map_err(|r| RfcError {
        code: -1,
        status: r.status().as_u16(),
        message: r.body_text(),
        key: "JSON_INVALID".into(),
    })?;

    // 接口元数据（连接池 + 元数据缓存）→ 契约
    let func = entry.func_name.clone();
    let params = crate::server::run_blocking(Arc::clone(&pool), move |conn| {
        crate::server::collect_function_params(conn, &func)
    })
    .await?;
    let contract = derive_contract(&params);
    if contract.inputs.is_empty() && contract.outputs.is_empty() {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: format!("函数 {} 的接口为空（非正常 RFC 接口）", entry.func_name),
            key: "INVOKE_CONTRACT_EMPTY".into(),
        });
    }

    let mut req = classify_body(&contract, &entry.func_name, &body, q.timeout_secs)?;
    apply_output_specs(&contract, &mut req);

    // 复用 /api/rfc 的执行+指标+审计（函数名标签用真实 SAP 名）
    let resp = crate::server::run_invoke_and_log(pool, addr.ip().to_string(), req)
        .await?
        .0;
    let cap = effective_cap(q.limit, entry.max_rows);
    Ok(Json(flatten_response(&resp, cap)))
}

/// 交付端口子路由（挂 /api 鉴权层内）。handler 需要连接池 → 状态类型固定。
pub fn router() -> Router<SharedPool> {
    Router::new().route(
        "/api/invokes/*alias",
        axum::routing::post(flat_invoke_handler),
    )
}

// ========================================================================
// 测试：契约派生 / 分类 / 扁平化全部纯函数级（无 SAP 依赖）
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn param(name: &str, type_name: &'static str, direction: &'static str, optional: bool) -> FunctionParam {
        let kind = type_name;
        let fields = if kind == "STRUCTURE" || kind == "TABLE" {
            Some(vec![
                crate::api::FieldDef {
                    name: "SUB1".into(),
                    type_name: "CHAR",
                    length: 10,
                    decimals: 0,
                    description: String::new(),
                    fields: None,
                },
                crate::api::FieldDef {
                    name: "SUB2".into(),
                    type_name: "INT",
                    length: 4,
                    decimals: 0,
                    description: String::new(),
                    fields: None,
                },
            ])
        } else {
            None
        };
        FunctionParam {
            name: name.into(),
            type_name,
            direction,
            length: 10,
            decimals: 0,
            optional,
            default: String::new(),
            description: format!("desc of {name}"),
            fields,
        }
    }

    fn sample_contract() -> FlatContract {
        derive_contract(&[
            param("IV_A", "INT", "IMPORT", false),
            param("IV_ADDR", "STRUCTURE", "IMPORT", true),
            param("SEL_RANGE", "TABLE", "TABLES", true),
            param("EV_SUM", "INT", "EXPORT", true),
            param("ES_HDR", "STRUCTURE", "EXPORT", true),
            param("T_ROWS", "TABLE", "TABLES", true),
            param("CH_COUNT", "INT", "CHANGING", true),
        ])
    }

    fn names(v: &[ContractParam]) -> Vec<&str> {
        v.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn contract_derives_input_output_sets() {
        let c = sample_contract();
        // TABLES（SEL_RANGE/T_ROWS）双向：出现在输入与输出两侧
        assert_eq!(
            names(&c.inputs),
            ["IV_A", "IV_ADDR", "SEL_RANGE", "T_ROWS", "CH_COUNT"]
        );
        assert_eq!(
            names(&c.outputs),
            ["SEL_RANGE", "EV_SUM", "ES_HDR", "T_ROWS", "CH_COUNT"]
        );
        // CHANGING 双向：既能输入也能读回
        let ch_in = c.inputs.iter().find(|p| p.name == "CH_COUNT").unwrap();
        let ch_out = c.outputs.iter().find(|p| p.name == "CH_COUNT").unwrap();
        assert_eq!(ch_in.kind, ParamKind::Scalar);
        assert_eq!(ch_out.kind, ParamKind::Scalar);
        // 子字段展开且大写
        let t = c.outputs.iter().find(|p| p.name == "T_ROWS").unwrap();
        assert_eq!(t.fields, ["SUB1", "SUB2"]);
    }

    /// ScalarValue 无 PartialEq——经 untagged JSON 序列化后比对。
    fn sv(v: &ScalarValue) -> Value {
        serde_json::to_value(v).unwrap()
    }

    #[test]
    fn classify_splits_scalar_struct_table_and_is_case_insensitive() {
        let c = sample_contract();
        let body = json!({
            "iv_a": 20,
            "iv_addr": {"sub1": "x", "SUB2": 3},
            "sel_range": [{"sub1": "A"}],
            "CH_COUNT": 5
        });
        let req = classify_body(&c, "Z_X", &body, Some(30)).unwrap();
        assert_eq!(req.func_name, "Z_X");
        assert_eq!(req.timeout_secs, Some(30));
        assert_eq!(sv(&req.inputs["IV_A"]), json!(20));
        assert_eq!(sv(&req.inputs["CH_COUNT"]), json!(5));
        // 结构体字段键归一大写
        assert_eq!(sv(&req.struct_inputs["IV_ADDR"]["SUB1"]), json!("x"));
        assert_eq!(sv(&req.struct_inputs["IV_ADDR"]["SUB2"]), json!(3));
        assert_eq!(req.table_inputs["SEL_RANGE"].len(), 1);
        assert_eq!(sv(&req.table_inputs["SEL_RANGE"][0]["SUB1"]), json!("A"));
    }

    #[test]
    fn classify_rejects_unknown_keys_with_allowed_list() {
        let c = sample_contract();
        let err = classify_body(&c, "Z_X", &json!({"typo_key": 1}), None).unwrap_err();
        assert_eq!(err.status, 400);
        assert_eq!(err.key, "INVOKE_PARAM_UNKNOWN");
        assert!(err.message.contains("typo_key"));
        assert!(err.message.contains("IV_A"), "应列出合法参数: {}", err.message);
    }

    #[test]
    fn classify_rejects_wrong_shapes() {
        let c = sample_contract();
        // 非对象体
        assert_eq!(classify_body(&c, "Z_X", &json!([1]), None).unwrap_err().key, "INVOKE_BODY_INVALID");
        // 表传了对象
        let e = classify_body(&c, "Z_X", &json!({"SEL_RANGE": {}}), None).unwrap_err();
        assert_eq!(e.key, "INVOKE_VALUE_INVALID");
        // 标量传了 bool
        let e = classify_body(&c, "Z_X", &json!({"IV_A": true}), None).unwrap_err();
        assert_eq!(e.key, "INVOKE_VALUE_INVALID");
    }

    #[test]
    fn output_specs_fill_auto_everywhere() {
        let c = sample_contract();
        let mut req = InvokeRequest {
            func_name: "Z_X".into(),
            ..Default::default()
        };
        apply_output_specs(&c, &mut req);
        assert!(req.auto_outputs.contains(&"EV_SUM".to_string()));
        assert!(req.auto_outputs.contains(&"CH_COUNT".to_string()));
        let t = &req.table_outputs["T_ROWS"];
        assert_eq!(t.len(), 2);
        assert!(t.iter().all(|f| f.auto && f.max_len.is_none()));
        assert!(req.struct_outputs.contains_key("ES_HDR"));
    }

    fn sample_response(rows: usize) -> InvokeResponse {
        let mut scalars = HashMap::new();
        scalars.insert("EV_SUM".into(), ScalarValue::Int(42));
        let mut structs = HashMap::new();
        let mut hdr = HashMap::new();
        hdr.insert("SUB1".into(), ScalarValue::Chars("h".into()));
        hdr.insert("SUB2".into(), ScalarValue::Int(7));
        structs.insert("ES_HDR".into(), hdr);
        let mut tables = HashMap::new();
        let table: Vec<HashMap<String, ScalarValue>> = (0..rows)
            .map(|i| {
                let mut r = HashMap::new();
                r.insert("SUB1".into(), ScalarValue::Chars(format!("r{i}")));
                r.insert("SUB2".into(), ScalarValue::Int(i as i32));
                r
            })
            .collect();
        tables.insert("T_ROWS".into(), table);
        InvokeResponse {
            func: "Z_X".into(),
            scalars,
            tables,
            structs,
            return_table: None,
        }
    }

    #[test]
    fn flatten_is_flat_and_typed() {
        let v = flatten_response(&sample_response(3), 100);
        assert_eq!(v["EV_SUM"], json!(42));
        assert_eq!(v["ES_HDR"]["SUB1"], json!("h"));
        assert_eq!(v["ES_HDR"]["SUB2"], json!(7));
        assert_eq!(v["T_ROWS"].as_array().unwrap().len(), 3);
        assert_eq!(v["T_ROWS"][1]["SUB2"], json!(1));
        assert!(v.get("_truncated").is_none());
    }

    #[test]
    fn flatten_marks_truncation() {
        let v = flatten_response(&sample_response(250), 100);
        assert_eq!(v["T_ROWS"].as_array().unwrap().len(), 100);
        assert_eq!(v["_truncated"], json!(["T_ROWS"]));
    }

    #[test]
    fn cap_precedence_and_clamp() {
        assert_eq!(effective_cap(None, None), 100);
        assert_eq!(effective_cap(None, Some(500)), 500);
        assert_eq!(effective_cap(Some(7), Some(500)), 7);
        assert_eq!(effective_cap(Some(0), None), 1, "下限 1");
        assert_eq!(effective_cap(Some(999_999), None), 10_000, "上限对齐 MAX_OUTPUT_ROWS");
    }
}
