//! CTS 传输请求管理（sapcli 借鉴批次 v0.21）。
//!
//! ADT 传输组织器通道（`cts/transportrequests`）：创建 / 查看 / 释放 / 删除。
//! 真实开发系统上每个写操作都要挂 transport——本批次之前网关只能**接收**
//! transport 号（objects 写入端点的 `transport` 参数），现在 agent 能自己开单。
//!
//! 契约来源：sapcli `sap/adt/cts.py`（含 `test/unit/fixtures_adt.py` 的
//! tm:request / tm:task / tm:abap_object 属性名）。

use crate::error::RfcError;
use crate::explore::first_line;
use crate::objects::{adt_exception_error, walk_xml};
use crate::systems::Sap;
use axum::response::IntoResponse;
use serde::Serialize;

// ========================================================================
// 路由
// ========================================================================

pub fn router() -> axum::Router<crate::server::SharedState> {
    axum::Router::new()
        .route(
            "/api/cts/requests",
            axum::routing::post(create_request_handler),
        )
        .route(
            "/api/cts/requests/{number}",
            axum::routing::get(get_request_handler).delete(delete_request_handler),
        )
        .route(
            "/api/cts/requests/{number}/release",
            axum::routing::post(release_request_handler),
        )
}

// ========================================================================
// 类型映射（TransportTypes，sapcli 同款）
// ========================================================================

/// 传输请求类型：人类名 → ADT 单字符码
const TRANSPORT_TYPES: &[(&str, &str)] = &[
    ("workbench", "K"),
    ("customizing", "W"),
    ("transport-of-copies", "T"),
    ("development-correction", "S"),
    ("repair", "R"),
];

/// ADT transportorganizer 响应 MIME
const ACCEPT_ORGANIZER: &str = "application/vnd.sap.adt.transportorganizer.v1+xml";

// ========================================================================
// 创建：POST /api/cts/requests
// ========================================================================

#[derive(serde::Deserialize)]
struct CreateBody {
    /// workbench(默认) | customizing | transport-of-copies | development-correction | repair
    #[serde(default)]
    r#type: Option<String>,
    description: String,
    /// 目标系统（workbench 必填）或目标 client（customizing）
    #[serde(default)]
    target: Option<String>,
    /// 请求所有者（缺省 = ADT 登录用户）
    #[serde(default)]
    owner: Option<String>,
}

async fn create_request_handler(
    sap: Sap,
    axum::Json(body): axum::Json<CreateBody>,
) -> Result<axum::response::Response, RfcError> {
    let description = body.description.trim();
    if description.is_empty() {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "description 必填（transport 的短描述）".into(),
            key: "CTS_DESC_REQUIRED".into(),
        });
    }
    let type_human = body.r#type.as_deref().unwrap_or("workbench");
    let type_code = TRANSPORT_TYPES
        .iter()
        .find(|(h, _)| *h == type_human)
        .map(|(_, c)| *c)
        .ok_or_else(|| RfcError {
            code: -1,
            status: 400,
            message: format!(
                "未知传输类型 {type_human:?}——可用: {}",
                TRANSPORT_TYPES
                    .iter()
                    .map(|(h, _)| *h)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            key: "CTS_TYPE_UNKNOWN".into(),
        })?;

    let adt = sap.adt.as_ref().ok_or_else(crate::adt::adt_disabled)?;
    let owner = body
        .owner
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_uppercase)
        .unwrap_or_else(|| adt.user().to_string());
    let target = body.target.as_deref().unwrap_or("").trim();

    let body_xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<tm:root xmlns:tm=\"http://www.sap.com/cts/adt/tm\" tm:useraction=\"newrequest\">\n  \
<tm:request tm:desc=\"{}\" tm:type=\"{}\" tm:target=\"{}\" tm:cts_project=\"\">\n    \
<tm:task tm:owner=\"{}\"/>\n  \
</tm:request>\n\
</tm:root>",
        crate::objects::xml_attr_escape(description),
        type_code,
        crate::objects::xml_attr_escape(target),
        crate::objects::xml_attr_escape(&owner),
    );

    let resp = adt
        .request_raw(
            axum::http::Method::POST,
            "cts/transportrequests",
            &[],
            Some(body_xml.as_bytes()),
            Some("text/plain"),
            ACCEPT_ORGANIZER,
            &[],
            None,
            None,
        )
        .await?;
    let text = String::from_utf8_lossy(&resp.body).to_string();
    if !(200..300).contains(&resp.status) {
        return Err(cts_err(resp.status, &text, "创建传输请求失败"));
    }
    // 响应在属性里带 tm:number（sapcli 靠正则抠；这里走解析器）
    let number = walk_xml(&text)
        .into_iter()
        .find(|e| e.attr("number").is_some())
        .and_then(|e| e.attr("number").map(str::to_string));
    let number = number.ok_or_else(|| RfcError {
        code: -1,
        status: 502,
        message: format!("创建成功但响应里没有 tm:number: {}", first_line(&text)),
        key: "CTS_NUMBER_MISSING".into(),
    })?;

    Ok(axum::Json(serde_json::json!({
        "number": number,
        "type": type_human,
        "type_code": type_code,
        "description": description,
        "target": target,
        "owner": owner,
        "status": "D",
    }))
    .into_response())
}

// ========================================================================
// 查看：GET /api/cts/requests/{number}
// ========================================================================

#[derive(Debug, Serialize)]
pub struct TransportTask {
    pub number: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct TransportRequest {
    pub number: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typ: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// R=已释放 D=开发中 ?=未知
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub released: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<TransportTask>,
}

async fn get_request_handler(
    sap: Sap,
    axum::extract::Path(number): axum::extract::Path<String>,
) -> Result<axum::response::Response, RfcError> {
    let number = validate_number(&number)?;
    let adt = sap.adt.as_ref().ok_or_else(crate::adt::adt_disabled)?;
    let resp = adt
        .request_raw(
            axum::http::Method::GET,
            &format!("cts/transportrequests/{number}"),
            &[],
            None,
            None,
            ACCEPT_ORGANIZER,
            &[],
            None,
            None,
        )
        .await?;
    let text = String::from_utf8_lossy(&resp.body).to_string();
    if !(200..300).contains(&resp.status) {
        return Err(cts_err(resp.status, &text, "读取传输请求失败"));
    }
    let req = parse_transport(&text, &number)?;
    Ok(axum::Json(req).into_response())
}

/// 解析 tm:request/tm:task/tm:abap_object 层级（fixture 契约见 fixtures_adt.py）。
fn parse_transport(text: &str, want: &str) -> Result<TransportRequest, RfcError> {
    let elems = walk_xml(text);
    let req_idx = elems
        .iter()
        .position(|e| e.name == "request" && e.attr("number").map(|n| n == want).unwrap_or(false))
        .ok_or_else(|| RfcError {
            code: -1,
            status: 404,
            message: format!("响应里没有传输请求 {want}（可能已被删除）"),
            key: "CTS_REQUEST_NOT_FOUND".into(),
        })?;
    let req_elem = &elems[req_idx];

    let mut tasks: Vec<TransportTask> = Vec::new();
    for e in &elems[req_idx + 1..] {
        match e.name.as_str() {
            "task" => {
                tasks.push(TransportTask {
                    number: e.attr("number").unwrap_or("").to_string(),
                    owner: e.attr("owner").map(str::to_string),
                    description: e.attr("desc").map(str::to_string),
                    status: e.attr("status").map(str::to_string),
                    objects: Vec::new(),
                });
            }
            "abap_object" if !tasks.is_empty() => {
                let mut m = serde_json::Map::new();
                for k in [
                    "pgmid",
                    "type",
                    "name",
                    "wbtype",
                    "obj_info",
                    "position",
                    "lock_status",
                ] {
                    if let Some(v) = e.attr(k) {
                        m.insert(k.to_string(), serde_json::Value::String(v.to_string()));
                    }
                }
                if let Some(last) = tasks.last_mut() {
                    last.objects.push(serde_json::Value::Object(m));
                }
            }
            _ => {}
        }
    }

    Ok(TransportRequest {
        number: req_elem.attr("number").unwrap_or(want).to_string(),
        typ: req_elem.attr("type").map(str::to_string),
        owner: req_elem.attr("owner").map(str::to_string),
        description: req_elem.attr("desc").map(str::to_string),
        status: req_elem.attr("status").map(str::to_string),
        released: req_elem.attr("status") == Some("R"),
        target: req_elem
            .attr("target")
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        tasks,
    })
}

// ========================================================================
// 释放：POST /api/cts/requests/{number}/release
// ========================================================================

#[derive(serde::Deserialize, Default)]
struct ReleaseQuery {
    #[serde(default)]
    recursive: Option<bool>,
}

/// 释放一个请求（或任务）号：POST {n}/newreleasejobs，checkReport status == "released"。
async fn release_one(
    adt: &crate::adt::AdtEndpoint,
    number: &str,
) -> Result<(bool, String, String), RfcError> {
    let resp = adt
        .request_raw(
            axum::http::Method::POST,
            &format!("cts/transportrequests/{number}/newreleasejobs"),
            &[],
            None,
            None,
            ACCEPT_ORGANIZER,
            &[],
            None,
            None,
        )
        .await?;
    let text = String::from_utf8_lossy(&resp.body).to_string();
    if !(200..300).contains(&resp.status) {
        return Err(cts_err(resp.status, &text, &format!("释放 {number} 失败")));
    }
    let report = walk_xml(&text)
        .into_iter()
        .find(|e| e.name == "checkReport");
    let status = report
        .as_ref()
        .and_then(|e| e.attr("status"))
        .unwrap_or("")
        .to_string();
    let status_text = report
        .as_ref()
        .and_then(|e| e.attr("statusText"))
        .unwrap_or("")
        .to_string();
    Ok((status == "released", status, status_text))
}

async fn release_request_handler(
    sap: Sap,
    axum::extract::Path(number): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<ReleaseQuery>,
) -> Result<axum::response::Response, RfcError> {
    let number = validate_number(&number)?;
    let adt = sap.adt.as_ref().ok_or_else(crate::adt::adt_disabled)?;
    let recursive = q.recursive.unwrap_or(false);

    // recursive：先把未释放的子任务逐个释放（sapcli _release_children 同序）
    let mut task_results: Vec<serde_json::Value> = Vec::new();
    if recursive {
        let req = fetch_transport(adt, &number).await?;
        for task in &req.tasks {
            if task.status.as_deref() == Some("R") {
                continue;
            }
            let (ok, status, status_text) = release_one(adt, &task.number).await?;
            task_results.push(serde_json::json!({
                "number": task.number,
                "released": ok,
                "status": status,
                "status_text": status_text,
            }));
        }
    }
    let (ok, status, status_text) = release_one(adt, &number).await?;

    Ok(axum::Json(serde_json::json!({
        "number": number,
        "released": ok,
        "status": status,
        "status_text": status_text,
        "tasks": task_results,
    }))
    .into_response())
}

// ========================================================================
// 删除：DELETE /api/cts/requests/{number}
// ========================================================================

async fn delete_request_handler(
    sap: Sap,
    axum::extract::Path(number): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<ReleaseQuery>,
) -> Result<axum::response::Response, RfcError> {
    let number = validate_number(&number)?;
    let adt = sap.adt.as_ref().ok_or_else(crate::adt::adt_disabled)?;
    let recursive = q.recursive.unwrap_or(false);

    let mut task_results: Vec<serde_json::Value> = Vec::new();
    if recursive {
        // 先取当前任务清单再删（删除请求本体后任务随行，但显式删除子任务
        // 是 sapcli recursive 的顺序；任务号删除失败只记警告继续）
        let req = fetch_transport(adt, &number).await?;
        for task in &req.tasks {
            match adt
                .request_raw(
                    axum::http::Method::DELETE,
                    &format!("cts/transportrequests/{}", task.number),
                    &[],
                    None,
                    None,
                    ACCEPT_ORGANIZER,
                    &[],
                    None,
                    None,
                )
                .await
            {
                Ok(r) if (200..300).contains(&r.status) => {
                    task_results.push(serde_json::json!({"number": task.number, "deleted": true}));
                }
                Ok(r) => {
                    let text = String::from_utf8_lossy(&r.body).to_string();
                    task_results.push(serde_json::json!({
                        "number": task.number, "deleted": false,
                        "error": first_line(&text),
                    }));
                }
                Err(e) => {
                    task_results.push(serde_json::json!({
                        "number": task.number, "deleted": false, "error": e.message,
                    }));
                }
            }
        }
    }

    let resp = adt
        .request_raw(
            axum::http::Method::DELETE,
            &format!("cts/transportrequests/{number}"),
            &[],
            None,
            None,
            ACCEPT_ORGANIZER,
            &[],
            None,
            None,
        )
        .await?;
    if !(200..300).contains(&resp.status) {
        let text = String::from_utf8_lossy(&resp.body).to_string();
        return Err(cts_err(resp.status, &text, "删除传输请求失败"));
    }

    Ok(axum::Json(serde_json::json!({
        "number": number,
        "deleted": true,
        "tasks": task_results,
    }))
    .into_response())
}

// ========================================================================
// 共用
// ========================================================================

async fn fetch_transport(
    adt: &crate::adt::AdtEndpoint,
    number: &str,
) -> Result<TransportRequest, RfcError> {
    let resp = adt
        .request_raw(
            axum::http::Method::GET,
            &format!("cts/transportrequests/{number}"),
            &[],
            None,
            None,
            ACCEPT_ORGANIZER,
            &[],
            None,
            None,
        )
        .await?;
    let text = String::from_utf8_lossy(&resp.body).to_string();
    if !(200..300).contains(&resp.status) {
        return Err(cts_err(resp.status, &text, "读取传输请求失败"));
    }
    parse_transport(&text, number)
}

/// 传输请求号白名单：字母数字（如 A4HK900123 / ED1K900123 / K0242AC110002...）
fn validate_number(n: &str) -> Result<String, RfcError> {
    let n = n.trim();
    if n.is_empty() || n.len() > 32 || !n.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: format!("非法传输请求号 {n:?}（字母数字，≤32 字符）"),
            key: "CTS_NUMBER_INVALID".into(),
        });
    }
    Ok(n.to_string())
}

/// CTS 非成功响应 → RfcError。ExceptionResourceNotFound（已删除的请求号）
/// 特判 404——共享解析器把它映成 409（objects 侧锁冲突语义），CTS 这边
/// 「请求不存在」就是 404。
pub(crate) fn cts_err(status: u16, body: &str, action: &str) -> RfcError {
    if body.contains("exc:exception") {
        let mut err = adt_exception_error(body, action);
        if body.contains("ExceptionResourceNotFound") {
            err.status = 404;
        }
        return err;
    }
    RfcError {
        code: -1,
        status: if status == 200 { 502 } else { status },
        message: format!("{action}（HTTP {status}）: {}", first_line(body)),
        key: "CTS_REQUEST_FAILED".into(),
    }
}
