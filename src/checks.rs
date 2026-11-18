//! ABAP Unit / ATC 运行器（sapcli 借鉴批次 v0.21）。
//!
//! 两个「跑起来看结果」的异步编排，都是 POST 建单 → 取回 ID → 取结果的三步流：
//! - `POST /api/tests/aunit` —— ABAP Unit（`abapunit/runs` 异步 API，非旧同步协议）；
//! - `POST /api/atc/run` —— ATC 代码检查（`atc/worklists` 建单 → `atc/runs` 执行 → 取 findings）。
//!
//! 契约来源：sapcli `sap/adt/api/aunit.py` + `sap/adt/atc.py`，请求/响应 XML
//! 逐字段对齐 `test/unit/fixtures_adt_aunit.py` / `fixtures_adt_atc.py`。
//!
//! 轮询决策：**不用 `withLongPolling=true`**——长轮询由服务器挂住连接，
//! 可能撞 ADT 端点的 reqwest 客户端超时；改 1s 间隔短轮询（结果等价）。

use crate::error::RfcError;
use crate::explore::{adt_err, first_line};
use crate::objects::walk_xml;
use crate::systems::Sap;
use axum::response::IntoResponse;
use serde::Serialize;
use std::time::{Duration, Instant};

// ========================================================================
// 路由
// ========================================================================

pub fn router() -> axum::Router<crate::server::SharedState> {
    axum::Router::new()
        .route("/api/tests/aunit", axum::routing::post(aunit_run_handler))
        .route("/api/atc/run", axum::routing::post(atc_run_handler))
}

// ========================================================================
// 共用：对象集描述
// ========================================================================

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ObjectRef {
    /// class | program | interface | fugr | func | package
    pub r#type: String,
    pub name: String,
}

impl ObjectRef {
    pub fn upper_name(&self) -> String {
        self.name.trim().to_uppercase()
    }
}

/// aunit 的 OSL 类型码（sapcli OBJECT_TYPE_MAP）
fn osl_type(t: &str) -> Option<&'static str> {
    match t {
        "class" => Some("CLAS"),
        "program" => Some("PROG"),
        "interface" => Some("INTF"),
        "function-group" | "fugr" => Some("FUGR"),
        "function-module" | "func" => Some("FUNC"),
        _ => None,
    }
}

// ========================================================================
// POST /api/tests/aunit —— ABAP Unit
// ========================================================================

#[derive(serde::Deserialize)]
struct AunitBody {
    objects: Vec<ObjectRef>,
    /// json(默认，解析 ABAPUnit XML) | junit4(原样透传) | raw(原样透传)
    #[serde(default)]
    format: Option<String>,
    /// 整个 run（建单+轮询+取结果）的超时秒数，默认 120
    #[serde(default)]
    timeout_secs: Option<u64>,
}

const AUNIT_RUN_CONTENT_TYPE: &str = "application/vnd.sap.adt.api.abapunit.run.v2+xml";
const ACCEPT_AUNIT_RESULTS: &str = "application/vnd.sap.adt.api.abapunit.run-result.v1+xml";
const ACCEPT_JUNIT_RESULTS: &str = "application/vnd.sap.adt.api.junit.run-result.v1+xml";
const AUNIT_DEFAULT_TIMEOUT: u64 = 120;

async fn aunit_run_handler(
    sap: Sap,
    axum::Json(body): axum::Json<AunitBody>,
) -> Result<axum::response::Response, RfcError> {
    let format = body.format.as_deref().unwrap_or("json");
    if !matches!(format, "json" | "junit4" | "raw") {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: format!("format 只支持 json|junit4|raw，收到 {format:?}"),
            key: "AUNIT_FORMAT_UNKNOWN".into(),
        });
    }
    if body.objects.is_empty() {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "objects 必填（至少一个 {type,name}；package 与普通类型不可混用）".into(),
            key: "AUNIT_OBJECTS_REQUIRED".into(),
        });
    }
    let has_package = body.objects.iter().any(|o| o.r#type == "package");
    if has_package && body.objects.iter().any(|o| o.r#type != "package") {
        // sapcli 的包语义：出现 package 就把**全部**条目当包名——容易悄悄跑错，
        // 网关直接拒绝混合，宁可响一点
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "package 与普通类型不可混用：包跑一次，普通对象分开跑".into(),
            key: "AUNIT_MIXED_OBJECTS".into(),
        });
    }

    let deadline = Instant::now()
        + Duration::from_secs(body.timeout_secs.unwrap_or(AUNIT_DEFAULT_TIMEOUT).max(1));
    let adt = sap.adt.as_ref().ok_or_else(crate::adt::adt_disabled)?;

    // ① 建单 → Location: …/abapunit/runs/{id}
    let xml = build_aunit_run_xml(&body.objects)?;
    let resp = adt
        .request_raw(
            axum::http::Method::POST,
            "abapunit/runs",
            &[],
            Some(xml.as_bytes()),
            Some(AUNIT_RUN_CONTENT_TYPE),
            "*/*",
            &[],
            None,
            None,
        )
        .await?;
    let text = String::from_utf8_lossy(&resp.body).to_string();
    if !(200..300).contains(&resp.status) {
        return Err(adt_err(resp.status, &text, "创建 ABAP Unit run 失败"));
    }
    let run_id = resp
        .location
        .as_deref()
        .and_then(|l| l.rsplit('/').next())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| RfcError {
            code: -1,
            status: 502,
            message: format!("ABAP Unit run 响应缺 Location 头: {}", first_line(&text)),
            key: "AUNIT_RUN_ID_MISSING".into(),
        })?;

    // ② 轮询（1s 间隔短轮询，受 deadline 约束）
    loop {
        if Instant::now() >= deadline {
            return Err(RfcError {
                code: -1,
                status: 504,
                message: format!("ABAP Unit run {run_id} 超时（未在期限内跑完）"),
                key: "AUNIT_TIMEOUT".into(),
            });
        }
        let resp = adt
            .request_raw(
                axum::http::Method::GET,
                &format!("abapunit/runs/{run_id}"),
                &[],
                None,
                None,
                "*/*",
                &[],
                None,
                None,
            )
            .await?;
        let text = String::from_utf8_lossy(&resp.body).to_string();
        if !(200..300).contains(&resp.status) {
            return Err(adt_err(resp.status, &text, "轮询 ABAP Unit run 失败"));
        }
        let progress = walk_xml(&text).into_iter().find(|e| e.name == "progress");
        let status = progress
            .as_ref()
            .and_then(|e| e.attr("status"))
            .unwrap_or("");
        if status.eq_ignore_ascii_case("FINISHED") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }

    // ③ 取结果：json/解析 ABAPUnit XML；junit4/raw 原样透传
    let accept = if format == "junit4" {
        ACCEPT_JUNIT_RESULTS
    } else {
        ACCEPT_AUNIT_RESULTS
    };
    let resp = adt
        .request_raw(
            axum::http::Method::GET,
            &format!("abapunit/results/{run_id}"),
            &[],
            None,
            None,
            accept,
            &[],
            None,
            None,
        )
        .await?;
    let text = String::from_utf8_lossy(&resp.body).to_string();
    if !(200..300).contains(&resp.status) {
        return Err(adt_err(resp.status, &text, "获取 ABAP Unit 结果失败"));
    }
    if format == "raw" {
        return xml_response(&text, ACCEPT_AUNIT_RESULTS);
    }
    if format == "junit4" {
        return xml_response(&text, ACCEPT_JUNIT_RESULTS);
    }
    let parsed = parse_aunit_results(&text)?;
    Ok(axum::Json(serde_json::json!({
        "run_id": run_id,
        "summary": parsed.summary,
        "programs": parsed.programs,
        "alerts": parsed.alerts_flat,
    }))
    .into_response())
}

/// 构造 aunit run 请求体（对齐 sapcli build_test_run 的序列化形态）。
fn build_aunit_run_xml(objects: &[ObjectRef]) -> Result<String, RfcError> {
    let mut set = String::new();
    let has_package = objects.iter().any(|o| o.r#type == "package");
    if has_package {
        set.push_str("<osl:objectSet xmlns:osl=\"http://www.sap.com/api/osl\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:type=\"osl:multiPropertySet\">");
        for o in objects {
            let name = o.upper_name();
            if name.is_empty() {
                return Err(RfcError {
                    code: -1,
                    status: 400,
                    message: "objects[].name 不能为空".into(),
                    key: "AUNIT_NAME_REQUIRED".into(),
                });
            }
            set.push_str(&format!(
                "<osl:property key=\"package\" value=\"{}\"/>",
                crate::objects::xml_attr_escape(&name)
            ));
        }
    } else {
        set.push_str("<osl:objectSet xmlns:osl=\"http://www.sap.com/api/osl\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:type=\"osl:flatObjectSet\">");
        for o in objects {
            let code = osl_type(&o.r#type).ok_or_else(|| RfcError {
                code: -1,
                status: 400,
                message: format!(
                    "未知对象类型 {:?}——可用 class|program|interface|fugr|func|package",
                    o.r#type
                ),
                key: "AUNIT_TYPE_UNKNOWN".into(),
            })?;
            set.push_str(&format!(
                "<osl:object name=\"{}\" type=\"{}\"/>",
                crate::objects::xml_attr_escape(&o.upper_name()),
                code
            ));
        }
    }
    set.push_str("</osl:objectSet>");

    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<aunit:run xmlns:aunit=\"http://www.sap.com/adt/api/aunit\" title=\"Run\" context=\"ABAP Unit Test Run\">\n\
<aunit:options>\n\
<aunit:scope ownTests=\"true\" foreignTests=\"true\" addForeignTestsAsPreview=\"true\"/>\n\
<aunit:riskLevel harmless=\"true\" dangerous=\"true\" critical=\"true\"/>\n\
<aunit:duration short=\"true\" medium=\"true\" long=\"true\"/>\n\
</aunit:options>\n{set}\n</aunit:run>"
    ))
}

// ========================================================================
// ABAPUnit 结果 XML → JSON
// ========================================================================

#[derive(Debug, Serialize)]
pub struct AunitAlert {
    pub kind: String,
    pub severity: String,
    pub title: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stack: Vec<String>,
    // 归属（扁平视图用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}

struct AunitParsed {
    summary: serde_json::Value,
    programs: serde_json::Value,
    alerts_flat: Vec<AunitAlert>,
}

/// 解析 `<aunit:runResult>` 嵌套：program → testClasses → testClass →
/// testMethods → testMethod → alerts → alert{title, details, stack}。
fn parse_aunit_results(xml: &str) -> Result<AunitParsed, RfcError> {
    let programs: Vec<serde_json::Value> = Vec::new();
    let mut flat: Vec<AunitAlert> = Vec::new();
    let mut tests = 0usize;
    let mut alert_count = 0usize;
    let mut duration = 0f64;

    // 当前归属游标（文档顺序遍历）
    let mut cur_program: Option<String> = None;
    let mut cur_class: Option<String> = None;
    let mut cur_method: Option<(String, Option<f64>)> = None;
    let mut cur_alert: Option<AunitAlert> = None;
    let mut in_alerts = false;

    let mut program_nodes: Vec<serde_json::Value> = Vec::new();
    let mut class_nodes: Vec<serde_json::Value> = Vec::new();
    let mut method_nodes: Vec<serde_json::Value> = Vec::new();

    for e in walk_xml(xml) {
        match e.name.as_str() {
            "program" => {
                flush_alert(&mut cur_alert, &mut method_nodes, &mut flat);
                // 新 program：把攒下的类挂进去，重置游标
                if !class_nodes.is_empty() || cur_program.is_some() {
                    program_nodes.push(serde_json::json!({
                        "name": cur_program.clone().unwrap_or_default(),
                        "test_classes": std::mem::take(&mut class_nodes),
                    }));
                }
                cur_program = Some(e.attr("name").unwrap_or("").to_string());
                cur_class = None;
                cur_method = None;
                in_alerts = false;
            }
            "testClass" => {
                flush_alert(&mut cur_alert, &mut method_nodes, &mut flat);
                if let Some(cls) = cur_class.take() {
                    class_nodes.push(flush_class(cls, &mut method_nodes));
                }
                cur_class = Some(e.attr("name").unwrap_or("").to_string());
                cur_method = None;
                in_alerts = false;
            }
            "testMethod" => {
                flush_alert(&mut cur_alert, &mut method_nodes, &mut flat);
                tests += 1;
                let secs = e.attr("executionTime").and_then(|v| v.parse::<f64>().ok());
                if let Some(s) = secs {
                    duration += s;
                }
                cur_method = Some((e.attr("name").unwrap_or("").to_string(), secs));
                method_nodes.push(serde_json::json!({
                    "name": cur_method.as_ref().unwrap().0,
                    "execution_time": secs,
                    "alerts": [],
                }));
                in_alerts = false;
            }
            "alerts" => in_alerts = true,
            "alert" if in_alerts => {
                flush_alert(&mut cur_alert, &mut method_nodes, &mut flat);
                alert_count += 1;
                cur_alert = Some(AunitAlert {
                    kind: e.attr("kind").unwrap_or("").to_string(),
                    severity: e.attr("severity").unwrap_or("").to_string(),
                    title: String::new(),
                    details: Vec::new(),
                    stack: Vec::new(),
                    program: cur_program.clone(),
                    test_class: cur_class.clone(),
                    method: cur_method.as_ref().map(|(n, _)| n.clone()),
                });
            }
            "title" if cur_alert.is_some() => {
                if let Some(a) = cur_alert.as_mut() {
                    a.title = e.text.clone();
                }
            }
            "detail" if cur_alert.is_some() => {
                if let Some(a) = cur_alert.as_mut() {
                    if let Some(t) = e.attr("text") {
                        a.details.push(t.to_string());
                    }
                }
            }
            "stackEntry" if cur_alert.is_some() => {
                if let Some(a) = cur_alert.as_mut() {
                    if let Some(d) = e.attr("description") {
                        a.stack.push(d.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    // 收尾：最后一个 alert / 类 / program 游标落位
    flush_alert(&mut cur_alert, &mut method_nodes, &mut flat);
    if let Some(cls) = cur_class.take() {
        class_nodes.push(flush_class(cls, &mut method_nodes));
    }
    if !class_nodes.is_empty() || cur_program.is_some() {
        program_nodes.push(serde_json::json!({
            "name": cur_program.clone().unwrap_or_default(),
            "test_classes": class_nodes,
        }));
    }

    let by_severity: serde_json::Map<String, serde_json::Value> = {
        let mut m = serde_json::Map::new();
        for sev in ["critical", "fatal", "nonFatal", "tolerable"] {
            let n = flat.iter().filter(|a| a.severity == sev).count();
            if n > 0 {
                m.insert(sev.to_string(), serde_json::json!(n));
            }
        }
        m
    };

    Ok(AunitParsed {
        summary: serde_json::json!({
            "programs": programs.len(),
            "test_classes": flat.iter().filter_map(|a| a.test_class.clone()).collect::<Vec<_>>().len(),
            "tests": tests,
            "alerts": alert_count,
            "duration_secs": (duration * 1000.0).round() / 1000.0,
            "alerts_by_severity": by_severity,
        }),
        programs: serde_json::Value::Array(program_nodes),
        alerts_flat: flat,
    })
}

fn flush_class(name: String, methods: &mut Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "methods": std::mem::take(methods),
    })
}

fn attach_alert(a: AunitAlert, method_nodes: &mut [serde_json::Value], flat: &mut Vec<AunitAlert>) {
    let v = serde_json::to_value(&a).unwrap_or(serde_json::Value::Null);
    // 挂到当前 method 的 alerts（无 method 的系统级 alert 只进扁平视图）
    if let (Some(last), serde_json::Value::Object(obj)) = (method_nodes.last_mut(), v) {
        if let Some(alerts) = last.get_mut("alerts") {
            if let Some(arr) = alerts.as_array_mut() {
                arr.push(serde_json::Value::Object(
                    obj.iter()
                        .filter(|(k, _)| {
                            k.as_str() != "program"
                                && k.as_str() != "test_class"
                                && k.as_str() != "method"
                        })
                        .map(|(k, val)| (k.clone(), val.clone()))
                        .collect(),
                ));
            }
        }
    }
    flat.push(a);
}

/// 把 pending alert 落到当前 method（换 method/class/program 边界必须先冲，
/// 否则上个方法的失败会挂到新方法头上）。
fn flush_alert(
    cur_alert: &mut Option<AunitAlert>,
    method_nodes: &mut [serde_json::Value],
    flat: &mut Vec<AunitAlert>,
) {
    if let Some(a) = cur_alert.take() {
        attach_alert(a, method_nodes, flat);
    }
}

fn xml_response(text: &str, content_type: &str) -> Result<axum::response::Response, RfcError> {
    let ct = axum::http::HeaderValue::from_str(content_type).map_err(|_| RfcError {
        code: -1,
        status: 500,
        message: "Content-Type 编码失败".into(),
        key: "CT_ENCODE".into(),
    })?;
    let mut resp = axum::response::Response::new(axum::body::Body::from(text.to_string()));
    resp.headers_mut()
        .insert(axum::http::header::CONTENT_TYPE, ct);
    Ok(resp)
}

// ========================================================================
// POST /api/atc/run —— ATC 代码检查
// ========================================================================

#[derive(serde::Deserialize)]
struct AtcBody {
    objects: Vec<ObjectRef>,
    /// 检查变体（缺省 → 读 atc/customizing 的 systemCheckVariant）
    #[serde(default)]
    variant: Option<String>,
    /// maximumVerdicts（默认 100）
    #[serde(default)]
    max_verdicts: Option<u32>,
    /// 结果是否包含已豁免 findings（默认不含）
    #[serde(default)]
    include_exempted: Option<bool>,
}

async fn atc_run_handler(
    sap: Sap,
    axum::Json(body): axum::Json<AtcBody>,
) -> Result<axum::response::Response, RfcError> {
    if body.objects.is_empty() {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "objects 必填（至少一个 {type,name}）".into(),
            key: "ATC_OBJECTS_REQUIRED".into(),
        });
    }
    let max_verdicts = body.max_verdicts.unwrap_or(100).clamp(1, 10_000);
    let adt = sap.adt.as_ref().ok_or_else(crate::adt::adt_disabled)?;

    // ① 变体：显式 > customizing
    let variant = match body
        .variant
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(v) => v.to_string(),
        None => {
            let resp = adt
                .request_raw(
                    axum::http::Method::GET,
                    "atc/customizing",
                    &[],
                    None,
                    None,
                    "application/xml, application/vnd.sap.atc.customizing-v1+xml",
                    &[],
                    None,
                    None,
                )
                .await?;
            let text = String::from_utf8_lossy(&resp.body).to_string();
            if !(200..300).contains(&resp.status) {
                return Err(adt_err(resp.status, &text, "读取 ATC customizing 失败"));
            }
            walk_xml(&text)
                .into_iter()
                .find(|e| e.name == "property" && e.attr("name") == Some("systemCheckVariant"))
                .and_then(|e| e.attr("value").map(str::to_string))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| RfcError {
                    code: -1,
                    status: 400,
                    message: "系统未配置默认检查变体——请显式传 variant（如 \"DEFAULT\"）".into(),
                    key: "ATC_VARIANT_REQUIRED".into(),
                })?
        }
    };

    // ② 建单：POST atc/worklists?checkVariant={v}（响应体就是 worklist id）
    let resp = adt
        .request_raw(
            axum::http::Method::POST,
            "atc/worklists",
            &[("checkVariant", &variant)],
            None,
            None,
            "text/plain",
            &[],
            None,
            None,
        )
        .await?;
    let text = String::from_utf8_lossy(&resp.body).to_string();
    if !(200..300).contains(&resp.status) {
        return Err(adt_err(resp.status, &text, "创建 ATC worklist 失败"));
    }
    let worklist_id = text.trim().to_string();
    if worklist_id.is_empty() {
        return Err(RfcError {
            code: -1,
            status: 502,
            message: "ATC worklist 创建响应为空".into(),
            key: "ATC_WORKLIST_ID_MISSING".into(),
        });
    }

    // ③ 执行：POST atc/runs?worklistId={id}
    let run_xml = build_atc_run_xml(&body.objects, max_verdicts)?;
    let resp = adt
        .request_raw(
            axum::http::Method::POST,
            "atc/runs",
            &[("worklistId", &worklist_id)],
            Some(run_xml.as_bytes()),
            Some("application/xml"),
            "application/xml",
            &[],
            None,
            None,
        )
        .await?;
    let text = String::from_utf8_lossy(&resp.body).to_string();
    if !(200..300).contains(&resp.status) {
        return Err(adt_err(resp.status, &text, "执行 ATC 检查失败"));
    }
    // worklistRun 里的 infos（NO_OBJS / TOOL_FAILURE 等）保留透出
    let run_infos = parse_run_infos(&text);

    // ④ 取结果
    let include_exempted = body.include_exempted.unwrap_or(false).to_string();
    let resp = adt
        .request_raw(
            axum::http::Method::GET,
            &format!("atc/worklists/{worklist_id}"),
            &[("includeExemptedFindings", &include_exempted)],
            None,
            None,
            "application/atc.worklist.v1+xml",
            &[],
            None,
            None,
        )
        .await?;
    let text = String::from_utf8_lossy(&resp.body).to_string();
    if !(200..300).contains(&resp.status) {
        return Err(adt_err(resp.status, &text, "读取 ATC worklist 失败"));
    }
    let (objects, total, by_priority) = parse_atc_worklist(&text);

    Ok(axum::Json(serde_json::json!({
        "variant": variant,
        "worklist_id": worklist_id,
        "objects": objects,
        "total_findings": total,
        "by_priority": by_priority,
        "infos": run_infos,
    }))
    .into_response())
}

/// 构造 atc:run 请求（对齐 fixtures_adt_atc.py 的 ADT_XML_ATC_RUN_REQUEST_*）。
fn build_atc_run_xml(objects: &[ObjectRef], max_verdicts: u32) -> Result<String, RfcError> {
    let mut refs = String::new();
    for o in objects {
        let name = o.name.trim();
        if name.is_empty() {
            return Err(RfcError {
                code: -1,
                status: 400,
                message: "objects[].name 不能为空".into(),
                key: "ATC_NAME_REQUIRED".into(),
            });
        }
        let lower = name.to_lowercase();
        let uri = match o.r#type.as_str() {
            "class" => format!("oo/classes/{lower}"),
            "interface" => format!("oo/interfaces/{lower}"),
            "program" => format!("programs/programs/{lower}"),
            "fugr" | "function-group" => format!("functions/groups/{lower}"),
            "package" => format!("packages/{}", crate::adt::encode_path_segment(&lower)),
            other => {
                return Err(RfcError {
                    code: -1,
                    status: 400,
                    message: format!(
                        "ATC 暂不支持对象类型 {other:?}——可用 class|interface|program|fugr|package"
                    ),
                    key: "ATC_TYPE_UNKNOWN".into(),
                })
            }
        };
        refs.push_str(&format!(
            "<adtcore:objectReference adtcore:uri=\"/sap/bc/adt/{uri}\" adtcore:name=\"{}\"/>",
            crate::objects::xml_attr_escape(&name.to_uppercase())
        ));
    }

    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<atc:run xmlns:atc=\"http://www.sap.com/adt/atc\" maximumVerdicts=\"{max_verdicts}\">\n\
<objectSets xmlns:adtcore=\"http://www.sap.com/adt/core\">\n\
<objectSet kind=\"inclusive\">\n\
<adtcore:objectReferences>{refs}</adtcore:objectReferences>\n\
</objectSet>\n\
</objectSets>\n\
</atc:run>"
    ))
}

/// worklistRun 响应里的 infos（`<atcinfo:info><atcinfo:type>…` 子元素取文本）。
fn parse_run_infos(xml: &str) -> Vec<serde_json::Value> {
    let mut infos: Vec<serde_json::Value> = Vec::new();
    for e in walk_xml(xml) {
        match e.name.as_str() {
            "info" => infos.push(serde_json::json!({"type": "", "description": ""})),
            "type" if e.parent_is("info") => {
                if let Some(last) = infos.last_mut() {
                    last["type"] = serde_json::Value::String(e.text.clone());
                }
            }
            "description" if e.parent_is("info") => {
                if let Some(last) = infos.last_mut() {
                    last["description"] = serde_json::Value::String(e.text.clone());
                }
            }
            _ => {}
        }
    }
    infos.retain(|i| {
        i["type"] != serde_json::json!("") || i["description"] != serde_json::json!("")
    });
    infos
}

/// 解析 worklist：objects → findings；返回 (objects, total, by_priority)。
fn parse_atc_worklist(xml: &str) -> (Vec<serde_json::Value>, usize, serde_json::Value) {
    let elems = walk_xml(xml);
    let mut objects: Vec<serde_json::Value> = Vec::new();
    let mut cur_object: Option<serde_json::Map<String, serde_json::Value>> = None;
    let mut cur_findings: Vec<serde_json::Value> = Vec::new();
    let mut total = 0usize;
    let mut prios: std::collections::BTreeMap<String, usize> = Default::default();

    for e in &elems {
        match e.name.as_str() {
            "object" => {
                if let Some(mut o) = cur_object.take() {
                    o.insert(
                        "findings".into(),
                        serde_json::Value::Array(std::mem::take(&mut cur_findings)),
                    );
                    objects.push(serde_json::Value::Object(o));
                }
                let mut o = serde_json::Map::new();
                for k in ["uri", "type", "name", "packageName", "author"] {
                    if let Some(v) = e.attr(k) {
                        o.insert(k.to_string(), serde_json::Value::String(v.to_string()));
                    }
                }
                cur_object = Some(o);
            }
            "finding" => {
                total += 1;
                let prio = e.attr("priority").unwrap_or("").to_string();
                if !prio.is_empty() {
                    *prios.entry(prio.clone()).or_default() += 1;
                }
                let mut f = serde_json::Map::new();
                for k in [
                    "location",
                    "priority",
                    "checkId",
                    "checkTitle",
                    "messageId",
                    "messageTitle",
                    "exemptionApproval",
                    "exemptionKind",
                ] {
                    if let Some(v) = e.attr(k) {
                        f.insert(k.to_string(), serde_json::Value::String(v.to_string()));
                    }
                }
                cur_findings.push(serde_json::Value::Object(f));
            }
            _ => {}
        }
    }
    if let Some(mut o) = cur_object.take() {
        o.insert("findings".into(), serde_json::Value::Array(cur_findings));
        objects.push(serde_json::Value::Object(o));
    }

    let by_priority: serde_json::Map<String, serde_json::Value> = prios
        .into_iter()
        .map(|(k, v)| (k, serde_json::json!(v)))
        .collect();
    (objects, total, serde_json::Value::Object(by_priority))
}

// ========================================================================
// 单测：解析器 + XML 构造（fixture 对齐 sapcli test/unit/fixtures_adt_*.py）
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// sapcli fixtures_adt_aunit.py 的 AUNIT_RESULTS_XML（截取结构完整的骨架）。
    /// 一个 program、两个 testClass、三个 method（fail/warn/pass）、
    /// 失败方法带 alert{title, details, stack}。
    const AUNIT_RESULTS_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<aunit:runResult xmlns:aunit="http://www.sap.com/adt/aunit">
  <program adtcore:uri="/sap/bc/adt/oo/classes/zcl_hardcore" adtcore:type="CLAS/OC" adtcore:name="ZCL_HARDCORE" uriType="semantic" xmlns:adtcore="http://www.sap.com/adt/core">
    <testClasses>
      <testClass adtcore:name="LTCL_TEST" durationCategory="short" riskLevel="harmless" xmlns:adtcore="http://www.sap.com/adt/core">
        <testMethods>
          <testMethod adtcore:name="DO_THE_FAIL" executionTime="0.033" unit="s" xmlns:adtcore="http://www.sap.com/adt/core">
            <alerts>
              <alert kind="failedAssertion" severity="critical">
                <title>Critical Assertion Error: 'I am supposed to fail'</title>
                <details>
                  <detail text="True expected"/>
                </details>
                <stack>
                  <stackEntry adtcore:description="Include: ZCL_HARDCORE===CCAU Line: 19" xmlns:adtcore="http://www.sap.com/adt/core"/>
                </stack>
              </alert>
            </alerts>
          </testMethod>
          <testMethod adtcore:name="DO_THE_WARN" executionTime="0.033" unit="s" xmlns:adtcore="http://www.sap.com/adt/core">
            <alerts>
              <alert kind="failedAssertion" severity="tolerable">
                <title>Warning: 'I am supposed to warn'</title>
              </alert>
            </alerts>
          </testMethod>
          <testMethod adtcore:name="DO_THE_TEST" executionTime="0" unit="s" xmlns:adtcore="http://www.sap.com/adt/core"/>
        </testMethods>
      </testClass>
    </testClasses>
  </program>
</aunit:runResult>"#;

    #[test]
    fn aunit_results_parse_counts_and_alerts() {
        let parsed = parse_aunit_results(AUNIT_RESULTS_XML).expect("parse");
        let s = &parsed.summary;
        assert_eq!(s["tests"], serde_json::json!(3));
        assert_eq!(s["alerts"], serde_json::json!(2));
        assert_eq!(s["duration_secs"], serde_json::json!(0.066));
        // 扁平视图：归属字段齐全
        assert_eq!(parsed.alerts_flat.len(), 2);
        let fail = &parsed.alerts_flat[0];
        assert_eq!(fail.severity, "critical");
        assert_eq!(fail.program.as_deref(), Some("ZCL_HARDCORE"));
        assert_eq!(fail.test_class.as_deref(), Some("LTCL_TEST"));
        assert_eq!(fail.method.as_deref(), Some("DO_THE_FAIL"));
        assert_eq!(fail.details, vec!["True expected".to_string()]);
        assert_eq!(fail.stack.len(), 1);
        // 嵌套视图：program → class → method → alerts
        let programs = parsed.programs.as_array().expect("programs array");
        assert_eq!(programs.len(), 1);
        assert_eq!(programs[0]["name"], "ZCL_HARDCORE");
        let classes = programs[0]["test_classes"].as_array().unwrap();
        assert_eq!(classes.len(), 1);
        let methods = classes[0]["methods"].as_array().unwrap();
        assert_eq!(methods.len(), 3);
        // 失败挂在它自己的方法上（跨方法边界不泄漏）——warn 在 method[1]
        assert_eq!(methods[0]["alerts"].as_array().unwrap().len(), 1);
        assert_eq!(methods[0]["alerts"][0]["severity"], "critical");
        assert_eq!(methods[1]["alerts"][0]["severity"], "tolerable");
        assert_eq!(methods[2]["alerts"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn aunit_xml_builder_matches_sapcli_shape() {
        let objs = vec![
            ObjectRef {
                r#type: "class".into(),
                name: "cl_foo".into(),
            },
            ObjectRef {
                r#type: "program".into(),
                name: "zprog".into(),
            },
        ];
        let xml = build_aunit_run_xml(&objs).unwrap();
        assert!(xml.contains(r#"<aunit:run xmlns:aunit="http://www.sap.com/adt/api/aunit" title="Run" context="ABAP Unit Test Run">"#));
        assert!(xml.contains(
            r#"<aunit:scope ownTests="true" foreignTests="true" addForeignTestsAsPreview="true"/>"#
        ));
        assert!(
            xml.contains(r#"<aunit:riskLevel harmless="true" dangerous="true" critical="true"/>"#)
        );
        assert!(xml.contains(r#"xsi:type="osl:flatObjectSet">"#));
        assert!(xml.contains(r#"<osl:object name="CL_FOO" type="CLAS"/>"#));
        assert!(xml.contains(r#"<osl:object name="ZPROG" type="PROG"/>"#));
        // 包形态 → multiPropertySet
        let pkgs = vec![ObjectRef {
            r#type: "package".into(),
            name: "zpkg".into(),
        }];
        let xml = build_aunit_run_xml(&pkgs).unwrap();
        assert!(xml.contains(r#"xsi:type="osl:multiPropertySet">"#));
        assert!(xml.contains(r#"<osl:property key="package" value="ZPKG"/>"#));
    }

    /// sapcli fixtures_adt_atc.py 的 worklist fixture（2 对象、3 findings、1 空）。
    const ATC_WORKLIST_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<atcworklist:worklist xmlns:atcworklist="http://www.sap.com/adt/atc/worklist" atcworklist:id="CB76" atcworklist:timestamp="2023-03-27T05:32:22Z" atcworklist:usedObjectSet="00" atcworklist:objectSetIsComplete="false">
    <atcworklist:objects>
        <atcobject:object xmlns:atcobject="http://www.sap.com/adt/atc/object" xmlns:adtcore="http://www.sap.com/adt/core" adtcore:uri="/sap/bc/adt/atc/objects/R3TR/APIS/OBJ1" adtcore:type="APIS" adtcore:name="OBJ1" adtcore:packageName="$TMP" atcobject:author="DEVELOPER">
            <atcobject:findings>
                <atcfinding:finding xmlns:atcfinding="http://www.sap.com/adt/atc/finding" adtcore:uri="/sap/bc/adt/atc/findings/itemid/A32/index/9" atcfinding:priority="1" atcfinding:checkId="005056AB" atcfinding:checkTitle="Test Environment (CHK_ZDM)" atcfinding:messageId="0898" atcfinding:messageTitle="Exception occurred" atcfinding:exemptionApproval="" atcfinding:exemptionKind="" xmlns:adtcore="http://www.sap.com/adt/core"/>
                <atcfinding:finding xmlns:atcfinding="http://www.sap.com/adt/atc/finding" adtcore:uri="/sap/bc/adt/atc/findings/itemid/A32/index/10" atcfinding:priority="2" atcfinding:checkId="005056AB" atcfinding:checkTitle="Test Environment (CHK_ZDM)" atcfinding:messageId="0899" atcfinding:messageTitle="Second message" atcfinding:exemptionApproval="" atcfinding:exemptionKind="" xmlns:adtcore="http://www.sap.com/adt/core"/>
                <atcfinding:finding xmlns:atcfinding="http://www.sap.com/adt/atc/finding" adtcore:uri="/x" atcfinding:priority="3" atcfinding:checkId="C2" atcfinding:checkTitle="Prio3" atcfinding:messageId="M" atcfinding:messageTitle="Prio three" atcfinding:exemptionApproval="" atcfinding:exemptionKind="" xmlns:adtcore="http://www.sap.com/adt/core"/>
            </atcobject:findings>
        </atcobject:object>
        <atcobject:object xmlns:atcobject="http://www.sap.com/adt/atc/object" xmlns:adtcore="http://www.sap.com/adt/core" adtcore:uri="/sap/bc/adt/atc/objects/R3TR/TABL/TBL1" adtcore:type="TABL" adtcore:name="TBL1" adtcore:packageName="$TMP" atcobject:author="DEVELOPER">
            <atcobject:findings/>
        </atcobject:object>
    </atcworklist:objects>
</atcworklist:worklist>"#;

    #[test]
    fn atc_worklist_parse_objects_and_priorities() {
        let (objects, total, by_priority) = parse_atc_worklist(ATC_WORKLIST_XML);
        assert_eq!(total, 3);
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0]["name"], "OBJ1");
        assert_eq!(objects[0]["packageName"], "$TMP");
        let findings = objects[0]["findings"].as_array().unwrap();
        assert_eq!(findings.len(), 3);
        assert_eq!(findings[0]["checkId"], "005056AB");
        assert_eq!(findings[0]["messageTitle"], "Exception occurred");
        assert_eq!(objects[1]["name"], "TBL1");
        assert_eq!(objects[1]["findings"].as_array().unwrap().len(), 0);
        assert_eq!(by_priority["1"], serde_json::json!(1));
        assert_eq!(by_priority["2"], serde_json::json!(1));
        assert_eq!(by_priority["3"], serde_json::json!(1));
    }

    #[test]
    fn atc_run_xml_matches_sapcli_fixture() {
        let objs = vec![ObjectRef {
            r#type: "class".into(),
            name: "zcl_z001_dpc".into(),
        }];
        let xml = build_atc_run_xml(&objs, 100).unwrap();
        // 对齐 fixtures_adt_atc.py 的 ADT_XML_ATC_RUN_REQUEST_CLASS（adtcore:name 大写）
        assert!(xml
            .contains(r#"<atc:run xmlns:atc="http://www.sap.com/adt/atc" maximumVerdicts="100">"#));
        assert!(xml.contains(r#"<objectSet kind="inclusive">"#));
        assert!(xml.contains(
            r#"adtcore:uri="/sap/bc/adt/oo/classes/zcl_z001_dpc" adtcore:name="ZCL_Z001_DPC""#
        ));
        // package URI 走 packages/ + percent 编码（$ → %24）
        let pkgs = vec![ObjectRef {
            r#type: "package".into(),
            name: "$iamtheking".into(),
        }];
        let xml = build_atc_run_xml(&pkgs, 69).unwrap();
        assert!(xml.contains(
            r#"adtcore:uri="/sap/bc/adt/packages/%24iamtheking" adtcore:name="$IAMTHEKING""#
        ));
        assert!(xml.contains(r#"maximumVerdicts="69""#));
    }

    #[test]
    fn atc_run_infos_parse() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<atcworklist:worklistRun xmlns:atcworklist="http://www.sap.com/adt/atc/worklist">
    <atcworklist:worklistId>ID1</atcworklist:worklistId>
    <atcworklist:infos>
        <atcinfo:info xmlns:atcinfo="http://www.sap.com/adt/atc/info">
            <atcinfo:type>NO_OBJS</atcinfo:type>
            <atcinfo:description>Selection does not contain objects</atcinfo:description>
        </atcinfo:info>
    </atcworklist:infos>
</atcworklist:worklistRun>"#;
        let infos = parse_run_infos(xml);
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0]["type"], "NO_OBJS");
        assert_eq!(
            infos[0]["description"],
            "Selection does not contain objects"
        );
    }
}
