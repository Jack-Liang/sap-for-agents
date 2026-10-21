# 方案：同时连接多个 SAP 系统（Multi-System）

> 状态：设计稿（未实施）。目标版本 v0.16+。
> 结论先行：**可行，且 SDK 层天然支持**——NWRFC SDK 的函数描述符缓存按 SYSID 分区，
> 多系统只要 SYSID 不同就互不干扰。工程量集中在网关自身的"单系统假设"拆除：
> 单一 `AppConfig.conn_params` → 单一 `SharedPool` → 全部 handler `State<SharedPool>`，
> 加上四处按系统维度才正确的全局状态（元数据缓存、ADT 配置、version 缓存、注册表条目）。

---

## 1. 目标与非目标

**目标**

- 一个网关进程同时连接 N 个 SAP 系统（典型：DEV + PRD，或多个租户/容器系统）。
- Agent / 外部消费方能**显式选择**目标系统；现有单系统部署**零破坏**升级。
- 注册表（跨会话记忆）与平坦交付端口在多系统下语义明确。

**非目标（本期不做）**

- 按系统的独立鉴权（`SAP_API_KEY` 仍是网关级；见 §9 开放问题）。
- 跨系统事务编排（如 DEV 数据搬运到 PRD）——那是上层用两次调用组合的事。
- SAP Server 模式（`SAP_ROLE=server`）的多目标注册。

---

## 2. 现状盘点：单系统绑定点在哪

| # | 绑定点 | 位置 | 说明 |
|---|--------|------|------|
| 1 | 连接参数 | `src/config.rs:130` | `AppConfig.conn_params` 只有一组 ASHOST/SYSNR/CLIENT/USER/PASSWD/LANG |
| 2 | 连接池 | `src/pool.rs:103` | `RfcConnectionPool` 本身与单组参数绑定，且**急切建首连**（`with_max_size` 里 `RfcConnection::new`，建连失败进程退出） |
| 3 | 路由状态 | `src/server.rs:28` | `SharedPool = Arc<RfcConnectionPool>` 是**全部**业务路由的 axum state；`app()` 单一 `with_state(pool)`（`server.rs:271`） |
| 4 | 元数据缓存 | `src/metadata.rs:56-58` | `FUNC_CACHE`/`TYPE_CACHE` 全局静态，**只按函数名/类型名作 key**——多系统下同名不同构型会互相污染 |
| 5 | ADT 配置 | `src/adt.rs:40` | `static ADT: OnceLock<AdtConfig>`：单 base_url + 单账号 + 单 CSRF 会话 |
| 6 | version 缓存 | `src/version.rs:35` | `SAP_INFO` 全局静态，单系统的 sysid/release |
| 7 | 注册表 | `src/registry.rs:63` | `Entry` 无系统维度；自动注册、`/api/invokes/{alias}`、OpenAPI 目录都默认"当前唯一系统" |
| 8 | 写后失效钩子 | `src/objects.rs:1554,1595,1733,2086,2166` | `drain_idle` / `invalidate_function` / `invalidate_type` 作用于唯一池/缓存 |
| 9 | 后台任务 | `src/openapi.rs:3537` | `registry_ops_watcher(Arc<SharedPool>)` 只盯一个池 |
| 10 | 探针/指标 | `src/server.rs:489` `/ready`；metrics | 单池 ping；连接池指标无系统标签 |

好消息：**执行引擎本身是干净的**。`executor.rs`/`api.rs` 拿到的是 `&RfcConnection`，
连接属于哪个系统对它们透明；`RfcConnection::system_info()`（`connection.rs:180`）
已经能廉价读出本连接的 SYSID。

---

## 3. SDK 层的关键事实（决定方案边界）

### 3.1 描述符缓存按 SYSID 分区 —— 多系统的天然地基

`sapnwrfc.h:4649-4654`（`RfcGetFunctionDesc` 文档）：

> The RFC Runtime maintains a cache for every R/3 System ID, as the meta data
> could be different from R/3 release to R/3 release.

即 SDK 进程级描述符缓存以 **(SYSID, 函数名)** 为 key。推论：

- **SYSID 不同的多系统**：各自的 `RfcGetFunctionDesc` 互不命中对方条目，天然安全。
  现有 v0.15 的 `SIGNATURE_STALE` 限制也自动变成**按系统**生效（A 系统改签名不影响
  B 系统的调用面）。
- **SYSID 相同的两个系统**（常见：克隆的 A4H 开发容器都叫 `A4H`）：**必须拒绝**。
  两个系统的同名函数会共享同一条缓存描述符，签名不同时按旧/错签名编组数据，
  且是**静默错**——比现在的 SIGNATURE_STALE 更危险。网关要做启动期/首连期检测：
  逐系统 `system_info()` 取 SYSID，发现重复直接报配置错误退出，报错信息给出
  "改 SID 或只连其中一个"的出路。

### 3.2 顺带发现：SDK 其实有缓存失效 API（可选跟进项）

`sapnwrfc.h:4722` 有 `RfcRemoveFunctionDesc(repositoryID, functionName, errorInfo)`，
文档明确说用于"后端签名已变、删除旧缓存"的场景。AGENTS.md 目前写的是
"SDK 描述符缓存无法在进程内失效"——从 SDK 头文件看未必成立（未实测）。
这与多系统正交，但值得做一次实验：若生效，`SIGNATURE_STALE` 可从"重启网关"
降级为"自动失效重查"。本期只在方案里登记，不改契约。

### 3.3 其他 SDK 事实

- SDK 原生支持一进程多目的地（每池各自 `RfcOpenConnection`），无需额外初始化；
  线程模型不变（现有 `spawn_blocking` + 每连接串行已经满足）。
- 类型描述符（`RfcGetTypeDesc`）同样按 SYSID 分区，结论同 3.1。

---

## 4. 方案总览

```
                          ┌────────────────────────────────────────┐
                          │  Arc<SystemRegistry>（新 axum state）    │
                          │                                        │
   /api/rfc               │  default ─► SystemHandle{pool,adt,     │
   /api/functions/...  ───┤              meta_cache, timeout}      │
   （无前缀 = 默认系统，      │                                        │
    100% 向后兼容）          │  "prd"  ─► SystemHandle{...}          │
                          │  "a4h"  ─► SystemHandle{...}           │
   /api/s/{system}/rfc  ──┤                                        │
   /api/s/{system}/...  ──┤  resolve(name) -> Arc<SystemHandle>     │
                          └────────────────────────────────────────┘
   /api/systems            系统清单 + 各系统健康度（新增，Agent 会话第一步）
```

三条设计主线：

1. **配置**：新 `sap_systems.toml` 声明 N 个命名系统；未提供时从现有 `SAP_*`
   环境变量合成一个名为 `default` 的系统——单系统部署完全无感。
2. **状态**：`SharedPool` 升级为 `Arc<SystemRegistry>`；每系统一个 `SystemHandle`
  （连接池 + ADT 端点 + 元数据缓存）。
3. **路由**：无前缀路径 = 默认系统（兼容）；`/api/s/{system}/...` 作用域前缀选系统。

---

## 5. 分层设计

### 5.1 配置层（`config.rs` 改造 + 新 `systems.rs`）

新文件 `sap_systems.toml`（`SAP_SYSTEMS_FILE` 覆盖路径）：

```toml
# 系统名 = URL 段（/api/s/<name>/...），小写字母数字与 -_，全局唯一
[systems.a4h]
ashost = "vhcala4h"
sysnr  = "00"
client = "001"
user   = "developer"
passwd = "${SAP_A4H_PASSWD}"     # 环境变量插值，文件本身可入库不带密钥
lang   = "EN"
default = true                    # 无前缀路径路由到它（必须恰好一个）

[systems.prd]
ashost = "sap-prd.example.com"
sysnr  = "10"
client = "100"
user   = "rfc_gateway"
passwd = "${SAP_PRD_PASSWD}"
pool_size = 4                     # 可选覆盖（默认 8）
adt_base_url = "https://sap-prd.example.com:44300"   # 可选，默认按 ashost 推导
read_only = true                  # 可选：该系统只允许读（默认 false）
```

要点：

- **每系统可覆盖项**：`pool_size`、`pool_idle_validate_secs`、`request_timeout_secs`、
  `adt_base_url`/`adt_user`/`adt_passwd`、`read_only`；未设走全局默认（全局值仍来自
  环境变量，语义不变）。
- **连接参数透传**：除固定快捷字段（ashost/sysnr/...）外，支持
  `[systems.x.conn]` 自由键值直接进 `RfcOpenConnection`——为 MSHOST/GROUP
  负载均衡等形态留后门，不用等网关逐个建模。
- **向后兼容**：无 `sap_systems.toml` 时，`config::load()` 按现有环境变量合成
  `[systems.default]`。`.env` 单系统用户升级后行为逐字节一致。
- 启动校验：恰好一个 `default`；系统名 URL-safe；`passwd` 插值缺失即报错。

### 5.2 状态层（新模块 `systems.rs`）

```rust
pub struct SystemRegistry {
    systems: HashMap<String, Arc<SystemHandle>>,
    order: Vec<String>,          // 配置声明序，列表端点稳定输出
    default: Arc<SystemHandle>,
}

pub struct SystemHandle {
    pub name: String,
    pub pool: RfcConnectionPool,        // 惰性首连（见下）
    pub adt: Option<AdtEndpoint>,       // 现 adt.rs 全局静态实例化
    pub timeout: std::time::Duration,   // 该系统的请求超时
    pub read_only: bool,
    sysid: OnceLock<String>,            // 首连时填（SYSID 查重 + 缓存 key 用）
}

pub type SharedState = Arc<SystemRegistry>;
```

**池惰性化**：`pool.rs` 增加惰性构造（`total=0`、`idle` 空，`acquire()` 现有逻辑
本就支持"无空闲则按需新建"）。分配策略：

- **默认系统保持急切建首连**——单系统部署"密码错启动即炸"的 fail-fast 语义原样保留；
- **非默认系统惰性** + 启动后台并行探测（每个几秒超时，只 warn 不阻塞监听）：
  PRD 暂时不可达不应拖死整个网关。探测顺便取 SYSID 做 §3.1 的查重。

**写后失效**（`objects.rs` 的 5 处钩子）：全部改为作用于**本次请求命中的系统**的
`SystemHandle`——`pool.drain_idle()` / `meta_cache.invalidate_*()` 天然就是实例方法了。

### 5.3 路由层（选系统的方式）

**选型：路径前缀 `/api/s/{system}`，否掉 Header / Query。** 理由：本网关的首要
受众是 Agent 与 codegen 消费方——前缀在 AGENTS.md 文档、curl 示例、浏览器历史、
OpenAPI `servers` 里都可见可复制；`X-SAP-System` 头对 openapi-generator 消费方
意味着自定义 header 参数，最容易丢；query 参数对 POST 语义含糊且同样易丢。

URL 形态（作用域前缀下不重复 `/api` 段）：

| 现有（默认系统，不变） | 作用域形态 |
|---|---|
| `POST /api/rfc` | `POST /api/s/prd/rfc` |
| `GET /api/functions/{name}` | `GET /api/s/a4h/functions/{name}` |
| `POST /api/invokes/{alias}` | `POST /api/s/a4h/invokes/{alias}` |
| `GET /ready` | `GET /api/s/prd/ready` |
| —— | `GET /api/systems`（新增：清单+健康+SYSID+是否默认） |

**实现注记（axum 0.8）**：`Router::nest` 不接受前缀里的路径参数（会 panic），
所以不能 `nest("/api/s/{system}", api_router)`。推荐做法是**单条通配路由 +
内部转发**：

```rust
// server.rs app() 里追加（示意）
.route("/api/s/{system}/{*rest}", axum::routing::any(scope_dispatcher))
```

`scope_dispatcher` 做三件事：① 从路径取 `{system}`，`registry.resolve()`，
未知系统 → 404 `SYSTEM_UNKNOWN`（附可用清单）；② 把解析出的
`Arc<SystemHandle>` 插入 request extensions；③ 将 URI 重写为 `/api/{rest}`
后 `api_router.clone().oneshot(req)` 内部转发——现有路由表、鉴权、限流、
只读守卫**一行不改**全部复用。

**handler 侧的机械改造**（一次性、可 grep 驱动）：约 39 处
`State(pool): State<SharedPool>` 改为：

```rust
async fn invoke_handler(
    State(state): State<SharedState>,
    sap: Sap,                        // 新 extractor：读 extensions，缺省 = 默认系统
    Json(body): Json<InvokeRequest>,
)
// 函数体内 pool.with_connection(...) → sap.pool.with_connection(...)
```

`run_blocking(pool, ...)` 等辅助函数签名同步换参。`openapi::registry_ops_watcher`
改为遍历全部系统 × published 条目。

**只读守卫**：`read_only_guard` 从全局开关改为查 `sap.read_only`（每系统可配，
见 §5.1）；runtime 档位维持网关级。

### 5.4 按系统迁移的状态

| 现全局静态 | 迁移去处 | 迁移方式 |
|---|---|---|
| `metadata::FUNC_CACHE/TYPE_CACHE`（只按名作 key） | `SystemHandle.meta_cache` | **随 handle 实例化**。比"全局缓存 key 加 SYSID 前缀"多改几处调用点，但彻底杜绝忘了带前缀的路径；`executor.rs` 的 `resolve_meta` 链路把 `&RfcConnection` 换成/加传 `&SystemHandle` 即可 |
| `adt::ADT` + CSRF 会话 | `SystemHandle.adt` | 实例化；CSRF token 本就是按 ADT 会话缓存的，每系统一份天然正确 |
| `version::SAP_INFO` | 每系统一份懒加载缓存 | `/api/version`：`sap` 字段保持 = **默认系统**（兼容），新增 `systems: {a4h: {...}, prd: {...}}` 全量块 |
| `/metrics` 池指标 | 加 `system` 标签 | `sap_pool_idle{system="prd"} 3` |
| auth / 限流 / runtime 档位 / 更新检查 | **维持网关级全局** | 与目标系统正交 |

### 5.5 注册表与交付端口（多系统下语义最容易碎的部分）

- **`Entry` 增加 `system: String` 字段**（serde 默认 `"default"`，旧 registry.json
  无损读入）。自动注册写入时记 `system` = 本次写请求命中的系统。
- **平坦端口按条目路由**：`POST /api/invokes/{alias}` 永远去 `entry.system`
  指向的池执行——别名是全局唯一命名空间，消费方不需要知道系统细节（这正是
  交付端口的意义）。作用域前缀形态 `/api/s/{sys}/invokes/{alias}` 校验
  `sys == entry.system`，不符返回 409 `SYSTEM_MISMATCH`（提示正确系统），
  防止"以为在 PRD 调其实打到 DEV"。
- **目录与文档**：`GET /api/registry?system=prd` 过滤；OpenAPI 目录的每个
  operation 元数据加 `system` 字段并用它做 tag（消费方在生成客户端里能看见）。
- **SIGNATURE_STALE 语义**：漂移守卫（`invoke.rs:413-431`）逐系统独立——
  SDK 缓存按 SYSID 分区，A 系统的签名变更只影响 A 系统的调用。

### 5.6 文档与周边

- `AGENTS.md`/`AGENTS.zh-CN.md`：工作流第 0 步前插一步
  `GET /api/systems`（单系统时该端点也返回 1 条，不影响旧流程）；
  所有端点示例补作用域形态。
- `index.html`/`docs.html`：系统下拉或清单展示（轻量）。
- `.env.example`、`docker-compose.yml`、`sap_systems.toml.example` 新模板。
- `README` §配置 增多系统小节。

---

## 6. 兼容性策略（升级零破坏承诺）

| 场景 | 升级后行为 |
|---|---|
| 只用环境变量的单系统部署 | 合成 `[systems.default]`，全部现有路径/语义/启动失败行为不变 |
| 现有 `registry.json` | 条目 `system` 缺省 `"default"`，无损读入；写回时带字段 |
| 现有 OpenAPI 消费方 | operation 多一个非破坏性元数据字段；`servers` 不变（仍是单网关地址） |
| 现有 MCP 客户端 | 工具多一个可选 `system` 参数（缺省默认系统），旧调用不变 |
| `/api/version` 的 `sap` 块 | 保持默认系统的信息（新增 `systems` 全量块为增量） |

唯一硬性不兼容：**同 SYSID 的多系统配置**——启动即拒绝（§3.1），这是正确性要求。

---

## 7. 实施阶段

| 阶段 | 内容 | 规模估算 |
|---|---|---|
| **P1 地基** | `sap_systems.toml` 解析 + env 合成；`SystemRegistry`/`SystemHandle`；池惰性化；39 处 handler 状态签名机械替换；`/api/systems` 端点；SYSID 查重 | 大头，~2/3 工作量；单系统行为不变的纯重构，可独立发版 |
| **P2 状态迁移** | metadata 缓存、ADT 配置、version 缓存、metrics 标签、写后失效钩子全部落到 SystemHandle | 中 |
| **P3 交付面** | registry `system` 字段、平坦端口按条目路由 + SYSTEM_MISMATCH、OpenAPI 目录、MCP `system` 参数、`/api/s/{sys}/ready` | 中 |
| **P4 可选跟进** | `RfcRemoveFunctionDesc` 实验（若有效则消灭 SIGNATURE_STALE）；每系统 `read_only` 细化落地；AGENTS.md/README 全面更新 | 小（实验先行） |

P1 结束即可发一个"无行为变化"的版本（多系统配置面已存在但缓存还是全局的，
单系统无感）；P2/P3 各自独立成版。全程不需要数据迁移。

## 8. 测试策略

- **单元**：toml 解析（默认系统唯一性/插值缺失/名字 URL-safe）；env 合成等价性；
  registry 旧文件读入默认 system。
- **stub-SDK 集成**（现有 CI 桩模式）：两个系统的 registry；
  `/api/s/unknown/...` → 404 SYSTEM_UNKNOWN；作用域与无前缀路由到不同池
  （桩可按连接参数区分）；`/api/systems` 输出。
- **真实系统冒烟**（本地 A4H）：双系统（可用同系统两份配置验证除 SYSID 查重外的
  全链路——注意 SYSID 查重要用能改 SID 的两套或 mock）；写后失效只影响目标系统。
- **回归**：现有全部端点在无前缀形态下的响应与改造前逐字段一致。

## 9. 开放问题（实现前需拍板）

1. **每系统独立鉴权**是否需要（一个 token 只能打 PRD）？建议 v1 不做，
   真有需求时 `SAP_API_KEY` 升级为 per-system map。
2. `/ready` 是否需要聚合形态（`?all=1`：全部 ready 才 200）？编排系统接入
   多系统时自然要，倾向 P3 顺手带上。
3. 作用域前缀用 `/api/s/{system}` 还是 `/systems/{system}/api`？前者短，
   后者更"资源化"。本方案取前者，无强偏好。
