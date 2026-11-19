//! 远程 ABAP 调试面（v0.22）：断点 / listen / attach / 栈 / 变量读写 / 步进。
//!
//! 契约依据 docs/DEBUGGER-CONTRACT.zh-CN.md（P0 实测版）。架构三层：
//! - 会话层 [`DebugSession`]（debug_session.rs）：独立 cookie jar + stateful 头；
//! - 本模块：每系统至多一个调试会话（`Slot`），listen→attach→栈在后台任务里
//!   原子 catch（防 attach 竞态窗），状态轮询纯读内存零 ADT 往返；
//! - REST 面：`/api/debug/**`，继承鉴权/限流/只读/多系统作用域（经 router 合并）。
//!
//! 调试器与 debuggee 分属两会话：触发走 `/api/abap/run`（每次强制新会话）或
//! `/api/rfc`（同用户），断点按用户解析、对同用户的新会话生效。
//!
//! read_only 分级：观察类操作（listen/栈/变量读/步进）放行——只读系统可排障；
//! 仅变量写（PUT …/variables/{name}）被只读守卫拦（见 server.rs is_read_only_blocked）。

use crate::debug_session::{adt_error_subtype, DebugRequest, DebugSession};
use crate::error::RfcError;
use crate::systems::Sap;
use axum::http::Method;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 断点/监听登记身份：`ideId` 网关标识（稳定即可）；`terminalId` 32 字符稳定
/// 十六进制（"sap-for-agents" 的 ASCII hex 28 位补 0000）。断点按
/// (requestUser, ideId, terminalId) 注册——换了就看不见自己的断点。
const IDE_ID: &str = "sfa";
const TERMINAL_ID: &str = "7361702d666f722d6167656e7473000000";

/// 会话空闲 TTL：keepalive 任务兼任清扫（超时未交互即拆除）。
const SESSION_TTL: Duration = Duration::from_secs(30 * 60);
/// keepalive 周期（ICM 会话型抖动实证：闲置即有被回收风险）。
const KEEPALIVE_TICK: Duration = Duration::from_secs(30);
/// listen 的默认等待窗（也是 debuggee 的等待窗——实测两者同值）。
const DEFAULT_LISTEN_SECS: u64 = 60;

// ========================================================================
// 会话槽：每系统至多一个
// ========================================================================

static SLOTS: Mutex<BTreeMap<String, Arc<Slot>>> = Mutex::new(BTreeMap::new());

struct Slot {
    run: Mutex<Option<Arc<Run>>>,
    /// 断点台账（用户态身份：user+ideId+terminalId；跨会话存续——
    /// P0 实证断点不绑会话，agent 流程"先设断点→再建会话"成立）
    bps: Mutex<Vec<Bp>>,
    /// 长命断点通道（缓存复用，防每次断点操作都开关 stateful 会话——
    /// 会话风暴会把 trial ICM 拖进抖动，2026-11-18 实证）。出错即置 None，
    /// 下次操作重开。
    bp_chan: Arc<tokio::sync::Mutex<Option<DebugSession>>>,
}

/// 一个进行中的调试会话（状态机 + 通道 + 台账）。
struct Run {
    sid: String,
    system: String,
    /// 该系统 ADT 端点（teardown 兜底开新会话用）
    ep: std::sync::Arc<crate::adt::AdtEndpoint>,
    /// 调试通道：catch/步进/变量期间持有锁（listen 是阻塞请求）。
    chan: tokio::sync::Mutex<DebugSession>,
    /// 断点/监听登记的用户（ADT responsible 用户，大写）。
    user: String,
    phase: Mutex<Phase>,
    /// 命中位置（catch 或最近一次步进后缓存）。
    position: Mutex<Option<Position>>,
    /// 调用栈缓存（stopped 时有效）。
    stack: Mutex<Vec<StackFrame>>,
    /// 结束/丢失原因。
    note: Mutex<Option<String>>,
    keepalive: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// 当前选中的栈帧 index（变量读写上下文；catch/步进后 = 光标帧 frames[0]）
    frame: Mutex<u64>,
    created: Instant,
    /// 最近一次 REST 交互（TTL 依据）。
    last_used: Mutex<Instant>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Listening,
    Stopped,
    Finished,
    Lost,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Listening => "listening",
            Phase::Stopped => "stopped",
            Phase::Finished => "finished",
            Phase::Lost => "lost",
        }
    }
}

#[derive(Clone, Serialize)]
struct Position {
    program: String,
    include: String,
    line: u64,
    uri: String,
}

#[derive(Clone, Serialize)]
struct StackFrame {
    index: u64,
    program: String,
    include: String,
    line: u64,
    event_type: String,
    event_name: String,
    uri: String,
    /// 帧切换的 PUT 目标（`/sap/bc/adt/debugger/stack/type/ABAP/position/N`；
    /// 版本缺该属性时按 index 推导）
    stack_uri: String,
}

#[derive(Clone, Serialize)]
struct Bp {
    id: String,
    uri: String,
    line: u64,
    /// SAP 拒收原因（无 id 的条目——声明行/注释行无语句）
    error: Option<String>,
}

fn slot_err(status: u16, key: &str, message: impl Into<String>) -> RfcError {
    RfcError {
        code: -1,
        status,
        message: message.into(),
        key: key.into(),
    }
}

/// capabilities.debug（默认系统视角——与 adt 能力同源：有 ADT 即可调试）。
static CAPABILITY_DEBUG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 写入 capabilities.debug（main 启动期 + setup 热加载首个默认系统时）。
pub fn init_capability_debug(enabled: bool) {
    CAPABILITY_DEBUG.store(enabled, std::sync::atomic::Ordering::SeqCst);
}

/// capabilities.debug 当前值。
pub fn capabilities_debug_enabled() -> bool {
    CAPABILITY_DEBUG.load(std::sync::atomic::Ordering::SeqCst)
}

/// 临时短命通道（断点操作用——断点按用户态身份存续，不依赖会话）。
/// 断点通道（槽内缓存的长命会话；None 或出错时重开）。失败自动失效——
/// 下次操作重建，不做无谓重试。
async fn bp_channel<'a>(
    sap: &'a Sap,
    slot: &'a Arc<Slot>,
) -> Result<tokio::sync::MutexGuard<'a, Option<DebugSession>>, RfcError> {
    // ⚠️ 单锁设计（2026-11-20 楔死根因修复）：绝不在持有 tokio Mutex 守卫时
    // 对同一把锁再 lock/lock_owned——tokio Mutex 不可重入，旧快速路径
    // 「持 g → lock_owned().await」= 永久自死锁（首次 bp 操作后的所有 bp
    // 操作全部楔死，重启才解）。现在整段持单守卫：慢路径（DebugSession::open
    // 两次往返）期间持锁串行化，顺带消除并发双开通道的浪费。守卫可跨 await
    // （tokio MutexGuard: Send），宏在 body 期间持它正是所需的独占语义。
    let mut g = slot.bp_chan.lock().await;
    if g.is_none() {
        let ep = sap.adt.as_ref().ok_or_else(crate::adt::adt_disabled)?.clone();
        let fresh = DebugSession::open(ep).await.map_err(|e| {
            slot_err(502, "ADT_DEBUG_SESSION_FAILED", format!("调试通道建立失败: {}", e.message))
        })?;
        *g = Some(fresh);
    }
    Ok(g)
}

/// 通道出错时丢弃缓存（下次操作重开）。
async fn drop_bp_channel(slot: &Arc<Slot>) {
    slot.bp_chan.lock().await.take();
}
/// 宏：断点操作走缓存通道，通道级错误自动弃缓存。
macro_rules! with_bp_chan {
    ($sap:expr, $slot:expr, $chan:ident, $body:expr) => {{
        let mut g = bp_channel($sap, $slot).await?;
        let $chan = g.as_mut().expect("bp_channel 保证 Some");
        let out = (async { $body }).await;
        match out {
            Ok(v) => Ok(v),
            Err(e) => {
                // 先释放守卫再 drop_bp_channel——同一把 tokio Mutex，持锁嵌套
                // 即自死锁（与 bp_channel 快速路径同款错误，见彼处注释）。
                drop(g);
                drop_bp_channel($slot).await;
                Err(e)
            }
        }
    }};
}

/// 该系统 ADT 登录用户（断点 requestUser；触发方须同用户）。
fn adt_user(sap: &Sap) -> Result<String, RfcError> {
    Ok(sap
        .adt
        .as_ref()
        .ok_or_else(crate::adt::adt_disabled)?
        .user()
        .to_string())
}

/// 取（或建）系统槽。
fn slot_for(system: &str) -> Result<Arc<Slot>, RfcError> {
    let mut m = SLOTS.lock().map_err(|_| poisoned())?;
    Ok(m.entry(system.to_string())
        .or_insert_with(|| Arc::new(Slot {
                run: Mutex::new(None),
                bps: Mutex::new(Vec::new()),
                bp_chan: Arc::new(tokio::sync::Mutex::new(None)),
            }))
        .clone())
}

/// 活动会话总数（metrics gauge）。
fn sessions_total() -> usize {
    SLOTS.lock().map(|m| m.len()).unwrap_or(0)
}

fn update_sessions_gauge() {
    metrics::gauge!("sap_debug_sessions_active").set(sessions_total() as f64);
}

// ========================================================================
// 状态机辅助
// ========================================================================

fn set_phase(run: &Run, phase: Phase, note: Option<String>) {
    *run.phase.lock().unwrap() = phase;
    *run.note.lock().unwrap() = note;
}

/// catch 后台任务：listen → attach → 栈，背靠背（防 attach 竞态窗）。
/// listen 阻塞至 debuggee 命中或超时（200 空体 = 无人命中）。
async fn catch_loop(run: Arc<Run>, listen_secs: u64) {
    let mut chan = run.chan.lock().await;
    let user = run.user.clone();

    // 1) listen（Accept: application/vnd.sap.as+xml；空体 = 超时无人）
    let listen_q = format!(
        "debugger/listeners?debuggingMode=user&requestUser={user}&ideId={IDE_ID}&terminalId={TERMINAL_ID}&timeout={listen_secs}"
    );
    let lr = chan
        .request(
            Method::POST,
            &listen_q,
            &DebugRequest {
                accept: "application/vnd.sap.as+xml",
                timeout: Some(Duration::from_secs(listen_secs + 20)),
                ..Default::default()
            },
        )
        .await;
    let lr = match lr {
        Ok(r) => r,
        Err(e) => {
            set_phase(&run, Phase::Lost, Some(e.message));
            return;
        }
    };
    if lr.status != 200 {
        let sub = adt_error_subtype(&lr.body);
        set_phase(
            &run,
            Phase::Lost,
            Some(format!("listen 失败: ADT {}{}", lr.status, sub.map(|s| format!(" ({s})")).unwrap_or_default())),
        );
        return;
    }
    if lr.body.is_empty() {
        set_phase(
            &run,
            Phase::Finished,
            Some(format!("listener 等待 {listen_secs}s 无 debuggee 命中（先设断点，再触发同用户代码）")),
        );
        return;
    }
    let body = String::from_utf8_lossy(&lr.body).into_owned();
    let did = xml_text(&body, "DEBUGGEE_ID").unwrap_or_default();
    if did.is_empty() {
        set_phase(&run, Phase::Lost, Some("listen 响应无 DEBUGGEE_ID".into()));
        return;
    }
    let position = Position {
        program: xml_text(&body, "PRG_CURR").unwrap_or_default(),
        include: xml_text(&body, "INCL_CURR").unwrap_or_default(),
        line: xml_text(&body, "LINE_CURR").and_then(|s| s.trim().parse().ok()).unwrap_or(0),
        uri: xml_text(&body, "URI").unwrap_or_default(),
    };

    // 2) attach（debuggee 只在等待期可附加——背靠背是硬约束）
    let attach_q = format!(
        "debugger?method=attach&debuggeeId={did}&debuggingMode=user&requestUser={user}&dynproDebugging=true"
    );
    let ar = chan
        .request(
            Method::POST,
            &attach_q,
            &DebugRequest { accept: "*/*", ..Default::default() },
        )
        .await;
    if let Err(e) = ar {
        set_phase(&run, Phase::Lost, Some(format!("attach 失败: {}", e.message)));
        return;
    }
    let ar = ar.unwrap();
    if ar.status != 200 {
        let sub = adt_error_subtype(&ar.body);
        set_phase(
            &run,
            Phase::Lost,
            Some(format!("attach 失败: ADT {}{}", ar.status, sub.map(|s| format!(" ({s})")).unwrap_or_default())),
        );
        return;
    }

    // 3) 栈（816 走资源 GET 形态；404 = 老版本，回落 dispatcher POST——契约 §5）
    match fetch_stack(&mut chan).await {
        Ok(frames) => {
            let pos = frames.first().map(|f| Position {
                program: f.program.clone(),
                include: f.include.clone(),
                line: f.line,
                uri: f.uri.clone(),
            });
            // 帧编号从内向外递增（stackPosition），光标帧 = frames[0]
            let cursor = frames.first().map(|f| f.index).unwrap_or(0);
            *run.stack.lock().unwrap() = frames;
            *run.position.lock().unwrap() = pos.or(Some(position));
            *run.frame.lock().unwrap() = cursor;
            set_phase(&run, Phase::Stopped, None);
            metrics::counter!("sap_debug_catches_total").increment(1);
        }
        Err(e) => set_phase(&run, Phase::Lost, Some(format!("attach 后读栈失败: {}", e.message))),
    }
}

/// 读调用栈：资源 GET 形态失败且 404 时回落 dispatcher POST（其余错误原样上抛）。
async fn fetch_stack(chan: &mut DebugSession) -> Result<Vec<StackFrame>, RfcError> {
    let q = "debugger/stack?method=getStack&emode=_&semanticURIs=true";
    let r = chan
        .request(
            Method::GET,
            q,
            &DebugRequest { accept: "*/*", ..Default::default() },
        )
        .await?;
    if r.status == 200 {
        return Ok(parse_stack(&String::from_utf8_lossy(&r.body)));
    }
    if r.status != 404 {
        let sub = adt_error_subtype(&r.body);
        return Err(slot_err(
            r.status,
            "ADT_DEBUG_FAILED",
            format!(
                "读栈失败: ADT {}{}",
                r.status,
                sub.map(|s| format!(" ({s})")).unwrap_or_default()
            ),
        ));
    }
    // dispatcher 形态（7.50 等）：同文档，POST 到 debugger 资源本身
    let r2 = chan
        .request(
            Method::POST,
            "debugger?method=getStack&emode=_&semanticURIs=true",
            &DebugRequest { accept: "*/*", ..Default::default() },
        )
        .await?;
    if r2.status != 200 {
        let sub = adt_error_subtype(&r2.body);
        return Err(slot_err(
            r2.status,
            "ADT_DEBUG_FAILED",
            format!(
                "读栈失败（dispatcher 形态）: ADT {}{}",
                r2.status,
                sub.map(|s| format!(" ({s})")).unwrap_or_default()
            ),
        ));
    }
    Ok(parse_stack(&String::from_utf8_lossy(&r2.body)))
}

/// 步进/放行：返回 (是否 debuggee 已结束, 新位置)。
async fn step_and_refresh(
    chan: &mut DebugSession,
    run: &Run,
    action: &str,
) -> Result<(bool, Option<Position>, Vec<StackFrame>), RfcError> {
    let method = match action {
        "step_into" => "stepInto",
        "step_over" => "stepOver",
        "step_out" => "stepReturn",
        "resume" => "stepContinue",
        other => {
            return Err(slot_err(
                400,
                "DEBUG_ACTION_INVALID",
                format!("未知 action: {other}（step_into/step_over/step_out/resume）"),
            ))
        }
    };
    let r = chan
        .request(
            Method::POST,
            &format!("debugger?method={method}"),
            &DebugRequest { accept: "*/*", ..Default::default() },
        )
        .await?;
    let sub = adt_error_subtype(&r.body);
    if r.status != 200 {
        // continue 后 debuggee 结束是正常终局（契约 §7）
        if action == "resume" && sub.as_deref() == Some("debuggeeEnded") {
            return Ok((true, None, Vec::new()));
        }
        return Err(slot_err(
            r.status,
            "ADT_DEBUG_FAILED",
            format!(
                "{action} 失败: ADT {}{}",
                r.status,
                sub.map(|s| format!(" ({s})")).unwrap_or_default()
            ),
        ));
    }
    let frames = fetch_stack(chan).await?;
    let pos = frames.first().map(|f| Position {
        program: f.program.clone(),
        include: f.include.clone(),
        line: f.line,
        uri: f.uri.clone(),
    });
    let cursor = frames.first().map(|f| f.index).unwrap_or(0);
    *run.stack.lock().unwrap() = frames.clone();
    *run.position.lock().unwrap() = pos.clone();
    *run.frame.lock().unwrap() = cursor;
    Ok((false, pos, frames))
}

/// 枚举变量：parent=None → 自动发现本地根（@ROOT 子层的 @LOCALS 类节点）再列出；
/// parent=Some(id) → 直接展开该节点。返回 (层级发现的子节点, 变量数组)。
async fn list_variables(
    chan: &mut DebugSession,
    parent: Option<&str>,
) -> Result<(Vec<(String, String)>, Vec<DebugVariable>), RfcError> {
    match parent {
        Some(id) => {
            let (kids, vars) = child_variables(chan, id).await?;
            // 展开中间层（@ROOT 的直接孩子通常是 @LOCALS 等容器而非变量）
            if vars.is_empty() && kids.len() == 1 {
                let (k2, v2) = child_variables(chan, &kids[0].0).await?;
                return Ok((k2, v2));
            }
            Ok((kids, vars))
        }
        None => {
            // 自动发现：@ROOT 的孩子两种形态（都是实测）——
            // a) 容器节点（@LOCALS / @GLOBALS+@PARAMETERS，P0 816 手测形态）
            // b) 直接就是变量 id（P2 网关实测：@ROOT 直接给 LV_X）
            let (kids, vars) = child_variables(chan, "@ROOT").await?;
            if !vars.is_empty() {
                return Ok((kids, vars));
            }
            let mut all_vars = Vec::new();
            let mut labels = Vec::new();
            for (id, name) in &kids {
                if id.starts_with('@') {
                    // 容器层——继续下探
                    let (_, v2) = child_variables(chan, id).await?;
                    if !v2.is_empty() {
                        labels.push((format!("{name} ({id})"), String::new()));
                    }
                    all_vars.extend(v2);
                } else {
                    // 孩子即变量——按 id 直读完整记录
                    let vs = read_variables(chan, &[id]).await?;
                    all_vars.extend(vs);
                }
            }
            Ok((labels, all_vars))
        }
    }
}

/// getVariables：按 id 直读变量记录（name 求值用）。
async fn read_variables(
    chan: &mut DebugSession,
    ids: &[&str],
) -> Result<Vec<DebugVariable>, RfcError> {
    let mut items = String::new();
    for id in ids {
        items.push_str(&format!(
            "<STPDA_ADT_VARIABLE><ID>{}</ID></STPDA_ADT_VARIABLE>",
            xml_escape(id)
        ));
    }
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><asx:abap xmlns:asx="http://www.sap.com/abapxml" version="1.0"><asx:values><DATA>{items}</DATA></asx:values></asx:abap>"#
    );
    let r = chan
        .request(
            Method::POST,
            "debugger?method=getVariables",
            &DebugRequest {
                accept: "application/vnd.sap.as+xml",
                content_type: Some(
                    "application/vnd.sap.as+xml;charset=UTF-8;dataname=com.sap.adt.debugger.Variables",
                ),
                body: Some(body.as_bytes()),
                ..Default::default()
            },
        )
        .await?;
    if r.status != 200 {
        let sub = adt_error_subtype(&r.body);
        return Err(slot_err(
            r.status,
            "ADT_DEBUG_FAILED",
            format!(
                "读变量失败: ADT {}{}",
                r.status,
                sub.map(|s| format!(" ({s})")).unwrap_or_default()
            ),
        ));
    }
    Ok(parse_variables(&String::from_utf8_lossy(&r.body)))
}

/// getChildVariables：返回 (孩子节点[CHILD_ID, CHILD_NAME], 变量数组)。
async fn child_variables(
    chan: &mut DebugSession,
    parent: &str,
) -> Result<(Vec<(String, String)>, Vec<DebugVariable>), RfcError> {
    let parent = parent.trim();
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><asx:abap xmlns:asx="http://www.sap.com/abapxml" version="1.0"><asx:values><DATA><HIERARCHIES><STPDA_ADT_VARIABLE_HIERARCHY><PARENT_ID>{}</PARENT_ID></STPDA_ADT_VARIABLE_HIERARCHY></HIERARCHIES></DATA></asx:values></asx:abap>"#,
        xml_escape(parent)
    );
    let r = chan
        .request(
            Method::POST,
            "debugger?method=getChildVariables",
            &DebugRequest {
                accept: "application/vnd.sap.as+xml",
                content_type: Some(
                    "application/vnd.sap.as+xml;charset=UTF-8;dataname=com.sap.adt.debugger.ChildVariables",
                ),
                body: Some(body.as_bytes()),
                ..Default::default()
            },
        )
        .await?;
    if r.status != 200 {
        let sub = adt_error_subtype(&r.body);
        return Err(slot_err(
            r.status,
            "ADT_DEBUG_FAILED",
            format!(
                "读变量失败: ADT {}{}",
                r.status,
                sub.map(|s| format!(" ({s})")).unwrap_or_default()
            ),
        ));
    }
    let s = String::from_utf8_lossy(&r.body).into_owned();
    let kids = xml_all_blocks(&s, "STPDA_ADT_VARIABLE_HIERARCHY")
        .into_iter()
        .map(|b| {
            (
                xml_text(&b, "CHILD_ID").unwrap_or_default(),
                xml_text(&b, "CHILD_NAME").unwrap_or_default(),
            )
        })
        .filter(|(id, _)| !id.is_empty())
        .collect();
    let vars = parse_variables(&s);
    Ok((kids, vars))
}

// ========================================================================
// XML 轻解析（契约文档的形态在手，不做通用解析器）
// ========================================================================

/// 首个 `<tag>` 的内文（未闭合/未找到 → None）。
fn xml_text(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let i = s.find(&open)?;
    let rest = &s[i + open.len()..];
    let j = rest.find(&format!("</{tag}>"))?;
    Some(unescape_xml(&rest[..j]))
}

/// 反复取 `<tag>` 内文块（非嵌套假设——契约里这几类都是平铺的）。
fn xml_all_blocks(s: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        match after.find(&close) {
            Some(j) => {
                out.push(after[..j].to_string());
                rest = &after[j + close.len()..];
            }
            None => break,
        }
    }
    out
}

/// 自闭合/带属性标签的属性值：`<tag a="x" b="y"/>` 里的 `attr`。
fn xml_attr(s: &str, tag: &str, attr: &str) -> Option<String> {
    let open = format!("<{tag}");
    let i = s.find(&open)?;
    let rest = &s[i + open.len()..];
    let end = rest.find('>').map(|j| j).unwrap_or(rest.len());
    let head = &rest[..end];
    let needle = format!("{attr}=\"");
    let a = head.find(&needle)?;
    let v = &head[a + needle.len()..];
    let j = v.find('"')?;
    Some(v[..j].to_string())
}

/// 平铺块切分：按起始标签切段（自闭合属性型标签用）。
fn xml_split_tags(s: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}");
    s.split(&open).skip(1).map(|c| format!("<{tag}{c}")).collect()
}

fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 栈文档 → 帧数组（`<stackEntry …/>` 属性型）。
fn parse_stack(s: &str) -> Vec<StackFrame> {
    xml_split_tags(s, "stackEntry")
        .into_iter()
        .filter_map(|chunk| {
            let g = |a: &str| xml_attr(&chunk, "stackEntry", a).unwrap_or_default();
            let index: u64 = g("stackPosition").trim().parse().ok()?;
            let stack_uri = {
                let su = g("stackUri");
                if su.is_empty() {
                    format!("/sap/bc/adt/debugger/stack/type/ABAP/position/{index}")
                } else {
                    su
                }
            };
            Some(StackFrame {
                index,
                program: g("programName"),
                include: g("includeName"),
                line: g("line").trim().parse().unwrap_or(0),
                event_type: g("eventType"),
                event_name: g("eventName"),
                uri: g("uri"),
                stack_uri,
            })
        })
        .collect()
}

#[derive(Clone, Serialize)]
struct DebugVariable {
    id: String,
    name: String,
    value: String,
    declared_type: String,
    actual_type: String,
    table_lines: Option<u64>,
    incomplete: bool,
}

/// 变量文档 → 变量数组（`<STPDA_ADT_VARIABLE><ID>…` 内文型；容器是
/// `<VARIABLES>`——不要找 "VARIABLE" 开头标签，会撞上容器名）。
fn parse_variables(s: &str) -> Vec<DebugVariable> {
    xml_all_blocks(s, "STPDA_ADT_VARIABLE")
        .into_iter()
        .filter_map(|b| {
            let id = xml_text(&b, "ID")?;
            Some(DebugVariable {
                id: id.clone(),
                name: xml_text(&b, "NAME").unwrap_or_else(|| id.clone()),
                value: xml_text(&b, "VALUE").unwrap_or_default(),
                declared_type: xml_text(&b, "DECLARED_TYPE_NAME").unwrap_or_default(),
                actual_type: xml_text(&b, "ACTUAL_TYPE_NAME").unwrap_or_default(),
                table_lines: xml_text(&b, "TABLE_LINES")
                    .and_then(|t| t.trim().parse().ok()),
                incomplete: xml_text(&b, "IS_VALUE_INCOMPLETE")
                    .map(|t| t.trim().eq_ignore_ascii_case("true"))
                    .unwrap_or(false),
            })
        })
        .collect()
}

// ========================================================================
// REST 处理器
// ========================================================================

pub fn router() -> axum::Router<crate::server::SharedState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route(
            "/api/debug/breakpoints",
            post(bp_set_handler)
                .get(bp_list_handler)
                .delete(bp_clear_handler),
        )
        .route(
            "/api/debug/breakpoints/{id}",
            axum::routing::delete(bp_delete_handler),
        )
        .route("/api/debug/sessions", post(session_create_handler))
        .route(
            "/api/debug/sessions/{sid}",
            get(session_status_handler).delete(session_close_handler),
        )
        .route(
            "/api/debug/sessions/{sid}/stack",
            get(session_stack_handler),
        )
        .route(
            "/api/debug/sessions/{sid}/variables",
            get(session_variables_handler),
        )
        .route(
            "/api/debug/sessions/{sid}/variables/{name}",
            axum::routing::put(variable_write_handler),
        )
        .route(
            "/api/debug/sessions/{sid}/control",
            post(session_control_handler),
        )
}

/// 取当前系统的活动会话（sid 不匹配 → 404；无会话 → 409 DEBUG_NO_SESSION）。
fn take_run(system: &str, sid: &str) -> Result<Arc<Run>, RfcError> {
    let m = SLOTS.lock().map_err(|_| poisoned())?;
    let slot = m.get(system).ok_or_else(|| {
        slot_err(409, "DEBUG_NO_SESSION", "该系统没有活动调试会话（先 POST /api/debug/sessions）")
    })?;
    let run = slot.run.lock().map_err(|_| poisoned())?;
    let r = run.as_ref().ok_or_else(|| {
        slot_err(409, "DEBUG_NO_SESSION", "该系统没有活动调试会话（先 POST /api/debug/sessions）")
    })?;
    if r.sid != sid {
        return Err(slot_err(
            404,
            "DEBUG_SESSION_UNKNOWN",
            format!("sid {sid} 与该系统当前会话不符（每系统至多一个会话）"),
        ));
    }
    let out = Arc::clone(r);
    drop(run);
    *out.last_used.lock().unwrap() = Instant::now();
    Ok(out)
}

/// 通道空闲快照（catch/步进期间忙）。
async fn run_chan(run: &Arc<Run>) -> tokio::sync::MutexGuard<'_, DebugSession> {
    run.chan.lock().await
}

fn require_stopped(run: &Run) -> Result<(), RfcError> {
    let phase = *run.phase.lock().unwrap();
    if phase == Phase::Stopped {
        return Ok(());
    }
    Err(slot_err(
        409,
        "DEBUG_NOT_STOPPED",
        format!(
            "会话状态为 {}（仅 stopped 可操作）{}",
            phase.as_str(),
            run.note.lock().unwrap().as_deref().map(|n| format!("：{n}")).unwrap_or_default()
        ),
    ))
}

/// 断点集合请求体。target 三选一：uri 直给 / kind+name（fm 需 group）/ 全部省略 =
/// 空集合（清空——POST 是整集合替换语义）。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BpSetBody {
    #[serde(default)]
    targets: Vec<BpTarget>,
    /// 允许 SAP 标准代码断点生效（默认 false——不开时标准码断点被接受但
    /// 永不触发，契约 §2）。语义上属于断点集合而非会话（断点不绑会话），
    /// 挂 POST 根属性 systemDebugging="true"。
    #[serde(default)]
    pub(crate) system_debug: bool,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct BpTarget {
    /// "prog" | "class" | "fm"；或直接给 uri（kind 省略）
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
    /// fm 形态的函数组（kind=fm 时必填）
    #[serde(default)]
    group: Option<String>,
    line: u64,
    #[serde(default)]
    condition: Option<String>,
}

fn bp_uri(t: &BpTarget) -> Result<String, RfcError> {
    // ADT 调试器断点资源的 URI-mapping **大小写敏感**（P0/P2 实证：大写类名
    // "URI-Mapping cannot be performed"，小写规范形可落位）——统一小写化。
    let name = |n: &Option<String>, what: &str| {
        n.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|s| s.to_lowercase()).ok_or_else(|| {
            slot_err(400, "DEBUG_BP_TARGET_INVALID", format!("{what} 缺 name"))
        })
    };
    // 断点资源要求完整 ADT URI（缺 /sap/bc/adt/ 前缀 → "URI-Mapping cannot be performed"）
    let full = |rel: String| {
        if rel.starts_with("/sap/bc/adt/") {
            rel
        } else {
            format!("/sap/bc/adt/{rel}")
        }
    };
    match t.kind.as_deref() {
        // uri 原样透传（大小写敏感——ADT URI 不走大写归一）；缺前缀自动补
        Some("uri") => t
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|u| full(u.to_string()))
            .ok_or_else(|| slot_err(400, "DEBUG_BP_TARGET_INVALID", "uri 形态缺 name")),
        Some("prog") => Ok(full(format!(
            "programs/programs/{}/source/main",
            name(&t.name, "prog")?
        ))),
        Some("class") => Ok(full(format!(
            "oo/classes/{}/source/main",
            name(&t.name, "class")?
        ))),
        Some("fm") => {
            let fm = name(&t.name, "fm")?;
            let group = t.group.as_deref().map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| {
                slot_err(400, "DEBUG_BP_TARGET_INVALID", "kind=fm 需要 group（函数组名）")
            })?;
            Ok(full(format!(
                "functions/groups/{}/fmodules/{}/source/main",
                group.to_lowercase(),
                fm
            )))
        }
        Some(other) => Err(slot_err(
            400,
            "DEBUG_BP_TARGET_INVALID",
            format!("未知 kind: {other}（prog/class/fm/uri）"),
        )),
        None => Err(slot_err(
            400,
            "DEBUG_BP_TARGET_INVALID",
            "缺 kind（prog/class/fm/uri）",
        )),
    }
}

impl Run {
    /// 该系统 ADT 端点（teardown 兜底开新会话用）。
    fn adt_endpoint(&self) -> std::sync::Arc<crate::adt::AdtEndpoint> {
        self.ep.clone()
    }

    /// 距最近一次 REST 交互（TTL 依据）。
    fn idle_for(&self) -> Duration {
        self.last_used.lock().map(|g| g.elapsed()).unwrap_or_default()
    }
}

/// 断点整集合替换（POST 语义）。空 targets = 清空。
/// 断点是用户态身份（user+ideId+terminalId），不绑会话——走临时短命通道，
/// agent 流程"先设断点→再建会话→再触发"成立（P0 实证）。
async fn bp_replace(
    chan: &mut DebugSession,
    user: &str,
    targets: &[BpTarget],
    system_debug: bool,
) -> Result<Vec<Bp>, RfcError> {
    let mut items = String::new();
    for t in targets {
        let uri = bp_uri(t)?;
        let full = format!("{uri}#start={}", t.line);
        let cond = t
            .condition
            .as_deref()
            .map(|c| format!(" condition=\"{}\"", xml_escape(c)))
            .unwrap_or_default();
        items.push_str(&format!(
            "<breakpoint kind=\"line\" enabled=\"true\" adtcore:uri=\"{}\"{cond}/>",
            xml_escape(&full)
        ));
    }
    let sysattr = if system_debug { " systemDebugging=\"true\"" } else { "" };
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<dbg:breakpoints xmlns:dbg="http://www.sap.com/adt/debugger" xmlns:adtcore="http://www.sap.com/adt/core" debuggingMode="user" scope="external" requestUser="{}" terminalId="{TERMINAL_ID}" ideId="{IDE_ID}"{}>{items}</dbg:breakpoints>"#,
        user,
        sysattr,
    );
    let r = chan
        .request(
            Method::POST,
            "debugger/breakpoints",
            &DebugRequest {
                accept: "*/*",
                content_type: Some("application/xml"),
                body: Some(body.as_bytes()),
                ..Default::default()
            },
        )
        .await?;
    if r.status != 200 {
        let sub = adt_error_subtype(&r.body);
        return Err(slot_err(
            r.status,
            "ADT_DEBUG_FAILED",
            format!(
                "设断点失败: ADT {}{}",
                r.status,
                sub.map(|s| format!(" ({s})")).unwrap_or_default()
            ),
        ));
    }
    // 响应解析：id 有 = 落点；id 无 + errorMessage = 拒收（显式上报，契约 §2）
    let s = String::from_utf8_lossy(&r.body).into_owned();
    let mut placed = Vec::new();
    for chunk in xml_split_tags(&s, "breakpoint") {
        let id = xml_attr(&chunk, "breakpoint", "id").unwrap_or_default();
        let uri = xml_attr(&chunk, "breakpoint", "adtcore:uri")
            .or_else(|| xml_attr(&chunk, "breakpoint", "uri"))
            .unwrap_or_default();
        let err = xml_attr(&chunk, "breakpoint", "errorMessage").filter(|e| !e.is_empty());
        let line = uri
            .rsplit("#start=")
            .next()
            .and_then(|l| l.parse().ok())
            .unwrap_or(0);
        if !id.is_empty() {
            placed.push(Bp { id, uri, line, error: None });
        } else {
            placed.push(Bp {
                id: String::new(),
                uri,
                line,
                error: err.or_else(|| Some("SAP 未给 id（该行无语句或不可放置）".into())),
            });
        }
    }
    Ok(placed)
}

pub(crate) async fn bp_set_handler(
    sap: Sap,
    axum::Json(body): axum::Json<BpSetBody>,
) -> Result<axum::response::Response, RfcError> {
    let user = adt_user(&sap)?;
    let slot = slot_for(sap.name.as_str())?;
    let placed = with_bp_chan!(&sap, &slot, chan, bp_replace(chan, &user, &body.targets, body.system_debug).await)?;
    let ok: Vec<Bp> = placed.iter().filter(|b| b.error.is_none()).cloned().collect();
    let rejected: Vec<&Bp> = placed.iter().filter(|b| b.error.is_some()).collect();
    *slot.bps.lock().unwrap() = ok.clone();
    Ok(axum::Json(json!({
        "count": ok.len(),
        "breakpoints": ok,
        "rejected": rejected,
        "note": "POST 为整集合替换（SAP 断点资源语义）；fm 形态需 group；该用户有活动调试会话时摘 listener 会连带清断点",
    }))
    .into_response())
}

pub(crate) async fn bp_list_handler(sap: Sap) -> Result<axum::response::Response, RfcError> {
    let slot = slot_for(sap.name.as_str())?;
    let ledger = slot.bps.lock().unwrap().clone();
    // 尝试向 SAP 复核（GET 常回空体——空体时回台账）
    let r = with_bp_chan!(&sap, &slot, chan,
        chan
            .request(
                Method::GET,
                "debugger/breakpoints?scope=external&debuggingMode=user",
                &DebugRequest { accept: "*/*", ..Default::default() },
            )
            .await
    )?;
    let bps = if r.status == 200 && !r.body.is_empty() {
        let s = String::from_utf8_lossy(&r.body).into_owned();
        let list: Vec<Bp> = xml_split_tags(&s, "breakpoint")
            .into_iter()
            .filter_map(|chunk| {
                let id = xml_attr(&chunk, "breakpoint", "id")?;
                let uri = xml_attr(&chunk, "breakpoint", "adtcore:uri")
                    .or_else(|| xml_attr(&chunk, "breakpoint", "uri"))
                    .unwrap_or_default();
                let line = uri.rsplit("#start=").next().and_then(|l| l.parse().ok()).unwrap_or(0);
                Some(Bp { id, uri, line, error: None })
            })
            .collect();
        if list.is_empty() {
            ledger
        } else {
            *slot.bps.lock().unwrap() = list.clone();
            list
        }
    } else {
        ledger
    };
    Ok(axum::Json(json!({ "count": bps.len(), "breakpoints": bps })).into_response())
}

pub(crate) async fn bp_clear_handler(sap: Sap) -> Result<axum::response::Response, RfcError> {
    let user = adt_user(&sap)?;
    let slot = slot_for(sap.name.as_str())?;
    with_bp_chan!(&sap, &slot, chan, bp_replace(chan, &user, &[], false).await)?;
    slot.bps.lock().unwrap().clear();
    Ok(axum::Json(json!({ "cleared": true })).into_response())
}

pub(crate) async fn bp_delete_handler(
    sap: Sap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<axum::response::Response, RfcError> {
    let slot = slot_for(sap.name.as_str())?;
    let cur = slot.bps.lock().unwrap().clone();
    let keep: Vec<BpTarget> = cur
        .iter()
        .filter(|b| b.id != id && !b.id.is_empty())
        .map(|b| {
            let (uri, line) = match b.uri.rsplit_once("#start=") {
                Some((u, l)) => (u.to_string(), l.parse().unwrap_or(0)),
                None => (b.uri.clone(), b.line),
            };
            BpTarget {
                kind: Some("uri".into()),
                name: Some(uri),
                group: None,
                line,
                condition: None,
            }
        })
        .collect();
    if keep.len() == cur.iter().filter(|b| !b.id.is_empty()).count() {
        return Err(slot_err(404, "DEBUG_BP_UNKNOWN", format!("断点 {id} 不在本会话台账中")));
    }
    let user = adt_user(&sap)?;
    let placed = with_bp_chan!(&sap, &slot, chan, bp_replace(chan, &user, &keep, false).await)?;
    *slot.bps.lock().unwrap() = placed.iter().filter(|b| b.error.is_none()).cloned().collect();
    Ok(axum::Json(json!({ "deleted": id, "remaining": keep.len() })).into_response())
}

/// 建调试会话并开始 listen。监听在后台任务里阻塞至命中或超时；
/// agent 侧轮询 GET /api/debug/sessions/{sid}。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionCreateBody {
    /// listen 等待窗（秒）；也是 debuggee 的等待窗
    #[serde(default = "default_listen_secs")]
    pub(crate) listen_secs: u64,
}

fn default_listen_secs() -> u64 {
    DEFAULT_LISTEN_SECS
}

pub(crate) async fn session_create_handler(
    sap: Sap,
    body: Option<axum::Json<SessionCreateBody>>,
) -> Result<axum::response::Response, RfcError> {
    let SessionCreateBody { listen_secs } = body.map(|axum::Json(b)| b).unwrap_or(SessionCreateBody {
        listen_secs: DEFAULT_LISTEN_SECS,
    });
    let listen_secs = listen_secs.clamp(1, 600);
    let ep = sap.adt.as_ref().ok_or_else(crate::adt::adt_disabled)?.clone();

    // 每系统一个槽
    let slot = {
        let mut m = SLOTS.lock().map_err(|_| poisoned())?;
        m.entry(sap.name.clone()).or_insert_with(|| {
            Arc::new(Slot {
                run: Mutex::new(None),
                bps: Mutex::new(Vec::new()),
                bp_chan: Arc::new(tokio::sync::Mutex::new(None)),
            })
        })
        .clone()
    };
    {
        let cur = slot.run.lock().map_err(|_| poisoned())?;
        if cur.is_some() {
            let sid = cur.as_ref().map(|r| r.sid.clone()).unwrap_or_default();
            let phase = cur.as_ref().map(|r| phase_str(r)).unwrap_or("?");
            return Err(slot_err(
                409,
                "DEBUG_SESSION_EXISTS",
                format!("该系统已有调试会话 {sid}（状态 {phase}）——每系统至多一个；先 DELETE /api/debug/sessions/{sid}"),
            ));
        }
    }

    let sid = new_sid();
    let run_ep = ep.clone();
    let chan = DebugSession::open(ep).await.map_err(|e| slot_err(
        502,
        "ADT_DEBUG_SESSION_FAILED",
        format!("调试会话建立失败: {}", e.message),
    ))?;
    let user = chan_user(&chan);
    let run = Arc::new(Run {
        sid: sid.clone(),
        system: sap.name.clone(),
        ep: run_ep,
        chan: tokio::sync::Mutex::new(chan),
        user,
        phase: Mutex::new(Phase::Listening),
        position: Mutex::new(None),
        stack: Mutex::new(Vec::new()),
        note: Mutex::new(None),
        keepalive: Mutex::new(None),
        frame: Mutex::new(0),
        created: Instant::now(),
        last_used: Mutex::new(Instant::now()),
    });
    *slot.run.lock().map_err(|_| poisoned())? = Some(Arc::clone(&run));
    update_sessions_gauge();

    // keepalive + TTL 清扫（每会话一个轻任务；忙时跳过，超时自拆）
    {
        let krun = Arc::clone(&run);
        let handle = tokio::spawn(async move {
            let mut tick = tokio::time::interval(KEEPALIVE_TICK);
            tick.tick().await; // 首拍立即
            loop {
                tick.tick().await;
                if krun.idle_for() > SESSION_TTL {
                    tracing::info!(system = %krun.system, sid = %krun.sid, "调试会话闲置超 TTL，拆除");
                    let _ = teardown(&krun.system, &krun).await;
                    remove_slot(&krun.system, &krun.sid);
                    break;
                }
                // try_lock：catch/步进进行中就跳过这一拍
                if let Ok(chan) = krun.chan.try_lock() {
                    let _ = chan.keepalive().await;
                }
            }
        });
        *run.keepalive.lock().unwrap() = Some(handle);
    }

    // catch 后台任务
    let crun = Arc::clone(&run);
    tokio::spawn(async move {
        catch_loop(crun, listen_secs).await;
    });

    Ok((
        axum::http::StatusCode::ACCEPTED,
        axum::Json(json!({
            "sid": sid,
            "system": sap.name,
            "state": "listening",
            "listen_secs": listen_secs,
            "note": "轮询 GET /api/debug/sessions/{sid}；命中后 state=stopped。触发：设断点后用 /api/abap/run 或 /api/rfc 跑同用户代码",
        })),
    )
        .into_response())
}

fn chan_user(chan: &DebugSession) -> String {
    chan.user().to_string()
}

fn phase_str(run: &Run) -> &'static str {
    run.phase.lock().unwrap().as_str()
}

fn new_sid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("dbg-{}-{:x}", n, SEQ.fetch_add(1, Ordering::Relaxed))
}

use std::time::SystemTime;

/// 关停序列（顺序与通道选择都是硬约束，2026-11-19 挂死实证）：
///
/// 1. **listener 先摘，且绝不走调试会话本身**——注册按 user 单键，任意同登录
///    连接可删（契约 §3；vsp 的 ADTStopListenerVia 正是为此）。调试会话在
///    listening 期被阻塞的 listen 占着、在 debuggee ended 后服务端已死，在这条
///    会话上发请求会排队到 60s 客户端超时——close 挂死的根因。走槽内缓存的
///    断点通道（bp_chan，独立会话）；失败再开一条新会话兜底。
/// 2. **仅 Stopped 态才发 detach**：listening 期无附加 debuggee（listener 已删，
///    等待中的 debuggee 随之放行）；resume 后（Finished）服务端调试会话已终结，
///    detach 只会挂在死上下文上。detach 用 try_lock——拿不到就说明会话忙，
///    listener 已删、无需强等。每请求 10s 短超时，close 最坏 ~20s 必返回。
async fn teardown(sap_name: &str, run: &Run) -> Result<(), RfcError> {
    if let Some(h) = run.keepalive.lock().unwrap().take() {
        h.abort();
    }
    // 拿一条「另一条连接」：优先槽内断点通道，失败/缺失则新开
    let slot = slot_for(sap_name)?;
    let via: Option<tokio::sync::OwnedMutexGuard<Option<DebugSession>>> = {
        let g = slot.bp_chan.lock().await;
        if g.is_some() {
            drop(g);
            Some(slot.bp_chan.clone().lock_owned().await)
        } else {
            None
        }
    };
    let q = format!(
        "debugger/listeners?debuggingMode=user&requestUser={}",
        run.user
    );
    let mut listener_ok = false;
    if let Some(guard) = via {
        if let Some(chan) = guard.as_ref() {
            listener_ok = listener_delete(chan, &q).await.is_ok();
        }
    }
    if !listener_ok {
        // 兜底：新开一条短命会话删 listener（teardown 低频，可接受）
        drop_bp_channel(&slot).await;
        let ep = run.adt_endpoint();
        if let Ok(fresh) = DebugSession::open(ep).await {
            listener_ok = listener_delete(&fresh, &q).await.is_ok();
        }
    }
    if !listener_ok {
        tracing::warn!(system = %run.system, sid = %run.sid, "摘 listener 失败（断点可能残留；可用 DELETE /api/debug/breakpoints?all=true 清场）");
    }

    // debuggee 侧：仅 Stopped 且能立刻拿到会话时才 detach
    let phase = *run.phase.lock().unwrap();
    if phase == Phase::Stopped {
        if let Ok(chan) = run.chan.try_lock() {
            let det = chan
                .request(
                    Method::POST,
                    "debugger?method=detach",
                    &DebugRequest { accept: "*/*", timeout: Some(Duration::from_secs(10)), ..Default::default() },
                )
                .await;
            let clean = matches!(&det, Ok(r) if r.status == 200 || r.status == 404)
                || det.as_ref().map(|r| adt_error_subtype(&r.body).as_deref() == Some("noSessionAttached")).unwrap_or(false);
            if !clean {
                // 回落 stepContinue（10s）；ended/noSessionAttached = 干净
                if let Ok(r2) = chan
                    .request(
                        Method::POST,
                        "debugger?method=stepContinue",
                        &DebugRequest { accept: "*/*", timeout: Some(Duration::from_secs(10)), ..Default::default() },
                    )
                    .await
                {
                    let sub = adt_error_subtype(&r2.body);
                    tracing::debug!(?sub, status = r2.status, "detach 回落 stepContinue 结果");
                }
            }
        }
    }
    Ok(())
}

/// 按用户单键摘 listener（10s 短超时；200/204/404 都算干净——404 = 本就没登记）。
async fn listener_delete(chan: &DebugSession, q: &str) -> Result<(), RfcError> {
    let r = chan
        .request(Method::DELETE, q, &DebugRequest {
            accept: "*/*",
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        })
        .await?;
    if (200..300).contains(&r.status) || r.status == 404 {
        Ok(())
    } else {
        Err(slot_err(r.status, "ADT_DEBUG_FAILED", format!("摘 listener 失败: ADT {}", r.status)))
    }
}

fn remove_slot(system: &str, sid: &str) {
    // ⚠️ 先释放 SLOTS 守卫再 update_sessions_gauge——后者内部再锁 SLOTS，
    // std Mutex 不可重入：持锁嵌套 = 自死锁（2026-11-19 close 挂死 + 网关
    // 整体假死的根因：死等线程逐步吃光全部 tokio worker）。
    {
        let mut m = SLOTS.lock().unwrap();
        let remove = m
            .get(system)
            .and_then(|s| s.run.lock().ok())
            .map(|r| r.as_ref().map(|x| x.sid == sid).unwrap_or(false))
            .unwrap_or(false);
        if remove {
            m.remove(system);
        }
    }
    update_sessions_gauge();
}

pub(crate) async fn session_status_handler(
    sap: Sap,
    axum::extract::Path(sid): axum::extract::Path<String>,
) -> Result<axum::response::Response, RfcError> {
    let run = take_run(sap.name.as_str(), &sid)?;
    let phase = *run.phase.lock().unwrap();
    let pos = run.position.lock().unwrap().clone();
    let note = run.note.lock().unwrap().clone();
    let stack_len = run.stack.lock().unwrap().len();
    let mut out = json!({
        "sid": run.sid,
        "system": run.system,
        "state": phase.as_str(),
        "age_secs": run.created.elapsed().as_secs(),
        "idle_secs": run.idle_for().as_secs(),
        "stack_frames": stack_len,
        "breakpoints": SLOTS
            .lock()
            .map(|m| m.get(&run.system).and_then(|s| s.bps.lock().ok().map(|b| b.len())).unwrap_or(0))
            .unwrap_or(0),
    });
    if let Some(p) = &pos {
        out["position"] = json!(p);
    }
    if let Some(n) = &note {
        out["note"] = json!(n);
    }
    Ok(axum::Json(out).into_response())
}

pub(crate) async fn session_close_handler(
    sap: Sap,
    axum::extract::Path(sid): axum::extract::Path<String>,
) -> Result<axum::response::Response, RfcError> {
    let run = take_run(sap.name.as_str(), &sid)?;
    let _ = teardown(&run.system, &run).await;
    set_phase(&run, Phase::Finished, Some("已显式关闭（detach + 摘 listener + 清断点）".into()));
    // 摘 listener = 结束该用户外部调试，断点被 SAP 连带清空（契约 §3）——台账同步清
    if let Ok(m) = SLOTS.lock() {
        if let Some(slot) = m.get(&run.system) {
            if let Ok(mut b) = slot.bps.lock() {
                b.clear();
            }
        }
    }
    remove_slot(&run.system, &run.sid);
    Ok(axum::Json(json!({
        "closed": true,
        "sid": sid,
        "note": "摘 listener 连带清除了该用户全部外部断点（SAP 语义）",
    }))
    .into_response())
}

pub(crate) async fn session_stack_handler(
    sap: Sap,
    axum::extract::Path(sid): axum::extract::Path<String>,
) -> Result<axum::response::Response, RfcError> {
    let run = take_run(sap.name.as_str(), &sid)?;
    require_stopped(&run)?;
    let frames = run.stack.lock().unwrap().clone();
    Ok(axum::Json(json!({ "count": frames.len(), "frames": frames })).into_response())
}

pub(crate) async fn session_variables_handler(
    sap: Sap,
    axum::extract::Path(sid): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<VarQuery>,
) -> Result<axum::response::Response, RfcError> {
    let run = take_run(sap.name.as_str(), &sid)?;
    require_stopped(&run)?;
    let mut chan = run_chan(&run).await;
    if let Some(f) = q.frame {
        if f != *run.frame.lock().unwrap() {
            switch_frame(&mut chan, &run, f).await?;
        }
    }
    let frame_now = *run.frame.lock().unwrap();
    let (kids, vars) = list_variables(&mut chan, q.parent.as_deref()).await?;
    Ok(axum::Json(json!({
        "frame": frame_now,
        "hierarchy": kids.iter().map(|(id, name)| json!({"id": id, "name": name})).collect::<Vec<_>>(),
        "variables": vars,
        "note": "结构/内表用 ?parent=<id> 展开；@ROOT 自动下探到本地变量层；?frame=N 切换栈帧读调用者变量",
    }))
    .into_response())
}

/// 切帧：PUT 帧的 stackUri（契约 §5），服务端当前帧随之改变——后续
/// getVariables/getChildVariables 读的就是该帧的本地变量。
async fn switch_frame(
    chan: &mut DebugSession,
    run: &Arc<Run>,
    target: u64,
) -> Result<(), RfcError> {
    let frames = run.stack.lock().unwrap().clone();
    let f = frames.iter().find(|x| x.index == target).ok_or_else(|| {
        slot_err(
            404,
            "DEBUG_FRAME_UNKNOWN",
            format!("帧 {target} 不在调用栈中（0..={}）", frames.len().saturating_sub(1)),
        )
    })?;
    let rel = f
        .stack_uri
        .strip_prefix("/sap/bc/adt/")
        .unwrap_or(&f.stack_uri)
        .to_string();
    let r = chan
        .request(
            Method::PUT,
            &rel,
            &DebugRequest {
                accept: "*/*",
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            },
        )
        .await?;
    if r.status != 200 {
        let sub = adt_error_subtype(&r.body);
        return Err(slot_err(
            r.status,
            "ADT_DEBUG_FAILED",
            format!(
                "切帧 {target} 失败: ADT {}{}",
                r.status,
                sub.map(|s| format!(" ({s})")).unwrap_or_default()
            ),
        ));
    }
    *run.frame.lock().unwrap() = target;
    Ok(())
}

#[derive(Deserialize)]
pub(crate) struct VarQuery {
    #[serde(default)]
    pub(crate) parent: Option<String>,
    /// 目标栈帧（缺省 = 当前帧；?frame=N 先 PUT 切帧再读——调用者的本地变量）
    #[serde(default)]
    pub(crate) frame: Option<u64>,
}

pub(crate) async fn variable_write_handler(
    sap: Sap,
    axum::extract::Path((sid, name)): axum::extract::Path<(String, String)>,
    axum::Json(body): axum::Json<VarWriteBody>,
) -> Result<axum::response::Response, RfcError> {
    let run = take_run(sap.name.as_str(), &sid)?;
    require_stopped(&run)?;
    let value = body.value;
    let mut chan = run_chan(&run).await;
    let q = format!(
        "debugger?method=setVariableValue&variableName={}",
        xml_escape(&name)
    );
    let r = chan
        .request(
            Method::POST,
            &q,
            &DebugRequest {
                accept: "*/*",
                body: Some(value.as_bytes()),
                ..Default::default()
            },
        )
        .await?;
    if r.status != 200 {
        let sub = adt_error_subtype(&r.body);
        return Err(slot_err(
            r.status,
            "ADT_DEBUG_FAILED",
            format!(
                "写变量 {name} 失败: ADT {}{}",
                r.status,
                sub.map(|s| format!(" ({s})")).unwrap_or_default()
            ),
        ));
    }
    metrics::counter!("sap_debug_var_writes_total").increment(1);
    // 回读确认
    let confirmed = read_variables(&mut chan, &[&name])
        .await
        .ok()
        .and_then(|vs| vs.into_iter().next());
    Ok(axum::Json(json!({
        "name": name,
        "value": value,
        "confirmed": confirmed.as_ref().map(|v| v.value.clone()),
        "variable": confirmed,
    }))
    .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VarWriteBody {
    pub(crate) value: String,
}

pub(crate) async fn session_control_handler(
    sap: Sap,
    axum::extract::Path(sid): axum::extract::Path<String>,
    axum::Json(body): axum::Json<ControlBody>,
) -> Result<axum::response::Response, RfcError> {
    let run = take_run(sap.name.as_str(), &sid)?;
    require_stopped(&run)?;
    let mut chan = run_chan(&run).await;
    let (ended, pos, frames) = step_and_refresh(&mut chan, &run, &body.action).await?;
    drop(chan);
    metrics::counter!("sap_debug_steps_total", "action" => body.action.clone()).increment(1);
    if ended {
        set_phase(&run, Phase::Finished, Some("debuggee 已结束（resume）".into()));
        return Ok(axum::Json(json!({
            "action": body.action,
            "ended": true,
            "state": "finished",
        }))
        .into_response());
    }
    Ok(axum::Json(json!({
        "action": body.action,
        "ended": false,
        "state": "stopped",
        "position": pos,
        "stack_frames": frames.len(),
    }))
    .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlBody {
    /// step_into | step_over | step_out | resume
    pub(crate) action: String,
}

/// 优雅停机：teardown 全部活动调试会话（detach + 摘 listener + 连带清断点）。
/// 残留 listener 行会静默吞掉该用户的下一个 debuggee（契约 §3），进程退出前
/// 必须清掉。并发执行、总预算 20s（超时放弃仅 WARN——停机不可被 SAP 卡死；
/// 极端残留可用 DELETE /api/debug/breakpoints?all=true 手工清场）。
pub(crate) async fn shutdown_all() {
    let runs: Vec<(String, Arc<Run>)> = {
        let Ok(m) = SLOTS.lock() else { return };
        m.iter()
            .filter_map(|(name, slot)| {
                slot.run
                    .lock()
                    .ok()
                    .and_then(|r| r.as_ref().map(|x| (name.clone(), Arc::clone(x))))
            })
            .collect()
    };
    if runs.is_empty() {
        return;
    }
    tracing::info!(count = runs.len(), "停机清理调试会话");
    let mut set = tokio::task::JoinSet::new();
    for (name, run) in runs {
        set.spawn(async move {
            let _ = teardown(&name, &run).await;
        });
    }
    if tokio::time::timeout(Duration::from_secs(20), set.join_all()).await.is_err() {
        tracing::warn!("停机清理调试会话超时（20s），放弃剩余清理");
    }
}

fn poisoned() -> RfcError {
    RfcError {
        code: -1,
        status: 500,
        message: "调试状态内部锁损坏".into(),
        key: "ADT_INTERNAL".into(),
    }
}

// ========================================================================
// 单测（纯解析器，无 SAP）
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stack_document() {
        let s = r#"<?xml version="1.0" encoding="utf-8"?><dbg:stack debugCursorStackIndex="0" xmlns:dbg="http://www.sap.com/adt/debugger">
<stackEntry stackPosition="0" stackType="MAIN_PROGRAM" stackUri="/sap/bc/adt/debugger/stack/type/ABAP/position/0" programName="ZTMP_DEBUG_SFA_CL=============CP" includeName="ZTMP_DEBUG_SFA_CL=============CM002" line="12" eventType="METHOD" eventName="RUN" sourceType="ABAP" systemProgram="false" isVit="false" uri="/sap/bc/adt/oo/classes/ztmp_debug_sfa_cl/source/main#start=12"/>
<stackEntry stackPosition="1" programName="ZCL_SFA_RUN_X" includeName="ZCL_SFA_RUN_X===CM001" line="3" eventType="METHOD" eventName="MAIN" sourceType="ABAP" uri="/x#start=3"/>
</dbg:stack>"#;
        let frames = parse_stack(s);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].index, 0);
        assert_eq!(frames[0].line, 12);
        assert_eq!(frames[0].include, "ZTMP_DEBUG_SFA_CL=============CM002");
        assert_eq!(frames[1].index, 1);
        assert_eq!(frames[1].event_name, "MAIN");
    }

    #[test]
    fn parses_variables_and_hierarchy() {
        let s = r#"<?xml version="1.0" encoding="utf-8"?><asx:abap version="1.0" xmlns:asx="http://www.sap.com/abapxml"><asx:values><DATA><VARIABLES>
<STPDA_ADT_VARIABLE><ID>LV_X</ID><NAME>LV_X</NAME><DECLARED_TYPE_NAME>I</DECLARED_TYPE_NAME><ACTUAL_TYPE_NAME>I</ACTUAL_TYPE_NAME><KIND>variable</KIND><VALUE>1 </VALUE><HEX_VALUE>01000000</HEX_VALUE><LENGTH>4</LENGTH><TABLE_LINES>0</TABLE_LINES><IS_VALUE_INCOMPLETE>false</IS_VALUE_INCOMPLETE></STPDA_ADT_VARIABLE>
</VARIABLES><HIERARCHIES><STPDA_ADT_VARIABLE_HIERARCHY><PARENT_ID>@ROOT</PARENT_ID><CHILD_ID>@LOCALS</CHILD_ID><CHILD_NAME>Locals</CHILD_NAME></STPDA_ADT_VARIABLE_HIERARCHY></HIERARCHIES></DATA></asx:values></asx:abap>"#;
        let vars = parse_variables(s);
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].id, "LV_X");
        assert_eq!(vars[0].value, "1 ");
        assert_eq!(vars[0].declared_type, "I");
        assert!(!vars[0].incomplete);
    }

    #[test]
    fn extracts_error_subtype() {
        let s = r#"<exc:exception xmlns:exc="http://www.sap.com/abapxml/types/communicationframework"><properties><entry key="com.sap.adt.communicationFramework.subType">noSessionAttached</entry></properties></exc:exception>"#;
        assert_eq!(adt_error_subtype(s.as_bytes()).as_deref(), Some("noSessionAttached"));
        assert_eq!(adt_error_subtype(b""), None);
    }

    #[test]
    fn xml_text_and_blocks() {
        let s = "<r><STPDA_DEBUGGEE><DEBUGGEE_ID>AB1</DEBUGGEE_ID><LINE_CURR>3</LINE_CURR></STPDA_DEBUGGEE><STPDA_DEBUGGEE><DEBUGGEE_ID>CD2</DEBUGGEE_ID><LINE_CURR>9</LINE_CURR></STPDA_DEBUGGEE></r>";
        assert_eq!(xml_text(s, "DEBUGGEE_ID").as_deref(), Some("AB1"));
        let blocks = xml_all_blocks(s, "STPDA_DEBUGGEE");
        assert_eq!(blocks.len(), 2);
        assert_eq!(xml_text(&blocks[1], "LINE_CURR").as_deref(), Some("9"));
    }

    #[test]
    fn bp_uri_builder() {
        let t = |kind: &str, name: &str, group: Option<&str>| BpTarget {
            kind: Some(kind.into()),
            name: Some(name.into()),
            group: group.map(Into::into),
            line: 1,
            condition: None,
        };
        assert_eq!(
            bp_uri(&t("prog", "zfoo", None)).unwrap(),
            "/sap/bc/adt/programs/programs/zfoo/source/main"
        );
        assert_eq!(
            bp_uri(&t("class", "zcl_x", None)).unwrap(),
            "/sap/bc/adt/oo/classes/zcl_x/source/main"
        );
        assert_eq!(
            bp_uri(&t("fm", "z_fm", Some("zgrp"))).unwrap(),
            "/sap/bc/adt/functions/groups/zgrp/fmodules/z_fm/source/main"
        );
        assert!(bp_uri(&t("fm", "z_fm", None)).is_err(), "fm 缺 group 必须报错");
        // uri 形态：全路径原样、缺前缀自动补
        assert_eq!(
            bp_uri(&t("uri", "/sap/bc/adt/programs/programs/X/source/main", None)).unwrap(),
            "/sap/bc/adt/programs/programs/X/source/main"
        );
        assert_eq!(
            bp_uri(&t("uri", "programs/programs/X/source/main", None)).unwrap(),
            "/sap/bc/adt/programs/programs/X/source/main"
        );
    }
}
