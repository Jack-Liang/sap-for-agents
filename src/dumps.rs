//! ABAP 短转储（ST22）结构化读取。
//!
//! ADT 代理（`/api/adt/**`）透传原始 Atom feed 与纯文本正文；本模块在其上
//! 提供解析后的 JSON，让调用方（AI）免于拉取并阅读 45KB–1MB 的原始文本：
//!
//! - 列表/聚合：`GET /sap/bc/adt/runtime/dumps` 的 Atom feed → 结构化条目。
//!   错误类型与终止程序是 Atom category，用户是 entry author，均为结构化
//!   字段——列表与聚合**零详情请求**，不碰正文文本。
//! - 详情：`GET /sap/bc/adt/runtime/dump/{key}/formatted` 的英文文本 →
//!   头表（Runtime Errors / Except. / Application Component…）、终止点
//!   （include/line/procedure/main program）与调用栈。
//!
//! 解析规则移植自 vibing-steampunk 项目在 7.50/7.57/7.58 真机验证过的实现
//! （仅移植算法事实，代码为 Rust 重写），关键取舍：
//! - 详情文本按英文标签匹配；非英文 logon 的系统返回空字段而非错值；
//! - "Application Component = Not assigned" 归一化为空——自定义代码几乎都
//!   未分配组件，把它当值会让所有未分配对象看起来是同一个社区；
//! - 栈帧列位置在 release 间会漂移，按「从两端向内」解析而非固定列宽；
//! - 行号 include 都为空/0 时视为「无源码位置」（如死在 SYSTEM-EXIT），
//!   不记录，避免无关 dump 匹配到不存在的位置上。

use crate::adt::encode_path_segment;
use crate::error::RfcError;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

// ========================================================================
// DTO
// ========================================================================

/// 一条短转储（来自 feed 的结构化条目）。
#[derive(Debug, Serialize)]
pub struct DumpEntry {
    /// 详情端点用的 key（从 entry 的 rel=self 链接或 id 派生），
    /// 传给 `GET /api/dumps/{key}/detail`
    pub key: String,
    /// 发生时间（feed 原文 RFC3339 字符串）
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// 运行时错误类型（如 TIME_OUT / RAISE_EXCEPTION）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    /// 终止的 ABAP 程序（报表名或类的 class pool 名）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// 消息标题（feed entry title）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// feed entry 原始 id（资源 URI，调试用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// RFC3339 → epoch 秒，仅服务端排序用，不序列化
    #[serde(skip)]
    pub at_secs: Option<i64>,
}

/// 按 (错误类型, 终止程序) 聚合的转储组。
#[derive(Debug, Serialize)]
pub struct DumpGroup {
    pub error_type: String,
    pub program: String,
    /// 组内条数——「什么在反复失败」的直接答案
    pub count: usize,
    /// 组内最早/最晚条目的时间（feed 原文 RFC3339 字符串）
    pub first: String,
    pub last: String,
    /// 组内去重用户（已排序）
    pub users: Vec<String>,
    /// 最新一条的 key，可直接接 `GET /api/dumps/{key}/detail`
    pub latest_key: String,
    /// 最新一条的消息标题
    pub latest_message: String,
    /// 排序用的 last epoch 秒（不序列化）
    #[serde(skip)]
    last_secs: i64,
}

/// 调用栈的一个帧（最内层在前，保持 SAP 打印顺序）。
#[derive(Debug, Serialize)]
pub struct DumpFrame {
    pub position: u32,
    #[serde(rename = "type")]
    pub type_name: String,
    pub program: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub include: String,
    pub line: u32,
    /// 帧名（CL_X=>METHOD / FORM 名 / FM 名…）
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
}

/// 单个转储的结构化详情（解析自 `/formatted` 英文文本）。
#[derive(Debug, Serialize)]
pub struct DumpDetail {
    pub key: String,
    /// 头表「Runtime Errors」
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error_type: String,
    /// 头表「Except.」——非异常类运行时错误为空
    #[serde(skip_serializing_if = "String::is_empty")]
    pub exception: String,
    /// 头表「ABAP: Program」。注意它与 feed 的终止程序不一定相同：
    /// RAISE_EXCEPTION 时 feed 指向抛异常的标准类，头表指向调用它的自定义类，
    /// 两者都是事实，组件归属以头表为准
    #[serde(skip_serializing_if = "String::is_empty")]
    pub program: String,
    /// 头表「Application Component」；Not assigned 归一化为空
    #[serde(skip_serializing_if = "String::is_empty")]
    pub component: String,
    // ---- 终止点（Information on where terminated 章节）----
    #[serde(skip_serializing_if = "String::is_empty")]
    pub include: String,
    /// 出错行号；0 = 无源码位置（如死在 SYSTEM-EXIT）
    pub line: u32,
    /// 死在哪个过程（方法/FORM/函数）
    #[serde(skip_serializing_if = "String::is_empty")]
    pub procedure: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub main_program: String,
    /// Active Calls/Events 调用栈（最内层在前）
    pub stack: Vec<DumpFrame>,
    /// 头表全部标签→值（Date/Time、User、Host 等未单独建模的行都在这里）
    pub header: BTreeMap<String, String>,
}

// ========================================================================
// feed 解析（Atom → DumpEntry）
// ========================================================================

/// 从 id 或 self 链接派生详情 key：去掉 query/fragment，取最后一个
/// `/dump/` 或 `/dumps/` 之后的片段（feed 指向 /vit/ 查看器资源，
/// 可读的详情资源去掉该段）。
fn dump_key_from(id_or_href: &str) -> String {
    let s = id_or_href
        .split(['?', '#'])
        .next()
        .unwrap_or(id_or_href)
        .trim();
    if let Some(i) = s.rfind("/dump/") {
        return s[i + "/dump/".len()..].to_string();
    }
    if let Some(i) = s.rfind("/dumps/") {
        return s[i + "/dumps/".len()..].to_string();
    }
    s.to_string()
}

/// 元素限定名的本地部分（去掉命名空间前缀，如 `atom:entry` → `entry`）。
fn local_name(qname: &[u8]) -> &str {
    let start = qname
        .iter()
        .position(|&b| b == b':')
        .map(|i| i + 1)
        .unwrap_or(0);
    std::str::from_utf8(&qname[start..]).unwrap_or("")
}

/// XML 文本/属性值解码（UTF-8 宽松 + 实体反转义）。
fn xml_decode(raw: &[u8]) -> String {
    let lossy = String::from_utf8_lossy(raw).into_owned();
    quick_xml::escape::unescape(&lossy)
        .map(|c| c.into_owned())
        .unwrap_or(lossy)
}

/// 解析 Atom feed。仅依赖结构化字段（category/author/published），
/// 不解析正文的转义标记。
pub fn parse_feed(xml: &str) -> Result<Vec<DumpEntry>, RfcError> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let feed_err = |msg: &str| RfcError {
        code: -1,
        status: 502,
        message: format!("短转储 feed 解析失败（{}）", msg),
        key: "DUMP_FEED_INVALID".into(),
    };

    let mut reader = Reader::from_str(xml);
    let mut entries = Vec::new();

    let mut saw_feed = false;
    let mut in_entry = false;
    let mut in_author = false;
    // 当前正在收集文本的 entry 直属子元素名（"" = 不收集）
    let mut capturing = "";
    let mut text = String::new();

    let mut e_id = String::new();
    let mut e_title = String::new();
    let mut e_published = String::new();
    let mut e_author = String::new();
    let mut e_categories: Vec<(String, String)> = Vec::new(); // (term, label)
    let mut e_self_href: Option<String> = None;

    let read_attrs = |e: &quick_xml::events::BytesStart<'_>| -> Vec<(String, String)> {
        // (key, value) 属性对；命名空间声明跳过
        let mut out = Vec::new();
        for a in e.attributes().flatten() {
            if !a.key.as_ref().starts_with(b"xmlns") {
                out.push((
                    local_name(a.key.as_ref()).to_string(),
                    xml_decode(a.value.as_ref()),
                ));
            }
        }
        out
    };

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref()).to_string();
                if !saw_feed && name == "feed" {
                    saw_feed = true;
                    continue;
                }
                match name.as_str() {
                    "entry" if saw_feed && !in_entry => {
                        in_entry = true;
                        e_id.clear();
                        e_title.clear();
                        e_published.clear();
                        e_author.clear();
                        e_categories.clear();
                        e_self_href = None;
                        capturing = "";
                    }
                    "author" if in_entry => in_author = true,
                    "name" if in_entry && in_author => {
                        capturing = "name";
                        text.clear();
                    }
                    "id" | "title" | "published" if in_entry && !in_author => {
                        // capturing 用字面量而非借用 name（name 是本分支局部 String）
                        capturing = match name.as_str() {
                            "id" => "id",
                            "title" => "title",
                            _ => "published",
                        };
                        text.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                let name = local_name(e.name().as_ref()).to_string();
                if !in_entry {
                    continue;
                }
                if name == "category" {
                    let attrs = read_attrs(&e);
                    let term = attrs.iter().find(|(k, _)| k == "term").map(|(_, v)| v.clone());
                    let label = attrs.iter().find(|(k, _)| k == "label").map(|(_, v)| v.clone());
                    if let (Some(t), Some(l)) = (term, label) {
                        e_categories.push((t, l));
                    }
                } else if name == "link" {
                    let attrs = read_attrs(&e);
                    let rel = attrs.iter().find(|(k, _)| k == "rel").map(|(_, v)| v.clone());
                    let href = attrs.iter().find(|(k, _)| k == "href").map(|(_, v)| v.clone());
                    if rel.as_deref() == Some("self") {
                        if let Some(h) = href {
                            e_self_href = Some(h);
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if !capturing.is_empty() {
                    text.push_str(&xml_decode(t.as_ref()));
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref()).to_string();
                if !in_entry {
                    continue;
                }
                match name.as_str() {
                    "author" => {
                        in_author = false;
                        if capturing == "name" {
                            capturing = "";
                        }
                    }
                    _ if name == capturing => match capturing {
                        "id" => e_id = text.trim().to_string(),
                        "title" => e_title = text.trim().to_string(),
                        "published" => e_published = text.trim().to_string(),
                        "name" => e_author = text.trim().to_uppercase(),
                        _ => {}
                    },
                    "entry" => {
                        let error_type = e_categories
                            .iter()
                            .find(|(_, l)| l.eq_ignore_ascii_case("ABAP runtime error"))
                            .map(|(t, _)| t.trim().to_string())
                            .filter(|s| !s.is_empty());
                        let program = e_categories
                            .iter()
                            .find(|(_, l)| l.eq_ignore_ascii_case("Terminated ABAP program"))
                            .map(|(t, _)| t.trim().to_string())
                            .filter(|s| !s.is_empty());
                        // key 优先取 rel=self 链接，回退 entry id
                        let key_src = e_self_href
                            .clone()
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| e_id.clone());
                        let key = if key_src.is_empty() {
                            String::new()
                        } else {
                            dump_key_from(&key_src)
                        };
                        entries.push(DumpEntry {
                            key,
                            at: e_published.clone(),
                            user: if e_author.is_empty() {
                                None
                            } else {
                                Some(e_author.clone())
                            },
                            error_type,
                            program,
                            message: if e_title.is_empty() {
                                None
                            } else {
                                Some(e_title.clone())
                            },
                            id: if e_id.is_empty() {
                                None
                            } else {
                                Some(e_id.clone())
                            },
                            at_secs: rfc3339_to_secs(&e_published),
                        });
                        in_entry = false;
                        capturing = "";
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(feed_err(&e.to_string())),
        }
    }

    if !saw_feed {
        // ADT 对部分错误（如鉴权失败被 ICF 兜底）可能返回非 Atom 文档
        return Err(feed_err("响应不是合法的 Atom feed"));
    }
    Ok(entries)
}

/// RFC3339 → epoch 秒（仅用于排序比较，绝对值无关紧要）。
/// 支持 `2026-08-24T01:20:09[.fff](Z|±HH:MM)`。
fn rfc3339_to_secs(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        std::str::from_utf8(&b[r]).ok()?.parse::<i64>().ok()
    };
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    if b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    if !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let hour = num(11..13)?;
    let min = num(14..16)?;
    let sec = num(17..19)?;
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    // 秒后可跟小数秒，再跟时区
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    let offset = match b.get(i) {
        Some(&b'Z') | Some(&b'z') => 0i64,
        Some(sign @ (b'+' | b'-')) => {
            let sign = if *sign == b'-' { -1i64 } else { 1 };
            if b.get(i + 3) != Some(&b':') {
                return None;
            }
            let oh = num(i + 1..i + 3)?;
            let om = num(i + 4..i + 6)?;
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    Some(days * 86400 + hour * 3600 + min * 60 + sec - offset)
}

/// 儒略日算法（Howard Hinnant days_from_civil）：公历 Y-M-D → 距 1970-01-01 的天数。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]，3 月为岁首
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

// ========================================================================
// 聚合
// ========================================================================

/// 按 (错误类型, 终止程序) 折叠：回答「什么在反复失败」而非「失败过什么」。
/// 按条数降序；同数按最新时间降序（正在发生的排在已停止的前面）。
pub fn group_dumps(entries: &[DumpEntry]) -> Vec<DumpGroup> {
    struct Acc {
        error_type: String,
        program: String,
        count: usize,
        first: (i64, String),
        last: (i64, String),
        latest: (i64, String, String), // (secs, key, message)
        users: BTreeSet<String>,
    }

    let mut accs: Vec<Acc> = Vec::new();
    let mut index: BTreeMap<(String, String), usize> = BTreeMap::new();

    for e in entries {
        let key = (
            e.error_type.clone().unwrap_or_default(),
            e.program.clone().unwrap_or_default(),
        );
        let secs = e.at_secs.unwrap_or(i64::MIN);
        let idx = match index.get(&key) {
            Some(&i) => i,
            None => {
                accs.push(Acc {
                    error_type: key.0.clone(),
                    program: key.1.clone(),
                    count: 0,
                    first: (i64::MAX, String::new()),
                    last: (i64::MIN, String::new()),
                    latest: (i64::MIN, String::new(), String::new()),
                    users: BTreeSet::new(),
                });
                index.insert(key, accs.len() - 1);
                accs.len() - 1
            }
        };
        let a = &mut accs[idx];
        a.count += 1;
        // 无有效时间的条目不参与 first/last/latest 计算
        if e.at_secs.is_some() {
            if secs < a.first.0 {
                a.first = (secs, e.at.clone());
            }
            if secs > a.last.0 {
                a.last = (secs, e.at.clone());
            }
            if secs >= a.latest.0 {
                a.latest = (secs, e.key.clone(), e.message.clone().unwrap_or_default());
            }
        }
        if let Some(u) = &e.user {
            a.users.insert(u.clone());
        }
    }

    let mut groups: Vec<DumpGroup> = accs
        .into_iter()
        .map(|a| DumpGroup {
            error_type: a.error_type,
            program: a.program,
            count: a.count,
            first: a.first.1,
            last: a.last.1,
            users: a.users.into_iter().collect(),
            latest_key: a.latest.1,
            latest_message: a.latest.2,
            last_secs: a.last.0,
        })
        .collect();
    groups.sort_by(|x, y| {
        y.count
            .cmp(&x.count)
            .then(y.last_secs.cmp(&x.last_secs))
            .then_with(|| x.error_type.cmp(&y.error_type))
            .then_with(|| x.program.cmp(&y.program))
    });
    groups
}

// ========================================================================
// /formatted 文本解析 → DumpDetail
// ========================================================================

const STACK_CHAPTER: &str = "Active Calls/Events";
const TERMINATION_CHAPTER: &str = "Information on where terminated";

/// 解析 `/formatted` 英文文本。找不到的章节/字段返回空值而非报错——
/// 本地化（非英文 logon）系统返回空字段比返回错值更安全。
pub fn parse_formatted(text: &str) -> DumpDetail {
    let lines: Vec<&str> = text.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    let header = parse_dump_header(&lines);
    let (include, line) = parse_termination(&lines);
    let (procedure, main_program) = parse_termination_proc(&lines);
    DumpDetail {
        key: String::new(),
        error_type: header.get("Runtime Errors").cloned().unwrap_or_default(),
        exception: header.get("Except.").cloned().unwrap_or_default(),
        program: header.get("ABAP: Program").cloned().unwrap_or_default(),
        component: normalize_component(header.get("Application Component").map(String::as_str)),
        include,
        line,
        procedure,
        main_program,
        stack: parse_stack(&lines),
        header,
    }
}

/// 头表：第一个竖线围栏行之前的「标签    值」行。
/// 哪些行存在取决于转储类别（资源瓶颈类既无程序也无组件），按标签读、
/// 允许缺行，而不是假定固定行集。
fn parse_dump_header(lines: &[&str]) -> BTreeMap<String, String> {
    let mut header = BTreeMap::new();
    for line in lines {
        let trimmed = line.trim_end_matches(' ');
        let row = trimmed.trim_start();
        // 竖线围栏行 = 章节开始，头表结束
        if row.starts_with('|') {
            break;
        }
        // 全横线分隔行 / 空行跳过
        if row.is_empty() || row.trim_matches('-').trim().is_empty() {
            continue;
        }
        // 行首空格 = 续行（被换行截断的长值），不作为标签行
        if trimmed.starts_with(' ') {
            continue;
        }
        if let Some((label, value)) = split_header_row(trimmed) {
            header.insert(label, value);
        }
    }
    header
}

/// 标签与值之间以 ≥2 个空格分列（2 个即可与 "Application Component"
/// 内部的单空格区分）。
fn split_header_row(row: &str) -> Option<(String, String)> {
    let bytes = row.as_bytes();
    let mut gap = None;
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b' ' && bytes[i + 1] == b' ' {
            gap = Some(i);
            break;
        }
    }
    let gap = gap?;
    let label = row[..gap].trim();
    let value = row[gap..].trim();
    if label.is_empty() || value.is_empty() {
        return None;
    }
    Some((label.to_string(), value.to_string()))
}

/// "Not assigned" 归一化为空（自定义代码通常未分配组件，
/// 当值会让所有未分配对象看起来是同一个社区）。
fn normalize_component(raw: Option<&str>) -> String {
    let v = raw.unwrap_or_default().trim();
    if v.is_empty() || v.eq_ignore_ascii_case("Not assigned") {
        String::new()
    } else {
        v.to_string()
    }
}

/// 找章节：第一个含标题的竖线围栏行。
fn find_chapter_row(lines: &[&str], title: &str) -> Option<usize> {
    lines
        .iter()
        .position(|l| l.trim_start().starts_with('|') && l.contains(title))
}

/// 章节正文拼成单行（SAP 会在定宽处断行、甚至句中断行，
/// 行级匹配会在 include 名很长的转储上恰好失配，所以先拼接）。
fn join_chapter(lines: &[&str], title: &str) -> String {
    let Some(start) = find_chapter_row(lines, title) else {
        return String::new();
    };
    let mut body: Vec<String> = Vec::new();
    for line in lines.iter().skip(start + 1) {
        let row = line.trim();
        if !row.starts_with('|') {
            break; // 无围栏 = 章节外
        }
        body.push(row.trim_matches('|').trim().to_string());
    }
    body.join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 终止点：`termination point is in line N of include "X"`。
/// 行 0 + 空 include（死在 SYSTEM-EXIT 等）不记录——那是「无源码位置」，
/// 记下来会让无关转储匹配到不存在的位置上。
fn parse_termination(lines: &[&str]) -> (String, u32) {
    let text = join_chapter(lines, TERMINATION_CHAPTER);
    let needle = "termination point is in line ";
    let Some(i) = text.find(needle) else {
        return (String::new(), 0);
    };
    let rest = &text[i + needle.len()..];
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let Ok(line) = digits.parse::<u32>() else {
        return (String::new(), 0);
    };
    let after = rest[digits.len()..].trim_start();
    let Some(quoted) = after.strip_prefix("of include \"") else {
        return (String::new(), 0);
    };
    let Some(end) = quoted.find('"') else {
        return (String::new(), 0);
    };
    let include = quoted[..end].trim();
    if line == 0 || include.is_empty() {
        return (String::new(), 0);
    }
    (include.to_string(), line)
}

/// 过程与主程序：`occurred inABAP program or include "X", in "Y"` /
/// `main program was "Z"`。断行恰好落在 in 与 ABAP 之间时 SAP 会吞掉空格，
/// 因此空格可选。
fn parse_termination_proc(lines: &[&str]) -> (String, String) {
    let text = join_chapter(lines, TERMINATION_CHAPTER);
    let mut procedure = String::new();
    let mut main_program = String::new();

    if let Some(i) = text.find("occurred in") {
        let mut rest = &text[i + "occurred in".len()..];
        rest = rest.trim_start();
        if let Some(quoted) = rest.strip_prefix("ABAP program or include \"") {
            if let Some(end) = quoted.find('"') {
                let after = &quoted[end + 1..];
                // ", in "Y""
                if let Some(tail) = after.strip_prefix(", in \"") {
                    if let Some(end2) = tail.find('"') {
                        procedure = tail[..end2].trim().to_string();
                    }
                }
            }
        }
    }
    if let Some(i) = text.find("main program was \"") {
        let tail = &text[i + "main program was \"".len()..];
        if let Some(end) = tail.find('"') {
            main_program = tail[..end].trim().to_string();
        }
    }
    (procedure, main_program)
}

/// 调用栈：Active Calls/Events 章节里竖线围栏的帧行 + 下一行的帧名。
/// 列位置在 release 间漂移（行号列不固定），按「末字段=行号、倒数第二=
/// include、倒数第三=程序、帧号与程序之间=类型」从两端向内解析；
/// 类型可能是两个词（"MODULE (PBO)"）。
fn parse_stack(lines: &[&str]) -> Vec<DumpFrame> {
    let Some(start) = find_chapter_row(lines, STACK_CHAPTER) else {
        return Vec::new();
    };
    let mut frames = Vec::new();
    let mut i = start + 1;
    while i < lines.len() {
        let row = lines[i].trim();
        if !row.starts_with('|') {
            i += 1;
            continue;
        }
        // 章节在下一个章节（Selected Variables）处结束
        if !frames.is_empty() && row.contains("Selected Variables") {
            break;
        }
        if let Some(mut frame) = parse_frame_row(row) {
            // 帧名在紧随的下一行，单独一个词
            if let Some(name) = lines.get(i + 1).and_then(|row| parse_frame_name(row)) {
                frame.name = name;
                i += 1;
            }
            frames.push(frame);
        }
        i += 1;
    }
    frames
}

fn parse_frame_row(row: &str) -> Option<DumpFrame> {
    let fields: Vec<&str> = row
        .trim()
        .trim_matches('|')
        .split_whitespace()
        .collect();
    // 帧号 + 类型 + 程序 + include + 行号，至少 5 个字段
    if fields.len() < 5 {
        return None;
    }
    let position: u32 = fields[0].parse().ok()?;
    let line: u32 = fields[fields.len() - 1].parse().ok()?;
    let include = fields[fields.len() - 2];
    let program = fields[fields.len() - 3];
    Some(DumpFrame {
        position,
        type_name: fields[1..fields.len() - 3].join(" "),
        program: program.to_string(),
        include: if include == "???" {
            String::new()
        } else {
            include.to_string()
        },
        line,
        name: String::new(),
    })
}

/// 帧名行：竖线围栏内恰好一个词；一排横线不是名字。
fn parse_frame_name(row: &str) -> Option<String> {
    let trimmed = row.trim();
    if !trimmed.starts_with('|') {
        return None;
    }
    let fields: Vec<&str> = trimmed.trim_matches('|').split_whitespace().collect();
    if fields.len() != 1 {
        return None;
    }
    let name = fields[0];
    if name.trim_matches('-').is_empty() {
        return None;
    }
    Some(name.to_string())
}

// ========================================================================
// 网络读取（走 ADT 内部通道）
// ========================================================================

/// 读转储 feed（ADT 原样返回， newest-first）。`from`/`to` 为
/// `yyyyMMddHHmmss`（UTC）时透传给 ADT，让其在服务端翻页，比事后过滤便宜。
pub async fn fetch_feed(from: Option<&str>, to: Option<&str>) -> Result<Vec<DumpEntry>, RfcError> {
    let mut rel = String::from("runtime/dumps");
    let mut sep = '?';
    for (k, v) in [("from", from), ("to", to)] {
        if let Some(v) = v.filter(|s| !s.is_empty()) {
            rel.push(sep);
            rel.push_str(k);
            rel.push('=');
            rel.push_str(v);
            sep = '&';
        }
    }
    let (status, _ct, body) = crate::adt::adt_get_raw(&rel, "*/*").await?;
    if status != 200 {
        return Err(RfcError {
            code: -1,
            status: 502,
            message: format!("短转储 feed 请求失败（ADT 返回 {}）", status),
            key: "DUMP_FEED_UNAVAILABLE".into(),
        });
    }
    let xml = String::from_utf8_lossy(&body);
    parse_feed(&xml)
}

/// 读单个转储的结构化详情（内部拉 `/formatted` 英文文本并解析）。
///
/// key 取自列表端点返回的 `key` 字段（feed entry 的 rel=self 链接派生）。
/// ADT 404 时区分两种情况返回同一个语义：dump 不存在，或当前 release
/// 只有 feed 没有详情资源（如 7.50）。
pub async fn fetch_detail(key: &str) -> Result<DumpDetail, RfcError> {
    let key = key.trim();
    if key.is_empty() || key.contains("..") {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "dump key 不能为空且不能包含 .. 段".into(),
            key: "DUMP_KEY_INVALID".into(),
        });
    }
    // 强制英文渲染：解析按英文标签匹配，非英文返回空字段而非错值
    let rel = format!(
        "runtime/dump/{}/formatted?sap-language=EN",
        encode_path_segment(key)
    );
    let (status, _ct, body) = crate::adt::adt_get_raw(&rel, "*/*").await?;
    match status {
        200 => {
            let text = String::from_utf8_lossy(&body);
            let mut detail = parse_formatted(&text);
            detail.key = key.to_string();
            Ok(detail)
        }
        404 => Err(RfcError {
            code: -1,
            status: 404,
            message: "dump 详情不可得（不存在，或当前 release 不提供详情资源）".into(),
            key: "DUMP_DETAIL_UNAVAILABLE".into(),
        }),
        s => Err(RfcError {
            code: -1,
            status: 502,
            message: format!("dump 详情请求失败（ADT 返回 {}）", s),
            key: "DUMP_DETAIL_UNAVAILABLE".into(),
        }),
    }
}

// ========================================================================
// 测试
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 feed 的形状：错误类型与终止程序是 category，用户是 author，
    /// self 链接指向 /vit/ 查看器（详情资源要去掉该段）。
    const FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title type="text">Runtime Errors</title>
  <updated>2026-08-24T10:00:00.000Z</updated>
  <author><name>SYSTEM</name></author>
  <entry>
    <id>/sap/bc/adt/vit/runtime/dumps/20260824012009%20a4h%20DEVELOPER_001</id>
    <title type="text">Time limit exceeded.</title>
    <published>2026-08-24T01:20:09.000Z</published>
    <updated>2026-08-24T01:20:09.000Z</updated>
    <author><name>developer</name></author>
    <category term="TIME_OUT" label="ABAP runtime error"/>
    <category term="SAPLTHFB" label="Terminated ABAP program"/>
    <link rel="self" href="/sap/bc/adt/vit/runtime/dumps/20260824012009%20a4h%20DEVELOPER_001"/>
    <link rel="alternate" href="/sap/bc/adt/runtime/dumps?foo=1"/>
  </entry>
  <entry>
    <id>/sap/bc/adt/vit/runtime/dumps/20260823081500%20a4h%20DEVELOPER_002</id>
    <title type="text">Time limit exceeded.</title>
    <published>2026-08-23T08:15:00.000Z</published>
    <updated>2026-08-23T08:15:00.000Z</updated>
    <author><name>developer</name></author>
    <category term="TIME_OUT" label="ABAP runtime error"/>
    <category term="SAPLTHFB" label="Terminated ABAP program"/>
  </entry>
  <entry>
    <id>/sap/bc/adt/vit/runtime/dumps/20260823101010%20a4h%20TRAINING_003</id>
    <title type="text">An exception &amp; occurred.</title>
    <published>2026-08-23T10:10:10.000Z</published>
    <updated>2026-08-23T10:10:10.000Z</updated>
    <author><name>training</name></author>
    <category term="RAISE_EXCEPTION" label="ABAP runtime error"/>
    <category term="ZCL_ORDER_POST=========CP" label="Terminated ABAP program"/>
  </entry>
</feed>"#;

    #[test]
    fn parse_feed_extracts_structured_fields() {
        let entries = parse_feed(FEED).unwrap();
        assert_eq!(entries.len(), 3);
        let e = &entries[0];
        assert_eq!(e.error_type.as_deref(), Some("TIME_OUT"));
        assert_eq!(e.program.as_deref(), Some("SAPLTHFB"));
        // 用户统一大写
        assert_eq!(e.user.as_deref(), Some("DEVELOPER"));
        assert_eq!(e.at, "2026-08-24T01:20:09.000Z");
        // key 从 self 链接派生，去掉 /vit/ 段
        assert_eq!(e.key, "20260824012009%20a4h%20DEVELOPER_001");
        assert!(e.at_secs.is_some());
    }

    #[test]
    fn parse_feed_decodes_entities_and_falls_back_to_id() {
        let entries = parse_feed(FEED).unwrap();
        let e = &entries[2];
        // &amp; 实体解码
        assert_eq!(e.message.as_deref(), Some("An exception & occurred."));
        // 无 self 链接的条目回退用 id 派生 key
        assert_eq!(e.key, "20260823101010%20a4h%20TRAINING_003");
        // feed 级 author（SYSTEM）不会串到 entry
        assert_eq!(e.user.as_deref(), Some("TRAINING"));
    }

    #[test]
    fn parse_feed_rejects_non_atom_document() {
        assert!(parse_feed("<html><body>401</body></html>").is_err());
        assert!(parse_feed("").is_err());
    }

    #[test]
    fn group_dumps_counts_and_orders() {
        let entries = parse_feed(FEED).unwrap();
        let groups = group_dumps(&entries);
        assert_eq!(groups.len(), 2);
        // 条数降序：TIME_OUT + SAPLTHFB（2 条）在前
        assert_eq!(groups[0].error_type, "TIME_OUT");
        assert_eq!(groups[0].program, "SAPLTHFB");
        assert_eq!(groups[0].count, 2);
        assert_eq!(groups[0].users, vec!["DEVELOPER"]);
        // first/last 取组内极值时间
        assert_eq!(groups[0].first, "2026-08-23T08:15:00.000Z");
        assert_eq!(groups[0].last, "2026-08-24T01:20:09.000Z");
        // latest 指向最新一条
        assert_eq!(groups[0].latest_key, "20260824012009%20a4h%20DEVELOPER_001");
        assert_eq!(groups[0].latest_message, "Time limit exceeded.");
        // 第二组
        assert_eq!(groups[1].error_type, "RAISE_EXCEPTION");
        assert_eq!(groups[1].program, "ZCL_ORDER_POST=========CP");
    }

    /// /formatted 英文文本的形状：头表（标签+≥2 空格+值）、竖线围栏章节、
    /// 帧行两行一组（字段行 + 名字行）。
    const FORMATTED: &str = "\r
Runtime Errors         RAISE_EXCEPTION\r
Date                   24.08.2026 01:20:09\r
Short text             An exception occurred that was not caught.\r
User                   DEVELOPER\r
Host                   a4h\r
Application Component  Not assigned\r
ABAP: Program          ZCL_ORDER_POST=========CP\r
Except.                CX_DYNAMIC_CHECK\r
\r
--------------------------------------------------------------------------------\r
|Information on where terminated                                                  |\r
|The termination point is in line 54 of include                                  |\r
|\"ZCL_ORDER_POST=========CP\".                                                    |\r
|The termination occurred in ABAP program or include \"SAPMSSY1\", in            |\r
|\"ZCL_ORDER_POST=>POST\".                                                         |\r
|The main program was \"SAPMSSY1\".                                                |\r
--------------------------------------------------------------------------------\r
|Active Calls/Events                                                              |\r
--------------------------------------------------------------------------------\r
|No   Ty.          Program                        Include                        |\r
|2    FUNCTION     SAPMSSY1                        SAPMSSY1            36        |\r
|REMOTE_FUNCTION_CALL                                                             |\r
|1    METHOD       ZCL_ORDER_POST=========CP     ZCL_ORDER_POST=====CP 54        |\r
|ZCL_ORDER_POST=>POST                                                             |\r
--------------------------------------------------------------------------------\r
|Selected Variables                                                               |\r
--------------------------------------------------------------------------------\r
|No. 8 Ty. METHOD Name LV_COUNT                                                   |\r
";

    #[test]
    fn parse_formatted_extracts_header_termination_stack() {
        let d = parse_formatted(FORMATTED);
        assert_eq!(d.error_type, "RAISE_EXCEPTION");
        assert_eq!(d.exception, "CX_DYNAMIC_CHECK");
        assert_eq!(d.program, "ZCL_ORDER_POST=========CP");
        // Not assigned → 空
        assert_eq!(d.component, "");
        // 头表全量在 header 里
        assert_eq!(d.header.get("User").map(String::as_str), Some("DEVELOPER"));
        assert_eq!(d.header.get("Host").map(String::as_str), Some("a4h"));
        // 终止点（include 名很长被 SAP 断行，拼接后匹配）
        assert_eq!(d.include, "ZCL_ORDER_POST=========CP");
        assert_eq!(d.line, 54);
        assert_eq!(d.procedure, "ZCL_ORDER_POST=>POST");
        assert_eq!(d.main_program, "SAPMSSY1");
        // 调用栈：最内层在前，帧名取自下一行
        assert_eq!(d.stack.len(), 2);
        assert_eq!(d.stack[0].position, 2);
        assert_eq!(d.stack[0].type_name, "FUNCTION");
        assert_eq!(d.stack[0].program, "SAPMSSY1");
        assert_eq!(d.stack[0].include, "SAPMSSY1");
        assert_eq!(d.stack[0].line, 36);
        assert_eq!(d.stack[0].name, "REMOTE_FUNCTION_CALL");
        assert_eq!(d.stack[1].name, "ZCL_ORDER_POST=>POST");
    }

    #[test]
    fn parse_formatted_empty_on_localized_or_missing_chapters() {
        // 德文/缺章节：返回空字段而非错值
        let d = parse_formatted("Laufzeitfehler         TIME_OUT\n\n|Aktive Aufrufe/Ereignisse|\n");
        assert_eq!(d.error_type, "");
        assert!(d.stack.is_empty());
        assert_eq!(d.line, 0);
    }

    #[test]
    fn parse_termination_skips_zero_line() {
        // 死在 SYSTEM-EXIT：line 0 + 空 include → 不记录
        let text = "|Information on where terminated|\n|The termination point is in line 0 of include \" \".|\n";
        let lines: Vec<&str> = text.split('\n').collect();
        let (include, line) = parse_termination(&lines);
        assert_eq!(include, "");
        assert_eq!(line, 0);
    }

    #[test]
    fn parse_frame_row_reads_from_ends_inward() {
        // 类型含两词（MODULE (PBO)）也不串位
        let f = parse_frame_row("|3    MODULE (PBO)  SAPLZFG   LZFGU01   120|").unwrap();
        assert_eq!(f.position, 3);
        assert_eq!(f.type_name, "MODULE (PBO)");
        assert_eq!(f.program, "SAPLZFG");
        assert_eq!(f.include, "LZFGU01");
        assert_eq!(f.line, 120);
        // ??? include → 空
        let f = parse_frame_row("|4    FORM  PROG1   ???   7|").unwrap();
        assert_eq!(f.include, "");
        // 表头行/名字行不是帧行
        assert!(parse_frame_row("|No   Ty.          Program                        Include|").is_none());
        assert!(parse_frame_row("|REMOTE_FUNCTION_CALL|").is_none());
        // 横线不是帧名
        assert!(parse_frame_name("|------------------|").is_none());
        assert_eq!(
            parse_frame_name("|CL_X=>METHOD|").as_deref(),
            Some("CL_X=>METHOD")
        );
    }

    #[test]
    fn key_derivation_and_encoding() {
        assert_eq!(
            dump_key_from("/sap/bc/adt/vit/runtime/dumps/20260824012009%20a4h?x=1#f"),
            "20260824012009%20a4h"
        );
        assert_eq!(dump_key_from("20260824012009 a4h"), "20260824012009 a4h");
        assert_eq!(encode_path_segment("2026 ab/cd"), "2026%20ab%2Fcd");
        assert_eq!(encode_path_segment("ABC-1_x.y~z"), "ABC-1_x.y~z");
    }

    #[test]
    fn rfc3339_to_secs_handles_offsets_and_fraction() {
        let a = rfc3339_to_secs("2026-08-24T01:20:09.000Z").unwrap();
        let b = rfc3339_to_secs("2026-08-24T03:20:09.500+02:00").unwrap();
        assert_eq!(a, b); // 同一时刻，不同表示
        assert!(rfc3339_to_secs("not a date").is_none());
        assert!(rfc3339_to_secs("2026-08-24T01:20:09").is_none()); // 缺时区
    }
}
