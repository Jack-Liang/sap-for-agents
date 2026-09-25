//! MCP（Model Context Protocol）服务器：`POST /mcp`。
//!
//! 让 Claude 等原生说 MCP 的 Agent 直接把本网关挂载为工具服务器，
//! 无需任何 HTTP 胶水。协议层为**手写的 JSON-RPC 2.0 子集**（零新依赖，
//! 与手写 OpenAPI 规范同一哲学）：
//!
//! - 传输：Streamable HTTP 的无状态模式——单条 JSON 消息一次 POST，
//!   响应即回（不做 SSE 流、不持会话；GET /mcp 按规范返回 405）
//! - 方法：`initialize` / `notifications/*` / `ping` / `tools/list` / `tools/call`
//! - 通知（无 id 的消息）按规范回 202 空体
//! - 版本协商：回显客户端请求的 protocolVersion（未知则回本服支持版本）
//!
//! 工具面 = 网关既有能力的薄封装（读元数据 + 读源码 + 读转储 + 通用调用 +
//! 语法检查），全部复用 server/discovery/executor 的内部入口，行为与 REST
//! 端点完全一致（含超时、连接池重试、审计）。写编排（objects 写端点）刻意
//! 不进 v1 工具面——Agent 需要改代码时让它走 REST（见 AGENTS.md）。
//!
//! 挂在 api 路由上：与 /api/* 同一套鉴权（SAP_API_KEY）与限流。

use crate::error::RfcError;
use crate::server::SharedPool;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

/// 本服支持的 MCP 协议版本（Streamable HTTP 无状态模式）
const PROTOCOL_VERSION: &str = "2025-03-26";

// ========================================================================
// 工具清单（inputSchema 为 JSON Schema 子集）
// ========================================================================

fn str_param(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}

/// tools/list 返回的清单。纯函数，单测锁定。
pub fn tools_manifest() -> Vec<Value> {
    vec![
        json!({
            "name": "search_functions",
            "description": "Fuzzy-search SAP function modules (RFC/BAPI) by name pattern, e.g. BAPI_USER_*",
            "inputSchema": { "type": "object", "properties": {
                "pattern": str_param("Wildcard pattern, * matches anything (required)"),
                "group": { "type": "string", "description": "Optional function-group filter" },
                "max_results": { "type": "integer", "description": "Cap (default 20, max 100)" }
            }, "required": ["pattern"] }
        }),
        json!({
            "name": "get_function_interface",
            "description": "Inspect a function module's full interface: parameters with names/types/directions/nested fields. ALWAYS do this before invoke_rfc.",
            "inputSchema": { "type": "object", "properties": {
                "name": str_param("Function module name, uppercase (required)")
            }, "required": ["name"] }
        }),
        json!({
            "name": "get_function_doc",
            "description": "Read a function module's SE37 documentation (short/long text, per-parameter docs)",
            "inputSchema": { "type": "object", "properties": {
                "name": str_param("Function module name"),
                "lang": { "type": "string", "description": "Language code (default from server)" }
            }, "required": ["name"] }
        }),
        json!({
            "name": "invoke_rfc",
            "description": "Invoke any SAP function module. Body semantics identical to POST /api/rfc: inputs (IMPORT scalars), table_inputs, struct_inputs, string_outputs/int_outputs/auto_outputs (EXPORT reads), table_outputs, read_return, timeout_secs. Parameter names are UPPERCASE and case-sensitive.",
            "inputSchema": { "type": "object", "properties": {
                "func_name": str_param("Function module name (required, uppercase)"),
                "inputs": { "type": "object", "description": "IMPORT scalar params → value" },
                "table_inputs": { "type": "object", "description": "TABLES input params → array of row objects" },
                "struct_inputs": { "type": "object", "description": "Top-level IMPORT structures → {field: value}" },
                "string_outputs": { "type": "object", "description": "EXPORT string params → max length (null = auto)" },
                "auto_outputs": { "type": "array", "description": "EXPORT scalar params to read by true type", "items": { "type": "string" } },
                "table_outputs": { "type": "object", "description": "EXPORT tables to traverse → field list" },
                "read_return": { "type": "boolean", "description": "Auto-read the BAPI RETURN table" },
                "timeout_secs": { "type": "integer", "description": "Per-call timeout override (≥1)" }
            }, "required": ["func_name"] }
        }),
        json!({
            "name": "read_function_source",
            "description": "Read a function module's ABAP source (auto-falls back to ADT on RPY failures)",
            "inputSchema": { "type": "object", "properties": {
                "name": str_param("Function module name"),
                "prologue": { "type": "boolean", "description": "Also inline signatures of CALL FUNCTION dependencies (default false)" }
            }, "required": ["name"] }
        }),
        json!({
            "name": "read_program_source",
            "description": "Read an ABAP program/report source",
            "inputSchema": { "type": "object", "properties": {
                "name": str_param("Program name")
            }, "required": ["name"] }
        }),
        json!({
            "name": "get_ddic_type",
            "description": "List all fields of a DDIC structure/table type (e.g. BAPIRET2)",
            "inputSchema": { "type": "object", "properties": {
                "name": str_param("DDIC type name")
            }, "required": ["name"] }
        }),
        json!({
            "name": "get_ddic_field",
            "description": "Inspect one field's semantics: data element, domain, fixed values",
            "inputSchema": { "type": "object", "properties": {
                "table": str_param("Table/structure containing the field"),
                "field": str_param("Field name"),
                "lang": { "type": "string", "description": "Language code" }
            }, "required": ["table", "field"] }
        }),
        json!({
            "name": "read_table",
            "description": "Read rows of a transparent table (wraps RFC_READ_TABLE, no truncation)",
            "inputSchema": { "type": "object", "properties": {
                "table": str_param("Table name (required)"),
                "fields": { "type": "array", "items": { "type": "string" }, "description": "Field names to read (required)" },
                "where": { "type": "array", "items": { "type": "string" }, "description": "WHERE fragments, one per line" },
                "rowcount": { "type": "integer", "description": "Max rows (default 1000, max 10000)" }
            }, "required": ["table", "fields"] }
        }),
        json!({
            "name": "list_dumps",
            "description": "List ABAP short dumps (ST22), newest first",
            "inputSchema": { "type": "object", "properties": {
                "limit": { "type": "integer", "description": "Max entries (default 50)" }
            } }
        }),
        json!({
            "name": "get_dump_detail",
            "description": "One dump's parsed detail: header, termination point, call stack (key from list_dumps)",
            "inputSchema": { "type": "object", "properties": {
                "key": str_param("Dump key from list_dumps")
            }, "required": ["key"] }
        }),
        json!({
            "name": "where_used",
            "description": "Where-used list of a function module: which programs/function groups call it. Needs the SAP usage index (on-prem OK; ABAP trial/cloud systems may return empty with a note).",
            "inputSchema": { "type": "object", "properties": {
                "name": str_param("Function module name"),
                "max": { "type": "integer", "description": "Max entries (default 200)" }
            }, "required": ["name"] }
        }),
        json!({
            "name": "write_source",
            "description": "Write FULL source of an ABAP object (prog/incl/class/intf/func/fugr/cds/tabl; tabl = DDIC table DDL 'define table ...'; stru = DDIC structure DDL 'define structure ...') and activate. Destructive: overwrites everything. Pass create:true + description to create the object when missing. For func: FM parameter signatures are part of the source (write them inline in the FUNCTION statement: 'FUNCTION zfm IMPORTING VALUE(iv) TYPE string EXPORTING VALUE(ev) TYPE string. ... ENDFUNCTION.'); classic *\" comment blocks are converted automatically. rfc_enabled:true additionally marks the function module remote-enabled. For surgical edits prefer edit_code.",
            "inputSchema": { "type": "object", "properties": {
                "type": { "type": "string", "enum": ["prog", "incl", "class", "intf", "func", "fugr", "cds", "tabl", "stru"], "description": "Object type" },
                "name": str_param("Object name (uppercase)"),
                "source": str_param("Full ABAP source"),
                "group": { "type": "string", "description": "Function group (func only; auto-resolved when omitted)" },
                "transport": { "type": "string", "description": "Transport request (optional)" },
                "create": { "type": "boolean", "description": "Create the object when missing (default false)" },
                "description": { "type": "string", "description": "Title for auto-creation (required with create:true)" },
                "devclass": { "type": "string", "description": "Package for auto-creation (default $TMP)" },
                "rfc_enabled": { "type": "boolean", "description": "func only: mark the function module remote-enabled (processingType=rfc)" },
                "doc": { "type": "string", "description": "func only: API documentation (Markdown) stored on the registry entry and shown in the OpenAPI catalog" }
            }, "required": ["type", "name", "source"] }
        }),
        json!({
            "name": "edit_code",
            "description": "Surgical edit of an existing ABAP object: unique find-and-replace + activate. old_string must match exactly one place; CRLF differences are normalized and a case-insensitive unique fallback applies (SAP stores original case). Empty old_string only works on an empty/new object (with create:true it initializes a new object with new_string). rfc_enabled:true (func only) marks the module remote-enabled after the edit.",
            "inputSchema": { "type": "object", "properties": {
                "type": { "type": "string", "enum": ["prog", "incl", "class", "intf", "func", "fugr", "cds", "tabl", "stru"], "description": "Object type" },
                "name": str_param("Object name"),
                "old_string": str_param("Text to replace (match exactly one place; empty only for empty/new objects)"),
                "new_string": str_param("Replacement text"),
                "group": { "type": "string", "description": "Function group (func only)" },
                "transport": { "type": "string", "description": "Transport request (optional)" },
                "create": { "type": "boolean", "description": "Create the object when missing; new_string becomes the initial source (default false)" },
                "description": { "type": "string", "description": "Title for auto-creation" },
                "rfc_enabled": { "type": "boolean", "description": "func only: mark remote-enabled after the write" },
                "doc": { "type": "string", "description": "func only: API documentation (Markdown) stored on the registry entry and shown in the OpenAPI catalog" }
            }, "required": ["type", "name", "old_string", "new_string"] }
        }),
        json!({
            "name": "create_object",
            "description": "Create an ABAP object shell (prog/incl/class/intf/func/fugr/cds/tabl/stru/package) via ADT. Optional 'source' writes+activates the first version in one call. package: devclass = parent package, software_component optional (candidates ZLOCAL/LOCAL/HOME tried in order).",
            "inputSchema": { "type": "object", "properties": {
                "type": { "type": "string", "enum": ["prog", "incl", "class", "intf", "func", "fugr", "cds", "tabl", "stru", "package"], "description": "Object type" },
                "name": str_param("Object name (uppercase)"),
                "description": str_param("Object title / short text (required)"),
                "devclass": { "type": "string", "description": "Package (default $TMP); for package type: parent package" },
                "group": { "type": "string", "description": "Function group (func only)" },
                "transport": { "type": "string", "description": "Transport request (optional)" },
                "software_component": { "type": "string", "description": "package only: software component" },
                "source": str_param("Optional first source (written + activated in one call)"),
                "rfc_enabled": { "type": "boolean", "description": "func only: mark remote-enabled" },
                "doc": { "type": "string", "description": "func only: API documentation (Markdown) stored on the registry entry and shown in the OpenAPI catalog" }
            }, "required": ["type", "name", "description"] }
        }),
        json!({
            "name": "delete_object",
            "description": "Delete an ABAP object (lock → DELETE → done). Deleting a fugr removes its function modules too. Use for cleanup of scratch objects; irreversible.",
            "inputSchema": { "type": "object", "properties": {
                "type": { "type": "string", "enum": ["prog", "incl", "class", "intf", "func", "fugr", "cds", "tabl", "stru", "package"], "description": "Object type" },
                "name": str_param("Object name"),
                "group": { "type": "string", "description": "Function group (func only)" },
                "transport": { "type": "string", "description": "Transport request (optional)" }
            }, "required": ["type", "name"] }
        }),
        json!({
            "name": "list_registry_apis",
            "description": "List APIs in the gateway's registry — the shared cross-session memory of agent-built interfaces (each entry: alias, func_name, intent, notes/pitfalls, example invoke body, status draft/published). ALWAYS call this FIRST in a new session before searching SAP functions: if a matching entry exists, invoke it via its func_name instead of re-exploring. Remote-enabled functions you create through the gateway are auto-registered as drafts — enrich them via PUT /api/registry/{alias} (REST).",
            "inputSchema": { "type": "object", "properties": {
                "q": { "type": "string", "description": "Optional substring filter (alias/func_name/intent, case-insensitive)" },
                "include_deleted": { "type": "boolean", "description": "Also include tombstoned entries (default false)" }
            } }
        }),
        json!({
            "name": "get_gateway_info",
            "description": "Gateway self-description: version, git commit, capability switches (auth/read_only/adt/rate_limit) and the SAP system info (sysid/release/host/os/client, cached from the first call). Call this first in a new session to learn whether writes are allowed and whether SAP is reachable — never fails on SAP outage (sap becomes null).",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "syntax_check",
            "description": "Syntax-check ABAP source WITHOUT storing it (object name anchors the check; use a Z name for scratch checks)",
            "inputSchema": { "type": "object", "properties": {
                "type": { "type": "string", "enum": ["prog", "incl", "class", "intf", "func", "fugr", "cds", "tabl", "stru"], "description": "Object type" },
                "name": str_param("Object name (a Z name works for scratch checks)"),
                "source": str_param("Full ABAP source to check")
            }, "required": ["type", "name", "source"] }
        }),
    ]
}

// ========================================================================
// JSON-RPC 帧与分发
// ========================================================================

fn rpc_result(id: Value, result: Value) -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        axum::Json(json!({
            "jsonrpc": "2.0", "id": id, "result": result
        })),
    )
        .into_response()
}

fn rpc_error(id: Value, code: i32, message: &str) -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        axum::Json(json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": code, "message": message }
        })),
    )
        .into_response()
}

/// 通知（无 id）：按 MCP Streamable HTTP 规范回 202 空体
fn accepted() -> Response {
    axum::http::StatusCode::ACCEPTED.into_response()
}

/// tools/call 的成功载荷：结果 JSON 装进 text content（MCP 惯例）
fn tool_ok(payload: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": payload.to_string() }],
        "isError": false
    })
}

/// 工具执行失败的 MCP 错误载荷（协议层仍 200，isError: true——MCP 语义：
/// 工具级失败不是传输失败）
fn tool_err(e: &RfcError) -> Value {
    json!({
        "content": [{ "type": "text", "text": json!({
            "error": { "status": e.status, "code": e.code, "key": e.key, "message": e.message }
        }).to_string() }],
        "isError": true
    })
}

/// `POST /mcp` 入口：解析单条 JSON-RPC 消息并分发。
pub async fn mcp_handler(
    axum::extract::State(pool): axum::extract::State<SharedPool>,
    body: Result<axum::Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(axum::Json(msg)) = body else {
        return rpc_error(Value::Null, -32700, "Parse error: body is not valid JSON");
    };
    // Streamable HTTP 无状态模式：一次一条消息，不接受批量
    if msg.is_array() {
        return rpc_error(
            Value::Null,
            -32600,
            "Batch requests not supported (stateless mode)",
        );
    }
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(json!({}));

    // 无 id = 通知：接受即可（initialized / cancelled 等都无需动作）
    if msg.get("id").is_none() {
        return accepted();
    }

    match method {
        "initialize" => rpc_result(
            id,
            json!({
                "protocolVersion": params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or(PROTOCOL_VERSION),
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "sap-for-agents",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": "SAP NWRFC→REST gateway as tools. New session? Start with list_registry_apis (cross-session memory of agent-built APIs), then get_gateway_info. Workflow for new work: search_functions → get_function_interface → get_function_doc → invoke_rfc. Parameter names are UPPERCASE. Code edits: use the REST API (see /agents.md)."
            }),
        ),
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({ "tools": tools_manifest() })),
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match call_tool(pool, name, args).await {
                Ok(payload) => rpc_result(id, tool_ok(payload)),
                Err(e) => rpc_result(id, tool_err(&e)),
            }
        }
        other => rpc_error(id, -32601, &format!("Method not found: {other}")),
    }
}

// ========================================================================
// 工具执行：复用 server/discovery/executor 既有入口
// ========================================================================

/// 便捷：阻塞线程内跑一个元数据查询（走 run_blocking = 池 + 重试 + 超时）
async fn call_tool(pool: SharedPool, name: &str, args: Value) -> Result<Value, RfcError> {
    match name {
        "search_functions" => {
            let pattern = req_str(&args, "pattern")?;
            let group = args
                .get("group")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let max = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(20)
                .min(100) as usize;
            let out = crate::server::run_blocking(pool, move |conn| {
                crate::discovery::search_functions(conn, &pattern, &group, max)
            })
            .await?;
            Ok(json!({ "count": out.len(), "functions": out }))
        }
        "get_function_interface" => {
            let fname = req_str(&args, "name")?.to_uppercase();
            crate::api::validate_func_name(&fname)?;
            let resp_name = fname.clone();
            let view = crate::server::run_blocking(pool, move |conn| {
                crate::server::collect_function_params(conn, &fname)
            })
            .await?;
            Ok(json!({ "name": resp_name, "params": view.params }))
        }
        "get_function_doc" => {
            let fname = req_str(&args, "name")?.to_uppercase();
            crate::api::validate_func_name(&fname)?;
            let lang = args
                .get("lang")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let out = crate::server::run_blocking(pool, move |conn| {
                crate::discovery::read_function_doc(conn, &fname, &lang, "")
            })
            .await?;
            Ok(serde_json::to_value(out).unwrap_or(json!({})))
        }
        "invoke_rfc" => {
            // args 本身就是 InvokeRequest（多带的工具字段被忽略）
            let mut req: crate::api::InvokeRequest =
                serde_json::from_value(args).map_err(|e| RfcError {
                    code: -1,
                    status: 400,
                    message: format!("invalid invoke_rfc arguments: {e}"),
                    key: "JSON_INVALID".into(),
                })?;
            req.func_name = req.func_name.to_uppercase();
            crate::api::validate_func_name(&req.func_name)?;
            // per-request 超时与 /api/rfc 同一 clamp 规则
            const MAX_TIMEOUT_SECS: u64 = 1800;
            let timeout = std::time::Duration::from_secs(
                req.timeout_secs
                    .filter(|&s| s >= 1)
                    .map(|s| s.min(MAX_TIMEOUT_SECS))
                    .unwrap_or(60),
            );
            let out = crate::server::run_blocking_with_timeout(pool, timeout, move |conn| {
                crate::executor::execute_collect(conn, &req)
            })
            .await?;
            serde_json::to_value(out).map_err(|e| RfcError {
                code: -1,
                status: 500,
                message: format!("serialize invoke result: {e}"),
                ..Default::default()
            })
        }
        "read_function_source" => {
            let fname = req_str(&args, "name")?.to_uppercase();
            crate::api::validate_func_name(&fname)?;
            let prologue = args
                .get("prologue")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let out = crate::server::run_blocking(pool, move |conn| {
                let lines = crate::discovery::read_function_source(conn, &fname)?;
                let prologue_text = if prologue {
                    let deps = crate::discovery::scan_called_functions(&lines, &fname, 50);
                    let p = crate::discovery::build_function_prologue(conn, &deps);
                    serde_json::to_value(&p).ok()
                } else {
                    None
                };
                Ok::<_, RfcError>(json!({
                    "name": fname, "count": lines.len(), "lines": lines,
                    "prologue": prologue_text,
                }))
            })
            .await?;
            Ok(out)
        }
        "read_program_source" => {
            let pname = req_str(&args, "name")?.to_uppercase();
            let resp_name = pname.clone();
            let out = crate::server::run_blocking(pool, move |conn| {
                crate::discovery::read_program_source(conn, &pname)
            })
            .await?;
            Ok(json!({ "name": resp_name, "count": out.len(), "lines": out }))
        }
        "get_ddic_type" => {
            let tname = req_str(&args, "name")?.to_uppercase();
            let resp_name = tname.clone();
            let out = crate::server::run_blocking(pool, move |conn| {
                crate::metadata::get_type_fields(conn, &tname)
            })
            .await?;
            let fields: Vec<_> = out
                .iter()
                .map(crate::api::FieldDef::from_type_field)
                .collect();
            Ok(json!({ "name": resp_name, "fields": fields }))
        }
        "get_ddic_field" => {
            let table = req_str(&args, "table")?.to_uppercase();
            let field = req_str(&args, "field")?.to_uppercase();
            let lang = args
                .get("lang")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let out = crate::server::run_blocking(pool, move |conn| {
                crate::discovery::read_ddic_field_info(conn, &table, &field, &lang)
            })
            .await?;
            Ok(serde_json::to_value(out).unwrap_or(json!({})))
        }
        "read_table" => {
            let table = req_str(&args, "table")?.to_uppercase();
            let fields: Vec<String> = args
                .get("fields")
                .and_then(|f| f.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if fields.is_empty() {
                return Err(RfcError {
                    code: -1,
                    status: 400,
                    message: "fields is required (non-empty array)".into(),
                    key: "FIELDS_EMPTY".into(),
                });
            }
            let where_clauses: Vec<String> = args
                .get("where")
                .and_then(|w| w.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let rowcount = args
                .get("rowcount")
                .and_then(|v| v.as_u64())
                .unwrap_or(1000)
                .min(10000) as u32;
            let resp_table = table.clone();
            let rows = crate::server::run_blocking(pool, move |conn| {
                crate::discovery::read_table(
                    conn,
                    &table,
                    &fields,
                    &where_clauses,
                    rowcount,
                    '\u{1}',
                )
            })
            .await?;
            Ok(json!({ "table": resp_table, "count": rows.len(), "rows": rows }))
        }
        "list_dumps" => {
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(50)
                .min(1000);
            let entries = crate::dumps::fetch_feed(None, None).await?;
            let out: Vec<_> = entries.into_iter().take(limit as usize).collect();
            Ok(json!({ "count": out.len(), "dumps": out }))
        }
        "get_dump_detail" => {
            let key = req_str(&args, "key")?;
            let detail = crate::dumps::fetch_detail(&key).await?;
            Ok(serde_json::to_value(detail).unwrap_or(json!({})))
        }
        "write_source" | "edit_code" | "create_object" => {
            use crate::objects::ObjectType;
            let tool = name; // 工具名（下方 name 会被对象名遮蔽）
            let otype = req_str(&args, "type")?;
            let name = req_str(&args, "name")?.to_uppercase();
            let group_hint = args.get("group").and_then(|v| v.as_str()).map(String::from);
            let transport = args
                .get("transport")
                .and_then(|v| v.as_str())
                .map(String::from);
            let create = args
                .get("create")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let description = args
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let devclass = args
                .get("devclass")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let rfc_enabled = args
                .get("rfc_enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let doc = args.get("doc").and_then(|v| v.as_str()).map(String::from);
            let obj_type = ObjectType::parse(&otype).ok_or_else(|| RfcError {
                code: -1,
                status: 400,
                message: format!(
                    "type 必须是 prog/incl/class/intf/func/fugr/cds/tabl/stru/package，收到: {otype}"
                ),
                key: "OBJECT_TYPE_INVALID".into(),
            })?;
            crate::server::validate_object_name(&name)?;
            let group =
                crate::server::resolve_group_if_needed(&pool, obj_type, &name, group_hint).await?;

            if tool == "create_object" {
                // 显式创建壳（可选首版源码一并写入激活）
                let spec = crate::objects::CreateSpec {
                    description: description.clone(),
                    devclass,
                    transport: transport.clone(),
                    software_component: args
                        .get("software_component")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                };
                crate::objects::create_object(&pool, obj_type, &name, &group, &spec).await?;
                let mut out = json!({ "created": true, "type": obj_type.api_name(), "name": name });
                if let Some(src) = args
                    .get("source")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                {
                    if obj_type.has_source() {
                        let outcome = crate::objects::write_object_source(
                            obj_type,
                            &name,
                            &group,
                            src,
                            transport.as_deref(),
                            true,
                            rfc_enabled,
                            doc.as_deref(),
                        )
                        .await?;
                        crate::objects::drain_pool_after_write(&pool, obj_type);
                        out["write"] = serde_json::to_value(outcome).unwrap_or_default();
                    }
                }
                return Ok(out);
            }

            let opts = crate::objects::WriteOpts {
                transport: transport.as_deref(),
                activate: true,
                create_desc: create.then_some(description.as_str()),
                rfc_enabled,
                doc: doc.as_deref(),
            };
            let outcome: serde_json::Value = if tool == "write_source" {
                let source = req_str(&args, "source")?;
                let o = crate::objects::write_object_maybe_create(
                    &pool, obj_type, &name, &group, &source, &opts,
                )
                .await?;
                serde_json::to_value(&o).unwrap_or_default()
            } else {
                let old_string = args
                    .get("old_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let new_string = args
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                crate::objects::replace_object_maybe_create(
                    &pool,
                    obj_type,
                    &name,
                    &group,
                    &old_string,
                    &new_string,
                    &opts,
                )
                .await?
            };
            Ok(outcome)
        }
        "delete_object" => {
            use crate::objects::ObjectType;
            let otype = req_str(&args, "type")?;
            let name = req_str(&args, "name")?.to_uppercase();
            let group_hint = args.get("group").and_then(|v| v.as_str()).map(String::from);
            let transport = args
                .get("transport")
                .and_then(|v| v.as_str())
                .map(String::from);
            let obj_type = ObjectType::parse(&otype).ok_or_else(|| RfcError {
                code: -1,
                status: 400,
                message: format!(
                    "type 必须是 prog/incl/class/intf/func/fugr/cds/tabl/stru/package，收到: {otype}"
                ),
                key: "OBJECT_TYPE_INVALID".into(),
            })?;
            crate::server::validate_object_name(&name)?;
            let group =
                crate::server::resolve_group_if_needed(&pool, obj_type, &name, group_hint).await?;
            let outcome =
                crate::objects::delete_object(obj_type, &name, &group, transport.as_deref())
                    .await?;
            crate::objects::drain_pool_after_write(&pool, obj_type);
            Ok(serde_json::to_value(outcome).unwrap_or_default())
        }
        "list_registry_apis" => {
            let q = args.get("q").and_then(|v| v.as_str()).map(String::from);
            let include_deleted = args
                .get("include_deleted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let entries = crate::registry::list_entries(q.as_deref(), include_deleted)?;
            let count = entries.len();
            Ok(json!({ "count": count, "entries": entries }))
        }
        "get_gateway_info" => {
            // 与 GET /api/version 同源（version 模块内含懒加载缓存）
            Ok(crate::version::gateway_info(&pool).await)
        }
        "where_used" => {
            let fname = req_str(&args, "name")?.to_uppercase();
            crate::api::validate_func_name(&fname)?;
            let max = args
                .get("max")
                .and_then(|v| v.as_u64())
                .unwrap_or(200)
                .min(2000) as usize;
            let resp_name = fname.clone();
            let usages = crate::server::run_blocking(pool, move |conn| {
                crate::discovery::read_where_used(conn, &fname, max)
            })
            .await?;
            Ok(json!({ "name": resp_name, "count": usages.len(), "usages": usages }))
        }
        "syntax_check" => {
            use crate::objects::ObjectType;
            let otype = req_str(&args, "type")?;
            let name = req_str(&args, "name")?.to_uppercase();
            let source = req_str(&args, "source")?;
            let group_hint = args.get("group").and_then(|v| v.as_str()).map(String::from);
            let obj_type = ObjectType::parse(&otype).ok_or_else(|| RfcError {
                code: -1,
                status: 400,
                message: format!("type 必须是 prog/incl/class/intf/func/fugr/cds，收到: {otype}"),
                key: "OBJECT_TYPE_INVALID".into(),
            })?;
            crate::server::validate_object_name(&name)?;
            let group =
                crate::server::resolve_group_if_needed(&pool, obj_type, &name, group_hint).await?;
            let base = obj_type.base_rel(&name, &group);
            // FM 语法检查同样按 SEDI 形态（与写入口径一致）
            let source = if obj_type == ObjectType::Function {
                crate::objects::normalize_function_source(&source)
            } else {
                source
            };
            let issues = crate::objects::syntax_check(&base, &source).await?;
            Ok(json!({ "type": otype, "name": name, "count": issues.len(), "issues": issues }))
        }
        _ => Err(RfcError {
            code: -1,
            status: 404,
            message: format!("unknown tool: {name}"),
            key: "TOOL_NOT_FOUND".into(),
        }),
    }
}

fn req_str(args: &Value, key: &str) -> Result<String, RfcError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(String::from)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RfcError {
            code: -1,
            status: 400,
            message: format!("missing required string argument: {key}"),
            key: "ARG_MISSING".into(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_covers_core_workflow() {
        let tools = tools_manifest();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in [
            "search_functions",
            "get_function_interface",
            "get_function_doc",
            "invoke_rfc",
            "read_function_source",
            "read_program_source",
            "get_ddic_type",
            "get_ddic_field",
            "read_table",
            "list_dumps",
            "get_dump_detail",
            "get_gateway_info",
            "list_registry_apis",
            "syntax_check",
        ] {
            assert!(names.contains(&expected), "缺少工具 {expected}");
        }
        // 每个工具都有 inputSchema 且 invoke_rfc 必填 func_name
        for t in &tools {
            assert!(t["inputSchema"].is_object(), "{} 缺 inputSchema", t["name"]);
        }
        let invoke = tools.iter().find(|t| t["name"] == "invoke_rfc").unwrap();
        assert_eq!(invoke["inputSchema"]["required"][0], "func_name");
    }

    #[test]
    fn protocol_version_constant_is_streamable_http() {
        // 2025-03-26 引入 Streamable HTTP；无状态模式是其合法子集
        assert_eq!(PROTOCOL_VERSION, "2025-03-26");
    }
}
