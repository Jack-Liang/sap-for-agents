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
                    if a.key.as_ref().starts_with("xmlns") {
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
                    if a.key.as_ref().starts_with("xmlns") {
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
            // quick-xml ≥0.42：文本事件不含实体（拆成 GeneralRef 独立事件），
            // 故按原始片段累积（Text 原文 + 重组的 &...;），End 时统一解码取 trim
            Ok(Event::Text(t)) => {
                if let Some(&idx) = open.last() {
                    elems[idx].text.push_str(t.as_ref());
                }
            }
            Ok(Event::GeneralRef(r)) => {
                if let Some(&idx) = open.last() {
                    let cell = &mut elems[idx];
                    cell.text.push('&');
                    cell.text.push_str(r.as_ref());
                    cell.text.push(';');
                }
            }
            Ok(Event::End(_)) => {
                if let Some(idx) = open.pop() {
                    let decoded = crate::dumps::xml_decode_of(&elems[idx].text);
                    elems[idx].text = decoded.trim().to_string();
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break, // 容错：解析不了就返回已收集的部分
        }
    }
    elems
}

fn local_name(qname: &str) -> &str {
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
    match count {
        0 => {
            // EOL 归一化重试（源码 LF、调用方给 CRLF 是最常见的失配原因）
            let norm_content = content.replace("\r\n", "\n");
            let norm_old = old.replace("\r\n", "\n");
            if norm_content.contains(&norm_old) {
                let norm_new = new.replace("\r\n", "\n");
                let updated = norm_content.replace(&norm_old, &norm_new);
                return Ok(if content.contains("\r\n") {
                    // 原文是 CRLF 风格则还原
                    restored_crlf(&updated)
                } else {
                    updated
                });
            }
            Err("找不到 old_string；请先读当前源码，按原文精确匹配（含缩进与空格）".into())
        }
        1 => Ok(content.replacen(old, new, 1)),
        n => Err(format!(
            "old_string 命中 {} 处，必须恰好 1 处；请附带更多上下文行使其唯一",
            n
        )),
    }
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
