//! ABAP 对象写入编排：**锁 → 写源码 → 解锁 → 激活** 在一次网关请求内完成。
//!
//! 这是 ADT 写入的正确形态（vsp / abapfs 双重实证）：ADT 的编辑锁绑定
//! ABAP 会话（`sap-contextid`），跨 HTTP 请求持锁必然失效——所以 lockHandle
//! 绝不暴露给调用方，编排端点内部串行完成全部步骤。
//!
//! 对象类型与资源（ADT URL 模式，vsp 真机校准）：
//! - 程序/报表：`programs/programs/{名}`
//! - 类：`oo/classes/{名}`
//! - 函数模块：`functions/groups/{组}/fmodules/{FM}`（组名经 RFC 搜索反解）
//!
//! 关键语义（移植自 vsp 的实测结论，含其踩过的坑）：
//! - `MODIFICATION_SUPPORT=NoModification` 不等于只读——本地/$TMP 对象就返回
//!   它且带有效锁柄；真正不可写的是「无 LOCK_HANDLE」；
//! - **先解锁再激活**，否则对象自身的 ENQUEUE 会以 403 拒绝激活；
//! - 激活失败是 **HTTP 200 + 消息清单**（E/A/X 型消息 = 失败；W/I 常见于
//!   成功激活），逻辑失败不升为传输错误；
//! - 消息的真实行号在 href 片段 `#start=行,列` 里，`line` 属性不可靠；
//! - 锁冲突（EU510「他人正在编辑」）返回 ADT 异常文档而非锁结果。

use crate::adt::{
    adt_get_text_lines, adt_request_raw, encode_path_segment, establish_stateful, AdtRawResponse,
};
use std::collections::HashMap;

use crate::api::{InvokeRequest, ScalarValue};
use crate::error::RfcError;

// ========================================================================
// 编排会话（锁→写→解锁 期间的专用 stateful 会话）
// ========================================================================

/// 有状态编排的会话：专用 CSRF token（与持有锁的 ABAP 会话同源）+ cookie jar。
/// ADT 的编辑锁存在 ABAP 会话（roll area）里，靠 `sap-contextid` 关联——
/// 每一步都要带 `X-sap-adt-sessiontype: stateful` 并回传上一步响应的
/// set-cookie，否则 PUT 报 423 InvalidLockHandle；token 与会话不同源则被
/// ICF 以「Service cannot be reached」拒绝。
struct WriteSession {
    token: String,
    pairs: Vec<(String, String)>,
}

impl WriteSession {
    /// 建立专用 stateful 会话（GET + Fetch + stateful）。
    async fn establish(base: &str) -> Result<Self, RfcError> {
        let (token, cookies) = establish_stateful(base).await?;
        Ok(Self {
            token,
            pairs: cookies,
        })
    }
    /// 吸收响应 set-cookie（同名覆盖）。
    fn absorb(&mut self, resp: &AdtRawResponse) {
        for (n, v) in &resp.cookies {
            match self.pairs.iter_mut().find(|(en, _)| en == n) {
                Some(p) => p.1 = v.clone(),
                None => self.pairs.push((n.clone(), v.clone())),
            }
        }
    }
    /// 拼 Cookie 头；为空时返回 None（回落到共享缓存会话）。
    fn cookie_header(&self) -> Option<String> {
        if self.pairs.is_empty() {
            return None;
        }
        Some(
            self.pairs
                .iter()
                .map(|(n, v)| format!("{}={}", n, v))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

/// 有状态请求头（stateful 会话标记）。
const STATEFUL: &[(&str, &str)] = &[("X-sap-adt-sessiontype", "stateful")];

// ========================================================================
// 对象类型与 URL
// ========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
    Program,
    Class,
    Function,
}

impl ObjectType {
    /// 路径段里的类型别名（prog/program、class/clas、func/function/fugr）。
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "prog" | "program" | "report" => Some(Self::Program),
            "class" | "clas" => Some(Self::Class),
            "func" | "function" | "fugr" => Some(Self::Function),
            _ => None,
        }
    }

    /// 对象基础资源（不含 /source/main 后缀，锁/解锁用这个）。
    /// `group` 仅 Function 需要（调用方已反解）。
    pub fn base_rel(&self, name: &str, group: &str) -> String {
        let name = encode_path_segment(&name.trim().to_uppercase());
        match self {
            Self::Program => format!("programs/programs/{}", name),
            Self::Class => format!("oo/classes/{}", name),
            Self::Function => format!(
                "functions/groups/{}/fmodules/{}",
                encode_path_segment(&group.trim().to_uppercase()),
                name
            ),
        }
    }

    /// 源码资源（写/读/语法检查的 artifact 地址）。
    pub fn source_rel(&self, name: &str, group: &str) -> String {
        format!("{}/source/main", self.base_rel(name, group))
    }
}

// ========================================================================
// 轻量 XML 遍历（quick-xml，按本地名匹配，无视命名空间前缀）
// ========================================================================

/// 一个元素：本地名、祖先链、属性、直接文本。
#[derive(Debug, Default)]
struct XmlElem {
    name: String,
    /// 祖先本地名（从外到内），含自身以外的全部层级
    parents: Vec<String>,
    attrs: Vec<(String, String)>,
    text: String,
}

impl XmlElem {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    /// 父链以 …/parent 结尾（不看更外层）
    fn parent_is(&self, parent: &str) -> bool {
        self.parents.last().map(String::as_str) == Some(parent)
    }
}

/// 遍历 XML 文档，产出每个元素（Start 与 Empty 都产出）。
/// 文本只累积到**直接**所属元素——子元素的文本不会污染父元素。
fn walk_xml(xml: &str) -> Vec<XmlElem> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut elems: Vec<XmlElem> = Vec::new();
    // 打开中元素的索引栈（与 reader 的元素栈对齐）
    let mut open: Vec<usize> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref()).to_string();
                let mut attrs = Vec::new();
                for a in e.attributes().flatten() {
                    if a.key.as_ref().starts_with(b"xmlns") {
                        continue;
                    }
                    attrs.push((
                        crate::dumps::xml_local_name_of(a.key.as_ref()).to_string(),
                        crate::dumps::xml_decode_of(a.value.as_ref()),
                    ));
                }
                elems.push(XmlElem {
                    parents: open.iter().map(|&i| elems[i].name.clone()).collect(),
                    name,
                    attrs,
                    text: String::new(),
                });
                open.push(elems.len() - 1);
            }
            Ok(Event::Empty(e)) => {
                let name = local_name(e.name().as_ref()).to_string();
                let mut attrs = Vec::new();
                for a in e.attributes().flatten() {
                    if a.key.as_ref().starts_with(b"xmlns") {
                        continue;
                    }
                    attrs.push((
                        crate::dumps::xml_local_name_of(a.key.as_ref()).to_string(),
                        crate::dumps::xml_decode_of(a.value.as_ref()),
                    ));
                }
                elems.push(XmlElem {
                    parents: open.iter().map(|&i| elems[i].name.clone()).collect(),
                    name,
                    attrs,
                    text: String::new(),
                });
            }
            Ok(Event::Text(t)) => {
                if let Some(&idx) = open.last() {
                    let decoded = crate::dumps::xml_decode_of(t.as_ref());
                    if !decoded.trim().is_empty() {
                        let cell = &mut elems[idx];
                        if !cell.text.is_empty() {
                            cell.text.push(' ');
                        }
                        cell.text.push_str(decoded.trim());
                    }
                }
            }
            Ok(Event::End(_)) => {
                open.pop();
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break, // 容错：解析不了就返回已收集的部分
        }
    }
    elems
}

fn local_name(qname: &[u8]) -> &str {
    crate::dumps::xml_local_name_of(qname)
}

/// URI 片段 `#start=行,列` 中取行号（0 = 无）。
/// 激活/语法检查消息的真实行号在片段里，`line` 属性不可靠（vsp 实测）。
fn uri_fragment_line(uri: &str) -> u32 {
    let Some(i) = uri.find("#start=") else {
        return 0;
    };
    let rest = &uri[i + "#start=".len()..];
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().unwrap_or(0)
}

/// URI 片段中取列号（0 = 无）。
fn uri_fragment_col(uri: &str) -> u32 {
    let Some(i) = uri.find("#start=") else {
        return 0;
    };
    let rest = &uri[i + "#start=".len()..];
    let after_line = match rest.find(',') {
        Some(c) => &rest[c + 1..],
        None => return 0,
    };
    let digits: String = after_line
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().unwrap_or(0)
}

// ========================================================================
// 锁
// ========================================================================

#[derive(Debug, Default)]
/// 锁结果。corruser/corrtext/is_local 目前仅诊断用途（保留解析完整性）。
#[allow(dead_code)]
pub struct LockInfo {
    pub lock_handle: String,
    pub corrnr: String,
    pub corruser: String,
    pub corrtext: String,
    pub is_local: bool,
    pub modification_support: String,
}

/// 解析 ADT 异常文档（锁冲突 EU510 / 权限等），产出 SAP 自己的说法。
fn adt_exception_error(body: &str, action: &str) -> RfcError {
    let elems = walk_xml(body);
    let id = elems
        .iter()
        .find(|e| e.name == "type")
        .and_then(|e| e.attr("id"))
        .unwrap_or("")
        .to_string();
    let message = elems
        .iter()
        .find(|e| e.name == "message" && !e.text.is_empty())
        .map(|e| e.text.clone())
        .unwrap_or_else(|| "SAP 返回了 ADT 异常文档".into());
    // EU510 是锁冲突（他人/他 会话正在编辑）→ 409；其余按 400 系语义给 409/403
    let (status, key) = if id.contains("EU510") {
        (409u16, "OBJECT_LOCKED".to_string())
    } else if message.contains("authorization") || message.contains("Authorization") {
        (403, "ADT_AUTH".to_string())
    } else {
        (409, format!("ADT_{}", id))
    };
    RfcError {
        code: -1,
        status,
        message: format!("{}: {}{}", action, message, {
            if id.is_empty() {
                String::new()
            } else {
                format!(" ({})", id)
            }
        }),
        key,
    }
}

/// 解析锁结果（asx:abap values DATA）。
fn parse_lock_result(body: &str) -> Result<LockInfo, RfcError> {
    if body.contains("exc:exception") {
        return Err(adt_exception_error(body, "锁定对象失败"));
    }
    let elems = walk_xml(body);
    let get = |tag: &str| -> String {
        elems
            .iter()
            .find(|e| e.name == tag && e.parent_is("DATA"))
            .map(|e| e.text.trim().to_string())
            .unwrap_or_default()
    };
    Ok(LockInfo {
        lock_handle: get("LOCK_HANDLE"),
        corrnr: get("CORRNR"),
        corruser: get("CORRUSER"),
        corrtext: get("CORRTEXT"),
        is_local: get("IS_LOCAL") == "X",
        modification_support: get("MODIFICATION_SUPPORT"),
    })
}

/// 取编辑锁（专用 stateful 会话内）。无 LOCK_HANDLE 才是真正不可写——
/// NoModification 对本地对象是常态。
async fn lock_object(base: &str, sess: &mut WriteSession) -> Result<LockInfo, RfcError> {
    let resp = adt_request_raw(
        axum::http::Method::POST,
        base,
        &[("_action", "LOCK"), ("accessMode", "MODIFY")],
        None,
        None,
        "application/vnd.sap.as+xml;charset=UTF-8;dataname=com.sap.adt.lock.result",
        STATEFUL,
        sess.cookie_header().as_deref(),
        Some(sess.token.as_str()),
    )
    .await?;
    sess.absorb(&resp);
    if resp.status != 200 {
        let body = String::from_utf8_lossy(&resp.body).to_string();
        if body.contains("exc:exception") {
            return Err(adt_exception_error(&body, "锁定对象失败"));
        }
        return Err(RfcError {
            code: -1,
            status: 502,
            message: format!("锁定对象失败（ADT 返回 {}）", resp.status),
            key: "ADT_LOCK_FAILED".into(),
        });
    }
    let body = String::from_utf8_lossy(&resp.body).to_string();
    let lock = parse_lock_result(&body)?;
    if lock.lock_handle.is_empty() {
        return Err(RfcError {
            code: -1,
            status: 409,
            message: format!(
                "对象不可经 ADT 修改（锁结果无 LOCK_HANDLE，modificationSupport={}）。\
                 常见原因：只读系统、缺开发权限、对象在 BTP 客户命名空间之外",
                lock.modification_support
            ),
            key: "OBJECT_NOT_MODIFIABLE".into(),
        });
    }
    Ok(lock)
}

/// 释放编辑锁（尽力而为：失败只记警告，不阻断后续激活）。
async fn unlock_object(
    base: &str,
    lock_handle: &str,
    sess: &mut WriteSession,
) -> Result<(), String> {
    let resp = adt_request_raw(
        axum::http::Method::POST,
        base,
        &[("_action", "UNLOCK"), ("lockHandle", lock_handle)],
        None,
        None,
        "*/*",
        STATEFUL,
        sess.cookie_header().as_deref(),
        Some(sess.token.as_str()),
    )
    .await
    .map_err(|e| e.message)?;
    sess.absorb(&resp);
    if (200..300).contains(&resp.status) {
        Ok(())
    } else {
        Err(format!("解锁失败（ADT 返回 {}）", resp.status))
    }
}

/// 结束 stateful 会话（服务器侧释放 ABAP 会话；尽力而为）。
async fn drop_session(base: &str, sess: &WriteSession) {
    let _ = adt_request_raw(
        axum::http::Method::GET,
        base,
        &[],
        None,
        None,
        "*/*",
        &[("X-sap-adt-sessiontype", "stateless")],
        sess.cookie_header().as_deref(),
        None,
    )
    .await;
}

// ========================================================================
// 激活
// ========================================================================

#[derive(Debug, serde::Serialize)]
pub struct ActivationMessage {
    pub severity: String,
    pub line: u32,
    pub text: String,
}

#[derive(Debug, serde::Serialize)]
pub struct ActivationOutcome {
    pub success: bool,
    pub activation_executed: bool,
    pub messages: Vec<ActivationMessage>,
    /// 仍处于未激活状态的关联对象（URI）
    pub inactive: Vec<String>,
    /// 失败时的「问题行」摘要（"Line N: 文本" 形态，首个即主因）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

/// 解析激活结果。**HTTP 200 不等于激活成功**——拒绝在消息清单里：
/// E/A/X 型消息或 activationExecuted="false"。
fn parse_activation_result(body: &str) -> ActivationOutcome {
    let mut out = ActivationOutcome {
        success: true,
        activation_executed: true,
        messages: Vec::new(),
        inactive: Vec::new(),
        problems: Vec::new(),
    };
    if body.trim().is_empty() {
        return out; // 空 body = 成功
    }
    let elems = walk_xml(body);

    if let Some(props) = elems.iter().find(|e| e.name == "properties") {
        out.activation_executed = props.attr("activationExecuted") != Some("false");
    }
    // 消息：msg 元素，其 shortText 的文本可能拆成多个 <txt>。
    // 归属按文档顺序：一个 shortText 属于它之前最近的那个 msg。
    let msg_idx: Vec<usize> = elems
        .iter()
        .enumerate()
        .filter(|(_, e)| e.name == "msg")
        .map(|(i, _)| i)
        .collect();
    for (n, &mi) in msg_idx.iter().enumerate() {
        let msg = &elems[mi];
        let next = msg_idx.get(n + 1).copied().unwrap_or(usize::MAX);
        let mut text = String::new();
        for (_, e) in elems
            .iter()
            .enumerate()
            .skip(mi + 1)
            .take_while(|(i, _)| *i < next)
        {
            if e.name == "shortText" || e.name == "txt" {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(&e.text);
            }
        }
        let severity = msg.attr("type").unwrap_or("").to_string();
        let href = msg.attr("href").unwrap_or("").to_string();
        // 行号：href 片段优先，line 属性兜底（生成类报表里 line 恒为 1）
        let line = {
            let frag = uri_fragment_line(&href);
            if frag > 0 {
                frag
            } else {
                msg.attr("line").and_then(|l| l.parse().ok()).unwrap_or(0)
            }
        };
        out.messages.push(ActivationMessage {
            severity,
            line,
            text: text.trim().to_string(),
        });
    }
    // 未激活对象：object > ref 的 uri
    for r in elems.iter().filter(|e| e.name == "ref" && e.parent_is("object")) {
        if let Some(uri) = r.attr("uri") {
            out.inactive.push(uri.to_string());
        }
    }

    let has_error = out.messages.iter().any(|m| {
        m.severity.contains('E') || m.severity.contains('A') || m.severity.contains('X')
    });
    out.success = out.activation_executed && !has_error;

    if !out.success {
        // 问题行：E/A/X 优先；一个都没有时连警告一起报（SAP 会用 W 说"已取消"）
        let mut msgs: Vec<&ActivationMessage> = out
            .messages
            .iter()
            .filter(|m| {
                m.severity.contains('E') || m.severity.contains('A') || m.severity.contains('X')
            })
            .collect();
        if msgs.is_empty() {
            msgs = out.messages.iter().collect();
        }
        out.problems = msgs
            .iter()
            .map(|m| {
                let text = if m.text.is_empty() {
                    "(SAP 未给出文本)"
                } else {
                    &m.text
                };
                if m.line > 0 {
                    format!("Line {}: {}", m.line, text)
                } else {
                    text.to_string()
                }
            })
            .collect();
        if out.problems.is_empty() {
            out.problems.push("激活被拒绝且 SAP 未说明原因；对象仍处于未激活状态".into());
        }
    }
    out
}

/// XML 属性转义。
fn xml_attr_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 激活对象（先解锁后激活是 SAP 要求，见模块注释）。
async fn activate_object(base: &str, name: &str) -> Result<ActivationOutcome, RfcError> {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<adtcore:objectReferences xmlns:adtcore=\"http://www.sap.com/adt/core\">\
<adtcore:objectReference adtcore:uri=\"{}\" adtcore:name=\"{}\"/>\
</adtcore:objectReferences>",
        xml_attr_escape(&format!("/sap/bc/adt/{}", base)),
        xml_attr_escape(name)
    );
    let resp = adt_request_raw(
        axum::http::Method::POST,
        "activation",
        &[("method", "activate"), ("preauditRequested", "true")],
        Some(body.as_bytes()),
        Some("application/xml"),
        "*/*",
        &[], // 激活无会话要求（解锁后执行）
        None,
        None,
    )
    .await?;
    if !(200..300).contains(&resp.status) {
        return Err(RfcError {
            code: -1,
            status: 502,
            message: format!("激活请求失败（ADT 返回 {}）", resp.status),
            key: "ADT_ACTIVATE_FAILED".into(),
        });
    }
    Ok(parse_activation_result(&String::from_utf8_lossy(&resp.body)))
}

// ========================================================================
// 语法检查（不写库：源码 base64 内嵌提交）
// ========================================================================

#[derive(Debug, serde::Serialize)]
pub struct SyntaxIssue {
    pub severity: String,
    pub line: u32,
    pub offset: u32,
    pub text: String,
}

/// 对任意源码做语法检查（当前库内容不受影响）。走 ADT checkruns。
pub async fn syntax_check(base: &str, source: &str) -> Result<Vec<SyntaxIssue>, RfcError> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(source);
    // checkObject 用对象 URL（不带 /source/main，防超长 URI）；
    // artifact 指到源码位置（类 include 例外，本端点不涉及）
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<chkrun:checkObjectList xmlns:chkrun=\"http://www.sap.com/adt/checkrun\" xmlns:adtcore=\"http://www.sap.com/adt/core\">\
<chkrun:checkObject adtcore:uri=\"{}\" chkrun:version=\"active\">\
<chkrun:artifacts>\
<chkrun:artifact chkrun:contentType=\"text/plain; charset=utf-8\" chkrun:uri=\"{}\">\
<chkrun:content>{}</chkrun:content>\
</chkrun:artifact>\
</chkrun:artifacts>\
</chkrun:checkObject>\
</chkrun:checkObjectList>",
        xml_attr_escape(&format!("/sap/bc/adt/{}", base)),
        xml_attr_escape(&format!("/sap/bc/adt/{}/source/main", base)),
        encoded
    );
    let resp = adt_request_raw(
        axum::http::Method::POST,
        "checkruns",
        &[("reporters", "abapCheckRun")],
        Some(body.as_bytes()),
        Some("application/*"),
        // vsp 实测注记：S/4 对所有厂商 content type 答 406、ERP 全部 200，
        // 唯一两边都接受的是 */*——这里不是偷懒而是可移植选择
        "*/*",
        &[], // 语法检查无状态
        None,
        None,
    )
    .await?;
    if !(200..300).contains(&resp.status) {
        return Err(RfcError {
            code: -1,
            status: 502,
            message: format!("语法检查失败（ADT 返回 {}）", resp.status),
            key: "ADT_SYNTAX_CHECK_FAILED".into(),
        });
    }
    Ok(parse_syntax_result(&String::from_utf8_lossy(&resp.body)))
}

fn parse_syntax_result(body: &str) -> Vec<SyntaxIssue> {
    let elems = walk_xml(body);
    elems
        .iter()
        .filter(|e| e.name == "checkMessage")
        .map(|m| {
            let uri = m.attr("uri").unwrap_or("");
            SyntaxIssue {
                severity: m.attr("type").unwrap_or("").to_string(),
                line: uri_fragment_line(uri),
                offset: uri_fragment_col(uri),
                text: m.attr("shortText").unwrap_or("").to_string(),
            }
        })
        .collect()
}

// ========================================================================
// 写入编排
// ========================================================================

#[derive(Debug, serde::Serialize)]
pub struct WriteOutcome {
    #[serde(rename = "type")]
    pub obj_type: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub source_url: String,
    pub written: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport_used: Option<String>,
    pub activated: Option<ActivationOutcome>,
    /// 编排过程中的非致命警告（如解锁失败——锁会随会话过期，但应可见）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

// ========================================================================
// 对象创建（prog/class/func 的壳；源码随后走正常写入管线）
// ========================================================================

/// 创建请求的可选项。
#[derive(Default)]
pub struct CreateSpec {
    /// 对象短描述（title）——SAP 必填
    pub description: String,
    /// 开发包（默认 $TMP；生产包需配合 transport）
    pub devclass: String,
    /// 传输请求号（可选）
    pub transport: Option<String>,
}

/// 创建对象壳。成功后对象存在（可能为空源码），随后的 PUT/replace 走既有
/// 写入编排。prog/func 走 RPY insert（实测可用、无 schema 猜谜）；class 走
/// ADT 标准创建（POST oo/classes，v5 schema——Eclipse 同款契约）。
pub async fn create_object(
    pool: &std::sync::Arc<crate::pool::RfcConnectionPool>,
    obj_type: ObjectType,
    name: &str,
    group: &str,
    spec: &CreateSpec,
) -> Result<(), RfcError> {
    if spec.description.trim().is_empty() {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "description 必填（SAP 对象需要 title/short text）".into(),
            key: "CREATE_DESC_REQUIRED".into(),
        });
    }
    match obj_type {
        ObjectType::Program => {
            let pname = name.to_uppercase();
            let title = spec.description.clone();
            let devclass = if spec.devclass.trim().is_empty() {
                "$TMP".to_string()
            } else {
                spec.devclass.trim().to_uppercase()
            };
            let transport = spec.transport.clone().unwrap_or_default();
            crate::server::run_blocking(std::sync::Arc::clone(pool), move |conn| {
                let req = crate::api::InvokeRequest {
                    func_name: "RPY_PROGRAM_INSERT".to_string(),
                    inputs: HashMap::from([
                        ("PROGRAM_NAME".to_string(), ScalarValue::Chars(pname.clone())),
                        ("PROGRAM_TYPE".to_string(), ScalarValue::Chars("1".to_string())),
                        ("TITLE_STRING".to_string(), ScalarValue::Chars(title.clone())),
                        ("DEVELOPMENT_CLASS".to_string(), ScalarValue::Chars(devclass.clone())),
                        // 免交互 + 直接保存
                        ("SUPPRESS_DIALOG".to_string(), ScalarValue::Chars("X".to_string())),
                        ("SAVE_INACTIVE".to_string(), ScalarValue::Chars(" ".to_string())),
                        ("TEMPORARY".to_string(), ScalarValue::Chars(" ".to_string())),
                        ("STATUS".to_string(), ScalarValue::Chars("A".to_string())),
                        ("APPLICATION".to_string(), ScalarValue::Chars(" ".to_string())),
                        ("AUTHORIZATION_GROUP".to_string(), ScalarValue::Chars(" ".to_string())),
                        ("EDIT_LOCK".to_string(), ScalarValue::Chars(" ".to_string())),
                        ("TRANSPORT_NUMBER".to_string(), ScalarValue::Chars(transport.clone())),
                    ]),
                    ..Default::default()
                };
                let _ = crate::executor::execute_collect(conn, &req)?;
                // SOURCE 不在此写（实测 trial 上 INSERT 的 SOURCE 表不落盘）——
                // 壳建好后由调用方走 PUT source/main 写入并激活
                Ok::<(), RfcError>(())
            })
            .await
        }
        ObjectType::Function => {
            if group.trim().is_empty() {
                return Err(RfcError {
                    code: -1,
                    status: 400,
                    message: "func 创建需要 group（函数组），或传 group_hint 由网关反解".into(),
                    key: "CREATE_GROUP_REQUIRED".into(),
                });
            }
            let fname = name.to_uppercase();
            let fgroup = group.trim().to_uppercase();
            let short = spec.description.clone();
            let transport = spec.transport.clone().unwrap_or_default();
            let devclass = if spec.devclass.trim().is_empty() {
                "$TMP".to_string()
            } else {
                spec.devclass.trim().to_uppercase()
            };
            let fm_insert = |fname: String, fgroup: String, short: String, transport: String| {
                move |conn: &crate::connection::RfcConnection| -> Result<(), RfcError> {
                    let req = InvokeRequest {
                        func_name: "RPY_FUNCTIONMODULE_INSERT".to_string(),
                        inputs: HashMap::from([
                            ("FUNCNAME".to_string(), ScalarValue::Chars(fname.clone())),
                            ("FUNCTION_POOL".to_string(), ScalarValue::Chars(fgroup.clone())),
                            ("SHORT_TEXT".to_string(), ScalarValue::Chars(short.clone())),
                            ("CORRNUM".to_string(), ScalarValue::Chars(transport.clone())),
                        ]),
                        ..Default::default()
                    };
                    let _ = crate::executor::execute_collect(conn, &req)?;
                    Ok(())
                }
            };
            match crate::server::run_blocking(
                std::sync::Arc::clone(pool),
                fm_insert(fname.clone(), fgroup.clone(), short.clone(), transport.clone()),
            )
            .await
            {
                Ok(()) => Ok(()),
                // 组不存在 → 自动建组（RS_FUNCTION_POOL_INSERT）后重试一次。
                // 包按序尝试：显式传入 → ZLOCAL（ABAP Cloud Trial 的本地包，
                // 该环境禁止 $TMP 的 FUGR："cannot be created without a package"）
                // → $TMP（on-prem 常规默认）。
                Err(e) if e.key.contains("INVALID_FUNCTION_POOL") || e.message.contains("652") => {
                    let pool_insert = |g: String, desc: String, dev: String, tr: String| {
                        move |conn: &crate::connection::RfcConnection| -> Result<(), RfcError> {
                            let req = InvokeRequest {
                                func_name: "RS_FUNCTION_POOL_INSERT".to_string(),
                                inputs: HashMap::from([
                                    ("FUNCTION_POOL".to_string(), ScalarValue::Chars(g.clone())),
                                    ("SHORT_TEXT".to_string(), ScalarValue::Chars(desc.clone())),
                                    ("DEVCLASS".to_string(), ScalarValue::Chars(dev.clone())),
                                    ("CORRNUM".to_string(), ScalarValue::Chars(tr.clone())),
                                    ("SUPPRESS_CORR_CHECK".to_string(), ScalarValue::Chars("X".to_string())),
                                    ("SUPPRESS_LANGUAGE_CHECK".to_string(), ScalarValue::Chars("X".to_string())),
                                    ("AUTHORITY_CHECK".to_string(), ScalarValue::Chars(" ".to_string())),
                                    ("NAMESPACE".to_string(), ScalarValue::Chars(" ".to_string())),
                                    ("RESPONSIBLE".to_string(), ScalarValue::Chars(" ".to_string())),
                                    ("UNICODE_CHECKS".to_string(), ScalarValue::Chars(" ".to_string())),
                                ]),
                                ..Default::default()
                            };
                            let _ = crate::executor::execute_collect(conn, &req)?;
                            Ok(())
                        }
                    };
                    let mut last_err: Option<RfcError> = None;
                    let mut candidates: Vec<String> = vec![devclass.clone()];
                    for fallback in ["ZLOCAL", "$TMP"] {
                        if !candidates.iter().any(|c| c.eq_ignore_ascii_case(fallback)) {
                            candidates.push(fallback.to_string());
                        }
                    }
                    for dev in candidates {
                        match crate::server::run_blocking(
                            std::sync::Arc::clone(pool),
                            pool_insert(fgroup.clone(), short.clone(), dev, transport.clone()),
                        )
                        .await
                        {
                            Ok(()) => {
                                last_err = None;
                                break;
                            }
                            // 组已存在的场景不算失败（并发/重复创建）
                            Err(e2) if e2.key.contains("POOL") && e2.message.contains("exist") => {
                                last_err = None;
                                break;
                            }
                            Err(e2) => last_err = Some(e2),
                        }
                    }
                    if let Some(e2) = last_err {
                        return Err(e2);
                    }
                    let resp_group = fgroup.clone();
                    let r = crate::server::run_blocking(
                        std::sync::Arc::clone(pool),
                        fm_insert(fname, fgroup.clone(), short, transport),
                    )
                    .await;
                    let fgroup = resp_group;
                    // ABAP Cloud Trial 自检：RS_FUNCTION_POOL_INSERT 可能返回成功
                    // 却不写 TADIR（组对象实际不存在）——后续 ADT 写入会以
                    // "FUGR cannot be created without a package" 失败。提前检出，
                    // 给出可操作的错误而不是静默假成功。
                    if r.is_ok() {
                        let g_chk = fgroup.clone();
                        let registered = crate::server::run_blocking(
                            std::sync::Arc::clone(pool),
                            move |conn| {
                                let rows = crate::discovery::read_table(
                                    conn, "TADIR",
                                    &["OBJECT".to_string(), "OBJ_NAME".to_string()],
                                    &[format!("OBJECT = 'FUGR' AND OBJ_NAME = '{}'", g_chk)],
                                    1, '\u{1}',
                                )
                                .unwrap_or_default();
                                Ok::<bool, RfcError>(!rows.is_empty())
                            },
                        )
                        .await
                        .unwrap_or(true);
                        if !registered {
                            return Err(RfcError {
                                code: -1,
                                status: 502,
                                message: format!(
                                    "函数组 {fgroup} 创建后未在 TADIR 注册（ABAP Cloud Trial 上 RPY 建组受限）。\
请在 SE80/ADT 手工创建该组（package 如 ZLOCAL）后重试"
                                ),
                                key: "FUGR_NOT_REGISTERED".into(),
                            });
                        }
                    }
                    r
                }
                Err(e) => Err(e),
            }
        }
        ObjectType::Class => {
            // ADT 标准创建（Eclipse 同款）：POST oo/classes?name=Z...&package=...
            let rel = "oo/classes";
            let query: Vec<(&str, &str)> = vec![
                ("name", name),
                ("package", if spec.devclass.trim().is_empty() { "$TMP" } else { spec.devclass.trim() }),
            ];
            // body 按该服务读取实例反推的 schema：abapClass root + adtcore: 前缀属性
            // （这台 ABAP Cloud Trial 实测：无前缀属性 → "could not be converted"；
            //   adtcore 属性 → 进到对象校验层。on-prem 标准版为 class:class。）
            let body = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<class:abapClass xmlns:class=\"http://www.sap.com/adt/oo/classes\" \
 xmlns:adtcore=\"http://www.sap.com/adt/core\" adtcore:name=\"{name}\" \
 adtcore:description=\"{}\" class:final=\"true\" class:visibility=\"public\"/>",
                xml_escape(&spec.description)
            );
            let resp = adt_request_raw(
                axum::http::Method::POST,
                rel,
                &query,
                Some(body.as_bytes()),
                Some("application/vnd.sap.adt.oo.classes.v5+xml"),
                "*/*",
                &[], // 无额外头；CSRF 由网关自动处理
                None,
                None,
            )
            .await?;
            if !(200..300).contains(&resp.status) {
                return Err(RfcError {
                    code: -1,
                    status: 502,
                    message: format!("ADT 创建失败: {}", String::from_utf8_lossy(&resp.body)),
                    key: "CREATE_FAILED".into(),
                });
            }
            Ok(())
        }
    }
}

/// 写入（可选对象不存在时先建壳）：PUT source 语义的共享入口。
/// `create_desc` 为 Some 时启用自动创建（描述即 title）。
/// 写入选项（maybe_create 系列共用）
pub struct WriteOpts<'a> {
    pub transport: Option<&'a str>,
    pub activate: bool,
    /// Some(description) 启用「对象不存在时自动创建」
    pub create_desc: Option<&'a str>,
}

pub async fn write_object_maybe_create(
    pool: &std::sync::Arc<crate::pool::RfcConnectionPool>,
    obj_type: ObjectType,
    name: &str,
    group: &str,
    source: &str,
    opts: &WriteOpts<'_>,
) -> Result<WriteOutcome, RfcError> {
    let WriteOpts { transport, activate, create_desc } = *opts;
    match write_object_source(obj_type, name, group, source, transport, activate).await {
        Ok(o) => Ok(o),
        Err(e) if create_desc.is_some() && is_object_not_exist(&e) => {
            let spec = CreateSpec {
                description: create_desc.unwrap_or_default().to_string(),
                devclass: String::new(),
                transport: transport.map(str::to_string),
            };
            create_object(pool, obj_type, name, group, &spec).await?;
            write_object_source(obj_type, name, group, source, transport, activate).await
        }
        Err(e) => Err(e),
    }
}

/// 唯一匹配查找替换（可选对象不存在时先建壳）：POST replace 语义的共享入口。
/// 返回写入 outcome（外加 replaced 标记在 `replaced` 字段）。
pub async fn replace_object_maybe_create(
    pool: &std::sync::Arc<crate::pool::RfcConnectionPool>,
    obj_type: ObjectType,
    name: &str,
    group: &str,
    old_string: &str,
    new_string: &str,
    opts: &WriteOpts<'_>,
) -> Result<serde_json::Value, RfcError> {
    let WriteOpts { transport, activate, create_desc } = *opts;
    let current = match read_current_source(obj_type, name, group).await {
        Ok(c) => c,
        Err(e) if create_desc.is_some() && is_object_not_exist(&e) => {
            let spec = CreateSpec {
                description: create_desc.unwrap_or_default().to_string(),
                devclass: String::new(),
                transport: transport.map(str::to_string),
            };
            create_object(pool, obj_type, name, group, &spec).await?;
            String::new()
        }
        Err(e) => return Err(e),
    };
    let updated = find_and_replace(&current, old_string, new_string).map_err(|m| RfcError {
        code: -1,
        status: 400,
        message: m,
        key: "REPLACE_FAILED".into(),
    })?;
    if updated == current {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "替换后内容与原文相同".into(),
            key: "REPLACE_NO_CHANGE".into(),
        });
    }
    let outcome = write_object_source(obj_type, name, group, &updated, transport, activate).await?;
    let mut v = serde_json::to_value(&outcome).unwrap_or_default();
    v["replaced"] = serde_json::json!(true);
    Ok(v)
}

/// 判断写入错误是否为「对象不存在」（可配合 create:true 自动建壳重试）。
pub fn is_object_not_exist(err: &RfcError) -> bool {
    (err.status == 409 && err.key == "ADT_ExceptionResourceNotFound")
        || err.message.contains("does not exist")
}

/// XML 属性转义（创建 body 用）。
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 编排写入：锁 → PUT 源码 → 解锁 → 激活。
///
/// - `transport`: 调用方指定的传输请求号；未指定时复用对象已绑定的
///   CORRNR（vsp issue #144：已捕获对象不传会收到假 409）；
/// - 激活是逻辑结果不是传输错误：失败时 Ok(WriteOutcome{activated:
///   Some(失败详情)})，由调用方决定如何呈现；
/// - PUT 失败会尽力解锁后返回错误，避免泄漏孤儿锁。
pub async fn write_object_source(
    obj_type: ObjectType,
    name: &str,
    group: &str,
    source: &str,
    transport: Option<&str>,
    activate: bool,
) -> Result<WriteOutcome, RfcError> {
    let type_name = match obj_type {
        ObjectType::Program => "prog",
        ObjectType::Class => "class",
        ObjectType::Function => "func",
    };
    let base = obj_type.base_rel(name, group);
    let source_rel = obj_type.source_rel(name, group);
    let mut warnings = Vec::new();

    // ⓪ 建立专用 stateful 会话：锁的签发、token 的签发必须同源，
    //    否则柄是死的（无状态签发）或请求被拒（跨会话 token）
    let mut sess = WriteSession::establish(&base).await?;

    // ① 锁
    let lock = lock_object(&base, &mut sess).await?;
    let transport_used = transport
        .map(str::to_string)
        .filter(|t| !t.trim().is_empty())
        .or_else(|| {
            let c = lock.corrnr.trim();
            (!c.is_empty()).then(|| c.to_string())
        });

    // ② PUT 源码；失败先解锁再报错（不留孤儿锁）
    let mut put_query: Vec<(String, String)> =
        vec![("lockHandle".into(), lock.lock_handle.clone())];
    if let Some(t) = &transport_used {
        put_query.push(("corrNr".into(), t.clone()));
    }
    let put_query_ref: Vec<(&str, &str)> = put_query
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let put_resp = match adt_request_raw(
        axum::http::Method::PUT,
        &source_rel,
        &put_query_ref,
        Some(source.as_bytes()),
        Some("text/plain; charset=utf-8"),
        "*/*",
        STATEFUL,
        sess.cookie_header().as_deref(),
        Some(sess.token.as_str()),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            // 尽力解锁；解锁失败只记警告
            if let Err(w) = unlock_object(&base, &lock.lock_handle, &mut sess).await {
                warnings.push(w);
            }
            drop_session(&base, &sess).await;
            return Err(RfcError {
                code: -1,
                status: 502,
                message: format!("写入源码失败: {}", e.message),
                key: "ADT_WRITE_FAILED".into(),
            });
        }
    };
    sess.absorb(&put_resp);
    if !(200..300).contains(&put_resp.status) {
        if let Err(w) = unlock_object(&base, &lock.lock_handle, &mut sess).await {
            warnings.push(w);
        }
        drop_session(&base, &sess).await;
        let body = String::from_utf8_lossy(&put_resp.body);
        let (status, key) = if put_resp.status == 409 {
            (409u16, "OBJECT_LOCKED".to_string())
        } else if put_resp.status == 412 {
            (412, "ETAG_CONFLICT".to_string())
        } else {
            (502, "ADT_WRITE_FAILED".to_string())
        };
        return Err(RfcError {
            code: -1,
            status,
            message: format!(
                "写入源码失败（ADT 返回 {}）: {}",
                put_resp.status,
                truncate(&body, 300)
            ),
            key,
        });
    }

    // ③ 先解锁再激活（SAP 要求：不解锁时对象自身 ENQUEUE 以 403 拒绝激活）
    if let Err(w) = unlock_object(&base, &lock.lock_handle, &mut sess).await {
        warnings.push(w);
    }
    drop_session(&base, &sess).await;

    // ④ 激活（逻辑结果，不升为传输错误）
    let name_upper = name.trim().to_uppercase();
    let activated = if activate {
        Some(activate_object(&base, &name_upper).await?)
    } else {
        None
    };

    Ok(WriteOutcome {
        obj_type: type_name.to_string(),
        name: name.trim().to_uppercase(),
        group: (obj_type == ObjectType::Function).then(|| group.to_uppercase()),
        source_url: source_rel,
        written: true,
        transport_used,
        activated,
        warnings,
    })
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

// ========================================================================
// replace_string（AI 编辑形态，算法照 abapfs mcpReplaceStringTool）
// ========================================================================

/// 在源码中做**唯一匹配**的查找替换。
/// 规则（abapfs 验证过的 AI 编辑契约）：
/// - `old` 为空：仅当当前源码完全为空时允许（新建对象的首写）；
/// - `old == new`：拒绝（无变化）；
/// - 0 处匹配：先做 EOL 归一化（\r\n→\n）重试，仍无 → 报错并提示先读源码；
/// - 多处匹配：报错并要求附带更多上下文行；
/// - 恰好 1 处：替换。
pub fn find_and_replace(content: &str, old: &str, new: &str) -> Result<String, String> {
    if old.is_empty() {
        if content.is_empty() {
            return Ok(new.to_string());
        }
        return Err(
            "old_string 仅允许在当前源码完全为空时为空；对象已有内容，必须给出要替换的精确文本"
                .into(),
        );
    }
    if old == new {
        return Err("old_string 与 new_string 相同，不会有任何变化".into());
    }
    let count = content.matches(old).count();
    if count == 1 {
        return Ok(content.replacen(old, new, 1));
    }
    if count > 1 {
        return Err(format!(
            "old_string 命中 {} 处，必须恰好 1 处；请附带更多上下文行使其唯一",
            count
        ));
    }
    // ===== 精确匹配 0 命中的兜底阶梯（每级都保持「恰好 1 处」约束）=====
    // 统一到 LF 空间做（源码 LF、调用方 CRLF 是最常见失配原因；写回按原文风格还原）
    let crlf = content.contains("\r\n");
    let norm_content = content.replace("\r\n", "\n");
    let norm_old_full = old.replace("\r\n", "\n");
    let norm_new = new.replace("\r\n", "\n");
    let apply = |hit_old: &str| -> Option<String> {
        // 命中数在归一化内容上复核，唯一才替换
        let n = norm_content.matches(hit_old).count();
        if n == 1 {
            Some(norm_content.replacen(hit_old, &norm_new, 1))
        } else {
            None
        }
    };
    // 阶梯 1：EOL 归一化后的精确匹配
    if let Some(updated) = apply(&norm_old_full) {
        return Ok(if crlf { restored_crlf(&updated) } else { updated });
    }
    // 阶梯 2：末尾 \n 修剪（锚点是最后一行时，调用方常多带一个换行）
    let old_trim = norm_old_full.trim_end_matches('\n');
    if !old_trim.is_empty() && old_trim != norm_old_full {
        if let Some(updated) = apply(old_trim) {
            return Ok(if crlf { restored_crlf(&updated) } else { updated });
        }
    }
    // 阶梯 3：大小写不敏感唯一匹配——Agent 按大写化视图做锚点、SAP 存的却是
    // 原文（或反之）的漂移陷阱（实测：RPY 读回会大写化、ADT 写路径读原文）。
    // 仅当 lower 后字节长度不变（ASCII 系源码）才启用，避免 Unicode 变长错位。
    let lc_content = norm_content.to_lowercase();
    if lc_content.len() == norm_content.len() {
        for cand in [old_trim, norm_old_full.as_str()] {
            if cand.is_empty() {
                continue;
            }
            let lc_old = cand.to_lowercase();
            if lc_old.len() != cand.len() {
                continue; // 变长字符防御：无法按字节定位，跳过
            }
            let n = lc_content.matches(&lc_old).count();
            if n == 1 {
                let idx = lc_content.find(&lc_old).unwrap();
                let mut updated = String::with_capacity(norm_content.len());
                updated.push_str(&norm_content[..idx]);
                updated.push_str(&norm_new);
                updated.push_str(&norm_content[idx + lc_old.len()..]);
                return Ok(if crlf { restored_crlf(&updated) } else { updated });
            }
            if n > 1 {
                return Err(format!(
                    "old_string（大小写不敏感）命中 {} 处，必须恰好 1 处；请附带更多上下文行使其唯一",
                    n
                ));
            }
        }
    }

    Err("找不到 old_string；已尝试精确/CRLF 归一/末行换行修剪/大小写不敏感唯一匹配。请先 GET 当前源码，按原文精确匹配（含缩进与空格）".into())
}

/// 把归一化后的纯 LF 文本还原为 CRLF 风格（原文是 CRLF 时）。
/// 输入已经不含 \r\n 对，每个 \n 都是裸换行。
fn restored_crlf(s: &str) -> String {
    s.replace('\n', "\r\n")
}

/// 读对象当前源码（ADT 通道，写路径的权威状态）。
pub async fn read_current_source(
    obj_type: ObjectType,
    name: &str,
    group: &str,
) -> Result<String, RfcError> {
    let rel = obj_type.source_rel(name, group);
    let lines = adt_get_text_lines(&rel).await?;
    Ok(lines.join("\n"))
}

// ========================================================================
// 测试
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_type_parses_aliases_and_builds_urls() {
        assert_eq!(ObjectType::parse("prog"), Some(ObjectType::Program));
        assert_eq!(ObjectType::parse("REPORT"), Some(ObjectType::Program));
        assert_eq!(ObjectType::parse("class"), Some(ObjectType::Class));
        assert_eq!(ObjectType::parse("FUGR"), Some(ObjectType::Function));
        assert_eq!(ObjectType::parse("table"), None);

        assert_eq!(
            ObjectType::Program.base_rel("ztest", ""),
            "programs/programs/ZTEST"
        );
        assert_eq!(
            ObjectType::Class.source_rel("zcl_foo", ""),
            "oo/classes/ZCL_FOO/source/main"
        );
        // 命名空间名：/ 编码为 %2F
        assert_eq!(
            ObjectType::Class.base_rel("/ui5/cl_repository_load", ""),
            "oo/classes/%2FUI5%2FCL_REPOSITORY_LOAD"
        );
        assert_eq!(
            ObjectType::Function.base_rel("z_fm", "zgroup"),
            "functions/groups/ZGROUP/fmodules/Z_FM"
        );
    }

    #[test]
    fn parse_lock_result_extracts_fields() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<asx:abap xmlns:asx="http://www.sap.com/abapxml" version="1.0">
  <asx:values>
    <DATA>
      <LOCK_HANDLE>grishko23TD5dTRff2WJt1Q==</LOCK_HANDLE>
      <CORRNR></CORRNR>
      <CORRUSER></CORRUSER>
      <CORRTEXT></CORRTEXT>
      <IS_LOCAL>X</IS_LOCAL>
      <MODIFICATION_SUPPORT>NoModification</MODIFICATION_SUPPORT>
    </DATA>
  </asx:values>
</asx:abap>"#;
        let lock = parse_lock_result(xml).unwrap();
        assert_eq!(lock.lock_handle, "grishko23TD5dTRff2WJt1Q==");
        assert!(lock.is_local);
        assert_eq!(lock.modification_support, "NoModification");
        assert!(lock.corrnr.is_empty());
    }

    #[test]
    fn parse_lock_result_reports_conflict_exception() {
        let xml = r#"<exc:exception xmlns:exc="http://www.sap.com/adt/exc">
  <type id="EU510"></type>
  <message>User DEVELOPER is currently editing the object</message>
</exc:exception>"#;
        let err = parse_lock_result(xml).unwrap_err();
        assert_eq!(err.status, 409);
        assert_eq!(err.key, "OBJECT_LOCKED");
        assert!(err.message.contains("DEVELOPER"));
        assert!(err.message.contains("EU510"));
    }

    #[test]
    fn parse_activation_empty_body_is_success() {
        let out = parse_activation_result("  \n");
        assert!(out.success);
        assert!(out.messages.is_empty());
    }

    #[test]
    fn parse_activation_refusal_with_line_from_href() {
        // HTTP 200 + 清单拒绝：E 型消息，行号在 href 片段（line 属性恒为 1）
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<chkl:checklist xmlns:chkl="http://www.sap.com/adt/checklist">
  <chkl:properties checkExecuted="true" activationExecuted="false"/>
  <msg type="W" line="1">
    <shortText><txt>Activation was cancelled.</txt><txt>"Editing canceled" (EU 202)</txt></shortText>
  </msg>
  <msg type="E" line="1" href="/sap/bc/adt/programs/programs/ZT/source/main#start=18,18">
    <shortText><txt>Tables with headers are no longer supported in the OO context.</txt></shortText>
  </msg>
</chkl:checklist>"#;
        let out = parse_activation_result(xml);
        assert!(!out.success);
        assert!(!out.activation_executed);
        assert_eq!(out.messages.len(), 2);
        // 拆成多个 <txt> 的 shortText 拼接完整
        assert!(out.messages[0].text.contains("Activation was cancelled."));
        assert!(out.messages[0].text.contains("EU 202"));
        // 行号取 href 片段而非 line 属性
        assert_eq!(out.messages[1].line, 18);
        // 问题行：E 优先，"Line 18: …" 形态
        assert_eq!(out.problems.len(), 1);
        assert!(out.problems[0].starts_with("Line 18: "));
        assert!(out.problems[0].contains("headers"));
    }

    #[test]
    fn parse_activation_success_with_warnings() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<chkl:checklist xmlns:chkl="http://www.sap.com/adt/checklist">
  <chkl:properties checkExecuted="true" activationExecuted="true"/>
  <msg type="W" line="3"><shortText><txt>Enhancement spot empty.</txt></shortText></msg>
</chkl:checklist>"#;
        let out = parse_activation_result(xml);
        assert!(out.success);
        assert_eq!(out.messages.len(), 1);
        assert!(out.problems.is_empty());
    }

    #[test]
    fn parse_syntax_result_extracts_line_offset() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<chkrun:checkRunReports xmlns:chkrun="http://www.sap.com/adt/checkrun">
  <chkrun:checkReport chkrun:uri="/sap/bc/adt/programs/programs/ZT#start=1,1">
    <chkrun:checkMessageList>
      <chkrun:checkMessage chkrun:uri="/sap/bc/adt/programs/programs/ZT/source/main#start=5,9"
        chkrun:type="E" chkrun:shortText='The type "ZFOO" is unknown.'/>
      <chkrun:checkMessage chkrun:uri="/sap/bc/adt/programs/programs/ZT/source/main#start=12,1"
        chkrun:type="W" chkrun:shortText="Unused variable LV_X."/>
    </chkrun:checkMessageList>
  </chkrun:checkReport>
</chkrun:checkRunReports>"#;
        let issues = parse_syntax_result(xml);
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].severity, "E");
        assert_eq!(issues[0].line, 5);
        assert_eq!(issues[0].offset, 9);
        assert!(issues[0].text.contains("ZFOO"));
        assert_eq!(issues[1].severity, "W");
        assert_eq!(issues[1].line, 12);
    }

    #[test]
    fn find_and_replace_unique_match() {
        let content = "REPORT ztest.\nDATA lv_x TYPE i.\nlv_x = 1.\n";
        let out = find_and_replace(content, "lv_x = 1.", "lv_x = 2.").unwrap();
        assert!(out.contains("lv_x = 2."));
        assert!(!out.contains("lv_x = 1."));
    }

    #[test]
    fn find_and_replace_rejects_ambiguous_and_missing() {
        let content = "a = 1.\nb = 1.\n";
        assert!(find_and_replace(content, "= 1.", "= 2.").is_err()); // 2 处
        assert!(find_and_replace(content, "no such line", "x").is_err()); // 0 处
        assert!(find_and_replace(content, "same", "same").is_err()); // 无变化
    }

    #[test]
    fn find_and_replace_eol_normalization() {
        // 内容 LF、old_string CRLF → 归一化后命中
        let content = "REPORT zt.\nWRITE 'x'.\n";
        let out = find_and_replace(content, "WRITE 'x'.\r\n", "WRITE 'y'.\r\n").unwrap();
        assert!(out.contains("WRITE 'y'."));
    }
    #[test]
    fn find_and_replace_case_insensitive_fallback() {
        // 大小写漂移：源码小写原文、锚点按大写化视图给（实测痛点）
        let content = "REPORT zprog.\nDATA lv_x TYPE i.\nlv_x = 1.";
        // 唯一命中（大小写不敏感）→ 兜底成功，new_string 原样写入
        let out = find_and_replace(content, "LV_X = 1.", "lv_x = 42.").unwrap();
        assert!(out.contains("lv_x = 42."), "{out}");
        assert!(out.contains("DATA lv_x"), "其余行保持原文: {out}");
        // 大小写不敏感多命中 → 明确报错
        let err = find_and_replace(content, "LV_X", "X").unwrap_err();
        assert!(err.contains("2 处"), "{err}");
    }

    #[test]
    fn find_and_replace_trailing_newline_fallback() {
        // 末行锚点多带 \n（历史必失败形态）
        let content = "REPORT zprog.\nWRITE 'end'.";
        let out = find_and_replace(content, "WRITE 'end'.\n", "WRITE 'new end'.").unwrap();
        assert_eq!(out, "REPORT zprog.\nWRITE 'new end'.");
    }

    #[test]
    fn find_and_replace_case_insensitive_with_crlf_restore() {
        // CRLF 原文 + 大小写兜底：写回时还原 CRLF 风格
        let content = "REPORT zprog.\r\nwrite 'x'.";
        let out = find_and_replace(content, "WRITE 'X'.", "WRITE 'y'.").unwrap();
        assert_eq!(out, "REPORT zprog.\r\nWRITE 'y'.");
    }

    #[test]
    fn find_and_replace_empty_old_only_on_empty_content() {
        assert_eq!(
            find_and_replace("", "", "REPORT znew.").unwrap(),
            "REPORT znew."
        );
        assert!(find_and_replace("existing", "", "x").is_err());
    }

    #[test]
    fn uri_fragment_parsing() {
        assert_eq!(uri_fragment_line("/x/source/main#start=18,7"), 18);
        assert_eq!(uri_fragment_col("/x/source/main#start=18,7"), 7);
        assert_eq!(uri_fragment_line("/x/source/main"), 0);
    }
}
