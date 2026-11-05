//! `GET /openapi.json` —— 导出本网关的 OpenAPI 3.0.3 规范。
//!
//! 目的与 `/agents.md` 一致：给 AI Agent / 代码生成器 / Postman 等工具一份
//! 机器可读的接口契约，让它们不用读人工文档就能自助构建调用。
//!
//! 设计：
//! - 规范主体是一份**数据驱动的 JSON 文件**（src/openapi-base.json，编译期
//!   include_str! 嵌入 [`SPEC_JSON`]），启动后
//!   `from_str` 解析再补丁插值（servers / version / 认证方案）。相比在
//!   `json!` 宏里手写 800 行嵌套，字面量没有宏定界符陷阱，且内容可用
//!   任何 JSON 工具直接校验/格式化。
//! - `servers` 按请求 Host 头动态推导（与首页 `index_handler` 同策略），
//!   规范里的示例地址自动匹配用户实际访问的 host:port。
//! - Bearer 认证按 `auth::is_enabled()` 动态出现：部署方设置了 `SAP_API_KEY`
//!   时，规范才声明安全方案，生成出的客户端才带 token。
//! - 每次请求现算（无缓存）：解析 + 组装成本低，换来零失效风险。

use axum::response::IntoResponse;
use serde_json::{json, Value};

/// 内置规范主体（OpenAPI 3.0.3）。占位符由 [`build_spec`] 在运行时替换：
/// - `REPLACE_BASE` → 实际访问地址（Host 头推导）
/// - `info.version` → 编译期 crate 版本
///
/// 内容正确性由单测 `spec_has_core_structure` 与
/// `spec_is_serializable_and_ref_resolvable` 锁定。
/// 规范主体外置为数据文件（编译期嵌入）——2026-11-05 审计 P1：
/// 此前是 3138 行的原始字符串字面量，占文件 77%，无法 lint/无法 jq。
const SPEC_JSON: &str = include_str!("openapi-base.json");

/// GET /openapi.json —— 返回 OpenAPI 3.0.3 规范（免鉴权，公开页）。
/// v0.13 起并入注册表 published 条目的平坦调用 operation（服务目录）——
/// 读的是后台 watcher 预热的缓存，不碰 SAP：公开页永远秒回、SAP 宕机不受影响。
pub async fn openapi_handler(
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    // 与 index_handler 同策略：从 Host 头推导访问地址（servers 字段）
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("127.0.0.1:3000");
    let base = format!("http://{host}");
    let auth_enabled = crate::auth::is_enabled();
    let mut spec = build_spec(&base, auth_enabled);
    if let Some(ops) = registry_ops_cached().await {
        if !ops.is_empty() {
            if let Some(paths) = spec["paths"].as_object_mut() {
                for (path, op) in ops.iter() {
                    paths.insert(path.clone(), op.clone());
                }
            }
            if auth_enabled {
                mark_api_security(&mut spec);
            }
            spec["info"]["description"] = json!(format!(
                "{} — This document additionally contains the API registry service catalog \
                 (each published entry maps to POST /api/invokes/{{alias}}, flat JSON in/out).",
                spec["info"]["description"].as_str().unwrap_or_default()
            ));
        }
    }
    axum::Json(spec).into_response()
}

/// 构建完整规范。`base_url` 进 servers；`auth_enabled` 决定是否声明 Bearer 安全方案。
/// pub 供单元测试直接断言结构。
pub fn build_spec(base_url: &str, auth_enabled: bool) -> Value {
    let mut spec: Value =
        serde_json::from_str(SPEC_JSON).expect("内置 SPEC_JSON 必须是合法 JSON（单测锁定）");
    // 运行时插值：访问地址 + crate 版本
    spec["servers"][0]["url"] = json!(base_url);
    spec["info"]["version"] = json!(env!("CARGO_PKG_VERSION"));

    // 认证方案 + /api/* 操作的安全标记（合并进来的动态 operation 也走同一段，
    // 见 mark_api_security——两个调用方共用）
    if auth_enabled {
        spec["components"]["securitySchemes"] = json!({
            "bearerAuth": {
                "type": "http",
                "scheme": "bearer",
                "description": "Authorization: Bearer <SAP_API_KEY>. Applies to /api/* endpoints only; probes and public pages stay open."
            }
        });
        mark_api_security(&mut spec);
    }

    spec
}

/// 给 spec 里全部 `/api/*` operation 打上 bearerAuth 标记（`/api/version` 等公开例外）。
/// build_spec 与动态合并（/api/openapi、注册表目录）共用，保证鉴权模式下
/// 生成器产出的客户端会带 token。
fn mark_api_security(spec: &mut Value) {
    if let Some(paths) = spec["paths"].as_object_mut() {
        for (path, item) in paths.iter_mut() {
            if !path.starts_with("/api/") {
                continue;
            }
            // /api/version、/api/systems、/api/setup 刻意公开：Agent 需要在拿到
            // token 之前知道 capabilities.auth（要不要 token）、有哪些系统、
            // 以及未配置模式下如何引导配置
            if path.as_str() == "/api/version"
                || path.as_str() == "/api/systems"
                || path.as_str() == "/api/setup"
            {
                continue;
            }
            if let Some(ops) = item.as_object_mut() {
                for (_method, op) in ops.iter_mut() {
                    op["security"] = json!([{ "bearerAuth": [] }]);
                }
            }
        }
    }
}

// ========================================================================
// 动态规范：按函数生成类型化 operation（GET /api/openapi?functions=A,B）
// ========================================================================

use crate::api::{FieldDef, FunctionParam};

/// RFCTYPE 名称（静态规范同款命名，如 "CHAR"/"INT"/"TABLE"）→ JSON Schema type。
/// 与 /api/rfc 的取值语义一致：BCD 进出都是字符串，BYTE/XSTRING 是 Base64 字符串。
fn schema_type_for(type_name: &str) -> &'static str {
    match type_name {
        "INT" | "INT1" | "INT2" | "INT8" => "integer",
        "FLOAT" => "number",
        _ => "string",
    }
}

/// 字段列表 → (properties, required)。required 只收 SAP 标记为必填的输入字段。
fn field_defs_to_properties(fields: &[FieldDef]) -> (serde_json::Map<String, Value>, Vec<String>) {
    let mut props = serde_json::Map::new();
    let mut required = Vec::new();
    for f in fields {
        let mut schema = match f.type_name {
            "STRUCTURE" => {
                let (sub, _) = field_defs_to_properties(f.fields.as_deref().unwrap_or(&[]));
                json!({ "type": "object", "properties": sub })
            }
            "TABLE" => {
                let (sub, _) = field_defs_to_properties(f.fields.as_deref().unwrap_or(&[]));
                json!({ "type": "array", "items": { "type": "object", "properties": sub } })
            }
            t => {
                let mut s = json!({ "type": schema_type_for(t) });
                if f.length > 0 {
                    s["maxLength"] = json!(f.length);
                }
                s
            }
        };
        if !f.description.is_empty() {
            schema["description"] = json!(f.description);
        }
        props.insert(f.name.clone(), schema);
        // SAP 字段必填信息在 ParamInfo 层；FieldDef 层没有，输入 required 由参数级决定
        let _ = &mut required;
    }
    (props, required)
}

/// 参数级 schema（含 maxLength/description），STRUCTURE/TABLE 展开子字段。
fn param_schema(p: &FunctionParam) -> Value {
    match p.type_name {
        "STRUCTURE" => {
            let (sub, _) = field_defs_to_properties(p.fields.as_deref().unwrap_or(&[]));
            json!({ "type": "object", "properties": sub })
        }
        "TABLE" => {
            let (sub, _) = field_defs_to_properties(p.fields.as_deref().unwrap_or(&[]));
            json!({ "type": "array", "items": { "type": "object", "properties": sub } })
        }
        t => {
            let mut s = json!({ "type": schema_type_for(t) });
            if p.length > 0 {
                s["maxLength"] = json!(p.length);
            }
            s
        }
    }
}

/// 为一个函数生成 `POST /api/functions/{name}/invoke` 的 operation。
/// 返回 (路径, operation)。纯函数（输入来自接口元数据），单测直接覆盖。
///
/// 请求体结构与 /api/rfc 完全同构；差异只在：func_name 由路径注入（不出现），
/// 各参数按真实类型/嵌套展开，输出参数以 auto_outputs/table_outputs 的
/// enum/示例形式提示。agent 拿到 operation 即等于拿到该 BAPI 的调用说明。
pub fn function_operation(name: &str, params: &[FunctionParam]) -> (String, Value) {
    let is_input = |p: &FunctionParam| p.direction == "IMPORT" || p.direction == "CHANGING";
    let is_output = |p: &FunctionParam| {
        p.direction == "EXPORT" || p.direction == "CHANGING" || p.direction == "TABLES"
    };

    // 输入分组
    let mut inputs_props = serde_json::Map::new();
    let mut inputs_required: Vec<String> = Vec::new();
    let mut struct_inputs_props = serde_json::Map::new();
    let mut table_inputs_props = serde_json::Map::new();
    for p in params.iter().filter(|p| is_input(p)) {
        let mut schema = param_schema(p);
        if !p.description.is_empty() {
            schema["description"] = json!(p.description);
        }
        if !p.default.is_empty() {
            schema["default"] = json!(p.default);
        }
        match p.type_name {
            "STRUCTURE" => {
                struct_inputs_props.insert(p.name.clone(), schema);
            }
            "TABLE" => {
                table_inputs_props.insert(p.name.clone(), schema);
            }
            _ => {
                if !p.optional {
                    inputs_required.push(p.name.clone());
                }
                inputs_props.insert(p.name.clone(), schema);
            }
        }
    }

    // 输出分组（提示性：标量 enum 进 auto_outputs，表/结构体给示例）
    let scalar_out: Vec<&str> = params
        .iter()
        .filter(|p| is_output(p) && p.type_name != "STRUCTURE" && p.type_name != "TABLE")
        .map(|p| p.name.as_str())
        .collect();
    let table_out: Vec<&str> = params
        .iter()
        .filter(|p| is_output(p) && p.type_name == "TABLE")
        .map(|p| p.name.as_str())
        .collect();
    let struct_out: Vec<&str> = params
        .iter()
        .filter(|p| is_output(p) && p.type_name == "STRUCTURE")
        .map(|p| p.name.as_str())
        .collect();
    let has_return_table = params
        .iter()
        .any(|p| is_output(p) && p.type_name == "TABLE" && p.name == "RETURN");

    // 组 requestBody schema
    let mut req_props = serde_json::Map::new();
    let mut example = serde_json::Map::new();
    if !inputs_props.is_empty() {
        req_props.insert(
            "inputs".into(),
            json!({
                "type": "object",
                "properties": inputs_props,
                "required": inputs_required,
            }),
        );
    }
    if !struct_inputs_props.is_empty() {
        req_props.insert(
            "struct_inputs".into(),
            json!({ "type": "object", "properties": struct_inputs_props }),
        );
    }
    if !table_inputs_props.is_empty() {
        req_props.insert(
            "table_inputs".into(),
            json!({ "type": "object", "properties": table_inputs_props }),
        );
    }
    if !scalar_out.is_empty() {
        req_props.insert(
            "auto_outputs".into(),
            json!({
                "type": "array",
                "items": { "type": "string", "enum": scalar_out },
                "description": "Output scalar parameters to read by their true DDIC type",
            }),
        );
        example.insert("auto_outputs".into(), json!(scalar_out));
    }
    if !table_out.is_empty() || !struct_out.is_empty() {
        let mut ex = serde_json::Map::new();
        for t in &table_out {
            ex.insert(
                (*t).to_string(),
                json!([{ "name": "<field>", "auto": true }]),
            );
        }
        for s in &struct_out {
            ex.insert((*s).to_string(), json!([{ "name": "<field>" }]));
        }
        req_props.insert(
            "table_outputs".into(),
            json!({
                "type": "object",
                "description": "Output tables (and top-level structures) to traverse; keys are parameter names",
                "example": ex,
            }),
        );
    }
    if has_return_table {
        req_props.insert(
            "read_return".into(),
            json!({ "type": "boolean", "default": true, "description": "Read the RETURN message table" }),
        );
        example.insert("read_return".into(), json!(true));
    }
    req_props.insert(
        "timeout_secs".into(),
        json!({ "type": "integer", "minimum": 1, "description": "Per-call timeout override (seconds)" }),
    );

    let path = format!("/api/functions/{name}/invoke");
    let operation = json!({
        "post": {
            "tags": ["invoke"],
            "operationId": format!("call_{name}"),
            "summary": format!("Invoke {name} (typed; func name comes from the path)"),
            "description": format!(
                "Typed operation generated from the DDIC interface metadata of {name}. \
    The body is structurally identical to POST /api/rfc (func_name is injected from the path; \
    a func_name key in the body is ignored). Inspect GET /api/functions/{name} for the full interface."
            ),
            "requestBody": {
                "required": true,
                "content": {
                    "application/json": {
                        "schema": {
                            "type": "object",
                            "properties": req_props,
                            "example": example,
                        }
                    }
                }
            },
            "responses": {
                "200": {
                    "description": "Invocation result (scalars/tables/structs/return_table)",
                    "content": { "application/json": { "schema": { "$ref": "#/components/schemas/InvokeResponse" } } }
                },
                "default": { "$ref": "#/components/responses/Error" }
            }
        }
    });
    (path, operation)
}

/// 接口读取失败时的占位 operation（缺口可见，不吞错——与 prologue 同哲学）。
pub fn function_operation_failed(name: &str, err_msg: &str) -> (String, Value) {
    let path = format!("/api/functions/{name}/invoke");
    let operation = json!({
        "post": {
            "tags": ["invoke"],
            "operationId": format!("call_{name}"),
            "summary": format!("Invoke {name} (interface metadata unavailable)"),
            "description": format!("Interface metadata could not be read: {err_msg}. \
    The operation itself still works — inspect GET /api/functions/{name} and call it via POST /api/rfc."),
            "responses": {
                "200": { "description": "Invocation result", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/InvokeResponse" } } } },
                "default": { "$ref": "#/components/responses/Error" }
            }
        }
    });
    (path, operation)
}

// ========================================================================
// 注册表驱动的服务目录（v0.13）：published 条目 → /api/invokes/{alias} 类型化
// operation，并入公开的 /openapi.json。契约按需派生（与 invoke 模块同一套
// 输入/输出分类），SAP 侧签名漂移由 TTL 重建自动跟随。
// ========================================================================

use crate::registry::Entry;
use std::sync::Arc;
use std::time::Duration as CacheTtl;

/// 一个 published 条目的平坦调用 operation。纯函数（输入 = 条目 + 接口元数据）。
/// 请求/响应 schema 全部扁平展开——消费方拿到的就是调用说明本身。
pub fn flat_invoke_operation(entry: &Entry, params: &[FunctionParam]) -> (String, Value) {
    // TABLES 双向：可输入（BAPI 选择表惯例）也是输出——与 invoke 模块同口径
    let is_input = |p: &FunctionParam| {
        p.direction == "IMPORT" || p.direction == "CHANGING" || p.direction == "TABLES"
    };
    let is_output = |p: &FunctionParam| {
        p.direction == "EXPORT" || p.direction == "CHANGING" || p.direction == "TABLES"
    };
    // 请求体：输入参数平铺（required = SAP 标记非可选）
    let mut req_props = serde_json::Map::new();
    let mut required = Vec::new();
    for p in params.iter().filter(|p| is_input(p)) {
        if !p.description.is_empty() {
            let mut s = param_schema(p);
            s["description"] = json!(p.description);
            req_props.insert(p.name.clone(), s);
        } else {
            req_props.insert(p.name.clone(), param_schema(p));
        }
        if !p.optional {
            required.push(p.name.clone());
        }
    }
    // 响应：输出参数平铺 + 截断标记
    let mut resp_props = serde_json::Map::new();
    for p in params.iter().filter(|p| is_output(p)) {
        resp_props.insert(p.name.clone(), param_schema(p));
    }
    resp_props.insert(
        "_truncated".into(),
        json!({
            "type": "array", "items": { "type": "string" },
            "description": "Present only when an output table exceeded its row cap and was truncated"
        }),
    );
    // summary/description：intent 与 notes 是条目的灵魂；example 原样内嵌
    let summary = if entry.intent.is_empty() {
        format!("Invoke {} (registered API)", entry.func_name)
    } else {
        entry.intent.clone()
    };
    let mut description = format!(
        "Flat invoke of SAP function {}. Response/output tables are row-capped ({} by default; \
         override per call with ?limit=).",
        entry.func_name,
        entry.max_rows.unwrap_or(crate::invoke::DEFAULT_TABLE_CAP),
    );
    if !entry.notes.is_empty() {
        description.push_str(&format!("\n\nNotes: {}", entry.notes));
    }
    // doc 是面向消费方的完整文档正文（Markdown；Redoc 会按 Markdown 渲染）
    if !entry.doc.is_empty() {
        description.push_str(&format!("\n\n---\n{}", entry.doc));
    }
    if let Some(ex) = &entry.example {
        description.push_str(&format!("\n\nExample request body: `{}`", ex));
    }
    let op_id: String = format!(
        "invoke_{}",
        entry
            .alias
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
    );
    let path = format!("/api/invokes/{}", entry.alias);
    let mut op = json!({
        "post": {
            "tags": ["registry"],
            "operationId": op_id,
            "summary": summary,
            "description": description,
            "parameters": [
                { "name": "limit", "in": "query", "required": false,
                  "schema": { "type": "integer", "default": entry.max_rows.unwrap_or(crate::invoke::DEFAULT_TABLE_CAP), "maximum": 10000 },
                  "description": "Row cap for output tables" },
                { "name": "timeout_secs", "in": "query", "required": false,
                  "schema": { "type": "integer", "minimum": 1 },
                  "description": "Per-call timeout override in seconds" }
            ],
            "requestBody": {
                "required": true,
                "content": { "application/json": { "schema": {
                    "type": "object",
                    "properties": req_props,
                    "required": required,
                    "description": "Flat body: parameter name → value (case-insensitive keys; \
                                    structures are objects, tables are arrays of row objects)"
                } } }
            },
            "responses": {
                "200": { "description": "Flat result: output parameter name → value", "content": {
                    "application/json": { "schema": { "type": "object", "properties": resp_props } } } },
                "400": { "$ref": "#/components/responses/Error" },
                "401": { "$ref": "#/components/responses/Error" },
                "404": { "$ref": "#/components/responses/Error" },
                "502": { "$ref": "#/components/responses/Error" },
                "504": { "$ref": "#/components/responses/Error" }
            }
        }
    });
    if crate::auth::is_enabled() {
        op["post"]["security"] = json!([{ "bearerAuth": [] }]);
    }
    (path, op)
}

/// 注册表 operation 缓存。generation 对齐注册表变更；TTL 兜底签名漂移
///（FM 接口变了但注册表没动的场景）。
struct RegistryOpsCache {
    generation: u64,
    built_at: std::time::Instant,
    ops: Arc<Vec<(String, Value)>>,
}

static REGISTRY_OPS: tokio::sync::RwLock<Option<RegistryOpsCache>> =
    tokio::sync::RwLock::const_new(None);

/// 启动期注入的多系统注册表（main 组装后调用一次）。
/// 目录重建按条目 system 从这里取连接；未注入（测试/异常路径）时目录为空。
static SHARED_STATE: std::sync::OnceLock<crate::server::SharedState> = std::sync::OnceLock::new();

/// 启动期注入多系统注册表（main 调一次）。
pub fn init_shared_state(state: crate::server::SharedState) {
    let _ = SHARED_STATE.set(state);
}

fn shared_state() -> Option<crate::server::SharedState> {
    SHARED_STATE.get().cloned()
}

/// 缓存 TTL：签名漂移跟随的上限（正常变更走 generation 立即失效）。
const REGISTRY_OPS_TTL: CacheTtl = CacheTtl::from_secs(600);

/// 重建全部 published 条目的 operation（借连接池逐个拉接口元数据）。
/// 单条目失败 → 占位 operation（缺口可见，不吞错，不拖垮整个目录）。
async fn rebuild_registry_ops(state: &crate::server::SharedState) -> Vec<(String, Value)> {
    let entries = match crate::registry::published_entries() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(key = %e.key, "注册表目录重建：读取 published 条目失败");
            return Vec::new();
        }
    };
    let mut ops = Vec::with_capacity(entries.len());
    for entry in entries {
        // 条目按其 system 字段路由到对应系统（v0.16 多系统）
        let Some(sys) = state.resolve(&entry.system) else {
            let path = format!("/api/invokes/{}", entry.alias);
            let placeholder = json!({
                "post": {
                    "tags": ["registry"],
                    "operationId": format!("invoke_{}", entry.alias.replace('/', "_")),
                    "summary": format!("{} (unknown system)", entry.intent),
                    "description": format!(
                        "Entry {} references system '{}' which is not configured in this                          gateway deployment. Update the entry (PUT /api/registry/{}) or                          add the system to sap_systems.toml.",
                        entry.alias, entry.system, entry.alias
                    ),
                    "responses": { "200": { "description": "Flat result" } }
                }
            });
            ops.push((path, placeholder));
            continue;
        };
        let func = entry.func_name.clone();
        let iface_sys = Arc::clone(&sys);
        let r = crate::server::run_blocking(Arc::clone(&sys), move |conn| {
            crate::server::collect_function_params(&iface_sys, conn, &func)
        })
        .await;
        let op = match r {
            Ok(view) => flat_invoke_operation(&entry, &view.params),
            Err(e) => {
                let path = format!("/api/invokes/{}", entry.alias);
                let placeholder = json!({
                    "post": {
                        "tags": ["registry"],
                        "operationId": format!("invoke_{}", entry.alias.replace('/', "_")),
                        "summary": format!("{} (interface metadata unavailable)", entry.intent),
                        "description": format!(
                            "Interface metadata could not be read for {}: {}. \
                             The endpoint still works; see GET /api/registry/{}.",
                            entry.func_name, e.message, entry.alias
                        ),
                        "responses": { "200": { "description": "Flat result" } }
                    }
                });
                (path, placeholder)
            }
        };
        ops.push(op);
    }
    ops
}

/// 取注册表 operation（缓存优先：代际一致且未过 TTL 直接复用）。
/// 重建失败（如 SAP 不可达）时退回旧缓存——公开规范永远可用，最多旧一点。
pub async fn registry_ops() -> Arc<Vec<(String, Value)>> {
    let gen = crate::registry::generation();
    if let Some(cached) = REGISTRY_OPS.read().await.as_ref() {
        if cached.generation == gen && cached.built_at.elapsed() < REGISTRY_OPS_TTL {
            return Arc::clone(&cached.ops);
        }
    }
    let ops = Arc::new(match shared_state() {
        Some(state) => rebuild_registry_ops(&state).await,
        None => Vec::new(),
    });
    *REGISTRY_OPS.write().await = Some(RegistryOpsCache {
        generation: gen,
        built_at: std::time::Instant::now(),
        ops: Arc::clone(&ops),
    });
    ops
}

/// 只读缓存快照（公开 /openapi.json handler 用——不触碰 SAP，永远秒回；
/// 缓存由后台 watcher 预热）。
async fn registry_ops_cached() -> Option<Arc<Vec<(String, Value)>>> {
    REGISTRY_OPS
        .read()
        .await
        .as_ref()
        .map(|c| Arc::clone(&c.ops))
}

/// 后台 watcher（main spawn）：代际变化、缓存为空或 TTL 到期时重建。
/// 让公开的 /openapi.json 永远命中缓存，不被 SAP 的健康状况绑架。
pub async fn registry_ops_watcher() {
    loop {
        tokio::time::sleep(CacheTtl::from_secs(2)).await;
        let gen = crate::registry::generation();
        let need = match REGISTRY_OPS.read().await.as_ref() {
            None => true, // 尚未预热
            Some(c) => c.generation != gen || c.built_at.elapsed() >= REGISTRY_OPS_TTL,
        };
        if need {
            let ops = match shared_state() {
                Some(state) => rebuild_registry_ops(&state).await,
                None => Vec::new(),
            };
            tracing::info!(count = ops.len(), generation = gen, "注册表服务目录已重建");
            *REGISTRY_OPS.write().await = Some(RegistryOpsCache {
                generation: gen,
                built_at: std::time::Instant::now(),
                ops: Arc::new(ops),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_has_core_structure() {
        let spec = build_spec("http://127.0.0.1:3000", false);
        assert_eq!(spec["openapi"], "3.0.3");
        assert!(spec["info"]["version"].as_str().is_some());
        assert_eq!(spec["servers"][0]["url"], "http://127.0.0.1:3000");
        // 主要端点齐全
        let paths = spec["paths"].as_object().unwrap();
        for p in [
            "/health",
            "/ready",
            "/metrics",
            "/agents.md",
            "/api/version",
            "/api/rfc",
            "/api/functions/search",
            "/api/functions/{name}",
            "/api/functions/{name}/doc",
            "/api/functions/{name}/source",
            "/api/programs/{name}/source",
            "/api/ddic/type/{name}",
            "/api/ddic/field/{table}/{field}",
            "/api/table/read",
            "/api/registry",
            "/api/registry/{alias}",
            "/api/invokes/audit",
            "/api/invokes/{alias}",
            "/api/dumps",
            "/api/dumps/grouped",
            "/api/dumps/{key}/detail",
            "/api/objects/{type}/{name}/source",
            "/api/objects/{type}/{name}/replace",
            "/api/objects/{type}/{name}/syntax",
            "/api/objects/{type}/{name}/create",
            "/api/objects/{type}/{name}",
            "/api/adt/{path}",
        ] {
            assert!(paths.contains_key(p), "缺少端点 {p}");
        }
        // 组件 schema 齐全
        let schemas = spec["components"]["schemas"].as_object().unwrap();
        for s in [
            "ScalarValue",
            "TypedScalar",
            "MaxLen",
            "VersionInfo",
            "FieldSpec",
            "FieldDef",
            "InvokeRequest",
            "InvokeResponse",
            "SearchRequest",
            "SearchResponse",
            "FunctionInterface",
            "FunctionParam",
            "FunctionDocResponse",
            "SourceResponse",
            "DdicTypeResponse",
            "FieldSemanticsResponse",
            "TableReadRequest",
            "TableReadResponse",
            "DumpEntry",
            "DumpGroup",
            "DumpFrame",
            "DumpDetail",
            "ObjectWriteBody",
            "ObjectReplaceBody",
            "ActivationOutcome",
            "WriteOutcome",
            "SyntaxIssue",
            "ErrorBody",
        ] {
            assert!(schemas.contains_key(s), "缺少 schema {s}");
        }
    }

    #[test]
    fn auth_disabled_has_no_security() {
        let spec = build_spec("http://127.0.0.1:3000", false);
        // 未启用认证：无 securitySchemes，操作不带 security 标记
        assert!(spec["components"]["securitySchemes"].is_null());
        assert!(spec["paths"]["/api/rfc"]["post"]["security"].is_null());
    }

    #[test]
    fn auth_enabled_marks_api_ops_only() {
        let spec = build_spec("http://127.0.0.1:3000", true);
        // 启用认证：声明 Bearer 方案
        assert_eq!(
            spec["components"]["securitySchemes"]["bearerAuth"]["scheme"],
            "bearer"
        );
        // /api/* 操作带 security
        assert_eq!(
            spec["paths"]["/api/rfc"]["post"]["security"][0]["bearerAuth"],
            json!([])
        );
        assert_eq!(
            spec["paths"]["/api/functions/search"]["post"]["security"][0]["bearerAuth"],
            json!([])
        );
        // 探针/公开页不带 security
        assert!(spec["paths"]["/health"]["get"]["security"].is_null());
        assert!(spec["paths"]["/agents.md"]["get"]["security"].is_null());
        assert!(spec["paths"]["/metrics"]["get"]["security"].is_null());
        // /api/version 虽在 /api/* 下，但刻意公开（Agent 需在拿到 token 前
        // 知道 capabilities.auth）——显式 security: [] 而非缺省
        assert_eq!(spec["paths"]["/api/version"]["get"]["security"], json!([]));
    }

    #[test]
    fn spec_is_serializable_and_ref_resolvable() {
        // 序列化往返（保证无不可序列化值）；$ref 指向的 schema 都存在
        let spec = build_spec("http://h:1", true);
        let round: Value = serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        let schemas = round["components"]["schemas"].as_object().unwrap();
        let text = round.to_string();
        // 粗校验：每个 "#/components/schemas/X" 引用都有对应定义
        for part in text.split("\"#/components/schemas/").skip(1) {
            let name: String = part
                .chars()
                .take_while(|c| *c != '"' && *c != '\\')
                .collect();
            assert!(schemas.contains_key(&name), "引用了不存在的 schema: {name}");
        }
    }

    /// 构造测试用 FunctionParam（含嵌套结构/表字段）
    fn fp(
        name: &str,
        type_name: &'static str,
        direction: &'static str,
        optional: bool,
        fields: Option<Vec<FieldDef>>,
    ) -> FunctionParam {
        FunctionParam {
            name: name.into(),
            type_name,
            direction,
            length: if type_name == "CHAR" { 255 } else { 0 },
            decimals: 0,
            optional,
            default: String::new(),
            description: String::new(),
            fields,
        }
    }

    #[test]
    fn function_operation_groups_inputs_and_outputs() {
        let params = vec![
            fp("REQUTEXT", "CHAR", "IMPORT", false, None),
            fp("ECHOTEXT", "CHAR", "EXPORT", false, None),
            fp("MAX_ROW", "INT", "IMPORT", true, None),
        ];
        let (path, op) = function_operation("STFC_CONNECTION", &params);
        assert_eq!(path, "/api/functions/STFC_CONNECTION/invoke");
        let post = &op["post"];
        assert_eq!(post["operationId"], "call_STFC_CONNECTION");
        // 输入标量：必填 REQUTEXT、可选 MAX_ROW（整数类型）
        let inputs =
            &post["requestBody"]["content"]["application/json"]["schema"]["properties"]["inputs"];
        assert_eq!(inputs["properties"]["REQUTEXT"]["type"], "string");
        assert_eq!(inputs["properties"]["REQUTEXT"]["maxLength"], 255);
        assert_eq!(inputs["properties"]["MAX_ROW"]["type"], "integer");
        assert_eq!(inputs["required"][0], "REQUTEXT");
        // 输出标量进 auto_outputs enum
        let auto = &post["requestBody"]["content"]["application/json"]["schema"]["properties"]
            ["auto_outputs"];
        assert_eq!(auto["items"]["enum"][0], "ECHOTEXT");
        // func_name 不出现在请求体（由路径注入）
        let text = post.to_string();
        assert!(!text.contains("\"func_name\""), "func_name 应由路径注入");
    }

    /// USERLIST/RETURN 共用的行字段构造器（FieldDef 未实现 Clone，各自构造）
    fn row_fields() -> Option<Vec<FieldDef>> {
        Some(vec![FieldDef {
            name: "USERNAME".into(),
            type_name: "CHAR",
            length: 12,
            decimals: 0,
            description: String::new(),
            fields: None,
        }])
    }

    #[test]
    fn function_operation_expands_structures_and_tables() {
        let params = vec![
            fp("USERLIST", "TABLE", "EXPORT", false, row_fields()),
            fp("RETURN", "TABLE", "EXPORT", false, row_fields()),
        ];
        let (_path, op) = function_operation("BAPI_USER_GETLIST", &params);
        let text = op.to_string();
        // 表输出给了 table_outputs 提示 + RETURN 触发 read_return
        assert!(text.contains("table_outputs"));
        assert!(text.contains("read_return"));
        // TABLE 行字段展开为数组元素对象
        assert!(
            op["post"]["requestBody"]["content"]["application/json"]["schema"]["properties"]
                ["table_outputs"]["example"]["USERLIST"][0]["name"]
                == "<field>"
        );
    }

    #[test]
    fn function_operation_failed_is_visible_placeholder() {
        let (path, op) = function_operation_failed("Z_MISSING", "FU_NOT_FOUND: ID:FL");
        assert_eq!(path, "/api/functions/Z_MISSING/invoke");
        let desc = op["post"]["description"].as_str().unwrap();
        assert!(desc.contains("FU_NOT_FOUND"), "占位应携带错误信息: {desc}");
    }

    /// v0.13：published 条目 → /api/invokes/{alias} 的平坦 operation。
    #[test]
    fn flat_invoke_operation_builds_typed_flat_contract() {
        use crate::registry::{Entry, EntryOrigin, EntryStatus};
        let entry = Entry {
            alias: "z_calc".into(),
            func_name: "Z_CALC".into(),
            system: "default".into(),
            group: Some("ZMATH".into()),
            intent: "calculator for frontend".into(),
            notes: "integers only".into(),
            doc: "## Usage\n\nPass two integers; get their sum.".into(),
            example: Some(json!({"iv_a": 1})),
            status: EntryStatus::Published,
            origin: EntryOrigin::Manual,
            max_rows: Some(500),
            created_at: "t".into(),
            updated_at: "t".into(),
        };
        let params = vec![
            FunctionParam {
                name: "IV_A".into(),
                type_name: "INT",
                direction: "IMPORT",
                length: 4,
                decimals: 0,
                optional: false,
                default: String::new(),
                description: String::new(),
                fields: None,
            },
            FunctionParam {
                name: "EV_SUM".into(),
                type_name: "INT",
                direction: "EXPORT",
                length: 4,
                decimals: 0,
                optional: true,
                default: String::new(),
                description: String::new(),
                fields: None,
            },
        ];
        let (path, op) = flat_invoke_operation(&entry, &params);
        assert_eq!(path, "/api/invokes/z_calc");
        assert_eq!(op["post"]["operationId"], "invoke_z_calc");
        assert_eq!(op["post"]["summary"], "calculator for frontend");
        let desc = op["post"]["description"].as_str().unwrap();
        assert!(
            desc.contains("integers only") && desc.contains("500"),
            "{desc}"
        );
        assert!(
            desc.contains("## Usage"),
            "doc 正文应流入 description: {desc}"
        );
        // 请求体：IV_A 必填且为 integer
        let body = &op["post"]["requestBody"]["content"]["application/json"]["schema"];
        assert_eq!(body["properties"]["IV_A"]["type"], "integer");
        assert_eq!(body["required"][0], "IV_A");
        // 响应：EV_SUM 平铺 + _truncated 说明
        let resp = &op["post"]["responses"]["200"]["content"]["application/json"]["schema"];
        assert_eq!(resp["properties"]["EV_SUM"]["type"], "integer");
        assert!(resp["properties"].get("_truncated").is_some());
        // limit 默认值来自条目 max_rows
        assert_eq!(op["post"]["parameters"][0]["schema"]["default"], 500);
    }

    #[test]
    fn schema_type_maps_numeric_families() {
        assert_eq!(schema_type_for("INT"), "integer");
        assert_eq!(schema_type_for("INT8"), "integer");
        assert_eq!(schema_type_for("FLOAT"), "number");
        assert_eq!(schema_type_for("BCD"), "string"); // BCD 走字符串语义
        assert_eq!(schema_type_for("CHAR"), "string");
        assert_eq!(schema_type_for("DATE"), "string");
    }
}
