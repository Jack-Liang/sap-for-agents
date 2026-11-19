# v0.22 设计文档：远程 ABAP 调试面（完整吸收 vibing-steampunk 调试能力）

状态：**P0/P1/P2 已实施（2026-11-19）**——REST 面完整、网关实测全链路通过；MCP 工具镜像缓期（见 §5）。前置阅读：AGENTS.md §7（ADT 代理）、§9（写编排）、§12（运行面）｜契约：[DEBUGGER-CONTRACT.zh-CN.md](./DEBUGGER-CONTRACT.zh-CN.md)

## 0. 一句话

让 agent 通过 REST 在 SAP 里**下断点、attach、看调用栈、读写变量、单步**——把
[oisee/vibing-steampunk](https://github.com/oisee/vibing-steampunk)（MIT）已验证的
ADT debugger 通道移植进 sap-for-agents 的网关形态（多系统 / auth / read_only / MCP）。

## 1. 背景与动机

现状的"调试"只有两种形态：`/api/abap/run` 打印式观察、`/api/dumps` 事后分析。
断点式调试缺位——长流程中间状态、只在特定输入下出现的错误，都只能靠反复改探针代码。

vsp 证明了这条路在 ADT REST 上走得通（断点/attach/步进/栈/变量读写/标准码调试/DAP），
且服务端零安装。本特性把它吸收为网关的一等能力。

移植合规：vsp 为 MIT 许可，ADT 契约（URL/XML 形态/状态语义）可直接移植，文档注明出处。

## 2. 可行性基线（2026-11-18 实测，P0 已完成 ✅）

**P0 全链路已在 A4H(816) 纯 HTTPS 打通**：断点 → abap/run 触发 → listener 命中（2s）→ attach → 栈 → 变量树（@ROOT→@LOCALS→LV_X=1）→ **写值 99** → stepOver → **读回 100** → stepContinue（debuggeeEnded）→ DELETE listener（断点连带清空）。完整契约见 **[DEBUGGER-CONTRACT.zh-CN.md](./DEBUGGER-CONTRACT.zh-CN.md)**——P1/P2 以它为实现依据。三个决定性事实：

| 事实 | 设计结论 |
|---|---|
| ✅ **`X-sap-adt-sessiontype: stateful` 头是命门**：无它 ICF 按无状态处理、请求结束即回收上下文，调试会话活不过下一个请求 | DebugSession 的每个请求（含 GET 与 CSRF fetch）都带该头 + `sap-contextid` cookie，`set-cookie` 回写 jar |
| ✅ **RFC 隧道不需要**：listen→attach→栈全程纯 HTTPS 可用 | vsp 的 `SADT_REST_RFC_ENDPOINT` 隧道整层跳过（那是给"HTTPS 不通"场景的） |
| ✅ **调试器与 debuggee 必须分属两个会话**：同会话时触发请求占住上下文，调试操作全部落空；且现有 `/api/adt` 代理 GET 不带 cookie、无 stateful 头、不回写 set-cookie | 触发通道 = `abap/run`（每次强制新会话，天然满足）；调试通道 = P1 独立 DebugSession |

其余基线：`adt.rs` 现状"每请求无状态 + 共享 CSRF 缓存（30min TTL）"（代码注释明确"CSRF 会话各自独立"）；本机 A4H 已知 ICM 会话型抖动（[[a4h-metadata-read-semantics]]）——保活与丢失恢复是一等设计项。

## 3. 架构：三层

```
REST/MCP ─→ debug.rs（状态机 + 每系统会话表）
                │  持有
                ▼
         DebugSession（stateful ADT 通道，新）
                │  sap-contextid 保持 + 心跳续租 + 会话内 CSRF
                ▼
           SAP ICF /sap/bc/adt/debugger/**
```

### 3.1 会话层（`debug_session.rs`，新文件）

- `DebugSession`：独立 cookie jar（`sap-contextid` 全程保持 + **每请求回写
  `set-cookie`**）+ **每请求（含 GET 与 CSRF fetch）带 `X-sap-adt-sessiontype: stateful`**（契约 §0 命门）+ 会话内 CSRF——与 `adt.rs` 共享缓存互不接触。
- **listen→attach→栈是一个原子"catch"**（后台任务背靠背执行，防 attach 竞态窗），
  catch 期间该会话忙（有状态会话一次一请求）；catch 完成后状态机进入 `stopped`，
  **状态轮询纯读内存、零 ADT 往返**。
- 取消 listen：从任意连接 `DELETE /listeners?…&requestUser=U`（注册按 user 单键，
  实测可跨会话摘除）→ 后台任务随 debuggee 放行而返回。
- **心跳续租**：tokio 后台任务周期性（默认 30s）发轻量 debugger 请求续租 ICM 会话；
  心跳失败 → 状态机标记 `session_lost`，可显式重建。
- 生命周期：显式创建 / 显式关闭（detach → DELETE listener，顺序固定）/ TTL 兜底
  回收（默认 30min 无交互）；网关停机 best-effort 同序清理（见 §6 风险 R1）。

### 3.2 状态机（`debug.rs`，新文件）

```
listening ──(catch: debuggee 命中→attach→栈)──→ stopped ⇄(step)⇄ stopped
    │                                            │
    │                                            └─(resume/detach)→ finished
    └─(listen 超时: 200 空体)→ finished
```

- 每系统**至多一个**活动会话（调试独占工作进程；第二个 `POST` → `409 DEBUG_SESSION_EXISTS`）。
- **listen 在网关后台任务里阻塞等待**（ADT listener 本身带 timeout 参数，实测
  debuggee 等待窗 ≈ 该值）；agent 侧 REST 轮询内存状态，不碰长挂 HTTP。
- 系统不可达/断连：状态机如实上报 `session_lost`，不自动重试破坏性动作。

### 3.3 API 面（`server.rs` 路由 + `api.rs`）

全部继承现有多系统作用域（`/api/s/{system}/debug/**`）、auth、rate-limit；
`/api/version` 的 `capabilities` 新增 `"debug": true`。

| 端点 | 语义 |
|---|---|
| `POST /api/debug/breakpoints` | 设断点：`{"targets":[{"kind":"prog","name":"ZFOO","line":42}]}`（类/FM 同理），返回 ADT 断点 id |
| `GET /api/debug/breakpoints` | 列出（网关视角 + ADT 实况） |
| `DELETE /api/debug/breakpoints/{id}`、`?all=true` | 删（`all` = 该用户全部外部断点——清残留利器） |
| `POST /api/debug/sessions` | 建会话并开始 listen：`{"listen_secs":120,"system_debug":false}` → `202 {"sid":…}` |
| `GET /api/debug/sessions/{sid}` | 状态轮询：`listening / stopped / finished / session_lost` + 当前位置（program/line/URI） |
| `GET /api/debug/sessions/{sid}/stack` | 调用栈（每帧 index/program/include/line/procedure/uri） |
| `GET /api/debug/sessions/{sid}/stack/{frame}/variables` | 变量树：`?depth=&max_elements=` 限深防爆（结构展开/内表分页） |
| `PUT /api/debug/sessions/{sid}/stack/{frame}/variables/{name}` | **写变量**（仅 stopped 态；`read_only` 拦 → 403 READ_ONLY） |
| `POST /api/debug/sessions/{sid}/control` | `{"action":"step_over"\|"step_into"\|"step_out"\|"resume"\|"to_line","line":…}` |
| `DELETE /api/debug/sessions/{sid}` | detach（debuggee 恢复运行）；`?delete_breakpoints=true` 顺带清本会话相关断点 |

MCP 侧镜像（**缓期至下一批次**——REST 面已完整服务 agent，MCP 工具需先把
handlers 抽成 `Arc<SystemHandle>` 入参的核函数，纯机械重构不影响契约）：
倾向收敛为 2 个带 action 参数的工具 `debug_manage` + `debug_inspect`。

### 3.4 策略决策（写死，不留给实现时摇摆）

1. **read_only 分级**：观察类操作（listen/status/stack/variables GET/step）在
   `read_only` 系统**放行**（只读系统也能排障）；变量写 `PUT` 被 403 READ_ONLY 拦。
2. **触发方是调用者**：网关只负责断点与调试会话，不负责"让代码跑到断点"——
   agent 用现有 `/api/rfc` 或 `/api/abap/run` 触发（同为网关用户，外部断点可见）。
3. **变量默认深度**：标量直接给值；结构/内表默认 depth=2、每层 50 元素，超出给
   `"_truncated"` 句柄（复用平坦端口的经验）。
4. **会话上限**：`SAP_DEBUG_MAX_SESSIONS_PER_SYSTEM`（默认 1）进 hot settings。

## 4. 实施计划（预估 ~1.5-2 周）

### P0 — 契约提取与手工打通（1-2 天）★ **已完成（2026-11-18）**
1. ✅ 读 vsp 源码（Go），提取 debugger 全链路 ADT 契约 → **[DEBUGGER-CONTRACT.zh-CN.md](./DEBUGGER-CONTRACT.zh-CN.md)**（断点 XML、listeners 阻塞语义、栈/变量/步进 URL 与 schema、subType 错误语义）。
2. ✅ A4H 上直连 ICM（curl + cookie jar + stateful 头）走通**完整闭环**，含变量写入真实生效的端到端证明（写 99 → 步过 → 读回 100）。关键收获：
   - `X-sap-adt-sessiontype: stateful` 是命门（见 §2）；
   - 变量枚举层次版本相关（@ROOT→@LOCALS vs @GLOBALS+@PARAMETERS），必须动态发现；
   - debuggee 等待窗口 ≈ listener timeout（实测 30s）；无 listener 时外部断点直接跳过、不挂死；
   - DELETE listener 连带清空该用户全部外部断点（清场即这一刀）；
   - ⚠️ 残余未知：**RFC 触发**（`/api/rfc`）路径未测——HTTP(classrun) 触发已证可用，不阻塞实现，文档先写"触发用 /api/abap/run（已验证）或 /api/rfc（未在所有版本验证）"。
3. ✅ 原始请求/响应样本存于契约笔记（mock fixture 用）；测试类已删、断点已清、listener 已摘（A4H 无残留）。

### P1 — 会话层（2-3 天）
- `debug_session.rs`：cookie jar、会话内 CSRF、心跳续租、TTL/关闭回收。
- 单测：用 P0 抓的原始响应做 fixture（mock ADT），覆盖心跳失败/会话丢失/并发互斥。
- 验收：会话在 5 分钟慢速轮询下保持有效（ICM 不回收）。

### P2 — 状态机 + REST + MCP（3-4 天）
- `debug.rs` 状态机、全部端点、MCP 工具、`capabilities.debug`、metrics
  （`sap_debug_sessions_active` gauge、`sap_debug_steps_total` counter）。
- 集成测试（`#[ignore]` 门控，复用 ZAGT_TEST 包惯例）：**全闭环自动化**——
  建临时程序+断点 → `/api/abap/run` 触发 → 轮询到 stopped → 栈/变量 → 写变量 →
  resume → 断言变量改动反映到执行结果 → 清理（程序删、断点删、会话关）。
- 验收：集成测试绿；手工 API 走查与 P0 记录一致。

### P3 — 文档与发布（1 天）
- AGENTS.md/README 新增 §14「ABAP 调试」（中英）；`.env.example`；`/docs` 渲染。
- 版本 **v0.22.0**。⚠️ 发布顺序提醒：main 上还压着 v0.20（runtime config）与
  v0.21（sapcli 吸收 + auth 块化）两个未发布批次（Cargo.toml 仍是 0.19.0）——
  先发 v0.20.0/v0.21.0（或合并一批），再发 v0.22.0，release notes 各自独立。

## 5. 明确不进 v0.22 的（vsp 能力的剩余面，登记后续）

| 能力 | 去向 | 原因 |
|---|---|---|
| DAP 服务器（VS Code/nvim/JetBrains 直调） | v0.23 候选 | 面向人类 IDE 的协议适配器；P2 若提前完成可作 stretch |
| AMDP/HANA 调试 | 独立特性 | 需直连 HANA 数据库，与网关 ADT/RFC 双通道完全正交 |
| Web/ICF 请求捕获（SOAP/OData 全栈） | v0.23 候选 | 依赖 HTTP 调试钩子，价值密度低于核心面 |

## 6. 风险与开放问题

- **R1 断点残留（最大运维风险）**：外部断点绑定用户且**不随 ICF 会话死亡消失**——
  网关半途死亡后残留断点会让该用户后续每次 RFC/HTTP 调用都"停进调试"导致挂死。
  对策：网关记账本会话所设断点；关闭/回收时默认清理；启动时探测该用户残留外部
  断点并 WARN；`DELETE ?all=true` 提供一键清场。文档用粗体写。
- **R2 工作进程独占**：调试停住 = 占住一个 WP；单实例 A4H 无所谓，PRD 必须警告
  （文档 + 默认每系统 1 会话）。
- **R3 ICM 会话抖动**（本机已知）：心跳续租 + `session_lost` 显式上抛，不静默重试。
- **R4 外部断点 × 触发路径**：HTTP(classrun) 触发 ✅ 已验证可用；**RFC 触发未测**（残余
  未知，不阻塞）——文档引导用 `/api/abap/run` 触发。
- **R5 816 与更新版本的契约漂移**：变量枚举根（@LOCALS vs @GLOBALS+@PARAMETERS）、
  栈资源形态（资源 GET vs dispatcher POST）、detach 可用性都属版本差异——实现按契约
  笔记的"动态发现"条目处理，未验证形态 `unknown` 上抛而不是猜。

## 6.5 实施期新增实证（P2，2026-11-18/19）

- **断点 URI 大小写敏感**：`oo/classes/ZCL_X/...` 大写形被 SAP 拒（"URI-Mapping
  cannot be performed"），小写规范形可落位——网关 builder 统一小写化（含 fm 的组名）。
- **断点不需要会话**：断点是用户态身份（requestUser+ideId+terminalId），agent 流程
  "先设断点→再建会话→再触发"成立——断点操作走**槽内缓存的长命通道**，绝不每次
  开/关 stateful 会话。
- **会话风暴实证**：高频开/关 stateful 会话（每次断点操作一个临时通道）把 trial
  A4H 的 ICM 拖进写失败抖动（activation 500、CSRF 签发失败；直连 discovery 却 200
  健康）。处置：容器重启清态 + 通道缓存化。这是 R1/R3 之外的新运维红线。
- **命中帧的变量层次随帧深变化**：classrun 调用链深处（frame index 13）时
  `@ROOT` 直接给变量（LV_X），浅帧时是 `@ROOT→@LOCALS`——自动发现必须两种都兼容
  （已实现：@ 前缀=容器下探，否则=变量直读）。

### P2 收尾期两个真实 bug（2026-11-19，均已修复 + 集成测试实证）

1. **teardown 通道选择错误 → close 挂死 ≥60s**：初版把 detach/stepContinue/
   DELETE-listener 全发在调试会话本身上。但 listening 期该会话被阻塞的 listen 占用、
   resume 后（debuggeeEnded）服务端调试上下文已死——请求排队到 60s 客户端超时。
   修正 = 契约 §3 的"任意另一条连接"模式：**listener DELETE 走槽内 bp_chan**
   （独立会话，10s 短超时，失败再开新会话兜底）；**仅 Stopped 态才 try_lock 发
   detach**（Finished 跳过、Listening 由 listener 删除兜住）。修后 close ≈ 11ms。
2. **`remove_slot` SLOTS 自死锁 → 网关整体假死**：持 SLOCKS 守卫调
   `update_sessions_gauge()`（内部再 `SLOTS.lock()`）——std Mutex 不可重入，
   close 线程永久死等；后续每个碰到 SLOTS 的请求各占死一个 tokio worker，
   worker 耗尽后**整个网关**（含 /health）无响应，且外观像"随机进程死亡"。
   修正 = 先释放守卫再刷 gauge。诊断手法：eprintln 打点 + 前台 3002 实例复现
   （teardown 1s 内全部完成、挂在其后——直接定位到 remove_slot）。

## 7. 与现有功能的关系

- `/api/abap/run` / `/api/rfc`：调试的**触发器**（网关用户 = 断点可见用户）；
- `/api/dumps`：断点够不着的崩溃仍是 dump 分析兜底——两者互补而非替代；
- `read_only`：分级放行（§3.4-1），只读生产系统获得"可观察不可改"的调试形态；
- 多系统/超时/auth/rate-limit：全部继承，无特例。
