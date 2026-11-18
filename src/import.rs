//! 包级导入（abapGit 反序列化模型移植，v0.21 第二阶段）。
//!
//! `POST /api/import/package/{devclass}`：接收**自家导出格式**（v1）的 zip，
//! 按依赖序批量安装到目标包。移植 abapGit 反序列化的四个核心机制
//! （zcl_abapgit_file_deserialize / zcl_abapgit_item_graph /
//! zcl_abapgit_dependencies / zcl_abapgit_objects_activation）：
//!
//! 1. **类型级依赖序**（abapGit 的 korrnum 表 + item_graph Kahn 排序的简化版）：
//!    创建序 packages(父先) → fugr 壳 → ddic(tabl/stru) → cds → intf → class
//!    → prog → incl → fugr-includes；
//! 2. **两阶段激活**（abapGit 全部建完再批量激活的形态）：所有写入
//!    `activate:false`，阶段末按 ddic → cds → intf → class → prog → incl
//!    → fugr-includes 逐对象激活，失败者第二轮重试（修复同批前向引用）；
//! 3. **覆盖策略**（abapGit overwrite/checks_adjust）：已存在对象默认覆盖写，
//!    `?overwrite=false` 跳过并记录；
//! 4. **dry_run 预检**（agent 防坑）：零写入地解析 + 探测存在性 + 输出计划。
//!
//! 本版导入**自家格式**（round-trip）；任意 abapGit 仓库 XML/AFF 解析（
//! VSEOCLASS/TPOOL/SOTR 等元数据通道）是三期——其机制研究结论见 AGENTS.md
//! 第 11 节。与导出的关键差异：abapGit 的批量激活用 `DD_MASS_ACT_C3`/`RS_
//! WORKING_OBJECTS_ACTIVATE`（ADT 侧无等价批量端点），网关改为逐对象激活 +
//! 重试轮，语义等价。

use crate::error::RfcError;
use crate::objects::ObjectType;
use crate::systems::Sap;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;

// ========================================================================
// 路由
// ========================================================================

/// 导入子路由。32MB 请求体上限（axum 默认 2MB，装不下稍大的包 zip）。
pub fn router() -> axum::Router<crate::server::SharedState> {
    use axum::extract::DefaultBodyLimit;
    axum::Router::new()
        .route(
            "/api/import/package/{*devclass}",
            axum::routing::post(import_package_handler),
        )
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
}

/// `POST /api/import/package/{devclass}` 查询参数。
#[derive(Deserialize, Default)]
struct ImportQuery {
    /// 已存在对象直接覆盖写（默认 true；false → 记 skipped_exists）
    #[serde(default)]
    overwrite: Option<bool>,
    /// 零写入预检：解析 + 探测存在性 + 输出计划（默认 false）
    #[serde(default)]
    dry_run: Option<bool>,
    /// 写完后两阶段激活（默认 true；false → 全部保持 inactive）
    #[serde(default)]
    activate: Option<bool>,
}

// ========================================================================
// 导出格式（v1）的解析模型
// ========================================================================

#[derive(Debug, Deserialize)]
struct ExportManifest {
    format: String,
    folder_logic: String,
    package: String,
    #[serde(default)]
    packages: Vec<ManifestPackage>,
    #[serde(default)]
    objects: Vec<ManifestObject>,
}

#[derive(Debug, Deserialize)]
struct ManifestPackage {
    devclass: String,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    ctext: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ManifestObject {
    /// R3TR/网关类型：prog/incl/class/intf/cds/tabl/stru/fugr（func 为未来保留）
    #[serde(rename = "type")]
    kind: String,
    name: String,
    #[serde(default)]
    group: Option<String>,
    package: String,
    #[serde(default)]
    path: Option<String>,
    /// export 状态：exported/expanded/skipped/error
    #[serde(default)]
    status: String,
    #[serde(default)]
    description: Option<String>,
}

// ========================================================================
// 纯函数（可单测）
// ========================================================================

/// 包重映射（PREFIX 式安装）：manifest 根包 → 目标包；子包 → `{目标}_{叶名}`。
/// 对象名不变（abapGit PREFIX 语义：重映射的是包，不是名字）。
fn remap_package(manifest_pkg: &str, manifest_root: &str, target: &str) -> Option<String> {
    let pkg = manifest_pkg.trim().to_uppercase();
    let root = manifest_root.trim().to_uppercase();
    let tgt = target.trim().to_uppercase();
    if pkg == root {
        return Some(tgt);
    }
    // 子包：剥掉 "{root}_" 前缀拿叶名
    let leaf = pkg.strip_prefix(&format!("{}_", root))?;
    if leaf.is_empty() {
        return None;
    }
    Some(format!("{}_{}", tgt, leaf))
}

/// 类型 → 写入创建阶段（小者先建）。返回 None = 不在导入范围。
fn creation_phase(kind: &str) -> Option<u8> {
    match kind {
        "fugr" => Some(1), // 组壳先于组内 include
        "tabl" | "stru" => Some(2),
        "cds" => Some(3),
        "intf" | "class" | "prog" | "incl" | "func" => Some(4),
        _ => None, // 其他（sush/sotr/...）= skipped_unsupported
    }
}

/// 类型 → 激活阶段内序（两阶段激活的第二阶段；None = 无独立源码可激活：
/// package 建即生效、fugr 壳随成员激活）。
/// 序：ddic(0) → cds(1) → intf(2) → class(3) → prog(4) → incl(5) → fugr-incl(6)。
fn activation_rank(kind: &str, is_group_include: bool) -> Option<u8> {
    match kind {
        "tabl" | "stru" => Some(0),
        "cds" => Some(1),
        "intf" => Some(2),
        "class" => Some(3),
        "prog" => Some(4),
        // 组内 include 排在普通 include 之后（L-includes 引用组上下文）
        "incl" => Some(if is_group_include { 6 } else { 5 }),
        "func" => Some(7),
        _ => None,
    }
}

/// manifest 对象 → 网关 ObjectType（None = 导入范围外）。
fn object_type_of(kind: &str) -> Option<ObjectType> {
    match kind {
        "prog" => Some(ObjectType::Program),
        "incl" => Some(ObjectType::Include),
        "class" => Some(ObjectType::Class),
        "intf" => Some(ObjectType::Interface),
        "func" => Some(ObjectType::Function),
        "fugr" => Some(ObjectType::FunctionGroup),
        "cds" => Some(ObjectType::CdsView),
        "tabl" => Some(ObjectType::Table),
        "stru" => Some(ObjectType::Structure),
        _ => None,
    }
}

/// 从 .abapgit.xml 提取 FOLDER_LOGIC（asx:abap/values/DATA 下）。
/// 值本身无大小写歧义（FULL/PREFIX/MIXED），小写串上截取即可。
fn folder_logic_of(xml: &str) -> Result<String, String> {
    let lower = xml.to_lowercase();
    let open = "<folder_logic>";
    let open_pos = lower.find(open).ok_or("缺少 FOLDER_LOGIC 元素")?;
    let start = open_pos + open.len();
    let end = lower[start..]
        .find("</folder_logic>")
        .map(|e| start + e)
        .ok_or("FOLDER_LOGIC 未闭合")?;
    Ok(lower[start..end].trim().to_uppercase())
}

// ========================================================================
// 编排
// ========================================================================

async fn import_package_handler(
    sap: Sap,
    axum::extract::Path(devclass): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<ImportQuery>,
    body: axum::body::Bytes,
) -> Result<axum::Json<serde_json::Value>, RfcError> {
    let target = crate::export::validate_devclass(&devclass)?;
    let overwrite = q.overwrite.unwrap_or(true);
    let dry_run = q.dry_run.unwrap_or(false);
    let activate = q.activate.unwrap_or(true);
    if body.is_empty() {
        return Err(RfcError {
            code: -1,
            status: 400,
            message: "请求体为空（应为导出 zip 的原始字节）".into(),
            key: "IMPORT_BODY_EMPTY".into(),
        });
    }
    // zip 解包 → {路径: 文本}
    let files = read_zip(&body)?;
    // .abapgit.xml：格式识别 + folder_logic 校验
    let abapgit = files
        .get(".abapgit.xml")
        .ok_or_else(|| import_err("IMPORT_FOLDER_LOGIC", "zip 缺少 .abapgit.xml"))?;
    let logic = folder_logic_of(abapgit).map_err(|m| import_err("IMPORT_FOLDER_LOGIC", &m))?;
    if logic != "FULL" {
        return Err(import_err(
            "IMPORT_FOLDER_LOGIC",
            &format!("FOLDER_LOGIC={} 不受支持（网关导出恒为 FULL）", logic),
        ));
    }
    // manifest：权威对象清单
    let manifest_raw = files
        .get("export_manifest.json")
        .ok_or_else(|| import_err("IMPORT_MANIFEST_MISSING", "zip 缺少 export_manifest.json"))?;
    let manifest: ExportManifest = serde_json::from_str(manifest_raw).map_err(|e| {
        import_err(
            "IMPORT_MANIFEST_INVALID",
            &format!("manifest 解析失败: {}", e),
        )
    })?;
    if manifest.format != "sap-for-agents-export/1" {
        return Err(import_err(
            "IMPORT_FORMAT",
            &format!(
                "format={} 不是网关导出格式 sap-for-agents-export/1",
                manifest.format
            ),
        ));
    }
    // manifest 自身的 folder_logic 再验一层（.abapgit.xml 已验过——双保险）
    if manifest.folder_logic.trim().to_uppercase() != "FULL" {
        return Err(import_err(
            "IMPORT_FOLDER_LOGIC",
            &format!(
                "manifest folder_logic={} 非 FULL（网关导出恒为 FULL）",
                manifest.folder_logic
            ),
        ));
    }
    // 包重映射表（manifest 包名 → 目标包名）
    let mut remap: BTreeMap<String, String> = BTreeMap::new();
    for p in &manifest.packages {
        if let Some(to) = remap_package(&p.devclass, &manifest.package, &target) {
            remap.insert(p.devclass.trim().to_uppercase(), to);
        }
    }
    // 目标包树（按深度升序：父先建）。manifest 根不在 packages 列表时也纳入
    let mut pkg_plan: Vec<(String, String, Option<String>, Option<String>)> = Vec::new(); // (to, from, ctext, parent_to)
    for p in &manifest.packages {
        let from = p.devclass.trim().to_uppercase();
        let Some(to) = remap.get(&from) else {
            continue;
        };
        // 父包映射：manifest.parent 经重映射；缺失时回退 rsplit 启发式
        let parent_to = p
            .parent
            .as_deref()
            .map(|parent| parent.trim().to_uppercase())
            .and_then(|parent| remap.get(&parent).cloned())
            .or_else(|| to.rsplit_once('_').map(|(super_, _)| super_.to_string()));
        pkg_plan.push((to.clone(), from, p.ctext.clone(), parent_to));
    }
    pkg_plan.sort_by(|a, b| a.0.matches('_').count().cmp(&b.0.matches('_').count()));
    if !pkg_plan.iter().any(|(to, _, _, _)| to == &target) {
        pkg_plan.insert(
            0,
            (
                target.clone(),
                manifest.package.trim().to_uppercase(),
                None,
                None,
            ),
        );
    }

    let sys = sap.arc();
    let adt = sys.adt.as_ref().ok_or_else(crate::adt::adt_disabled)?;

    // 计划对象（仅 exported/expanded；跳过 export 侧 error/skipped——缺口可见）
    #[derive(Debug)]
    struct Planned {
        kind: String,
        name: String,
        group: String,
        pkg_to: String,
        path: Option<String>,
        description: String,
        is_group_include: bool,
    }
    let mut planned: Vec<Planned> = Vec::new();
    let mut skipped_unsupported: Vec<serde_json::Value> = Vec::new();
    for o in &manifest.objects {
        let supported = object_type_of(&o.kind).is_some();
        let importable = o.status == "exported" || o.status == "expanded";
        if !supported || !importable {
            skipped_unsupported.push(serde_json::json!({
                "type": o.kind, "name": o.name, "status": o.status,
                "reason": if !supported { "类型不在导入范围" } else { "导出侧已标记 error/skipped" },
            }));
            continue;
        }
        let pkg_from = o.package.trim().to_uppercase();
        let Some(pkg_to) = remap.get(&pkg_from).cloned() else {
            skipped_unsupported.push(serde_json::json!({
                "type": o.kind, "name": o.name, "status": o.status,
                "reason": format!("对象包 {} 不在 manifest packages 清单里", pkg_from),
            }));
            continue;
        };
        let group = o.group.clone().unwrap_or_default().trim().to_uppercase();
        planned.push(Planned {
            kind: o.kind.clone(),
            name: o.name.trim().to_uppercase(),
            group,
            pkg_to,
            path: o.path.clone(),
            description: o
                .description
                .clone()
                .filter(|d| !d.trim().is_empty())
                .unwrap_or_else(|| o.name.trim().to_uppercase()),
            is_group_include: o.kind == "incl" && o.group.is_some(),
        });
    }
    // 创建序（稳定排序：同阶段保持 manifest 顺序）
    planned.sort_by_key(|p| creation_phase(&p.kind).unwrap_or(9));

    // -------- dry_run：零写入探测 + 计划 --------
    if dry_run {
        let mut objects = Vec::new();
        for p in &planned {
            let Some(ot) = object_type_of(&p.kind) else {
                continue;
            };
            let state = crate::objects::read_adt_object_state(adt, ot, &p.name, &p.group).await?;
            let action = classify_action(state.exists, overwrite, "exported");
            objects.push(serde_json::json!({
                "type": p.kind, "name": p.name, "group": (!p.group.is_empty()).then(|| p.group.clone()),
                "package": p.pkg_to, "path": p.path,
                "exists": state.exists,
                "action": action,
            }));
        }
        let mut packages = Vec::new();
        for (to, from, ..) in &pkg_plan {
            let state =
                crate::objects::read_adt_object_state(adt, ObjectType::Package, to, "").await?;
            packages.push(serde_json::json!({
                "from": from, "to": to, "exists": state.exists,
                "action": if state.exists { "exists" } else { "created" },
            }));
        }
        return Ok(axum::Json(serde_json::json!({
            "format": "sap-for-agents-import/1",
            "dry_run": true,
            "package": manifest.package.trim().to_uppercase(),
            "target": target,
            "overwrite": overwrite,
            "packages": packages,
            "objects": objects,
            "skipped_unsupported": skipped_unsupported,
        })));
    }

    // -------- 实装 --------
    // ① 目标包树（缺则建；存在 → exists）
    let mut packages = Vec::new();
    for (to, from, ctext, parent_to) in &pkg_plan {
        let state = crate::objects::read_adt_object_state(adt, ObjectType::Package, to, "").await?;
        if state.exists {
            packages.push(serde_json::json!({"from": from, "to": to, "action": "exists"}));
            continue;
        }
        let spec = crate::objects::CreateSpec {
            description: ctext
                .clone()
                .filter(|c| !c.trim().is_empty())
                .unwrap_or_else(|| to.clone()),
            devclass: parent_to.clone().unwrap_or_default(),
            transport: None,
            software_component: None,
        };
        match crate::objects::create_object(&sys, ObjectType::Package, to, "", &spec).await {
            Ok(()) => {
                packages.push(serde_json::json!({"from": from, "to": to, "action": "created"}))
            }
            Err(e) => {
                packages.push(serde_json::json!({"from": from, "to": to, "action": "failed", "error": e.message}));
            }
        }
    }

    // ② 写入阶段（全部 activate:false）
    struct Done {
        kind: String,
        name: String,
        group: String,
        pkg_to: String,
        path: Option<String>,
        action: &'static str,
        /// 原地更新时如实回显实际所在包（abapGit 包移动不做——透明化代替）
        real_package: Option<String>,
        error: Option<String>,
        problems: Vec<String>,
        act_rank: Option<u8>,
    }
    let mut done: Vec<Done> = Vec::new();
    for p in &planned {
        let Some(ot) = object_type_of(&p.kind) else {
            continue;
        };
        // fugr 壳：仅创建（组无独立"源码"语义；成员 include 随后写入）
        if p.kind == "fugr" {
            let state = crate::objects::read_adt_object_state(adt, ot, &p.name, &p.group).await?;
            let action = if state.exists && !overwrite {
                "skipped_exists"
            } else {
                "created"
            };
            let error = if action == "skipped_exists" {
                None
            } else {
                let spec = crate::objects::CreateSpec {
                    description: p.description.clone(),
                    devclass: p.pkg_to.clone(),
                    transport: None,
                    software_component: None,
                };
                crate::objects::create_object(&sys, ot, &p.name, &p.group, &spec)
                    .await
                    .err()
                    .map(|e| e.message)
            };
            done.push(Done {
                kind: p.kind.clone(),
                name: p.name.clone(),
                group: p.group.clone(),
                pkg_to: p.pkg_to.clone(),
                path: p.path.clone(),
                action,
                real_package: if state.exists {
                    Some(p.pkg_to.clone())
                } else {
                    None
                },
                error,
                problems: Vec::new(),
                act_rank: None, // 组壳无独立激活
            });
            continue;
        }
        // 源码对象：存在性 → action；取源码（缺文件 → failed）
        let state = crate::objects::read_adt_object_state(adt, ot, &p.name, &p.group).await?;
        let action = classify_action(state.exists, overwrite, "exported");
        let mut entry = Done {
            kind: p.kind.clone(),
            name: p.name.clone(),
            group: p.group.clone(),
            pkg_to: p.pkg_to.clone(),
            path: p.path.clone(),
            action,
            real_package: None,
            error: None,
            problems: Vec::new(),
            act_rank: activation_rank(&p.kind, p.is_group_include),
        };
        if entry.action == "skipped_exists" || entry.action == "skipped_unchanged" {
            done.push(entry);
            continue;
        }
        let Some(path) = &p.path else {
            entry.action = "failed";
            entry.error = Some("manifest 条目缺 path（无源码文件）".into());
            done.push(entry);
            continue;
        };
        let Some(source) = files.get(path.as_str()) else {
            entry.action = "failed";
            entry.error = Some(format!("zip 内缺少源码文件 {}", path));
            done.push(entry);
            continue;
        };
        // 不存在先建壳（描述 = manifest description 或对象名），再写
        if !state.exists {
            if let Err(e) = crate::objects::create_object(
                &sys,
                ot,
                &p.name,
                &p.group,
                &crate::objects::CreateSpec {
                    description: p.description.clone(),
                    devclass: p.pkg_to.clone(),
                    transport: None,
                    software_component: None,
                },
            )
            .await
            {
                entry.action = "failed";
                entry.error = Some(format!("创建壳失败: {}", e.message));
                done.push(entry);
                continue;
            }
        }
        // 写入（activate:false——两阶段激活的第二阶段统一做）
        match crate::objects::write_object_source(
            &sys, ot, &p.name, &p.group, source, None, false, false, None,
        )
        .await
        {
            Ok(outcome) => {
                if let Some(act) = &outcome.activated {
                    if !act.success {
                        // 写激活不该发生（activate:false → activated=None）；
                        // 防御性记录
                        entry.problems = act.problems.clone();
                    }
                }
                entry.action = if state.exists { "updated" } else { "created" };
                if state.exists {
                    // 原地更新：TADIR 如实回显实际所在包（对象名全局唯一，
                    // 计划目标包 ≠ 实际包——透明化代替静默偏差）
                    let sys_c = Arc::clone(&sys);
                    let name_c = p.name.clone();
                    let group_c = p.group.clone();
                    let real = crate::server::run_blocking(Arc::clone(&sys), move |conn| {
                        let (tobj, tname) = crate::objects::tadir_key_of(ot, &name_c, &group_c);
                        crate::discovery::read_tadir_one(&sys_c.meta, conn, tobj, &tname)
                    })
                    .await?;
                    entry.real_package = real.map(|t| t.devclass).filter(|d| !d.trim().is_empty());
                }
            }
            Err(e) => {
                entry.action = "failed";
                entry.error = Some(e.message);
            }
        }
        done.push(entry);
    }

    // ③ 激活阶段（两阶段激活的第二阶段；activate=false → 全部保持 inactive）
    let mut activation: Vec<serde_json::Value> = Vec::new();
    if activate {
        let mut queue: Vec<(u8, usize)> = done
            .iter()
            .enumerate()
            .filter(|(_, d)| d.act_rank.is_some() && d.error.is_none())
            .map(|(i, d)| (d.act_rank.unwrap(), i))
            .collect();
        queue.sort_by_key(|(rank, _)| *rank);
        // 第一轮：按序激活（写阶段全部完成后才进入——前向引用的对象此时都已存在）
        let mut failed_idx: Vec<usize> = Vec::new();
        for (rank, i) in &queue {
            let d = &mut done[*i];
            let ot = object_type_of(&d.kind).unwrap_or(ObjectType::Program);
            let base = ot.base_rel(&d.name, &d.group);
            match crate::objects::activate_object(adt, &base, &d.name).await {
                Ok(outcome) if outcome.success => {
                    activation.push(serde_json::json!({
                        "type": d.kind, "name": d.name, "phase": rank,
                        "result": "activated",
                    }));
                    d.problems.clear();
                }
                Ok(outcome) => {
                    let problems: Vec<String> = outcome.problems.clone();
                    activation.push(serde_json::json!({
                        "type": d.kind, "name": d.name, "phase": rank,
                        "result": "activation_failed", "problems": problems,
                    }));
                    d.problems = problems;
                    failed_idx.push(*i);
                }
                Err(e) => {
                    activation.push(serde_json::json!({
                        "type": d.kind, "name": d.name, "phase": rank,
                        "result": "activation_failed", "error": e.message,
                    }));
                    d.problems = vec![e.message];
                    failed_idx.push(*i);
                }
            }
        }
        // 重试轮：激活失败者再来一遍（abapGit 同款语义：后激活者就位后，
        // 前向引用导致的失败往往自愈）
        for i in failed_idx {
            let d = &mut done[i];
            let ot = object_type_of(&d.kind).unwrap_or(ObjectType::Program);
            let base = ot.base_rel(&d.name, &d.group);
            let rank = d.act_rank.unwrap_or(9);
            let outcome = crate::objects::activate_object(adt, &base, &d.name).await;
            match outcome {
                Ok(o) if o.success => {
                    activation.push(serde_json::json!({
                        "type": d.kind, "name": d.name, "phase": rank, "result": "retried",
                    }));
                    d.problems.clear();
                }
                Ok(o) => {
                    let problems: Vec<String> = o.problems.clone();
                    activation.push(serde_json::json!({
                        "type": d.kind, "name": d.name, "phase": rank,
                        "result": "activation_failed", "problems": problems,
                    }));
                    d.problems = problems;
                }
                Err(e) => {
                    activation.push(serde_json::json!({
                        "type": d.kind, "name": d.name, "phase": rank,
                        "result": "activation_failed", "error": e.message,
                    }));
                    d.problems = vec![e.message];
                }
            }
        }
    }

    // ④ 汇总
    let objects_json: Vec<serde_json::Value> = done
        .iter()
        .map(|d| {
            let act_state = if !activate || d.act_rank.is_none() {
                "inactive"
            } else if d.error.is_some() {
                "skipped"
            } else if d.problems.is_empty() {
                "activated"
            } else {
                "activation_failed"
            };
            serde_json::json!({
                "type": d.kind,
                "name": d.name,
                "group": (!d.group.is_empty()).then(|| d.group.clone()),
                "package": d.pkg_to,
                "real_package": d.real_package,
                "path": d.path,
                "action": d.action,
                "activated": act_state,
                "error": d.error,
                "problems": (!d.problems.is_empty()).then(|| d.problems.clone()),
            })
        })
        .collect();
    let count_action = |needle: &str| -> usize {
        objects_json
            .iter()
            .filter(|o| o["action"] == needle)
            .count()
    };
    Ok(axum::Json(serde_json::json!({
        "format": "sap-for-agents-import/1",
        "dry_run": false,
        "package": manifest.package.trim().to_uppercase(),
        "target": target,
        "overwrite": overwrite,
        "activated_flag": activate,
        "packages": packages,
        "objects": objects_json,
        "skipped_unsupported": skipped_unsupported,
        "summary": {
            "created": count_action("created"),
            "updated": count_action("updated"),
            "skipped": objects_json.iter().filter(|o| o["action"].as_str().unwrap_or("").starts_with("skipped")).count(),
            "failed": count_action("failed"),
        },
        "activation": activation,
    })))
}

/// action 判定（exists × overwrite × export 状态）。
fn classify_action(exists: bool, overwrite: bool, export_status: &str) -> &'static str {
    if !exists {
        return "created";
    }
    if !overwrite {
        return "skipped_exists";
    }
    // 已存在且覆盖：内容一致则跳过（abapGit "no import required" 语义）
    if export_status == "unchanged" {
        "skipped_unchanged"
    } else {
        "updated"
    }
}

fn import_err(key: &str, msg: &str) -> RfcError {
    RfcError {
        code: -1,
        status: 400,
        message: msg.to_string(),
        key: key.into(),
    }
}

/// zip 解包为 {路径: UTF-8 文本}。非 UTF-8 / 目录条目跳过（目录无内容）。
fn read_zip(bytes: &[u8]) -> Result<BTreeMap<String, String>, RfcError> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| import_err("IMPORT_ZIP_INVALID", &format!("zip 解析失败: {}", e)))?;
    let mut files = BTreeMap::new();
    for i in 0..archive.len() {
        let mut f = archive
            .by_index(i)
            .map_err(|e| import_err("IMPORT_ZIP_INVALID", &format!("zip 条目读取失败: {}", e)))?;
        if f.is_dir() {
            continue;
        }
        let name = f.name().trim_start_matches("./").to_string();
        let mut content = String::new();
        use std::io::Read as _;
        f.read_to_string(&mut content).map_err(|e| {
            import_err(
                "IMPORT_ZIP_INVALID",
                &format!("{} 非 UTF-8 文本: {}", name, e),
            )
        })?;
        files.insert(name, content);
    }
    Ok(files)
}

// ========================================================================
// 测试（纯函数，FFI-free）
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 包重映射 ----

    #[test]
    fn package_remap_prefix_semantics() {
        // 根包 → 目标包
        assert_eq!(
            remap_package("ZFOO", "ZFOO", "MYTGT"),
            Some("MYTGT".to_string())
        );
        // 子包 → {目标}_{叶名}（对象名不变）
        assert_eq!(
            remap_package("ZFOO_BAR", "ZFOO", "MYTGT"),
            Some("MYTGT_BAR".to_string())
        );
        // 深层子包：整段叶名保留
        assert_eq!(
            remap_package("ZFOO_BAR_BAZ", "ZFOO", "MYTGT"),
            Some("MYTGT_BAR_BAZ".to_string())
        );
        // 大小写归一
        assert_eq!(
            remap_package("zfoo_bar", "Zfoo", "mytgt"),
            Some("MYTGT_BAR".to_string())
        );
        // 不属于该树 → None
        assert_eq!(remap_package("ZOTHER", "ZFOO", "MYTGT"), None);
        assert_eq!(remap_package("", "ZFOO", "MYTGT"), None);
    }

    // ---- 创建/激活阶段序 ----

    #[test]
    fn creation_phase_ordering() {
        assert_eq!(creation_phase("fugr"), Some(1)); // 组壳最早（包单独处理）
        assert_eq!(creation_phase("tabl"), Some(2));
        assert_eq!(creation_phase("stru"), Some(2));
        assert_eq!(creation_phase("cds"), Some(3));
        assert_eq!(creation_phase("intf"), Some(4));
        assert_eq!(creation_phase("class"), Some(4));
        assert_eq!(creation_phase("prog"), Some(4));
        assert_eq!(creation_phase("incl"), Some(4));
        assert_eq!(creation_phase("sush"), None); // 范围外
    }

    #[test]
    fn activation_rank_ordering() {
        assert_eq!(activation_rank("tabl", false), Some(0));
        assert_eq!(activation_rank("stru", false), Some(0));
        assert_eq!(activation_rank("cds", false), Some(1));
        assert_eq!(activation_rank("intf", false), Some(2));
        assert_eq!(activation_rank("class", false), Some(3));
        assert_eq!(activation_rank("prog", false), Some(4));
        assert_eq!(activation_rank("incl", false), Some(5));
        // 组内 include 最后（引用组上下文）
        assert_eq!(activation_rank("incl", true), Some(6));
        assert_eq!(activation_rank("func", false), Some(7));
        assert_eq!(activation_rank("fugr", false), None); // 壳无独立激活
    }

    // ---- folder_logic 解析 ----

    #[test]
    fn folder_logic_extraction() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<asx:abap xmlns:asx="http://www.sap.com/abapxml" version="1.0">
 <asx:values>
  <DATA>
   <MASTER_LANGUAGE>E</MASTER_LANGUAGE>
   <STARTING_FOLDER>/src/</STARTING_FOLDER>
   <FOLDER_LOGIC>FULL</FOLDER_LOGIC>
  </DATA>
 </asx:values>
</asx:abap>"#;
        assert_eq!(folder_logic_of(xml).unwrap(), "FULL");
        assert!(folder_logic_of("<no-logic/>").is_err());
    }

    // ---- action 判定矩阵 ----

    #[test]
    fn action_matrix() {
        // 不存在 → created（无论 overwrite）
        assert_eq!(classify_action(false, true, "exported"), "created");
        assert_eq!(classify_action(false, false, "exported"), "created");
        // 存在 × 覆盖 → updated
        assert_eq!(classify_action(true, true, "exported"), "updated");
        // 存在 × 不覆盖 → skipped_exists
        assert_eq!(classify_action(true, false, "exported"), "skipped_exists");
        // 导出侧标记未变更（预留字段）→ skipped_unchanged
        assert_eq!(
            classify_action(true, true, "unchanged"),
            "skipped_unchanged"
        );
    }

    // ---- manifest 解析 ----

    #[test]
    fn manifest_parses_and_defaults() {
        let json = r#"{
            "format": "sap-for-agents-export/1",
            "folder_logic": "full",
            "package": "ZFOO",
            "packages": [{"devclass": "ZFOO", "parent": null, "ctext": "root",
                          "devc_xml": "src/zfoo/package.devc.xml"},
                         {"devclass": "ZFOO_BAR", "parent": "ZFOO"}],
            "objects": [
                {"type": "class", "name": "ZFOO_CLS", "package": "ZFOO",
                 "path": "src/zfoo/zfoo_cls.clas.abap", "status": "exported"},
                {"type": "fugr", "name": "ZFOOGRP", "package": "ZFOO", "status": "expanded"},
                {"type": "sotr", "name": "X", "package": "ZFOO", "status": "skipped"}
            ]
        }"#;
        let m: ExportManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.package, "ZFOO");
        assert_eq!(m.objects.len(), 3);
        assert_eq!(m.objects[0].kind, "class");
        assert!(m.objects[0].description.is_none()); // 可选字段缺省
        assert_eq!(m.objects[1].status, "expanded"); // fugr 壳
    }
}
