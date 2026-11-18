//! ADT 快搜与 OpenSQL 控制台（sapcli 借鉴批次 v0.21）。
//!
//! 两个轻端点补齐 agent 的「探索」面：
//! - `GET /api/objects/search` —— ADT quick search（`repository/informationsystem/search`），
//!   跨全对象类型按前缀搜（现有 `POST /api/functions/search` 只搜函数模块）；
//! - `POST /api/table/osql` —— ADT 数据预览 freestyle（OpenSQL SELECT 直查），
//!   补 `RFC_READ_TABLE` 的短板（池表拒读、512 字节行宽限制、无 JOIN）。
//!
//! 契约来源：sapcli `sap/adt/search.py` + `sap/adt/datapreview.py`（含单测 fixture）。

use crate::error::RfcError;
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
            "/api/objects/search",
            axum::routing::get(objects_search_handler),
        )
        .route("/api/table/osql", axum::routing::post(table_osql_handler))
}

// ========================================================================
// GET /api/objects/search —— ADT quick search
// ========================================================================

#[derive(serde::Deserialize, Default)]
struct SearchQuery {
    q: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub name: String,
    /// ADT 对象类型（如 `CLAS/OC`、`TTYP/DA`、`PROG/P`）
    pub typ: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 所在包（部分结果带）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// ADT URI（可拿去 `ANY /api/adt/{path}` 直接读对象）
    pub uri: String,
}

/// quick search 默认条数（sapcli 同款 51；单请求浅搜别一次拖死 ICM）。
const SEARCH_DEFAULT_MAX: usize = 51;
const SEARCH_MAX_CAP: usize = 100;

async fn objects_search_handler(
    sap: Sap,
    axum::extract::Query(q): axum::extract::Query<SearchQuery>,
) -> Result<axum::response::Response, RfcError> {
    let term = q.q.as_deref().unwrap_or("").trim();
    if term.is_empty() {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "缺少搜索词：?q=<term>（如 ?q=BAPIRET）".into(),
            key: "SEARCH_QUERY_REQUIRED".into(),
        });
    }
    // sapcli 行为：非 `*` 结尾自动补前缀通配
    let term = if term.ends_with('*') {
        term.to_string()
    } else {
        format!("{term}*")
    };
    let max = q
        .max_results
        .unwrap_or(SEARCH_DEFAULT_MAX)
        .clamp(1, SEARCH_MAX_CAP);

    let adt = sap.adt.as_ref().ok_or_else(crate::adt::adt_disabled)?;
    let resp = adt
        .request_raw(
            axum::http::Method::GET,
            "repository/informationsystem/search",
            &[
                ("operation", "quickSearch"),
                ("maxResults", &max.to_string()),
                ("query", &term),
            ],
            None,
            None,
            "*/*",
            &[],
            None,
            None,
        )
        .await?;
    let body = String::from_utf8_lossy(&resp.body).to_string();
    if !(200..300).contains(&resp.status) {
        return Err(adt_err(resp.status, &body, "ADT quick search 失败"));
    }

    let hits: Vec<SearchHit> = walk_xml(&body)
        .into_iter()
        .filter(|e| e.name == "objectReference")
        .map(|e| SearchHit {
            name: e.attr("name").unwrap_or("").to_string(),
            typ: e.attr("type").unwrap_or("").to_string(),
            description: e.attr("description").map(str::to_string),
            package: e.attr("packageName").map(str::to_string),
            uri: e.attr("uri").unwrap_or("").to_string(),
        })
        .filter(|h| !h.name.is_empty())
        .collect();

    Ok(axum::Json(serde_json::json!({
        "query": term,
        "count": hits.len(),
        "objects": hits,
    }))
    .into_response())
}

// ========================================================================
// POST /api/table/osql —— ADT 数据预览 freestyle（OpenSQL SELECT）
// ========================================================================

#[derive(serde::Deserialize)]
struct OsqlBody {
    /// OpenSQL SELECT 语句（如 `select mandt, cccategory from t000`）
    sql: String,
    /// 最多返回行数（默认 100，上限 5000）
    #[serde(default)]
    rows: Option<usize>,
    /// data aging 开关（HANA 相关，默认 true）
    #[serde(default)]
    aging: Option<bool>,
    /// 本次查询超时秒数（默认沿用全局）
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct OsqlColumn {
    pub name: String,
    /// 服务器给的类型标注（部分系统为空）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typ: Option<String>,
}

const OSQL_DEFAULT_ROWS: usize = 100;
const OSQL_MAX_ROWS: usize = 5000;

async fn table_osql_handler(
    sap: Sap,
    axum::Json(body): axum::Json<OsqlBody>,
) -> Result<axum::response::Response, RfcError> {
    let sql = body.sql.trim();
    if sql.is_empty() {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "sql 必填（OpenSQL SELECT 语句）".into(),
            key: "OSQL_SQL_REQUIRED".into(),
        });
    }
    // 客户端侧软防线（服务端 freestyle 也只认 SELECT）：UPDATE/DELETE/DDL 拒收
    let head = sql.to_ascii_uppercase();
    if !(head.starts_with("SELECT") || head.starts_with("WITH")) {
        return Err(RfcError {
            code: -1,
            status: 400,
            message:
                "本端点只接受 OpenSQL SELECT（WITH CTE 亦可）——写操作请走 BAPI/RFC 或对象写入端点"
                    .into(),
            key: "OSQL_SELECT_ONLY".into(),
        });
    }
    let rows = body
        .rows
        .unwrap_or(OSQL_DEFAULT_ROWS)
        .clamp(1, OSQL_MAX_ROWS);
    let aging = body.aging.unwrap_or(true).to_string();

    let adt = sap.adt.as_ref().ok_or_else(crate::adt::adt_disabled)?;
    let rows_s = rows.to_string();
    let query: [(&str, &str); 2] = [("rowNumber", &rows_s), ("dataAging", &aging)];
    let fut = adt.request_raw(
        axum::http::Method::POST,
        "datapreview/freestyle",
        &query,
        Some(sql.as_bytes()),
        Some("text/plain"),
        "application/xml, application/vnd.sap.adt.datapreview.table.v1+xml",
        &[],
        None,
        None,
    );
    // per-request 超时覆盖：reqwest 客户端超时是端点级的，这里用
    // tokio 超时壳实现单次可配（超时即丢弃连接）
    let resp = match body.timeout_secs {
        Some(secs) => tokio::time::timeout(std::time::Duration::from_secs(secs.max(1)), fut)
            .await
            .map_err(|_| RfcError {
                code: -1,
                status: 504,
                message: format!("OpenSQL 查询超时（{secs}s）——可传 timeout_secs 放宽"),
                key: "OSQL_TIMEOUT".into(),
            })??,
        None => fut.await?,
    };
    let body_xml = String::from_utf8_lossy(&resp.body).to_string();
    if !(200..300).contains(&resp.status) {
        return Err(adt_err(resp.status, &body_xml, "OpenSQL 查询失败"));
    }

    // 列式 XML：columns/metadata(name) 按序出现，data 子元素按序跟随本列
    let mut columns: Vec<OsqlColumn> = Vec::new();
    let mut values: Vec<Vec<String>> = Vec::new();
    let mut current: Option<usize> = None;
    let mut total_rows: Option<usize> = None;
    for e in walk_xml(&body_xml) {
        match e.name.as_str() {
            "totalRows" => {
                total_rows = e.text.trim().parse().ok();
            }
            "metadata" => {
                columns.push(OsqlColumn {
                    name: e.attr("name").unwrap_or("").to_string(),
                    typ: e.attr("type").filter(|s| !s.is_empty()).map(str::to_string),
                });
                values.push(Vec::new());
                current = Some(values.len() - 1);
            }
            "data" if current.is_some() => {
                if let Some(ci) = current {
                    values[ci].push(e.text.trim().to_string());
                }
            }
            _ => {}
        }
    }

    // 列 → 行；空表（无行）时 rows 为空数组
    let nrows = values.first().map(Vec::len).unwrap_or(0);
    let mut rows_json: Vec<serde_json::Map<String, serde_json::Value>> = Vec::with_capacity(nrows);
    for i in 0..nrows {
        let mut m = serde_json::Map::new();
        for (c, col) in columns.iter().enumerate() {
            m.insert(
                col.name.clone(),
                serde_json::Value::String(values[c].get(i).cloned().unwrap_or_default()),
            );
        }
        rows_json.push(m);
    }

    Ok(axum::Json(serde_json::json!({
        "columns": columns,
        "rows": rows_json,
        "row_count": nrows,
        "total_rows": total_rows,
    }))
    .into_response())
}

// ========================================================================
// 共用
// ========================================================================

/// ADT 非成功响应 → RfcError：优先解析 `exc:exception`（SAP 自己的说法），
/// 否则带截断的原始 body。
pub(crate) fn adt_err(status: u16, body: &str, action: &str) -> RfcError {
    if body.contains("exc:exception") {
        let mut err = adt_exception_error(body, action);
        if (400..600).contains(&status) && err.status == 500 {
            err.status = status;
        }
        return err;
    }
    RfcError {
        code: -1,
        status: if status == 200 { 502 } else { status },
        message: format!("{action}（HTTP {status}）: {}", truncate_body(body)),
        key: "ADT_REQUEST_FAILED".into(),
    }
}

fn truncate_body(body: &str) -> String {
    let t = body.trim();
    if t.len() > 300 {
        format!("{}…", &t[..300])
    } else {
        t.to_string()
    }
}

/// 取响应 body 的首个非空行（错误信息里塞原文用，防多行 XML 刷屏）。
pub(crate) fn first_line(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(300)
        .collect()
}
