//! 包级导出 + 对象元数据读取（abapGit 吸收 MVP）。
//!
//! **导出**：`GET /api/export/package/{devclass}` 把一个包（含子包）序列化为
//! abapGit 风格文件树 zip——文件命名/目录约定沿用 abapGit（`{名}.{类型}.{扩展}`、
//! FULL 目录逻辑、命名空间 `#ns#`），agent 拿到即可 `git commit`、做 SAP↔本地
//! diff、做备份。元数据 XML（TPOOL/SOTR/VSEOCLASS 等）本版不生成——产出对
//! abapGit「源码兼容」但「不可导入」（导入互操作与 `POST /api/import` 留二期），
//! 因此 zip 根除 abapGit 识别用的 `.abapgit.xml` 外，另带自有
//! `export_manifest.json`（对象清单：作者/状态/跳过原因——缺口可见而非静默丢弃）。
//!
//! **元数据动词**：`GET /api/objects/{type}/{name}/meta` 的 RFC 侧编排也住这里
//! （TADIR author 兜底 + REPOSRC changed_by / 未激活版本判据），ADT 侧探测件
//! （存在性 / 锁探测）在 objects.rs。改动 abapGit 同名概念时参考其文档：
//! folder logic = zcl_abapgit_folder_logic，文件名 = zcl_abapgit_filename_logic。
//!
//! 关键目录事实（abapGit/AGENTS.md 双实证）：
//! - TADIR 无 FUNC 行——FM 成员按 ENLFDIR.AREA 枚举，不走目录查询；
//! - PROG 是大类：include 与程序同占 R3TR PROG，靠 REPOSRC.SUBC（I=include）分流；
//! - 结构与透明表同占 R3TR TABL，靠 DD02L.TABCLASS（INTAB/TRANSP）分流；
//! - TADIR.AUTHOR 只有创建者——「最后修改者」在 REPOSRC.UNAM（有 REPOSRC
//!   映射的类型才有）。
//!
//! 全部枚举查询共享一次 RFC 连接（run_blocking 闭包内串行），源码逐对象走
//! ADT 读取；单对象失败不整体失败（清单记 error 继续）。

use crate::error::RfcError;
use crate::objects::ObjectType;
use crate::systems::{Sap, SystemHandle};
use axum::response::IntoResponse;
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Arc;

// ========================================================================
// 路由
// ========================================================================

/// 导出子路由。合并进 server::app 的 /api 鉴权层内（与 invoke 同形态，
/// 状态固定为 SharedState；作用域形式 /api/s/{system}/export/package/**
/// 由 ScopeService 免费提供）。
pub fn router() -> axum::Router<crate::server::SharedState> {
    axum::Router::new().route(
        "/api/export/package/{*devclass}",
        axum::routing::get(export_package_handler),
    )
}

/// `GET /api/export/package/{devclass}` 查询参数。
#[derive(Deserialize, Default)]
struct ExportQuery {
    /// 递归子包（默认 true；TDEVC.PARENTCL BFS，深度上限 10）
    #[serde(default)]
    recurse: Option<bool>,
    /// 对象总数上限（含 FUGR 展开；默认 500，超出 400 EXPORT_TOO_LARGE）
    #[serde(default)]
    max_objects: Option<usize>,
    /// 只返回 JSON 清单（不读源码、不打 zip——agent 预览用）
    #[serde(default)]
    manifest: Option<bool>,
}

const DEFAULT_MAX_OBJECTS: usize = 500;
/// `max_objects` 参数的硬上限（防一次导出拖垮 ADT）。
const MAX_OBJECTS_HARD_CAP: usize = 2000;
/// BFS 子包深度上限（防 TDEVC 数据环）。
const MAX_PACKAGE_DEPTH: usize = 10;

async fn export_package_handler(
    sap: Sap,
    axum::extract::Path(devclass): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<ExportQuery>,
) -> Result<axum::response::Response, RfcError> {
    let root = validate_devclass(&devclass)?;
    let recurse = q.recurse.unwrap_or(true);
    let max_objects = q
        .max_objects
        .unwrap_or(DEFAULT_MAX_OBJECTS)
        .min(MAX_OBJECTS_HARD_CAP);
    let sys = sap.arc();
    // 枚举：TADIR/TDEVC/ENLFDIR/REPOSRC/DD02L 全部查询共享一次连接
    let root_c = root.clone();
    let inv = crate::server::run_blocking(Arc::clone(&sys), {
        let sys = Arc::clone(&sys);
        move |conn| collect_inventory(&sys.meta, conn, &root_c, recurse)
    })
    .await?;
    // 根包判定：TADIR 无任何行且 TDEVC 无记录 → 包不存在（存在的空包是合法导出）
    let absent = match inv.packages.first() {
        Some(p) => !p.tdevc_found && p.tadir_rows == 0,
        None => true,
    };
    if absent {
        return Err(RfcError {
            code: -1,
            status: 404,
            message: format!(
                "包 {} 不存在（TADIR 无 R3TR 行且 TDEVC 无记录）。注意 $TMP 与系统本地包\
                 在部分系统没有 TDEVC 行——导出请用真实开发包",
                root
            ),
            key: "PACKAGE_NOT_FOUND".into(),
        });
    }
    let total: usize = inv.packages.iter().map(|p| p.objects.len()).sum();
    if total > max_objects {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: format!(
                "包 {}（含子包）共 {} 个对象，超过上限 {}——可传 ?max_objects= 提高上限\
                 （≤2000）或拆子包导出",
                root, total, max_objects
            ),
            key: "EXPORT_TOO_LARGE".into(),
        });
    }
    let mut inv = inv;
    if q.manifest.unwrap_or(false) {
        return Ok(axum::Json(manifest_value(&sys, &root, &inv)).into_response());
    }
    // 逐对象读源（ADT 顺序循环；单对象失败记 error 继续）
    let adt = sys.adt.as_ref().ok_or_else(crate::adt::adt_disabled)?;
    fetch_sources(adt, &mut inv).await;
    // zip 组装（.abapgit.xml + export_manifest.json + 文件树）
    let files = collect_files(&inv);
    let bytes = build_zip(&sys, &root, &inv, &files)?;
    let disposition =
        axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{}.zip\"", root))
            .map_err(|_| RfcError {
                code: -1,
                status: 500,
                message: format!("包名 {} 不能作为下载文件名", root),
                key: "DEVCLASS_INVALID".into(),
            })?;
    let mut resp = axum::response::Response::new(axum::body::Body::from(bytes));
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        "application/zip".parse().unwrap(),
    );
    resp.headers_mut()
        .insert(axum::http::header::CONTENT_DISPOSITION, disposition);
    Ok(resp)
}

/// devclass 校验：大写化（保留命名空间前导 `/`，仅去尾部斜杠）；允许
/// [A-Z0-9_$/]（`$` = 本地包家族 $TMP/$…）；拒绝 `..`、控制字符。
/// pub(crate)：导入（import.rs）复用同一校验口径。
pub(crate) fn validate_devclass(raw: &str) -> Result<String, RfcError> {
    let d = raw.trim().trim_end_matches('/').to_uppercase();
    if d.is_empty() || d.len() > 40 {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: format!("包名非法（空或超过 40 字符）: {}", raw),
            key: "DEVCLASS_INVALID".into(),
        });
    }
    if d.contains("..") || d.chars().any(|c| c.is_control()) {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: format!("包名含非法字符: {}", raw),
            key: "DEVCLASS_INVALID".into(),
        });
    }
    let ok = d
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == '/' || c == '$');
    if !ok {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: format!("包名仅允许大写字母/数字/下划线/命名空间斜杠: {}", raw),
            key: "DEVCLASS_INVALID".into(),
        });
    }
    Ok(d)
}

// ========================================================================
// 枚举（同步，run_blocking 闭包内；全部查询共享一次连接）
// ========================================================================

/// 导出条目的分类结果（R3TR 目录类型 → 网关类型/文件形态）。
#[derive(Debug, Clone, PartialEq)]
enum ExportKind {
    Program,
    Include,
    Class,
    Interface,
    /// 函数模块（组名在 `ExportObject.group`）。当前枚举路径产出的是组内
    /// include 而非 FM 名（ENLFDIR 为表池表 RFC_READ_TABLE 拒读，FM↔include
    /// 映射经 RFC 不可得）——此变体暂不产生，保留给拿到映射的后续版本
    #[allow(dead_code)]
    Func,
    /// 函数组本身（无独立文件；清单记 status:"expanded"）
    Fugr,
    Cds,
    Table,
    Structure,
    /// 目录类型不在导出范围（SOTR/SMIM/...）；值 = 原始 R3TR 类型
    Skipped(String),
}

impl ExportKind {
    /// 文件命名 token 与扩展名（None = 无独立文件）。
    fn file_token(&self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Program => Some(("prog", "abap")),
            Self::Include => Some(("incl", "abap")),
            Self::Class => Some(("clas", "abap")),
            Self::Interface => Some(("intf", "abap")),
            Self::Func => Some(("func", "abap")),
            Self::Cds => Some(("cds", "ddl")),
            Self::Table => Some(("tabl", "ddl")),
            Self::Structure => Some(("stru", "ddl")),
            _ => None,
        }
    }

    /// 源码读取的网关类型（None = 无源码可读）。
    fn source_type(&self) -> Option<ObjectType> {
        match self {
            Self::Program => Some(ObjectType::Program),
            Self::Include => Some(ObjectType::Include),
            Self::Class => Some(ObjectType::Class),
            Self::Interface => Some(ObjectType::Interface),
            Self::Func => Some(ObjectType::Function),
            Self::Cds => Some(ObjectType::CdsView),
            Self::Table => Some(ObjectType::Table),
            Self::Structure => Some(ObjectType::Structure),
            _ => None,
        }
    }

    /// 清单 `type` 字段。
    fn manifest_type(&self) -> String {
        match self {
            Self::Program => "prog".into(),
            Self::Include => "incl".into(),
            Self::Class => "class".into(),
            Self::Interface => "intf".into(),
            Self::Func => "func".into(),
            Self::Fugr => "fugr".into(),
            Self::Cds => "cds".into(),
            Self::Table => "tabl".into(),
            Self::Structure => "stru".into(),
            Self::Skipped(o) => o.to_lowercase(),
        }
    }
}

/// 枚举出的单个对象（源码读取结果后填）。
#[derive(Debug, Clone)]
struct ExportObject {
    kind: ExportKind,
    /// 大写对象名（func = FM 名）
    name: String,
    /// func 的函数组；其他空
    group: String,
    /// 目录信息（清单展示用）
    author: String,
    created_on: String,
    /// 最后修改者（REPOSRC.UNAM；无映射的类型为空）
    changed_by: String,
    changed_on: String,
    /// 源码读取结果（manifest=true 时不读，保持 None）
    content: Option<String>,
    /// 读取失败原因（不整体失败——缺口可见）
    error: Option<String>,
}

/// 枚举出的一个包节点（含全部对象）。
#[derive(Debug, Clone, Default)]
struct PackageNode {
    devclass: String,
    /// TDEVC.CTEXT（无记录为空）
    ctext: String,
    /// 父包（根为空）
    parent: String,
    /// TADIR DEVC 行作者（无行为空）
    author: String,
    created_on: String,
    /// TDEVC 是否有此包记录（根包存在性判据）
    tdevc_found: bool,
    /// TADIR 行总数（含 DEVC；根包存在性判据）
    tadir_rows: usize,
    objects: Vec<ExportObject>,
}

/// 完整清单（根包在首位）。
#[derive(Debug, Clone, Default)]
struct Inventory {
    packages: Vec<PackageNode>,
}

/// BFS 收集根包 + 子包的全部 R3TR 对象并分类。
fn collect_inventory(
    meta: &crate::metadata::MetadataCache,
    conn: &crate::connection::RfcConnection,
    root: &str,
    recurse: bool,
) -> Result<Inventory, RfcError> {
    use crate::discovery as d;
    let mut out = Inventory::default();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back((root.to_string(), String::new()));
    let mut seen: HashSet<String> = HashSet::new();
    let mut depth = 0usize;
    while let Some((pkg, parent)) = queue.pop_front() {
        depth += 1;
        if depth > MAX_PACKAGE_DEPTH {
            tracing::warn!(package = %pkg, "子包深度超过上限，剩余子包跳过");
            continue;
        }
        if !seen.insert(pkg.clone()) {
            continue;
        }
        let tadir = d::read_tadir_rows(meta, conn, &pkg)?;
        let devc_self = d::read_tdevc_row(meta, conn, &pkg)?;
        let mut node = PackageNode {
            devclass: pkg.clone(),
            parent: parent.clone(),
            ctext: devc_self
                .as_ref()
                .map(|c| c.ctext.clone())
                .unwrap_or_default(),
            author: String::new(),
            created_on: String::new(),
            tdevc_found: devc_self.is_some(),
            tadir_rows: tadir.len(),
            objects: Vec::new(),
        };
        // 包作者来自 TADIR 的 DEVC 行（本地包可能没有）
        if let Some(t) = tadir.iter().find(|t| t.object == "DEVC") {
            node.author = t.author.clone();
            node.created_on = t.created_on.clone();
        }
        for t in tadir.iter().filter(|t| t.object != "DEVC") {
            let base = |kind: ExportKind| ExportObject {
                kind,
                name: t.obj_name.clone(),
                group: String::new(),
                author: t.author.clone(),
                created_on: t.created_on.clone(),
                changed_by: String::new(),
                changed_on: String::new(),
                content: None,
                error: None,
            };
            match t.object.as_str() {
                // PROG 大类：include 与程序同占 R3TR PROG，SUBC='I' 分流
                "PROG" => {
                    let info = d::read_reposrc_active(meta, conn, &t.obj_name)?;
                    let kind = match &info {
                        Some(r) if r.subc == "I" => ExportKind::Include,
                        _ => ExportKind::Program,
                    };
                    let mut o = base(kind);
                    if let Some(r) = info {
                        o.changed_by = r.unam;
                        o.changed_on = r.udat;
                    }
                    node.objects.push(o);
                }
                "CLAS" => node.objects.push(base(ExportKind::Class)),
                "INTF" => node.objects.push(base(ExportKind::Interface)),
                "DDLS" => node.objects.push(base(ExportKind::Cds)),
                // TABL 大类：TRANSP=透明表 / INTAB=结构（缺 DD02L 行按透明表兜底）
                "TABL" => {
                    let kind = match d::read_dd02l_tabclass(meta, conn, &t.obj_name)?.as_deref() {
                        Some("INTAB") => ExportKind::Structure,
                        _ => ExportKind::Table,
                    };
                    node.objects.push(base(kind));
                }
                // FUGR：组本身 + 环境服务展开全部组内 include（TOP/UXX/U00-99/自定义）。
                // ENLFDIR/TFDIR 是表池表 RFC_READ_TABLE 拒读（AD718）——FM 名映射
                // 拿不到，成员文件按 include 名导出（见 discovery::read_group_includes）
                "FUGR" => {
                    let group = t.obj_name.clone();
                    node.objects.push(base(ExportKind::Fugr));
                    for inc in d::read_group_includes(meta, conn, &group)? {
                        let (cb, co) = match d::read_reposrc_active(meta, conn, &inc)? {
                            Some(r) => (r.unam, r.udat),
                            None => (String::new(), String::new()),
                        };
                        node.objects.push(ExportObject {
                            kind: ExportKind::Include,
                            name: inc,
                            group: group.clone(),
                            author: t.author.clone(),
                            created_on: t.created_on.clone(),
                            changed_by: cb,
                            changed_on: co,
                            content: None,
                            error: None,
                        });
                    }
                }
                other => {
                    // 超范围目录类型（SOTR/SMIM/...）记录为 skipped——缺口可见
                    let mut o = base(ExportKind::Skipped(other.to_string()));
                    o.error = Some("类型不在导出范围（本版支持的 R3TR 类型：PROG/CLAS/INTF/FUGR/DDLS/TABL/DEVC）".into());
                    node.objects.push(o);
                }
            }
        }
        out.packages.push(node);
        if recurse {
            for child in d::read_tdevc_children(meta, conn, &pkg)? {
                if !child.devclass.trim().is_empty() {
                    queue.push_back((child.devclass, pkg.clone()));
                }
            }
        }
    }
    Ok(out)
}

/// 逐对象读源码（ADT；失败记 error 不中断）。
async fn fetch_sources(adt: &crate::adt::AdtEndpoint, inv: &mut Inventory) {
    for pkg in &mut inv.packages {
        for obj in &mut pkg.objects {
            let Some(t) = obj.kind.source_type() else {
                continue;
            };
            match crate::objects::read_current_source(adt, t, &obj.name, &obj.group).await {
                Ok(src) => obj.content = Some(src),
                Err(e) => obj.error = Some(e.message),
            }
        }
    }
}

// ========================================================================
// 文件命名（FULL 目录逻辑 + abapGit 约定；纯函数，可单测）
// ========================================================================

/// 命名空间名 → 文件名标记：`/NS/REST` → `#ns#rest`，普通名 → 小写。
/// 包目录（FULL 逻辑）与对象名共用——abapGit filename_logic 同款映射。
fn ns_token(name: &str) -> String {
    let n = name.trim().trim_start_matches('/').to_uppercase();
    match n.split_once('/') {
        Some((ns, tail)) if !ns.is_empty() => {
            format!("#{}#{}", ns.to_lowercase(), tail.to_lowercase())
        }
        // 异常形态（如 "/XYZ" 无尾段）：小写兜底
        _ => n.to_lowercase(),
    }
}

/// 包 → 文件夹（FULL 逻辑：文件夹 = 小写完整包名）。
fn package_folder(devclass: &str) -> String {
    ns_token(devclass)
}

/// 文件名主体转义：`%` → `%25`（`#` 是命名空间标记保留——ABAP 命名空间名
/// 不含 %/#，其余特殊字符对 Z 命名不会出现）。
fn escape_name_token(n: &str) -> String {
    n.replace('%', "%25")
}

/// 对象 → zip 相对路径 `src/{包目录}/{小写名}.{类型}.{扩展}`。
/// 无独立文件（Fugr/Package/Skipped）→ None。
fn object_rel_path(kind: &ExportKind, name: &str, devclass: &str) -> Option<String> {
    let (token, ext) = kind.file_token()?;
    let folder = package_folder(devclass);
    Some(format!(
        "src/{}/{}.{}.{}",
        folder,
        escape_name_token(&ns_token(name)),
        token,
        ext
    ))
}

// ========================================================================
// zip 组装
// ========================================================================

/// 已就绪的文件（路径 + 文本内容）。
struct ReadyFile {
    path: String,
    content: String,
}

/// 收集全部要写入 zip 的文件（含 .abapgit.xml 之外的树内容；manifest 在
/// build_zip 内生成——它需要最终状态）。空文件省略（abapGit 纪律）。
fn collect_files(inv: &Inventory) -> Vec<ReadyFile> {
    let mut files = Vec::new();
    for pkg in &inv.packages {
        let folder = package_folder(&pkg.devclass);
        // package.devc.xml（abapGit DEVC 最小形状：DEVCLASS/CTEXT/PARENTCL）
        files.push(ReadyFile {
            path: format!("src/{}/package.devc.xml", folder),
            content: devc_xml(&pkg.devclass, &pkg.ctext, &pkg.parent),
        });
        for obj in &pkg.objects {
            let Some(path) = object_rel_path(&obj.kind, &obj.name, &pkg.devclass) else {
                continue;
            };
            let Some(content) = obj.content.as_deref() else {
                continue; // 读取失败/未读取：不写文件，清单里可见
            };
            let normalized = normalize_text(content);
            if normalized.trim().is_empty() {
                continue; // 空文件省略
            }
            files.push(ReadyFile {
                path,
                content: normalized,
            });
        }
    }
    files
}

/// 文本归一（abapGit 文件纪律：LF 行尾 + 末尾换行；本版不写 BOM）。
fn normalize_text(s: &str) -> String {
    let s = s.replace("\r\n", "\n");
    if s.ends_with('\n') {
        s
    } else {
        format!("{}\n", s)
    }
}

/// package.devc.xml（abapGit DEVC 序列化器形状的子集）。
fn devc_xml(devclass: &str, ctext: &str, parent: &str) -> String {
    let xml_escape = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let parent_tag = if parent.trim().is_empty() {
        String::new()
    } else {
        format!("\n    <PARENTCL>{}</PARENTCL>", xml_escape(parent.trim()))
    };
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<abapGit version="v1.0.0" serializer="LCL_OBJECT_DEVC" serializer_version="v1.0.0">
 <asx:abap xmlns:asx="http://www.sap.com/abapxml" version="1.0">
  <asx:values>
   <V_PACKAGE>
    <DEVCLASS>{}</DEVCLASS>
    <CTEXT>{}</CTEXT>{}
   </V_PACKAGE>
  </asx:values>
 </asx:abap>
</abapGit>
"#,
        xml_escape(devclass),
        xml_escape(ctext.trim()),
        parent_tag
    )
}

/// abapGit 项目清单（仅识别用最小集；FOLDER_LOGIC=FULL 与本导出一致）。
fn abapgit_xml() -> String {
    r#"<?xml version="1.0" encoding="utf-8"?>
<asx:abap xmlns:asx="http://www.sap.com/abapxml" version="1.0">
 <asx:values>
  <DATA>
   <MASTER_LANGUAGE>E</MASTER_LANGUAGE>
   <STARTING_FOLDER>/src/</STARTING_FOLDER>
   <FOLDER_LOGIC>FULL</FOLDER_LOGIC>
  </DATA>
 </asx:values>
</asx:abap>
"#
    .to_string()
}

/// 导出清单 JSON（自有格式 sap-for-agents-export/1）。
fn manifest_value(sys: &Arc<SystemHandle>, root: &str, inv: &Inventory) -> serde_json::Value {
    let mut objects = Vec::new();
    for pkg in &inv.packages {
        for obj in &pkg.objects {
            let status = if obj.error.is_some() {
                "error"
            } else {
                match obj.kind {
                    ExportKind::Fugr => "expanded",
                    ExportKind::Skipped(_) => "skipped",
                    _ => "exported",
                }
            };
            let path = object_rel_path(&obj.kind, &obj.name, &pkg.devclass);
            let not_str = |s: &String| -> serde_json::Value {
                if s.trim().is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(s.trim())
                }
            };
            objects.push(serde_json::json!({
                "type": obj.kind.manifest_type(),
                "name": obj.name,
                "group": (!obj.group.is_empty()).then(|| obj.group.clone()),
                "package": pkg.devclass,
                "path": path,
                "status": status,
                "author": not_str(&obj.author),
                "created_on": not_str(&obj.created_on),
                "changed_by": not_str(&obj.changed_by),
                "changed_on": not_str(&obj.changed_on),
                "error": obj.error,
            }));
        }
    }
    let packages: Vec<serde_json::Value> = inv
        .packages
        .iter()
        .map(|p| {
            serde_json::json!({
                "devclass": p.devclass,
                "parent": (!p.parent.is_empty()).then(|| p.parent.clone()),
                "ctext": (!p.ctext.trim().is_empty()).then(|| p.ctext.trim()),
                "author": (!p.author.trim().is_empty()).then(|| p.author.trim()),
                "created_on": (!p.created_on.trim().is_empty()).then(|| p.created_on.trim()),
                "devc_xml": format!("src/{}/package.devc.xml", package_folder(&p.devclass)),
            })
        })
        .collect();
    serde_json::json!({
        "format": "sap-for-agents-export/1",
        "folder_logic": "full",
        "sysid": sys.sysid(),
        "package": root,
        "package_count": inv.packages.len(),
        "object_count": objects.len(),
        "packages": packages,
        "objects": objects,
        "note": "源码兼容 abapGit 命名/目录约定；元数据 XML 未生成（不可导入 abapGit，导入互操作见二期计划）",
    })
}

/// 组装 zip：.abapgit.xml + export_manifest.json + 文件树。
fn build_zip(
    sys: &Arc<SystemHandle>,
    root: &str,
    inv: &Inventory,
    files: &[ReadyFile],
) -> Result<Vec<u8>, RfcError> {
    use std::io::Write;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut put = |name: &str, content: &str| -> Result<(), RfcError> {
        let io_err = |e: std::io::Error| RfcError {
            code: -1,
            status: 500,
            message: format!("zip 写入失败（{}）: {}", name, e),
            key: "EXPORT_ZIP_FAILED".into(),
        };
        w.start_file(name, opts)
            .map_err(|e| io_err(std::io::Error::other(e.to_string())))?;
        w.write_all(content.as_bytes()).map_err(io_err)?;
        Ok(())
    };
    put(".abapgit.xml", &abapgit_xml())?;
    put(
        "export_manifest.json",
        &serde_json::to_string_pretty(&manifest_value(sys, root, inv)).unwrap_or_default(),
    )?;
    for f in files {
        put(&f.path, &f.content)?;
    }
    let cursor = w.finish().map_err(|e| RfcError {
        code: -1,
        status: 500,
        message: format!("zip 收尾失败: {}", e),
        key: "EXPORT_ZIP_FAILED".into(),
    })?;
    Ok(cursor.into_inner())
}

// ========================================================================
// 对象 meta 的 RFC 侧读取（GET /api/objects/{type}/{name}/meta 共用）
// ========================================================================

/// 对象 meta 的 RFC 侧读取结果（TADIR + REPOSRC 兜底通道）。
pub(crate) struct MetaRfcInfo {
    pub tadir_author: String,
    pub tadir_created_on: String,
    /// REPOSRC 侧的最后修改（prog/incl/fugr 有映射；ADT 属性缺失时兜底）
    pub reposrc_changed: Option<(String, String)>,
    /// REPOSRC 侧的未激活判据（R3STATE='I' 行存在 → 未激活；ADT version
    /// 属性缺失时兜底）
    pub reposrc_inactive: Option<bool>,
}

/// 读对象 meta 的 RFC 侧信息（TADIR + REPOSRC 兜底；一次连接内完成）。
pub(crate) fn read_meta_rfc(
    meta: &crate::metadata::MetadataCache,
    conn: &crate::connection::RfcConnection,
    obj_type: ObjectType,
    name: &str,
    group: &str,
) -> Result<MetaRfcInfo, RfcError> {
    use crate::discovery as d;
    let (tobj, tname) = crate::objects::tadir_key_of(obj_type, name, group);
    let tadir = d::read_tadir_one(meta, conn, tobj, &tname)?;
    let (tadir_author, tadir_created_on) =
        tadir.map(|t| (t.author, t.created_on)).unwrap_or_default();
    let reposrc_prog = crate::objects::reposrc_name_of(obj_type, name, group);
    let reposrc_changed = match &reposrc_prog {
        Some(p) => d::read_reposrc_active(meta, conn, p)?
            .filter(|r| !r.unam.trim().is_empty())
            .map(|r| (r.unam, r.udat)),
        None => None,
    };
    let reposrc_inactive = match (
        &reposrc_prog,
        crate::objects::active_check_supported(obj_type),
    ) {
        (Some(p), true) => Some(d::read_reposrc_inactive_exists(meta, conn, p)?),
        _ => None,
    };
    Ok(MetaRfcInfo {
        tadir_author,
        tadir_created_on,
        reposrc_changed,
        reposrc_inactive,
    })
}

/// `GET /api/objects/{type}/{name}/meta` 编排：ADT 元数据探测（一次 GET 拿
/// 存在性 + changedBy/changedAt + version 活跃态）+ 锁探测 + RFC 目录兜底，
/// 一次往返返回全部字段。func 组反解失败按"不存在"短路（而非报错）。
pub async fn read_object_meta(
    sys: Arc<SystemHandle>,
    obj_type: ObjectType,
    name: &str,
    group_hint: Option<String>,
) -> Result<serde_json::Value, RfcError> {
    // func 先反解组；反解失败 = FM 不存在（RFC 搜索找不到）
    let group = match crate::server::resolve_group_if_needed(&sys, obj_type, name, group_hint).await
    {
        Ok(g) => g,
        Err(e) if e.key == "FUNCTION_GROUP_NOT_FOUND" => return Ok(meta_absent(obj_type, name)),
        Err(e) => return Err(e),
    };
    let adt = sys.adt.as_ref().ok_or_else(crate::adt::adt_disabled)?;
    // 1) ADT 元数据探测（不存在短路——不做锁探测/表查询）
    let state = crate::objects::read_adt_object_state(adt, obj_type, name, &group).await?;
    if !state.exists {
        return Ok(meta_absent(obj_type, name));
    }
    // 2) 锁探测（blue DDIC/package 返回 None 锁语义）
    let probe = crate::objects::probe_lock_state(adt, obj_type, name, &group).await?;
    // 3) 目录信息 + REPOSRC 兜底（闭包要求 'static：name/group 克隆进阻塞任务）
    let name_c = name.to_string();
    let group_c = group.clone();
    let info = crate::server::run_blocking(Arc::clone(&sys), {
        let sys = Arc::clone(&sys);
        move |conn| read_meta_rfc(&sys.meta, conn, obj_type, &name_c, &group_c)
    })
    .await?;
    // 组合：ADT 属性为主，缺失时按类型走 REPOSRC 兜底
    let (changed_by, changed_by_via) = if !state.changed_by.trim().is_empty() {
        (state.changed_by.trim().to_string(), "adt")
    } else if let Some((u, _)) = &info.reposrc_changed {
        (u.clone(), "reposrc")
    } else {
        (String::new(), "none")
    };
    let (active, active_check) = if !state.version.trim().is_empty() {
        (Some(state.version.trim() == "active"), "adt")
    } else if let Some(inactive) = info.reposrc_inactive {
        (Some(!inactive), "reposrc")
    } else {
        (None, "unsupported_type")
    };
    let not_str = |s: &str| -> serde_json::Value {
        let t = s.trim();
        if t.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!(t)
        }
    };
    Ok(serde_json::json!({
        "type": obj_type.api_name(),
        "name": name.trim().to_uppercase(),
        "group": (obj_type.needs_group() && !group.is_empty()).then(|| group.to_uppercase()),
        "exists": true,
        "locked": probe.locked,
        "lock_mode": probe.mode,
        "lock_message": probe.message,
        "active": active,
        "active_check": active_check,
        "changed_by": not_str(&changed_by),
        "changed_on": not_str(&state.changed_at),
        "changed_by_via": changed_by_via,
        "author": not_str(&info.tadir_author),
        "created_on": not_str(&info.tadir_created_on),
        "author_via": if info.tadir_author.trim().is_empty() { "none" } else { "tadir" },
    }))
}

/// 不存在对象的 meta 形态（全部探测字段空语义）。
fn meta_absent(obj_type: ObjectType, name: &str) -> serde_json::Value {
    let lock_mode = if obj_type.is_blue_ddic() {
        "etag"
    } else if matches!(obj_type, ObjectType::Package) {
        "none"
    } else {
        "adt_lock"
    };
    serde_json::json!({
        "type": obj_type.api_name(),
        "name": name.trim().to_uppercase(),
        "group": null,
        "exists": false,
        "locked": null,
        "lock_mode": lock_mode,
        "lock_message": null,
        "active": null,
        "active_check": "not_checked",
        "changed_by": null,
        "changed_on": null,
        "changed_by_via": "none",
        "author": null,
        "created_on": null,
        "author_via": "none",
    })
}

// ========================================================================
// 测试（纯函数，FFI-free）
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- devclass 校验 ----

    #[test]
    fn devclass_validation() {
        assert_eq!(validate_devclass("zagt_test").unwrap(), "ZAGT_TEST");
        assert_eq!(validate_devclass("/NS/FOO").unwrap(), "/NS/FOO");
        assert_eq!(validate_devclass("  ZFOO  ").unwrap(), "ZFOO");
        assert!(validate_devclass("").is_err());
        assert!(validate_devclass("ZFOO; DROP").is_err());
        assert!(validate_devclass("ZFOO..").is_err());
        assert!(validate_devclass("中文包").is_err());
    }

    // ---- 包 → 文件夹（FULL 逻辑 + 命名空间标记） ----

    #[test]
    fn folder_logic_full_and_namespace() {
        assert_eq!(package_folder("ZAGT_TEST"), "zagt_test");
        assert_eq!(package_folder("/XYZ/FOO_BAR"), "#xyz#foo_bar");
        assert_eq!(package_folder("FOO"), "foo");
        // 异常形态（命名空间无尾段，非合法包名）：小写兜底
        assert_eq!(package_folder("/XYZ"), "xyz");
    }

    // ---- 文件名 token / 路径 ----

    #[test]
    fn object_paths_follow_abapgit_convention() {
        assert_eq!(
            object_rel_path(&ExportKind::Program, "ZFOO", "ZAGT_TEST").unwrap(),
            "src/zagt_test/zfoo.prog.abap"
        );
        assert_eq!(
            object_rel_path(&ExportKind::Func, "ZAGT_ADD", "ZAGT_TEST").unwrap(),
            "src/zagt_test/zagt_add.func.abap"
        );
        assert_eq!(
            object_rel_path(&ExportKind::Cds, "ZC_Foo", "ZAGT_TEST").unwrap(),
            "src/zagt_test/zc_foo.cds.ddl"
        );
        assert_eq!(
            object_rel_path(&ExportKind::Structure, "ZS_X", "ZAGT_TEST").unwrap(),
            "src/zagt_test/zs_x.stru.ddl"
        );
        // 命名空间对象：/NS/ 保留在路径里（包目录已是 #ns#）
        assert_eq!(
            object_rel_path(&ExportKind::Program, "/NS/ZFOO", "/NS/PKG").unwrap(),
            "src/#ns#pkg/#ns#zfoo.prog.abap"
        );
        // 无独立文件的类型
        assert!(object_rel_path(&ExportKind::Fugr, "ZG", "ZAGT_TEST").is_none());
        assert!(object_rel_path(&ExportKind::Skipped("SOTR".into()), "X", "Z").is_none());
        // % 转义
        assert_eq!(
            object_rel_path(&ExportKind::Program, "Z%FOO", "ZP").unwrap(),
            "src/zp/z%25foo.prog.abap"
        );
    }

    // ---- 文本归一 ----

    #[test]
    fn text_normalized_to_lf_with_trailing_newline() {
        assert_eq!(normalize_text("A\r\nB"), "A\nB\n");
        assert_eq!(normalize_text("A\nB\n"), "A\nB\n");
        assert_eq!(normalize_text(""), "\n");
    }

    // ---- package.devc.xml ----

    #[test]
    fn devc_xml_shape() {
        let xml = devc_xml("ZAGT_TEST", "测试包", "");
        assert!(xml.contains("<DEVCLASS>ZAGT_TEST</DEVCLASS>"));
        assert!(xml.contains("<CTEXT>测试包</CTEXT>"));
        assert!(!xml.contains("PARENTCL"));
        let xml2 = devc_xml("ZSUB", "子包", "ZROOT");
        assert!(xml2.contains("<PARENTCL>ZROOT</PARENTCL>"));
        // XML 转义
        let xml3 = devc_xml("ZX", "a<b&c", "");
        assert!(xml3.contains("a&lt;b&amp;c"));
    }

    // ---- abapgit.xml 清单 ----

    #[test]
    fn abapgit_manifest_has_required_fields() {
        let xml = abapgit_xml();
        assert!(xml.contains("<MASTER_LANGUAGE>E</MASTER_LANGUAGE>"));
        assert!(xml.contains("<STARTING_FOLDER>/src/</STARTING_FOLDER>"));
        assert!(xml.contains("<FOLDER_LOGIC>FULL</FOLDER_LOGIC>"));
    }

    // ---- 分类 ----

    #[test]
    fn manifest_type_mapping() {
        assert_eq!(ExportKind::Program.manifest_type(), "prog");
        assert_eq!(ExportKind::Skipped("SOTR".into()).manifest_type(), "sotr");
        assert_eq!(ExportKind::Func.manifest_type(), "func");
        assert_eq!(ExportKind::Fugr.manifest_type(), "fugr");
    }

    #[test]
    fn source_type_mapping() {
        assert_eq!(ExportKind::Func.source_type(), Some(ObjectType::Function));
        assert_eq!(ExportKind::Fugr.source_type(), None);
        assert_eq!(ExportKind::Table.source_type(), Some(ObjectType::Table));
    }

    // ---- meta_absent 形态 ----

    #[test]
    fn meta_absent_shape() {
        let v = meta_absent(ObjectType::Table, "zfoo");
        assert_eq!(v["exists"], false);
        assert_eq!(v["lock_mode"], "etag");
        assert_eq!(v["name"], "ZFOO");
        let v2 = meta_absent(ObjectType::Package, "zpkg");
        assert_eq!(v2["lock_mode"], "none");
        let v3 = meta_absent(ObjectType::Program, "zprog");
        assert_eq!(v3["lock_mode"], "adt_lock");
        assert_eq!(v3["active_check"], "not_checked");
    }
}
