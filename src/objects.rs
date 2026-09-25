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
    Include,
    Class,
    Interface,
    Function,
    FunctionGroup,
    CdsView,
    /// DDIC 透明表（源码化 DDIC「blue 对象」）：无编辑锁，etag 乐观并发
    Table,
    /// DDIC 结构（同为 blue 对象，`define structure` 方言）：与表同一套契约
    Structure,
    Package,
}

impl ObjectType {
    /// 路径段里的类型别名（vsp/abapfs 的类型注册表口径）。
    /// 注意：fugr 归函数组（FUGR/F），func 才是函数模块（FUGR/FF）。
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "prog" | "program" | "report" => Some(Self::Program),
            "incl" | "include" => Some(Self::Include),
            "class" | "clas" => Some(Self::Class),
            "intf" | "interface" => Some(Self::Interface),
            "func" | "function" | "fm" => Some(Self::Function),
            "fugr" | "fgroup" | "group" => Some(Self::FunctionGroup),
            "cds" | "ddls" => Some(Self::CdsView),
            "tabl" | "table" | "dtab" => Some(Self::Table),
            "stru" | "structure" => Some(Self::Structure),
            "package" | "pkg" | "devclass" => Some(Self::Package),
            _ => None,
        }
    }

    /// 是否源码化 DDIC「blue 对象」（表/结构）：走无锁 etag 写入通道。
    /// 其余 DDIC 类型（域/数据元素/表类型/视图）是结构编辑器对象，
    /// 无文本源码，不在此列（A4H 816 真机实证）。
    pub fn is_blue_ddic(self) -> bool {
        matches!(self, Self::Table | Self::Structure)
    }

    /// API 使用的规范类型名（响应 `type` 字段 / MCP 枚举）。
    pub fn api_name(&self) -> &'static str {
        match self {
            Self::Program => "prog",
            Self::Include => "incl",
            Self::Class => "class",
            Self::Interface => "intf",
            Self::Function => "func",
            Self::FunctionGroup => "fugr",
            Self::CdsView => "cds",
            Self::Table => "tabl",
            Self::Structure => "stru",
            Self::Package => "package",
        }
    }

    /// ADT 类型 ID（adtcore:type）。
    fn adt_type_id(&self) -> &'static str {
        match self {
            Self::Program => "PROG/P",
            Self::Include => "PROG/I",
            Self::Class => "CLAS/OC",
            Self::Interface => "INTF/OI",
            Self::Function => "FUGR/FF",
            Self::FunctionGroup => "FUGR/F",
            Self::CdsView => "DDLS/DF",
            Self::Table => "TABL/DT",
            Self::Structure => "STRU/DT",
            Self::Package => "DEVC/K",
        }
    }

    /// 是否有源码资源（package 无源码，只有元数据）。
    pub fn has_source(&self) -> bool {
        !matches!(self, Self::Package)
    }

    /// 是否需要函数组（仅函数模块：URL 嵌在组下）。
    pub fn needs_group(&self) -> bool {
        matches!(self, Self::Function)
    }

    /// URL 段大小写：CDS 家族用小写（vsp/abapfs 双实证），其余大写。
    fn url_name(&self, name: &str) -> String {
        let n = name.trim();
        let cased: String = if matches!(self, Self::CdsView) {
            n.to_lowercase()
        } else {
            n.to_uppercase()
        };
        encode_path_segment(&cased)
    }

    /// 对象基础资源（不含 /source/main 后缀，锁/解锁/删除用这个）。
    /// `group` 仅 Function 需要（调用方已反解）。
    pub fn base_rel(&self, name: &str, group: &str) -> String {
        match self {
            Self::Program => format!("programs/programs/{}", self.url_name(name)),
            Self::Include => format!("programs/includes/{}", self.url_name(name)),
            Self::Class => format!("oo/classes/{}", self.url_name(name)),
            Self::Interface => format!("oo/interfaces/{}", self.url_name(name)),
            Self::Function => format!(
                "functions/groups/{}/fmodules/{}",
                encode_path_segment(&group.trim().to_uppercase()),
                self.url_name(name)
            ),
            Self::FunctionGroup => format!("functions/groups/{}", self.url_name(name)),
            Self::CdsView => format!("ddic/ddl/sources/{}", self.url_name(name)),
            Self::Table => format!("ddic/tables/{}", self.url_name(name)),
            Self::Structure => format!("ddic/structures/{}", self.url_name(name)),
            Self::Package => format!("packages/{}", self.url_name(name)),
        }
    }

    /// 源码资源（写/读/语法检查的 artifact 地址）。
    pub fn source_rel(&self, name: &str, group: &str) -> String {
        format!("{}/source/main", self.base_rel(name, group))
    }

    /// 创建集合资源（POST 目标）+ XML 根元素与命名空间。
    /// Function 的父级是函数组（containerRef），其余是包（packageRef）。
    fn creation(&self, group: &str) -> (String, &'static str, &'static str) {
        match self {
            Self::Program => (
                "programs/programs".into(),
                "program:abapProgram",
                "http://www.sap.com/adt/programs/programs",
            ),
            Self::Include => (
                "programs/includes".into(),
                "include:abapInclude",
                "http://www.sap.com/adt/programs/includes",
            ),
            Self::Class => (
                "oo/classes".into(),
                "class:abapClass",
                "http://www.sap.com/adt/oo/classes",
            ),
            Self::Interface => (
                "oo/interfaces".into(),
                "intf:abapInterface",
                "http://www.sap.com/adt/oo/interfaces",
            ),
            Self::Function => (
                format!(
                    "functions/groups/{}/fmodules",
                    encode_path_segment(&group.trim().to_uppercase())
                ),
                "fmodule:abapFunctionModule",
                "http://www.sap.com/adt/functions/fmodules",
            ),
            Self::FunctionGroup => (
                "functions/groups".into(),
                "group:abapFunctionGroup",
                "http://www.sap.com/adt/functions/groups",
            ),
            Self::CdsView => (
                "ddic/ddl/sources".into(),
                "ddl:ddlSource",
                "http://www.sap.com/adt/ddic/ddlsources",
            ),
            // 表/结构是「源码化 DDIC」（blue 对象）：创建文档的根**就是**
            // blueSource 本身（adtcore 属性 + packageRef 子元素）——发 tbl:table
            // 根会被要求内嵌 blueSource 而内嵌源码又不被接受（A4H 816 真机实证）。
            // 结构的 DDL 方言是 define structure，表是 define table，其余契约一致。
            Self::Table | Self::Structure => (
                if matches!(self, Self::Table) {
                    "ddic/tables".into()
                } else {
                    "ddic/structures".into()
                },
                "blue:blueSource",
                "http://www.sap.com/wbobj/blue",
            ),
            // Package 走专用富 payload（见 create_package_body），不用通用模板
            Self::Package => (
                "packages".into(),
                "pack:package",
                "http://www.sap.com/adt/packages",
            ),
        }
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
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
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
        // 失败响应体带 SAP 的一手原因（会话限额/授权/服务状态），必须落日志——
        // 只给调用方一个 502 会把真实原因埋掉
        tracing::warn!(
            status = resp.status,
            base = %base,
            body = %truncate(&body, 1200),
            "ADT 锁定失败原始响应"
        );
        if body.contains("exc:exception") {
            return Err(adt_exception_error(&body, "锁定对象失败"));
        }
        return Err(RfcError {
            code: -1,
            status: 502,
            message: format!("锁定对象失败（ADT 返回 {}）: {}", resp.status, truncate(&body, 200)),
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
    for r in elems
        .iter()
        .filter(|e| e.name == "ref" && e.parent_is("object"))
    {
        if let Some(uri) = r.attr("uri") {
            out.inactive.push(uri.to_string());
        }
    }

    let has_error = out
        .messages
        .iter()
        .any(|m| m.severity.contains('E') || m.severity.contains('A') || m.severity.contains('X'));
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
            out.problems
                .push("激活被拒绝且 SAP 未说明原因；对象仍处于未激活状态".into());
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
    Ok(parse_activation_result(&String::from_utf8_lossy(
        &resp.body,
    )))
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
    /// 是否已把函数模块设为 remote-enabled（rfc_enabled:true 时出现）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rfc_enabled: Option<bool>,
    /// 注册表自动登记的 alias（remote-enabled 函数写入成功后出现；
    /// 登记失败不出现，只在 warnings 里说明）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registered_alias: Option<String>,
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
    /// 开发包（默认 $TMP；生产包需配合 transport）。Package 类型时表示父包（superPackage）
    pub devclass: String,
    /// 传输请求号（可选）
    pub transport: Option<String>,
    /// 仅 Package：软件组件（缺省按 ZLOCAL→LOCAL→HOME 逐个试）
    pub software_component: Option<String>,
}

/// ADT 创建是否「已存在」类错误（唯一不回退 RFC 的 4xx：重试也无意义）。
fn is_already_exists(msg: &str) -> bool {
    msg.contains("already exist")
        || msg.contains("already exists")
        || msg.contains("does already exist")
}

/// 通用创建 body（vsp/abapfs 同款契约）：adtcore 属性 + packageRef / containerRef。
/// `group` 仅 Function 用（containerRef 指向所属函数组）。
fn create_body_simple(
    obj_type: ObjectType,
    name: &str,
    group: &str,
    spec: &CreateSpec,
    responsible: Option<&str>,
) -> String {
    let (_, root, ns) = obj_type.creation(group);
    // 根元素名带类型专属前缀（group:/class:/fmodule:…），命名空间声明必须用同一前缀
    let prefix = root.split(':').next().unwrap_or("adtcore");
    let resp = responsible
        .map(|u| format!(" adtcore:responsible=\"{}\"", xml_attr_escape(u)))
        .unwrap_or_default();
    let parent = if obj_type == ObjectType::Function {
        // containerRef 带组名 + 组 URI（vsp 实证形态；URI 里的组名为小写）
        format!(
            "<adtcore:containerRef adtcore:name=\"{}\" adtcore:type=\"FUGR/F\" \
             adtcore:uri=\"/sap/bc/adt/functions/groups/{}\"/>",
            xml_attr_escape(group.trim().to_uppercase().as_str()),
            encode_path_segment(&group.trim().to_lowercase())
        )
    } else {
        format!(
            "<adtcore:packageRef adtcore:name=\"{}\"/>",
            xml_attr_escape(devclass_or_tmp(spec).as_str())
        )
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<{root} xmlns:{prefix}=\"{ns}\" xmlns:adtcore=\"http://www.sap.com/adt/core\" \
adtcore:description=\"{desc}\" adtcore:name=\"{name}\" adtcore:type=\"{tid}\"{resp}>\
{parent}\
</{root}>",
        root = root,
        prefix = prefix,
        ns = ns,
        desc = xml_attr_escape(spec.description.trim()),
        name = xml_attr_escape(name.trim()),
        tid = obj_type.adt_type_id(),
        resp = resp,
        parent = parent,
    )
}

/// Package 富 payload（vsp 实证：attributes/superPackage/transport 完整结构）。
fn create_package_body(
    name: &str,
    spec: &CreateSpec,
    swc: &str,
    responsible: Option<&str>,
) -> String {
    let resp = responsible
        .map(|u| format!(" adtcore:responsible=\"{}\"", xml_attr_escape(u)))
        .unwrap_or_default();
    let super_pkg = if spec.devclass.trim().is_empty() {
        "<pack:superPackage/>".to_string()
    } else {
        format!(
            "<pack:superPackage adtcore:name=\"{}\" adtcore:type=\"DEVC/K\"/>",
            xml_attr_escape(spec.devclass.trim())
        )
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<pack:package xmlns:pack=\"http://www.sap.com/adt/packages\" \
xmlns:adtcore=\"http://www.sap.com/adt/core\" \
adtcore:description=\"{desc}\" adtcore:name=\"{name}\" adtcore:type=\"DEVC/K\"{resp}>\
<pack:attributes pack:packageType=\"development\"/>\
{super_pkg}\
<pack:applicationComponent/>\
<pack:transport><pack:softwareComponent pack:name=\"{swc}\"/><pack:transportLayer pack:name=\"\"/></pack:transport>\
<pack:translation/><pack:useAccesses/><pack:packageInterfaces/><pack:subPackages/>\
</pack:package>",
        desc = xml_attr_escape(spec.description.trim()),
        name = xml_attr_escape(name.trim()),
        super_pkg = super_pkg,
        swc = xml_attr_escape(swc),
    )
}

// 辅助：默认开发包（$TMP）
fn devclass_or_tmp(spec: &CreateSpec) -> String {
    let d = spec.devclass.trim();
    if d.is_empty() {
        "$TMP".into()
    } else {
        d.to_uppercase()
    }
}

/// ADT 创建单次 POST。返回 Err 时 message 已含 SAP 的说法。
async fn create_post_adt(rel: &str, body: &str, transport: Option<&str>) -> Result<(), RfcError> {
    let mut query: Vec<(&str, &str)> = Vec::new();
    if let Some(t) = transport.filter(|t| !t.trim().is_empty()) {
        query.push(("corrNr", t));
    }
    let resp = adt_request_raw(
        axum::http::Method::POST,
        rel,
        &query,
        Some(body.as_bytes()),
        Some("application/*"),
        "*/*",
        &[], // 无额外头；CSRF 由网关自动处理
        None,
        None,
    )
    .await?;
    if (200..300).contains(&resp.status) {
        return Ok(());
    }
    let body_text = String::from_utf8_lossy(&resp.body).to_string();
    let (status, message, key) = if body_text.contains("exc:exception") {
        let e = adt_exception_error(&body_text, "ADT 创建对象失败");
        (e.status, e.message, e.key)
    } else {
        (
            502u16,
            format!(
                "ADT 创建失败（{}）: {}",
                resp.status,
                truncate(&body_text, 300)
            ),
            "CREATE_FAILED".to_string(),
        )
    };
    Err(RfcError {
        code: -1,
        status,
        message,
        key,
    })
}

/// ADT 标准创建（Eclipse/vsp/abapfs 同款 objectcreation XML POST）。
/// 适用于所有类型；Function 在组缺失时自动经 ADT 建组后重试一次。
async fn create_object_adt(
    obj_type: ObjectType,
    name: &str,
    group: &str,
    spec: &CreateSpec,
    responsible: Option<&str>,
) -> Result<(), RfcError> {
    let transport = spec.transport.as_deref();
    if obj_type == ObjectType::Package {
        // 软件组件因系统而异（A4H 认 ZLOCAL、BTP trial 认 LOCAL…）——逐个候选试，
        // 全部落败返回最后一个错误（vsp/本网关真机双实证）
        let mut candidates: Vec<String> = Vec::new();
        if let Some(swc) = spec
            .software_component
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            candidates.push(swc.trim().to_string());
        }
        for fallback in ["ZLOCAL", "LOCAL", "HOME"] {
            if !candidates.iter().any(|c| c.eq_ignore_ascii_case(fallback)) {
                candidates.push(fallback.to_string());
            }
        }
        let mut last: Option<RfcError> = None;
        for swc in &candidates {
            let body = create_package_body(name, spec, swc, responsible);
            match create_post_adt("packages", &body, transport).await {
                Ok(()) => return Ok(()),
                Err(e) if is_already_exists(&e.message) => return Err(e),
                Err(e) => last = Some(e),
            }
        }
        return Err(last.unwrap_or_else(|| RfcError {
            code: -1,
            status: 502,
            message: "ADT 创建包失败（无可用软件组件候选）".into(),
            key: "CREATE_FAILED".into(),
        }));
    }

    // Function：组不存在 → 先经 ADT 建组再重试。组的包候选：显式 → $TMP → ZLOCAL
    // （A4H $TMP 可用；ABAP Cloud Trial 常要求 ZLOCAL。ADT 建组在该环境同样有效——
    //  这是 RFC RS_FUNCTION_POOL_INSERT「返回成功却不写 TADIR」限制的正解）
    if obj_type == ObjectType::Function {
        let (rel, _, _) = obj_type.creation(group);
        let body = create_body_simple(obj_type, name, group, spec, responsible);
        let first = create_post_adt(&rel, &body, transport).await;
        let group_missing = match &first {
            Err(e) => e.key == "ADT_ExceptionResourceNotFound" || e.message.contains("not exist"),
            Ok(()) => false,
        };
        if !group_missing {
            return first;
        }
        for dev in dedup_candidates(&devclass_or_tmp(spec), &["ZLOCAL"]) {
            let gspec = CreateSpec {
                description: format!("function group {}", group.trim().to_uppercase()),
                devclass: dev,
                transport: spec.transport.clone(),
                software_component: None,
            };
            let (grel, _, _) = ObjectType::FunctionGroup.creation("");
            let gbody =
                create_body_simple(ObjectType::FunctionGroup, group, "", &gspec, responsible);
            match create_post_adt(&grel, &gbody, transport).await {
                // 建组成（或组其实已在——并发场景）即跳出；失败换下一个包候选
                Ok(()) => break,
                Err(ref e2) if is_already_exists(&e2.message) => break,
                Err(_) => continue,
            }
        }
        // 组就位（或建组失败——FM 重试自己会给准确错误）后重试一次
        return create_post_adt(&rel, &body, transport).await;
    }

    let (rel, _, _) = obj_type.creation(group);
    // blueSource 创建契约不含 responsible（服务端自动记创建用户），不带更稳
    let responsible = if obj_type.is_blue_ddic() {
        None
    } else {
        responsible
    };
    let body = create_body_simple(obj_type, name, group, spec, responsible);
    create_post_adt(&rel, &body, transport).await
}

/// 去重的包候选列表（首项显式给出，后接 fallback）。
fn dedup_candidates(explicit: &str, fallbacks: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if !explicit.trim().is_empty() {
        out.push(explicit.trim().to_uppercase());
    }
    for f in fallbacks {
        if !out.iter().any(|c| c.eq_ignore_ascii_case(f)) {
            out.push(f.to_string());
        }
    }
    out
}

/// 创建对象（对外入口）：**ADT-first**，prog/func 在 ADT 不可用/失败时回退 RFC RPY 路径。
/// prog/func 的 RFC 路径（实测可用、无 schema 猜谜）保留为兜底；其余类型只有 ADT 通道。
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
    if obj_type.needs_group() && group.trim().is_empty() {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "func 创建需要 group（函数组），或传 group_hint 由网关反解".into(),
            key: "CREATE_GROUP_REQUIRED".into(),
        });
    }
    let responsible = crate::adt::adt_user();
    let adt_result = create_object_adt(obj_type, name, group, spec, responsible.as_deref()).await;
    match adt_result {
        Ok(()) => Ok(()),
        // 已存在：重试任何路径都无意义，直接给 409
        Err(e) if is_already_exists(&e.message) => Err(RfcError {
            code: -1,
            status: 409,
            message: e.message,
            key: "OBJECT_EXISTS".into(),
        }),
        // prog/func：ADT 失败回退 RFC（老系统 ADT 服务缺失 / RPY 在该系统更宽容）
        Err(e) if matches!(obj_type, ObjectType::Program | ObjectType::Function) => {
            match create_object_rfc(pool, obj_type, name, group, spec).await {
                Ok(()) => Ok(()),
                Err(_) => Err(e), // RFC 也失败：ADT 的错误信息通常更标准，回抛 ADT 错误
            }
        }
        Err(e) => Err(e),
    }
}

/// RFC 通道创建（prog=RPY_PROGRAM_INSERT、func=RPY_FUNCTIONMODULE_INSERT + 自动建组）。
/// 仅作为 ADT 通道的回退；逻辑与历史版本一致（含 ABAP Cloud Trial 的 TADIR 自检）。
async fn create_object_rfc(
    pool: &std::sync::Arc<crate::pool::RfcConnectionPool>,
    obj_type: ObjectType,
    name: &str,
    group: &str,
    spec: &CreateSpec,
) -> Result<(), RfcError> {
    match obj_type {
        ObjectType::Program => {
            let pname = name.to_uppercase();
            let title = spec.description.clone();
            let devclass = devclass_or_tmp(spec);
            let transport = spec.transport.clone().unwrap_or_default();
            crate::server::run_blocking(std::sync::Arc::clone(pool), move |conn| {
                let req = crate::api::InvokeRequest {
                    func_name: "RPY_PROGRAM_INSERT".to_string(),
                    inputs: HashMap::from([
                        (
                            "PROGRAM_NAME".to_string(),
                            ScalarValue::Chars(pname.clone()),
                        ),
                        (
                            "PROGRAM_TYPE".to_string(),
                            ScalarValue::Chars("1".to_string()),
                        ),
                        (
                            "TITLE_STRING".to_string(),
                            ScalarValue::Chars(title.clone()),
                        ),
                        (
                            "DEVELOPMENT_CLASS".to_string(),
                            ScalarValue::Chars(devclass.clone()),
                        ),
                        // 免交互 + 直接保存
                        (
                            "SUPPRESS_DIALOG".to_string(),
                            ScalarValue::Chars("X".to_string()),
                        ),
                        (
                            "SAVE_INACTIVE".to_string(),
                            ScalarValue::Chars(" ".to_string()),
                        ),
                        ("TEMPORARY".to_string(), ScalarValue::Chars(" ".to_string())),
                        ("STATUS".to_string(), ScalarValue::Chars("A".to_string())),
                        (
                            "APPLICATION".to_string(),
                            ScalarValue::Chars(" ".to_string()),
                        ),
                        (
                            "AUTHORIZATION_GROUP".to_string(),
                            ScalarValue::Chars(" ".to_string()),
                        ),
                        ("EDIT_LOCK".to_string(), ScalarValue::Chars(" ".to_string())),
                        (
                            "TRANSPORT_NUMBER".to_string(),
                            ScalarValue::Chars(transport.clone()),
                        ),
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
            let fname = name.to_uppercase();
            let fgroup = group.trim().to_uppercase();
            let short = spec.description.clone();
            let transport = spec.transport.clone().unwrap_or_default();
            let devclass = devclass_or_tmp(spec);
            let fm_insert = |fname: String, fgroup: String, short: String, transport: String| {
                move |conn: &crate::connection::RfcConnection| -> Result<(), RfcError> {
                    let req = InvokeRequest {
                        func_name: "RPY_FUNCTIONMODULE_INSERT".to_string(),
                        inputs: HashMap::from([
                            ("FUNCNAME".to_string(), ScalarValue::Chars(fname.clone())),
                            (
                                "FUNCTION_POOL".to_string(),
                                ScalarValue::Chars(fgroup.clone()),
                            ),
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
                fm_insert(
                    fname.clone(),
                    fgroup.clone(),
                    short.clone(),
                    transport.clone(),
                ),
            )
            .await
            {
                Ok(()) => Ok(()),
                // 组不存在 → 自动建组（RS_FUNCTION_POOL_INSERT）后重试一次。
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
                                    (
                                        "SUPPRESS_CORR_CHECK".to_string(),
                                        ScalarValue::Chars("X".to_string()),
                                    ),
                                    (
                                        "SUPPRESS_LANGUAGE_CHECK".to_string(),
                                        ScalarValue::Chars("X".to_string()),
                                    ),
                                    (
                                        "AUTHORITY_CHECK".to_string(),
                                        ScalarValue::Chars(" ".to_string()),
                                    ),
                                    ("NAMESPACE".to_string(), ScalarValue::Chars(" ".to_string())),
                                    (
                                        "RESPONSIBLE".to_string(),
                                        ScalarValue::Chars(" ".to_string()),
                                    ),
                                    (
                                        "UNICODE_CHECKS".to_string(),
                                        ScalarValue::Chars(" ".to_string()),
                                    ),
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
                        let registered =
                            crate::server::run_blocking(std::sync::Arc::clone(pool), move |conn| {
                                let rows = crate::discovery::read_table(
                                    conn,
                                    "TADIR",
                                    &["OBJECT".to_string(), "OBJ_NAME".to_string()],
                                    &[format!("OBJECT = 'FUGR' AND OBJ_NAME = '{}'", g_chk)],
                                    1,
                                    '\u{1}',
                                )
                                .unwrap_or_default();
                                Ok::<bool, RfcError>(!rows.is_empty())
                            })
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
        _ => Err(RfcError {
            code: -1,
            status: 502,
            message: "该对象类型无 RFC 创建通道（仅 ADT）".into(),
            key: "CREATE_NO_RFC_FALLBACK".into(),
        }),
    }
}

// （CreateSpec 直接手工构造，无需 clone helper）

/// 写入（可选对象不存在时先建壳）：PUT source 语义的共享入口。
/// `create_desc` 为 Some 时启用自动创建（描述即 title）。
/// 写入选项（maybe_create 系列共用）
pub struct WriteOpts<'a> {
    pub transport: Option<&'a str>,
    pub activate: bool,
    /// Some(description) 启用「对象不存在时自动创建」
    pub create_desc: Option<&'a str>,
    /// 函数模块写入后设为 remote-enabled（processingType=rfc，元数据 PUT）
    pub rfc_enabled: bool,
    /// 随代码写入的接口文档（func 类）：存入注册表条目、流入 OpenAPI 目录正文。
    /// None = 不动条目已有文档；Some("") 视为未提供。
    pub doc: Option<&'a str>,
}

pub async fn write_object_maybe_create(
    pool: &std::sync::Arc<crate::pool::RfcConnectionPool>,
    obj_type: ObjectType,
    name: &str,
    group: &str,
    source: &str,
    opts: &WriteOpts<'_>,
) -> Result<WriteOutcome, RfcError> {
    let WriteOpts {
        transport,
        activate,
        create_desc,
        rfc_enabled,
        doc,
    } = *opts;
    match write_object_source(
        obj_type, name, group, source, transport, activate, rfc_enabled, doc,
    )
    .await
    {
        Ok(o) => {
            drain_pool_after_write(pool, obj_type);
            Ok(o)
        }
        Err(e) if create_desc.is_some() && is_object_not_exist(&e) => {
            let spec = CreateSpec {
                description: create_desc.unwrap_or_default().to_string(),
                devclass: String::new(),
                transport: transport.map(str::to_string),
                software_component: None,
            };
            create_object(pool, obj_type, name, group, &spec).await?;
            let out = write_object_source(
                obj_type, name, group, source, transport, activate, rfc_enabled, doc,
            )
            .await?;
            drain_pool_after_write(pool, obj_type);
            Ok(out)
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
    let WriteOpts {
        transport,
        activate,
        create_desc,
        rfc_enabled,
        doc,
    } = *opts;
    let current = match read_current_source(obj_type, name, group).await {
        Ok(c) => c,
        Err(e) if create_desc.is_some() && is_object_not_exist(&e) => {
            let spec = CreateSpec {
                description: create_desc.unwrap_or_default().to_string(),
                devclass: String::new(),
                transport: transport.map(str::to_string),
                software_component: None,
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
    let outcome = write_object_source(
        obj_type, name, group, &updated, transport, activate, rfc_enabled, doc,
    )
    .await?;
    drain_pool_after_write(pool, obj_type);
    let mut v = serde_json::to_value(&outcome).unwrap_or_default();
    v["replaced"] = serde_json::json!(true);
    Ok(v)
}

/// 判断写入错误是否为「对象不存在」（可配合 create:true 自动建壳重试）。
pub fn is_object_not_exist(err: &RfcError) -> bool {
    (err.status == 409 && err.key == "ADT_ExceptionResourceNotFound")
        || err.message.contains("does not exist")
}

/// 写钩子的池侧失效：func/fugr 变更影响 SAP 按连接缓存的函数组加载——
/// 排干空闲 RFC 连接，让下一次调用必然重建连接、重新加载组。
/// （FUNC_CACHE 的清理在 write_object_source/delete_object 内部已做。）
/// 对其他类型是无害空操作。
pub fn drain_pool_after_write(
    pool: &std::sync::Arc<crate::pool::RfcConnectionPool>,
    obj_type: ObjectType,
) {
    if matches!(obj_type, ObjectType::Function | ObjectType::FunctionGroup) {
        pool.drain_idle();
    }
}

/// 编排写入：锁 → PUT 源码 →（可选）rfc 元数据 PUT → 解锁 → 激活。
///
/// - `transport`: 调用方指定的传输请求号；未指定时复用对象已绑定的
///   CORRNR（vsp issue #144：已捕获对象不传会收到假 409）；
/// - `rfc_enabled`: 仅 Function；在同一把锁下 PUT 模块元数据把
///   processingType 置为 rfc（描述先读回再带上——PUT 是整文档替换，vsp 实证）；
/// - 函数模块源码先做 SEDI 规范化（经典 `*" 注释块`签名 → FUNCTION 语句内联，
///   见 [`normalize_function_source`]），否则新版 ADT 以 400
///   "Parameter comment blocks are not allowed" 拒收；
/// - 激活是逻辑结果不是传输错误：失败时 Ok(WriteOutcome{activated:
///   Some(失败详情)})，由调用方决定如何呈现；
/// - PUT 失败会尽力解锁后返回错误，避免泄漏孤儿锁。
#[allow(clippy::too_many_arguments)]
pub async fn write_object_source(
    obj_type: ObjectType,
    name: &str,
    group: &str,
    source: &str,
    transport: Option<&str>,
    activate: bool,
    rfc_enabled: bool,
    doc: Option<&str>,
) -> Result<WriteOutcome, RfcError> {
    if !obj_type.has_source() {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "该对象类型无源码资源（package 只有元数据）".into(),
            key: "NO_SOURCE_RESOURCE".into(),
        });
    }
    // 表/结构（blue 对象）不走锁编排：etag 乐观并发，单独通道
    if obj_type.is_blue_ddic() {
        return write_ddic_source(obj_type, name, source, transport, activate).await;
    }
    let type_name = obj_type.api_name();
    let base = obj_type.base_rel(name, group);
    let source_rel = obj_type.source_rel(name, group);
    // FM 源码规范化：经典注释块签名在这台 SEDI 系统会被拒收
    let source_owned: String;
    let source = if obj_type == ObjectType::Function {
        source_owned = normalize_function_source(source);
        &source_owned
    } else {
        source
    };
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

    // ②' 同一把锁下把 FM 设为 remote-enabled（元数据 PUT；源码已落库，
    //     失败不回滚只降级为警告 + rfc_enabled:false）
    let mut rfc_result: Option<bool> = None;
    if rfc_enabled && obj_type == ObjectType::Function {
        match fm_set_rfc_enabled(
            &base,
            &group_or_name(obj_type, name, group),
            &mut sess,
            &lock.lock_handle,
            transport_used.as_deref(),
        )
        .await
        {
            Ok(()) => rfc_result = Some(true),
            Err(w) => {
                warnings.push(format!("设置 remote-enabled 失败: {}", w));
                rfc_result = Some(false);
            }
        }
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

    // ⑤ 缓存失效：函数写入成功即清 FUNC_CACHE——下一次接口自省/auto_outputs
    //    必然拿到新签名（否则读到的是写入前的快照）。注意 SAP 侧函数组加载
    //    仍按连接缓存（池排干由调用方 drain_pool_after_write 处理）、SDK 描述符
    //    缓存进程级不可失效（删库重建的 FM 新参数要重启网关才可见，见 AGENTS.md）。
    if obj_type == ObjectType::Function {
        let cleared = crate::metadata::invalidate_function(name.trim());
        if cleared {
            tracing::debug!(func = %name.trim(), "写后清除 FUNC_CACHE 条目");
        }
    }

    // ⑥ 注册表自动登记：remote-enabled 函数写入成功即获得 draft 条目
    //    （幂等；保留 Agent 已写的 intent/notes）。doc 随代码更新条目文档。
    //    登记失败只降级为警告——注册表是网关本地状态，绝不让它影响 SAP 写入结果。
    let mut registered_alias = None;
    if obj_type == ObjectType::Function && rfc_result == Some(true) {
        let doc = doc.filter(|d| !d.trim().is_empty());
        match crate::registry::auto_register_func(&name_upper, (!group.trim().is_empty()).then(|| group.trim()), doc) {
            Ok(alias) => registered_alias = Some(alias),
            Err(e) => warnings.push(format!("注册表自动登记失败（不影响写入）: {}", e.message)),
        }
    }

    Ok(WriteOutcome {
        obj_type: type_name.to_string(),
        name: name.trim().to_uppercase(),
        group: obj_type.needs_group().then(|| group.to_uppercase()),
        source_url: source_rel,
        written: true,
        transport_used,
        rfc_enabled: rfc_result,
        registered_alias,
        activated,
        warnings,
    })
}

/// 表/结构（blue 对象）写入编排：GET etag → PUT 源码 → 激活。**全程无锁**。
///
/// DDIC 表和结构是「源码化 DDIC」，ADT 对它们不用编辑锁而用 etag 乐观并发——
/// 没有 stateful 会话、没有 lockHandle，也就没有 423 InvalidLockHandle 一族
/// 的问题（与 prog/class 的锁编排是两套并发模型）。etag 有两个真机实证的
/// 怪癖，都已在实现里吸收：
///
/// - **裸 `text/plain`**：服务端把请求的 content-type 字符串算进 etag，
///   GET（Accept）与 PUT（Content-Type）不一致就 412——charset 后缀都不行；
/// - **If-Match 不带引号**：ADT 把引号当作 etag 值的一部分（RFC 7232 反例）。
///
/// 对象不存在时 GET 的错误经 [`adt_exception_error`] 归一为
/// 409/ADT_ExceptionResourceNotFound，与锁编排路径同形态——
/// `create:true` 的建壳重试因此对表/结构同样生效。
async fn write_ddic_source(
    obj_type: ObjectType,
    name: &str,
    source: &str,
    transport: Option<&str>,
    activate: bool,
) -> Result<WriteOutcome, RfcError> {
    let base = obj_type.base_rel(name, "");
    let source_rel = obj_type.source_rel(name, "");

    // ① GET 现源拿 etag（裸 text/plain，与 PUT 的 Content-Type 同串）
    let get_resp = adt_request_raw(
        axum::http::Method::GET,
        &source_rel,
        &[],
        None,
        None,
        "text/plain",
        &[],
        None,
        None,
    )
    .await?;
    if !(200..300).contains(&get_resp.status) {
        let body = String::from_utf8_lossy(&get_resp.body).to_string();
        if body.contains("exc:exception") {
            // 对象缺失的 ADT 说法是 "Error while importing object X from the
            // database"——归一后 is_object_not_exist 可识别
            return Err(adt_exception_error(&body, "读取 DDIC 源码失败"));
        }
        return Err(RfcError {
            code: -1,
            status: 502,
            message: format!(
                "读取 DDIC 源码失败（ADT 返回 {}）: {}",
                get_resp.status,
                truncate(&body, 300)
            ),
            key: "ADT_SOURCE_UNAVAILABLE".into(),
        });
    }
    let etag = get_resp.etag.ok_or_else(|| RfcError {
        code: -1,
        status: 502,
        message: "DDIC 源码读取成功但未返回 ETag（无法做乐观并发写入）".into(),
        key: "ETAG_MISSING".into(),
    })?;

    // ② PUT（If-Match 裸值；corrNr 仅显式给 transport 时带——无锁响应可复用）
    let transport_used = transport
        .map(str::to_string)
        .filter(|t| !t.trim().is_empty());
    let mut query: Vec<(&str, &str)> = Vec::new();
    if let Some(t) = transport_used.as_deref() {
        query.push(("corrNr", t));
    }
    let put_resp = adt_request_raw(
        axum::http::Method::PUT,
        &source_rel,
        &query,
        Some(source.as_bytes()),
        Some("text/plain"),
        "*/*",
        &[("If-Match", etag.as_str())],
        None,
        None,
    )
    .await?;
    if !(200..300).contains(&put_resp.status) {
        let body = String::from_utf8_lossy(&put_resp.body).to_string();
        let (status, key) = if put_resp.status == 412 {
            (412u16, "ETAG_CONFLICT".to_string())
        } else if put_resp.status == 409 {
            (409, "OBJECT_LOCKED".to_string())
        } else {
            (502, "ADT_WRITE_FAILED".to_string())
        };
        return Err(RfcError {
            code: -1,
            status,
            message: format!(
                "写入 DDIC 源码失败（ADT 返回 {}）: {}",
                put_resp.status,
                truncate(&body, 300)
            ),
            key,
        });
    }

    // ③ 激活（与锁编排同一通道；逻辑结果，不升为传输错误）
    let name_upper = name.trim().to_uppercase();
    let activated = if activate {
        Some(activate_object(&base, &name_upper).await?)
    } else {
        None
    };

    Ok(WriteOutcome {
        obj_type: obj_type.api_name().to_string(),
        name: name_upper,
        group: None,
        source_url: source_rel,
        written: true,
        transport_used,
        rfc_enabled: None,
        // DDIC 写入路径不适用注册表（仅 rfc_enabled 函数会登记）
        registered_alias: None,
        activated,
        warnings: Vec::new(),
    })
}

/// FM 元数据 PUT 需要组名：group 参数为空时（不该发生）退回对象名占位。
fn group_or_name(_obj_type: ObjectType, _name: &str, group: &str) -> String {
    group.trim().to_uppercase()
}

/// 把函数模块设为 remote-enabled（processingType=rfc）。
/// 与源码 PUT 同锁执行：GET 元数据（带出描述/组件引用）→ PUT 整文档。
/// 元数据 PUT 是**整文档替换**——不带 description 会把对象描述清空（vsp 实证坑）。
async fn fm_set_rfc_enabled(
    fm_base_rel: &str,
    group: &str,
    sess: &mut WriteSession,
    lock_handle: &str,
    transport: Option<&str>,
) -> Result<(), String> {
    // 读当前元数据（v3 契约；stateful 会话内）
    let get_resp = adt_request_raw(
        axum::http::Method::GET,
        fm_base_rel,
        &[],
        None,
        None,
        "application/vnd.sap.adt.functions.fmodules.v3+xml",
        STATEFUL,
        sess.cookie_header().as_deref(),
        Some(sess.token.as_str()),
    )
    .await
    .map_err(|e| e.message)?;
    sess.absorb(&get_resp);
    if get_resp.status != 200 {
        return Err(format!("元数据读取返回 {}", get_resp.status));
    }
    let meta = String::from_utf8_lossy(&get_resp.body).to_string();
    let elems = walk_xml(&meta);
    let root = elems.iter().find(|e| e.name == "abapFunctionModule");
    let attr = |k: &str| -> String { root.and_then(|e| e.attr(k)).unwrap_or("").to_string() };
    // 已是 rfc → 幂等跳过
    if attr("processingType") == "rfc" {
        return Ok(());
    }
    let container_uri = elems
        .iter()
        .find(|e| e.name == "containerRef")
        .and_then(|e| e.attr("uri"))
        .unwrap_or("")
        .to_string();
    let release_state = attr("releaseState");
    let rfc_scope = attr("rfcScope");
    let rfc_version = attr("rfcVersion");
    let mut extra = String::new();
    if !release_state.is_empty() {
        extra.push_str(&format!(
            " fmodule:releaseState=\"{}\"",
            xml_attr_escape(&release_state)
        ));
    }
    if !rfc_scope.is_empty() {
        extra.push_str(&format!(
            " fmodule:rfcScope=\"{}\"",
            xml_attr_escape(&rfc_scope)
        ));
    }
    if !rfc_version.is_empty() {
        extra.push_str(&format!(
            " fmodule:rfcVersion=\"{}\"",
            xml_attr_escape(&rfc_version)
        ));
    }
    let fm_name = attr("name");
    let fm_name = if fm_name.is_empty() {
        fm_base_rel.rsplit('/').next().unwrap_or("").to_string()
    } else {
        fm_name
    };
    let desc = attr("description");
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<fmodule:abapFunctionModule xmlns:fmodule=\"http://www.sap.com/adt/functions/fmodules\" \
xmlns:adtcore=\"http://www.sap.com/adt/core\" \
adtcore:description=\"{desc}\" adtcore:name=\"{name}\" adtcore:type=\"FUGR/FF\" \
fmodule:processingType=\"rfc\"{extra}>\
<adtcore:containerRef adtcore:name=\"{group}\" adtcore:type=\"FUGR/F\" adtcore:uri=\"{uri}\"/>\
</fmodule:abapFunctionModule>",
        desc = xml_attr_escape(&desc),
        name = xml_attr_escape(&fm_name),
        extra = extra,
        group = xml_attr_escape(group),
        uri = xml_attr_escape(&container_uri),
    );
    let mut query: Vec<(&str, &str)> = vec![("lockHandle", lock_handle)];
    if let Some(t) = transport.filter(|t| !t.trim().is_empty()) {
        query.push(("corrNr", t));
    }
    let put_resp = adt_request_raw(
        axum::http::Method::PUT,
        fm_base_rel,
        &query,
        Some(body.as_bytes()),
        Some("application/vnd.sap.adt.functions.fmodules.v3+xml"),
        "*/*",
        STATEFUL,
        sess.cookie_header().as_deref(),
        Some(sess.token.as_str()),
    )
    .await
    .map_err(|e| e.message)?;
    sess.absorb(&put_resp);
    if (200..300).contains(&put_resp.status) {
        Ok(())
    } else {
        Err(format!(
            "ADT 返回 {}: {}",
            put_resp.status,
            truncate(&String::from_utf8_lossy(&put_resp.body), 200)
        ))
    }
}

// ========================================================================
// 对象删除（DELETE {base}?lockHandle；删除会消耗锁柄，无需解锁）
// ========================================================================

#[derive(Debug, serde::Serialize)]
pub struct DeleteOutcome {
    #[serde(rename = "type")]
    pub obj_type: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport_used: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// 删除对象：锁 → DELETE → 弃会话。删除成功即消耗锁柄（vsp 实证：不再 UNLOCK）；
/// DELETE 失败则尽力解锁，避免孤儿锁。
pub async fn delete_object(
    obj_type: ObjectType,
    name: &str,
    group: &str,
    transport: Option<&str>,
) -> Result<DeleteOutcome, RfcError> {
    let base = obj_type.base_rel(name, group);
    let mut warnings = Vec::new();
    // 表/结构（blue 对象）删除不需要编辑锁：无状态 DELETE 即可（真机实证 200）
    if obj_type.is_blue_ddic() {
        let transport_used = transport
            .map(str::to_string)
            .filter(|t| !t.trim().is_empty());
        let query: Vec<(&str, &str)> = match transport_used.as_deref() {
            Some(t) => vec![("corrNr", t)],
            None => vec![],
        };
        let resp = adt_request_raw(
            axum::http::Method::DELETE,
            &base,
            &query,
            None,
            None,
            "*/*",
            &[],
            None,
            None,
        )
        .await
        .map_err(|e| RfcError {
            code: -1,
            status: 502,
            message: format!("删除失败: {}", e.message),
            key: "ADT_DELETE_FAILED".into(),
        })?;
        if !(200..300).contains(&resp.status) {
            let body = String::from_utf8_lossy(&resp.body).to_string();
            if body.contains("exc:exception") {
                return Err(adt_exception_error(&body, "删除对象失败"));
            }
            return Err(RfcError {
                code: -1,
                status: 502,
                message: format!(
                    "删除对象失败（ADT 返回 {}）: {}",
                    resp.status,
                    truncate(&body, 300)
                ),
                key: "ADT_DELETE_FAILED".into(),
            });
        }
        return Ok(DeleteOutcome {
            obj_type: obj_type.api_name().to_string(),
            name: name.trim().to_uppercase(),
            group: None,
            deleted: true,
            transport_used,
            warnings,
        });
    }
    let mut sess = WriteSession::establish(&base).await?;
    let lock = lock_object(&base, &mut sess).await?;
    let transport_used = transport
        .map(str::to_string)
        .filter(|t| !t.trim().is_empty())
        .or_else(|| {
            let c = lock.corrnr.trim();
            (!c.is_empty()).then(|| c.to_string())
        });

    let mut query: Vec<(&str, &str)> = vec![("lockHandle", lock.lock_handle.as_str())];
    if let Some(t) = transport_used.as_deref() {
        query.push(("corrNr", t));
    }
    let resp = match adt_request_raw(
        axum::http::Method::DELETE,
        &base,
        &query,
        None,
        None,
        "*/*",
        STATEFUL,
        sess.cookie_header().as_deref(),
        Some(sess.token.as_str()),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            if let Err(w) = unlock_object(&base, &lock.lock_handle, &mut sess).await {
                warnings.push(w);
            }
            drop_session(&base, &sess).await;
            return Err(RfcError {
                code: -1,
                status: 502,
                message: format!("删除失败: {}", e.message),
                key: "ADT_DELETE_FAILED".into(),
            });
        }
    };
    sess.absorb(&resp);
    if !(200..300).contains(&resp.status) {
        // 删除失败：释放锁（对象还在，锁不该留）
        if let Err(w) = unlock_object(&base, &lock.lock_handle, &mut sess).await {
            warnings.push(w);
        }
        drop_session(&base, &sess).await;
        let body = String::from_utf8_lossy(&resp.body);
        if body.contains("exc:exception") {
            return Err(adt_exception_error(&body, "删除对象失败"));
        }
        return Err(RfcError {
            code: -1,
            status: 502,
            message: format!(
                "删除对象失败（ADT 返回 {}）: {}",
                resp.status,
                truncate(&body, 300)
            ),
            key: "ADT_DELETE_FAILED".into(),
        });
    }
    // 成功：DELETE 已消耗锁柄，直接结束会话
    drop_session(&base, &sess).await;
    // 写后失效：函数删除即清 FUNC_CACHE（防幽灵元数据）
    if obj_type == ObjectType::Function {
        let cleared = crate::metadata::invalidate_function(name.trim());
        if cleared {
            tracing::debug!(func = %name.trim(), "删除后清除 FUNC_CACHE 条目");
        }
    }
    // 注册表钩子：函数删除成功 → 墓碑化（保留条目留档；失败只记警告）
    if obj_type == ObjectType::Function {
        if let Err(e) = crate::registry::tombstone_func(name.trim()) {
            warnings.push(format!("注册表墓碑化失败（不影响删除）: {}", e.message));
        }
    }
    Ok(DeleteOutcome {
        obj_type: obj_type.api_name().to_string(),
        name: name.trim().to_uppercase(),
        group: obj_type.needs_group().then(|| group.to_uppercase()),
        deleted: true,
        transport_used,
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
// FM 源码 SEDI 规范化（经典注释块签名 → FUNCTION 语句内联）
// ========================================================================

/// 函数模块签名段关键字（SEDI 认可的顺序）。
const FM_SIG_SECTIONS: [&str; 6] = [
    "IMPORTING",
    "EXPORTING",
    "CHANGING",
    "TABLES",
    "RAISING",
    "EXCEPTIONS",
];

/// 把函数模块源码规范化为 SEDI（源码编辑器）形态。
///
/// 背景（A4H 7.5x 真机实证）：新版 ADT 写路径**拒绝**经典参数注释块
/// （400 "Parameter comment blocks are not allowed"），签名必须内联在
/// FUNCTION 语句里：`FUNCTION f IMPORTING VALUE(x) TYPE t EXPORTING ... .`。
/// 而 RFC 读路径（RPY_FUNCTIONMODULE_READ）返回的恰是经典形态——Agent
/// 读回再写就必然炸。本函数把两种历史形态转换为内联形态：
///
/// 1. `FUNCTION f.` + `*" IMPORTING ...` 注释块（RFC 读回形态）；
/// 2. `FUNCTION f.` + 独立 `IMPORTING ... .` 语句（手写疏漏形态）；
/// 3. 已内联 → 原样返回。
pub fn normalize_function_source(source: &str) -> String {
    let lines: Vec<&str> = source
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .collect();
    // 找 FUNCTION 语句起始行
    let Some(func_idx) = lines.iter().position(|l| {
        let t = l.trim_start();
        t.len() >= 9 && t[..9].eq_ignore_ascii_case("FUNCTION ")
    }) else {
        return source.to_string();
    };

    // 判定 FUNCTION 语句是否已带内联签名：语句内的段关键字出现在句点终结符之前
    let mut stmt_end: Option<usize> = None; // 终结行索引（含句点）
    let mut inline_sig = false;
    let mut depth = 0usize; // 括号深度（VALUE(x) 里的内容不算终结）
    'scan: for (i, line) in lines.iter().enumerate().skip(func_idx) {
        let bytes = line.as_bytes();
        let mut j = 0;
        while j < bytes.len() {
            match bytes[j] {
                b'(' => depth += 1,
                b')' => depth = depth.saturating_sub(1),
                b'.' if depth == 0
                    && (j + 1 >= bytes.len() || bytes[j + 1].is_ascii_whitespace()) =>
                {
                    stmt_end = Some(i);
                    break 'scan;
                }
                _ => {}
            }
            j += 1;
        }
        // 行内是否含段关键字（同一语句延续中）
        let mut word = String::new();
        for &b in bytes {
            if b.is_ascii_alphabetic() {
                word.push(b.to_ascii_uppercase() as char);
            } else {
                if FM_SIG_SECTIONS.contains(&word.as_str()) {
                    inline_sig = true;
                }
                word.clear();
            }
        }
        if FM_SIG_SECTIONS.contains(&word.as_str()) {
            inline_sig = true;
        }
    }
    let Some(end) = stmt_end else {
        return source.to_string(); // 找不到 FUNCTION 语句终结，无法安全处理
    };
    if inline_sig {
        return source.to_string(); // 已内联
    }

    // `FUNCTION f.` 之后收集参数定义：注释块（*" 前缀）或独立签名语句
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    let consume_until: usize; // body 起始行（两个分支都会赋值）
    let after: Vec<&str> = lines[end + 1..].to_vec();
    let is_sig_kw = |w: &str| FM_SIG_SECTIONS.iter().any(|k| w.eq_ignore_ascii_case(k));

    // 形态 1：*" 注释块
    let comment_lines: Vec<&str> = after
        .iter()
        .take_while(|l| {
            let t = l.trim_start();
            t.starts_with("*\"") || t.is_empty()
        })
        .cloned()
        .collect();
    if comment_lines.iter().any(|l| {
        let t = l.trim_start().trim_start_matches("*\"").trim();
        // 块里出现段关键字才视为参数注释块（排除纯分隔线/标题）
        t.split_whitespace().next().map(&is_sig_kw).unwrap_or(false)
    }) {
        for l in &comment_lines {
            let t = l.trim_start().trim_start_matches("*\"").trim();
            if t.is_empty() {
                continue;
            }
            let first = t.split_whitespace().next().unwrap_or("");
            if is_sig_kw(first) {
                sections.push((first.to_ascii_uppercase(), Vec::new()));
            } else if !sections.is_empty() {
                // 段内条目；跳过 "----- 分隔线与 "*"*"Local Interface: 标题
                if t.starts_with('"') || t.starts_with('-') || t.starts_with('*') {
                    continue;
                }
                sections.last_mut().unwrap().1.push(t.to_string());
            }
        }
        consume_until = end + 1 + comment_lines.len();
    } else {
        // 形态 2：独立签名语句（IMPORTING/... 行起始，句点行终止）
        let mut sig_end: Option<usize> = None;
        let mut started = false;
        for (i, l) in after.iter().enumerate() {
            let t = l.trim();
            if t.is_empty() {
                continue;
            }
            let first = t.split_whitespace().next().unwrap_or("");
            if !started {
                if is_sig_kw(first) {
                    started = true;
                } else {
                    break; // FUNCTION 行后直接是 body：无签名，无需转换
                }
            }
            if is_sig_kw(first) {
                // 段关键字行；同行剩余 token 是首个条目（IMPORTING VALUE(x) TYPE t 形态）
                let rest = t
                    .trim_end_matches('.')
                    .split_whitespace()
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join(" ");
                sections.push((first.to_ascii_uppercase(), Vec::new()));
                if !rest.is_empty() {
                    sections.last_mut().unwrap().1.push(rest);
                }
            } else if let Some(sec) = sections.last_mut() {
                sec.1.push(t.trim_end_matches('.').to_string());
            }
            if t.ends_with('.') {
                sig_end = Some(i);
                break;
            }
        }
        if let Some(se) = sig_end {
            consume_until = end + 1 + se + 1;
        } else {
            return source.to_string(); // 形态不完整：原样返回，让 SAP 的报错说话
        }
    }

    if sections.is_empty() {
        return source.to_string();
    }

    // 重建：FUNCTION 语句（无句点结尾的名字行）+ 段缩进块，末段以句点收尾
    let func_line = lines[func_idx]
        .trim()
        .trim_end_matches('.')
        .trim_end()
        .to_string();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    out.extend(lines[..func_idx].iter().map(|s| s.to_string()));
    out.push(func_line);
    for (kw, entries) in &sections {
        out.push(format!("  {}", kw));
        for e in entries {
            out.push(format!("    {}", e));
        }
    }
    // 末段句点：追加到最后一个条目行尾（或段关键字行尾——空段）
    if let Some(last) = out.last_mut() {
        last.push('.');
    } else {
        return source.to_string();
    }
    // body：空一行后接剩余行
    let rest = &lines[consume_until.min(lines.len())..];
    if rest.iter().any(|l| !l.trim().is_empty()) {
        out.push(String::new());
        out.extend(rest.iter().map(|s| s.to_string()));
    }
    let mut result = out.join("\n");
    // 去掉 body 前导多余空行（保留一个分隔）
    while result.contains("\n\n\n\n") {
        result = result.replace("\n\n\n\n", "\n\n\n");
    }
    result
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
        return Ok(if crlf {
            restored_crlf(&updated)
        } else {
            updated
        });
    }
    // 阶梯 2：末尾 \n 修剪（锚点是最后一行时，调用方常多带一个换行）
    let old_trim = norm_old_full.trim_end_matches('\n');
    if !old_trim.is_empty() && old_trim != norm_old_full {
        if let Some(updated) = apply(old_trim) {
            return Ok(if crlf {
                restored_crlf(&updated)
            } else {
                updated
            });
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
                return Ok(if crlf {
                    restored_crlf(&updated)
                } else {
                    updated
                });
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
        assert_eq!(ObjectType::parse("incl"), Some(ObjectType::Include));
        assert_eq!(ObjectType::parse("Include"), Some(ObjectType::Include));
        assert_eq!(ObjectType::parse("class"), Some(ObjectType::Class));
        assert_eq!(ObjectType::parse("intf"), Some(ObjectType::Interface));
        assert_eq!(ObjectType::parse("INTERFACE"), Some(ObjectType::Interface));
        assert_eq!(ObjectType::parse("func"), Some(ObjectType::Function));
        // fugr 现在指向函数组（FUGR/F），不再是函数模块
        assert_eq!(ObjectType::parse("fugr"), Some(ObjectType::FunctionGroup));
        assert_eq!(ObjectType::parse("fm"), Some(ObjectType::Function));
        assert_eq!(ObjectType::parse("cds"), Some(ObjectType::CdsView));
        assert_eq!(ObjectType::parse("ddls"), Some(ObjectType::CdsView));
        assert_eq!(ObjectType::parse("tabl"), Some(ObjectType::Table));
        assert_eq!(ObjectType::parse("TABLE"), Some(ObjectType::Table));
        assert_eq!(ObjectType::parse("dtab"), Some(ObjectType::Table));
        assert_eq!(ObjectType::parse("stru"), Some(ObjectType::Structure));
        assert_eq!(ObjectType::parse("STRUCTURE"), Some(ObjectType::Structure));
        assert_eq!(ObjectType::parse("package"), Some(ObjectType::Package));
        assert_eq!(ObjectType::parse("pkg"), Some(ObjectType::Package));
        assert_eq!(ObjectType::parse("foobar"), None);

        assert_eq!(
            ObjectType::Program.base_rel("ztest", ""),
            "programs/programs/ZTEST"
        );
        assert_eq!(
            ObjectType::Include.base_rel("ztest_incl", ""),
            "programs/includes/ZTEST_INCL"
        );
        assert_eq!(
            ObjectType::Class.source_rel("zcl_foo", ""),
            "oo/classes/ZCL_FOO/source/main"
        );
        assert_eq!(
            ObjectType::Interface.source_rel("zif_foo", ""),
            "oo/interfaces/ZIF_FOO/source/main"
        );
        // CDS 家族 URL 用小写（vsp/abapfs 双实证）
        assert_eq!(
            ObjectType::CdsView.source_rel("ZCDS_Foo", ""),
            "ddic/ddl/sources/zcds_foo/source/main"
        );
        assert_eq!(
            ObjectType::FunctionGroup.base_rel("zgroup", ""),
            "functions/groups/ZGROUP"
        );
        // 表：ddic/tables 下，源码在 source/main
        assert_eq!(
            ObjectType::Table.base_rel("zagw_t1", ""),
            "ddic/tables/ZAGW_T1"
        );
        assert_eq!(
            ObjectType::Table.source_rel("zagw_t1", ""),
            "ddic/tables/ZAGW_T1/source/main"
        );
        assert!(ObjectType::Table.has_source());
        assert!(!ObjectType::Table.needs_group());
        // 结构：ddic/structures 下，与表同为 blue 对象
        assert_eq!(
            ObjectType::Structure.base_rel("zagw_st1", ""),
            "ddic/structures/ZAGW_ST1"
        );
        assert_eq!(
            ObjectType::Structure.source_rel("zagw_st1", ""),
            "ddic/structures/ZAGW_ST1/source/main"
        );
        assert!(ObjectType::Structure.has_source());
        assert!(ObjectType::Table.is_blue_ddic());
        assert!(ObjectType::Structure.is_blue_ddic());
        assert!(!ObjectType::CdsView.is_blue_ddic());
        // 创建契约：根元素是 blue:blueSource（blue 对象，非 tbl:table）
        let (rel, root, ns) = ObjectType::Table.creation("");
        assert_eq!(rel, "ddic/tables");
        assert_eq!(root, "blue:blueSource");
        assert_eq!(ns, "http://www.sap.com/wbobj/blue");
        let (rel, root, ns) = ObjectType::Structure.creation("");
        assert_eq!(rel, "ddic/structures");
        assert_eq!(root, "blue:blueSource");
        assert_eq!(ns, "http://www.sap.com/wbobj/blue");
        assert_eq!(ObjectType::Package.base_rel("zpkg", ""), "packages/ZPKG");
        assert!(!ObjectType::Package.has_source());
        assert!(ObjectType::CdsView.has_source());
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
    fn create_body_simple_uses_package_and_container_ref() {
        let spec = CreateSpec {
            description: "hello & <world>".into(),
            devclass: "ZPKG".into(),
            ..Default::default()
        };
        let body = create_body_simple(ObjectType::Class, "ZCL_A", "", &spec, Some("DEVELOPER"));
        assert!(body.contains("adtcore:type=\"CLAS/OC\""));
        assert!(body.contains("adtcore:name=\"ZCL_A\""));
        assert!(body.contains("adtcore:description=\"hello &amp; &lt;world&gt;\""));
        assert!(body.contains("adtcore:responsible=\"DEVELOPER\""));
        assert!(body.contains("<adtcore:packageRef adtcore:name=\"ZPKG\"/>"));

        // Function 走 containerRef（组名大写属性 + 小写 URI）
        let fbody = create_body_simple(ObjectType::Function, "Z_FM", "Zgrp", &spec, None);
        assert!(fbody.contains("adtcore:type=\"FUGR/FF\""));
        assert!(fbody.contains("adtcore:containerRef adtcore:name=\"ZGRP\""));
        assert!(fbody.contains("adtcore:uri=\"/sap/bc/adt/functions/groups/zgrp\""));

        // 表：根是 blue:blueSource，packageRef 为子元素（真机实证契约）
        let tbody = create_body_simple(ObjectType::Table, "ZAGW_T1", "", &spec, None);
        assert!(tbody.contains("<blue:blueSource "));
        assert!(tbody.contains("xmlns:blue=\"http://www.sap.com/wbobj/blue\""));
        assert!(tbody.contains("adtcore:type=\"TABL/DT\""));
        assert!(tbody.contains("adtcore:name=\"ZAGW_T1\""));
        assert!(tbody.contains("<adtcore:packageRef adtcore:name=\"ZPKG\"/>"));
        assert!(!tbody.contains("<tbl:table"));

        // 结构：同一 blueSource 契约，仅类型 ID 与集合不同
        let sbody = create_body_simple(ObjectType::Structure, "ZAGW_ST1", "", &spec, None);
        assert!(sbody.contains("<blue:blueSource "));
        assert!(sbody.contains("adtcore:type=\"STRU/DT\""));
        assert!(sbody.contains("<adtcore:packageRef adtcore:name=\"ZPKG\"/>"));
    }

    #[test]
    fn normalize_fm_source_converts_comment_block() {
        // RFC 读回的经典形态：FUNCTION x. + *" 注释块
        let classic = "FUNCTION zfoo.\n*\"----------------------------------------------------------------------\n*\"*\"Local Interface:\n*\"  IMPORTING\n*\"     VALUE(IV_IN) TYPE  STRING\n*\"     VALUE(IV_N) TYPE  I OPTIONAL\n*\"  EXPORTING\n*\"     VALUE(EV_OUT) TYPE  STRING\n*\"----------------------------------------------------------------------\n\n  ev_out = iv_in.\nENDFUNCTION.";
        let out = normalize_function_source(classic);
        assert!(
            out.starts_with("FUNCTION zfoo\n"),
            "开头应是无句点的 FUNCTION 行: {out}"
        );
        assert!(out.contains("  IMPORTING\n    VALUE(IV_IN) TYPE  STRING\n    VALUE(IV_N) TYPE  I OPTIONAL\n  EXPORTING\n    VALUE(EV_OUT) TYPE  STRING."));
        assert!(!out.contains("*\""), "注释块应被移除: {out}");
        // body 原样保留
        assert!(out.contains("ev_out = iv_in."));
        assert!(out.ends_with("ENDFUNCTION."));
    }

    #[test]
    fn normalize_fm_source_converts_bare_statement_form() {
        // 独立 IMPORTING 语句形态（写入疏漏/手写）
        let bare = "FUNCTION zfoo.\nIMPORTING\n  VALUE(iv) TYPE string\nEXPORTING\n  VALUE(ev) TYPE string.\n\n  ev = iv.\nENDFUNCTION.";
        let out = normalize_function_source(bare);
        assert!(out.contains("FUNCTION zfoo\n  IMPORTING\n    VALUE(iv) TYPE string\n  EXPORTING\n    VALUE(ev) TYPE string."), "{out}");
        assert!(out.contains("ev = iv."));
    }

    #[test]
    fn normalize_fm_source_keeps_inline_and_plain_forms() {
        // 已内联：原样
        let inline = "FUNCTION zfoo IMPORTING VALUE(iv) TYPE string EXPORTING VALUE(ev) TYPE string.\n  ev = iv.\nENDFUNCTION.";
        assert_eq!(normalize_function_source(inline), inline);
        // 无签名：原样
        let plain = "FUNCTION zfoo.\n  WRITE 'x'.\nENDFUNCTION.";
        assert_eq!(normalize_function_source(plain), plain);
        // 非 FM 源码：原样
        assert_eq!(
            normalize_function_source("REPORT zprog.\n"),
            "REPORT zprog.\n"
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
