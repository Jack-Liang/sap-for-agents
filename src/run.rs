//! ABAP 临时执行（sapcli 借鉴批次 v0.21）。
//!
//! `POST /api/abap/run` —— 把 agent 的 ABAP 片段包进临时 `if_oo_adt_classrun`
//! 类，创建→预检→写入→激活→**新会话**执行→无条件删除，返回控制台输出。
//! sapcli README 原话："coding agents love `sapcli abap run`"——本端点就是
//! 该编排的服务端版，编排顺序与三个 hard-won 细节完全对齐：
//!
//! 1. 类名预算 ≤30 字符（`<prefix>_<user>_<random>`）；
//! 2. 执行前**强制刷新 CSRF/会话**：classrun 按 ABAP 会话缓存类元数据，
//!    同会话跑刚激活的类会得到 "Class does not implement if_oo_adt_classrun~
//!    main method!"（sapcli 靠 `connection.new_session()` 规避）；
//! 3. 删除放在 finally——执行失败/超时也不留临时类尸体。
//!
//! 业务失败遵循仓库约定（坑 12）：HTTP 200 + `ok:false` + `step`，只有
//! 传输层/系统级失败才是 4xx/5xx。

use crate::error::RfcError;
use crate::explore::first_line;
use crate::objects::{adt_exception_error, CreateSpec, ObjectType};
use crate::systems::Sap;
use axum::response::IntoResponse;
use serde::Serialize;
use std::time::Instant;

// ========================================================================
// 路由
// ========================================================================

pub fn router() -> axum::Router<crate::server::SharedState> {
    axum::Router::new().route("/api/abap/run", axum::routing::post(abap_run_handler))
}

// ========================================================================
// 请求/响应
// ========================================================================

#[derive(serde::Deserialize)]
struct RunBody {
    /// ABAP 片段（方法体语句，不含 CLASS/METHOD 骨架；`out->write()` 输出）
    source: String,
    /// 临时类所在包（默认 $TMP）
    #[serde(default)]
    package: Option<String>,
    /// 类名前缀（默认 zcl_sfa_run；最终名 = 前缀_用户_随机 ≤30 字符）
    #[serde(default)]
    prefix: Option<String>,
    /// 执行超时秒数（默认沿用 ADT 端点超时）
    #[serde(default)]
    timeout_secs: Option<u64>,
}

const DEFAULT_PREFIX: &str = "zcl_sfa_run";
const DEFAULT_PACKAGE: &str = "$TMP";
/// 30 字符是 ABAP 对象名硬上限；随机段至少 4 位防碰撞
const NAME_BUDGET: usize = 30;
const MIN_RANDOM: usize = 4;

#[derive(Debug, Serialize)]
pub struct RunOutcome {
    pub ok: bool,
    /// 失败时：syntax_check / create / activate / execute 之一
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    pub class: String,
    pub package: String,
    /// 执行耗时（仅 execute 成功时有意义）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 临时类是否已删除（finally 语义；false 时 delete_error 说明原因）
    pub deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_error: Option<String>,
    // —— 语法预检 / 激活失败时的结构化 finding ——
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<crate::objects::SyntaxIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

// ========================================================================
// 处理器
// ========================================================================

async fn abap_run_handler(
    sap: Sap,
    axum::Json(body): axum::Json<RunBody>,
) -> Result<axum::response::Response, RfcError> {
    let source = body.source.trim_matches(['\r', '\n']).to_string();
    if source.trim().is_empty() {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "source 必填（ABAP 片段，方法体语句）".into(),
            key: "ABAPRUN_SOURCE_REQUIRED".into(),
        });
    }
    let package = body
        .package
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_PACKAGE)
        .to_uppercase();
    let prefix = body
        .prefix
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_PREFIX)
        .to_lowercase();

    let sys = sap.arc();
    let adt = sys.adt.as_ref().ok_or_else(crate::adt::adt_disabled)?;

    // ① 类名：<prefix>_<user>_<random> ≤30 字符（sapcli 同款预算）
    let user: String = adt
        .user()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect();
    let user = if user.is_empty() {
        "agent".to_string()
    } else {
        user
    };
    let name = generate_class_name(&prefix, &user)?;

    let mut outcome = RunOutcome {
        ok: false,
        step: None,
        output: None,
        class: name.to_uppercase(),
        package: package.clone(),
        duration_ms: None,
        deleted: false,
        delete_error: None,
        issues: Vec::new(),
        problems: Vec::new(),
        warnings: Vec::new(),
    };

    let obj_type = ObjectType::Class;
    let base = obj_type.base_rel(&name, "");

    // ② 创建壳类（失败即止；deleted=false 因为没建成功）
    let spec = CreateSpec {
        description: "Temporary class created by sap-for-agents /api/abap/run".to_string(),
        devclass: package.clone(),
        transport: None,
        software_component: None,
    };
    if let Err(e) = crate::objects::create_object(&sys, obj_type, &name, "", &spec).await {
        if e.key == "OBJECT_EXISTS" {
            // 随机名碰撞：让调用方重试（下一次随机段不同）
            return Err(RfcError {
                code: -1,
                status: 409,
                message: format!("临时类名 {name} 撞名（几乎不可能，重试一次即可）"),
                key: "ABAPRUN_NAME_COLLISION".into(),
            });
        }
        return Err(e);
    }
    // ③ 语法预检（abapCheckRun，源码内联，不落库；E 级即止）
    match crate::objects::syntax_check(adt, &base, &source_code_for(&name, &source)).await {
        Ok(issues) => {
            let errors: Vec<_> = issues
                .iter()
                .filter(|i| i.severity.eq_ignore_ascii_case("E"))
                .collect();
            if !errors.is_empty() {
                outcome.issues = issues;
                outcome.step = Some("syntax_check".into());
                outcome.deleted = cleanup(&sys, obj_type, &name).await;
                return Ok(axum::Json(outcome).into_response());
            }
        }
        Err(e) => {
            // 预检失败不致命（部分系统 checkRun 缺失）：记警告继续走正式写入，
            // 激活阶段的 problems 会兜底报出
            outcome
                .warnings
                .push(format!("语法预检失败（继续）: {}", e.message));
        }
    }

    // ④ 写入 + 激活（完整锁编排，复用 objects 管线）
    let class_code = source_code_for(&name, &source);
    let written = crate::objects::write_object_source(
        &sys,
        obj_type,
        &name,
        "",
        &class_code,
        None,
        true,
        false,
        None,
    )
    .await;
    match written {
        Ok(w) => {
            if let Some(act) = &w.activated {
                if !act.success {
                    outcome.step = Some("activate".into());
                    outcome.problems = act.problems.clone();
                    outcome.warnings.extend(w.warnings.clone());
                    outcome.deleted = cleanup(&sys, obj_type, &name).await;
                    return Ok(axum::Json(outcome).into_response());
                }
            }
            outcome.warnings.extend(w.warnings.clone());
        }
        Err(e) => {
            outcome.step = Some("activate".into());
            outcome.problems.push(e.message.clone());
            outcome.deleted = cleanup(&sys, obj_type, &name).await;
            return Ok(axum::Json(outcome).into_response());
        }
    }

    // ⑤ 执行：新会话（强制刷新 token/cookie）→ POST oo/classrun/{name}
    let started = Instant::now();
    let run_fut = async {
        let (token, cookie) = adt.fresh_session_token().await?;
        let extra: Vec<(&str, &str)> = Vec::new();
        adt.request_raw(
            axum::http::Method::POST,
            &format!(
                "oo/classrun/{}",
                crate::adt::encode_path_segment(&name.to_lowercase())
            ),
            &[],
            None,
            None,
            "text/plain",
            &extra,
            cookie.as_deref(),
            Some(&token),
        )
        .await
    };
    let exec = match body.timeout_secs {
        Some(secs) => {
            match tokio::time::timeout(std::time::Duration::from_secs(secs.max(1)), run_fut).await {
                Ok(inner) => inner,
                Err(_) => Err(RfcError {
                    code: -1,
                    status: 504,
                    message: format!("ABAP 执行超时（{secs}s）——可传 timeout_secs 放宽"),
                    key: "ABAPRUN_TIMEOUT".into(),
                }),
            }
        }
        None => run_fut.await,
    };

    match exec {
        Ok(resp) if (200..300).contains(&resp.status) => {
            outcome.ok = true;
            outcome.output = Some(String::from_utf8_lossy(&resp.body).to_string());
            outcome.duration_ms = Some(started.elapsed().as_millis() as u64);
        }
        Ok(resp) => {
            let text = String::from_utf8_lossy(&resp.body).to_string();
            if text.contains("exc:exception") {
                let mut e = adt_exception_error(&text, "执行临时类失败");
                e.status = resp.status;
                outcome.step = Some("execute".into());
                outcome.problems.push(e.message.clone());
            } else {
                outcome.step = Some("execute".into());
                outcome
                    .problems
                    .push(format!("HTTP {}: {}", resp.status, first_line(&text)));
            }
        }
        Err(e) => {
            outcome.step = Some("execute".into());
            outcome.problems.push(e.message.clone());
        }
    }

    // ⑥ finally：无条件删除（执行失败/超时同样清理）
    outcome.deleted = cleanup(&sys, obj_type, &name).await;
    Ok(axum::Json(outcome).into_response())
}

// ========================================================================
// 辅助
// ========================================================================

/// classrun 类模板（sapcli `_CLASS_TEMPLATE` 同款结构）
fn source_code_for(name: &str, user_code: &str) -> String {
    let indented: String = user_code
        .lines()
        .map(|l| format!("        {l}\n"))
        .collect();
    format!(
        "\"! Temporary class created by sap-for-agents for adhoc ABAP execution.\n\
CLASS {name} DEFINITION\n  PUBLIC FINAL CREATE PUBLIC .\n\n  PUBLIC SECTION.\n    \
INTERFACES if_oo_adt_classrun.\n\
ENDCLASS.\n\n\
CLASS {name} IMPLEMENTATION.\n    METHOD if_oo_adt_classrun~main.\n\n{indented}\
    ENDMETHOD.\n\
ENDCLASS.\n",
        name = name.to_uppercase(),
        indented = indented
    )
}

/// `<prefix>_<user>_<random>`，总长 ≤30；随机段至少 MIN_RANDOM 位。
fn generate_class_name(prefix: &str, user: &str) -> Result<String, RfcError> {
    let prefix: String = prefix
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let budget = NAME_BUDGET - prefix.len() - 1 - user.len() - 1;
    if budget < MIN_RANDOM {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: format!(
                "prefix \"{prefix}\" + 用户 {user} 超出 30 字符类名预算（剩余随机段 {budget} < {MIN_RANDOM}）——请用更短的前缀"
            ),
            key: "ABAPRUN_PREFIX_TOO_LONG".into(),
        });
    }
    let random: String = std::iter::repeat_with(rand_char).take(budget).collect();
    Ok(format!("{prefix}_{user}_{random}"))
}

/// 进程级伪随机（临时类名足够；不追求密码学强度）。进程内一个 xorshift
/// 状态持续前进——每次调用重新播种会让相邻调用产生相关输出（实测退化成
/// `IXWXWXWX` 这类交替串），种子一次、序列 forever。
fn rand_char() -> char {
    static STATE: std::sync::Mutex<Option<u64>> = std::sync::Mutex::new(None);
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = match *guard {
        Some(s) => s,
        None => {
            // 种子：纳秒时钟异或地址熵（ASLR），避免同毫秒多实例同序列
            use std::time::{SystemTime, UNIX_EPOCH};
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let addr = &guard as *const _ as u64;
            nanos ^ addr.rotate_left(17) | 1
        }
    };
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    *guard = Some(state);
    char::from(b'a' + (state % 26) as u8)
}

/// 删除临时类；返回是否成功（失败只进 delete_error，不影响主结果）。
async fn cleanup(
    sys: &std::sync::Arc<crate::systems::SystemHandle>,
    obj_type: ObjectType,
    name: &str,
) -> bool {
    match crate::objects::delete_object(sys, obj_type, name, "", None).await {
        Ok(_) => {
            // 删除成功后 drain 连接（$TMP 对象写入会话惯常清理，objects 删除路径
            // 自带 drain 对 fugr/func；这里统一兜一次）
            true
        }
        Err(e) => {
            tracing::warn!(class = %name, error = %e.message, "临时类删除失败");
            false
        }
    }
}
