# AGENTS.md

[English](./AGENTS.md) | [简体中文](./AGENTS.zh-CN.md)

本文件指导 AI/Agent 如何使用 sap-for-agents 服务。

## 这是什么

sap-for-agents 是一个 **SAP NWRFC → REST 网关**：把 SAP 的 RFC/BAPI 函数暴露为 HTTP 接口。你（AI）通过它能在不安装 SAP 客户端的情况下，搜索、查询、调用 SAP 系统里的函数模块。

- **项目地址**：https://github.com/Jack-Liang/sap-for-agents
- **问题反馈**：https://github.com/Jack-Liang/sap-for-agents/issues

服务默认监听 `http://127.0.0.1:3000`（地址可由 `SAP_LISTEN_ADDR` 覆盖）。

## 认证（可选）

默认**免鉴权**（本机访问）。若部署方设置了环境变量 `SAP_API_KEY`，则所有 `/api/*` 端点要求请求头 `Authorization: Bearer <token>`：

```bash
curl -H "Authorization: Bearer <SAP_API_KEY>" http://127.0.0.1:3000/api/functions/BAPI_USER_GETLIST
```

- 未带 / 错 token → `401 {"code":401,"message":"..."}`。
- 探针 `/health`、`/ready`、`/api/version` 与公开页 `/`、`/agents.md`、`/openapi.json` **始终免鉴权**（不需要 token）。
- 是否启用由部署方决定。别靠试错，先查 `GET /api/version` 的 `capabilities.auth`；本机默认环境通常免鉴权。

## 你能做什么

| 目标 | 用哪个端点 |
|------|-----------|
| 网关还没配置过（首次启动）→ **引导式配置**：拿「该问用户什么」清单，采集后写入并热加载 | `GET /api/setup`（公开）→ `POST /api/setup/system` |
| 运行期配置管理：**读 / 改 / 删单个系统、切默认**（全部热生效，无需重启、不用碰文件） | `GET`·`PUT`·`DELETE /api/setup/system/{name}`、`PUT /api/setup/default` |
| 想运行期改**网关设置**（超时 / 限流 / 只读 / API Key / 语言…），不重启 | `GET`·`PUT /api/settings` |
| 新会话先**列系统清单**（多系统部署；单系统只返回一条——见下文「多系统」节） | `GET /api/systems`（公开） |
| 你（或人在 SE37）**改了某个 RFC 的参数**，网关还在报旧签名 → 重载该系统运行态（不重启） | `POST /api/admin/reload`（或 `POST /api/s/{system}/admin/reload`） |
| **改了 `.env` 或换了二进制**，需要进程真的重启（先排空再退出，由 supervisor 拉起） | `POST /api/admin/restart`（202；轮询 `/ready` 等它 503 → 200） |
| 新会话先**查注册表**：Agent 建过哪些接口的跨会话记忆（别名、意图、踩坑记录、调用示例） | `GET /api/registry` |
| 想发布/完善某个接口的契约（intent / notes / example） | `PUT /api/registry/{alias}` |
| 外部系统/前端想调用注册接口且**免学 SAP 方言**（扁平 JSON 进出） | `POST /api/invokes/{alias}` |
| 想要全部 published 条目的类型化服务目录规范（做 codegen） | `GET /openapi.json`（公开；或无参 `GET /api/openapi`） |
| 想查最近的调用历史排障（别名、函数、耗时、IP） | `GET /api/invokes/audit` |
| 新会话先**自描述**：网关版本/commit、能力开关（鉴权/只读/ADT/注册表/限流）、SAP sysid 与 release | `GET /api/version`（公开） |
| 不知道有哪些函数 → 按名字模糊搜索 | `POST /api/functions/search` |
| 知道函数名，想知道参数怎么填（永远新鲜——服务器端实时读） | `GET /api/functions/{name}` |
| 想读函数的完整文档（用途、示例） | `GET /api/functions/{name}/doc` |
| 想查某张表/结构有哪些字段 | `GET /api/ddic/type/{name}` |
| 想理解某个字段的含义、合法取值 | `GET /api/ddic/field/{table}/{field}` |
| 想看函数的 ABAP 源码（怎么实现的） | `GET /api/functions/{name}/source` |
| 想一次拿到函数源码 + 它所调用函数的签名（省 N 次往返） | `GET /api/functions/{name}/source?prologue=true` |
| 想看程序/报表/include 的源码 | `GET /api/programs/{name}/source` |
| 想读透明表数据（不用裸调 RFC_READ_TABLE） | `POST /api/table/read` |
| 想列 ABAP 短转储（结构化：错误类型、终止程序、用户、时间） | `GET /api/dumps` |
| 想知道**什么在反复失败**（按错误类型 + 终止程序聚合） | `GET /api/dumps/grouped` |
| 想看某个转储的调用栈/出错行/组件（免拉 45KB–1MB 的 ST22 正文） | `GET /api/dumps/{key}/detail` |
| 想要原始的完整 ST22 正文（发生了什么/错误分析） | `GET /api/adt/runtime/dump/{key}/formatted` |
| 想**创建** ABAP 对象（prog / incl / class / intf / func / fugr / cds / tabl / stru / package），可顺带首版源码 | `POST /api/objects/{type}/{name}/create` |
| 想**修改** ABAP 代码（上列所有带源码的类型） | `PUT /api/objects/{type}/{name}/source` |
| AI 式编辑（唯一匹配查找替换 + 自动激活） | `POST /api/objects/{type}/{name}/replace` |
| 语法检查（**不写库**，源码内嵌提交） | `POST /api/objects/{type}/{name}/syntax` |
| 想**读**对象源码（保留原文大小写；含 incl / intf / cds / fugr） | `GET /api/objects/{type}/{name}/source` |
| 想**删除**对象（删 fugr 会连带其中函数模块） | `DELETE /api/objects/{type}/{name}?group=&transport=` |
| 想**快照一个包**为 abapGit 风格文件树（git 友好 zip；备份 / SAP↔本地 diff） | `GET /api/export/package/{devclass}` |
| 想在**写之前**拿到对象的 exists / locked / active / 作者（一次往返） | `GET /api/objects/{type}/{name}/meta` |
| 想把**导出的 zip 装回系统**（round-trip；依赖序 + 两阶段激活） | `POST /api/import/package/{devclass}` |
| 想读/写 ABAP 类源码等 ADT（Eclipse 工具链）资源 | `ANY /api/adt/{path}` |
| **实际调用一个 SAP 函数** | `POST /api/rfc` |
| 想调用函数但**免填** `func_name`（类型化端点） | `POST /api/functions/{name}/invoke` |
| 想要本网关的机器可读 OpenAPI 规范 | `GET /openapi.json`（公开免鉴权） |
| 想要**按 BAPI 生成类型化 operation** 的规范（参数/类型/字段全展开） | `GET /api/openapi?functions=BAPI_X,BAPI_Y` |
| 改函数前想知道**谁在调用它**（where-used 引用查询） | `GET /api/functions/{name}/where-used` |
| 你是 MCP 客户端（Claude 等）——把网关挂载为 MCP 工具服务器 | `POST /mcp`（Streamable HTTP 无状态） |
| 跑一段 ABAP 代码（临时 classrun 类，自动删除） | `POST /api/abap/run` |
| 跑 ABAP Unit 测试（json / junit4 / raw 输出） | `POST /api/tests/aunit` |
| 跑 ATC 代码检查（发现项 JSON） | `POST /api/atc/run` |
| 创建 / 读取 / 释放 / 删除 CTS 传输请求 | `POST`·`GET` `/api/cts/requests…` |
| 按名称搜索**所有**对象类型（ADT quick search） | `GET /api/objects/search?q=` |
| OpenSQL SELECT 走 ADT 数据预览（池表、JOIN） | `POST /api/table/osql` |
| 远程调试 ABAP（断点 / 监听 / 调用栈 / **变量写** / 单步放行；只读系统可观察） | `POST`·`GET`·`DELETE /api/debug/**` |
| 每系统登录方式：SSO2 / SNC / ADT mTLS+OAuth2 / 鉴权插件 | `sap_systems.toml` 的 `auth.*` / `adt.*` 块（见 §13） |
| BTP trial oauth 系统**浏览器授权**（authorization_code → refresh token，v0.23） | `POST /api/setup/system/{name}/authorize` |
| 人类友好的交互式 API 文档 | `GET /docs`（Redoc 渲染规范） |

## 首次启动引导（Agent 引导配置）

全新部署（无 `sap_systems.toml`、无 `SAP_*` 连接环境变量）**照样启动监听**——配置完成前所有 SAP 业务端点统一返回 `503 SYSTEM_NOT_CONFIGURED`。看到这个状态就是你去引导用户的信号：

```bash
# 1. 探明状态：缺什么、该问用户哪些字段、写入是否允许
curl http://127.0.0.1:3000/api/setup
# → {"configured":false,"writable":true,"fields":[{"name":"ashost","required":"直连形态必填",...},...],
#    "example":{...},"connection_forms":{...}}

# 2. 按 fields 清单向用户采集：简短系统名（小写，如 a4h）、ashost/sysnr/client、
#    账号、密码。密码建议让用户提供 ${VAR} 环境变量引用——明文不落对话、
#    不落日志，连接时才从环境变量插值。

# 3. 提交：校验 → 真实试连 → 落盘 → 热加载（无需重启）
curl -X POST http://127.0.0.1:3000/api/setup/system -H "Content-Type: application/json" \
  -d '{"name":"a4h","ashost":"vhcala4h","sysnr":"00","client":"001",
       "user":"DEVELOPER","passwd":"${SAP_A4H_PASSWD}"}'
# → {"saved":true,"default":true,"file":"sap_systems.toml",
#    "connection":{"tested":true,"sysid":"A4H","release":"753",...},"systems_count":1,...}

# 4. 确认——网关此刻起完全可用
curl http://127.0.0.1:3000/api/systems
```

- 请求体字段与 `sap_systems.toml` 的 `[systems.{name}]` 表一一对应（未知键 → 422 拦拼写错误）；负载均衡连接不用 ashost/sysnr，改传 `conn: {"MSHOST": "...", "R3NAME": "...", "GROUP": "..."}`。
- 人类交接：桌面机器上未配置启动会自动在浏览器打开 `/setup` 引导页，页面提供一段复制即用的交接文字（描述的就是本流程，地址已填好）。用户把那段话粘给你时，直接按上面的步骤执行即可。配置完成后同一页面就是完整**配置台**（浏览器里增删改系统/切默认）。
- **落盘前先真实试连**——主机/账号/密码不对返回 `502 SETUP_CONNECT_FAILED`（或 `SETUP_CONNECT_TIMEOUT`），什么都不会写入。`test:false` 跳过试连（离线预配置场景，自担风险）。
- 成功 = 写入 `sap_systems.toml` **并热加载**。首个系统无条件成为默认系统；之后可用同样方式追加系统（`default:true` 可切换默认）。若网关此前跑在环境变量配置上，该系统也会一并写入文件——重启后系统集合不变。
- `409 SETUP_NAME_EXISTS` → 换个名字；`409 SYSTEM_SYSID_CONFLICT` → 两个后端共用 SYSID（见「多系统」节）；`403 SETUP_DISABLED` → 部署方设了 `SAP_SETUP=off`；`403 SETUP_LOCKED` → **绑定非回环地址且未鉴权**的部署拒绝配置写入（防滥用：防凭据中继/内网探测/DNS rebinding 改配置）——出路在部署侧：设 `SAP_API_KEY` 走鉴权远程访问，或 `SAP_SETUP=force` 显式放开

### 运行期系统管理（v0.20：无需重启、不用碰文件）

四个管理动词共用同一道写入门（见上）与同一套"先试连后落盘"纪律：

```bash
# 读单个系统的配置（密码永不回显——只给 ${VAR} 引用形态或"已设置"标志）
curl http://127.0.0.1:3000/api/setup/system/a4h
# → {"name":"a4h","default":true,"ashost":"vhcala4h","sysnr":"00",...,"passwd":{"ref":"${SAP_A4H_PASSWD}"},"source":"toml"}

# 编辑（半更新：未提供的字段保持原值——只改密码就只传 passwd）
curl -X PUT http://127.0.0.1:3000/api/setup/system/a4h -H "Content-Type: application/json" -d '{"passwd":"${SAP_A4H_PASSWD2}"}'
# → 校验 → 试连（"test":false 可跳过）→ toml 条目原地替换 → 热替换句柄（旧池由在途请求自然排空）

# 删除（热卸载；删的是默认系统时自动提升剩余第一个为新默认）
curl -X DELETE http://127.0.0.1:3000/api/setup/system/a4h
# 删最后一个系统要求 ?force=true（显式确认回到未配置模式：503 SYSTEM_NOT_CONFIGURED）

# 切默认系统（此后无前缀路径打到它）——热切换
curl -X PUT http://127.0.0.1:3000/api/setup/default -H "Content-Type: application/json" -d '{"name":"a4h"}'
```

- **不支持改名**（名字是 URL 段 + toml 键 + 审计/注册表引用的三重标识）——删除后重建。
- 每次写入同时原地更新 `sap_systems.toml`，重启网关系统集合不变（文件始终是唯一事实源）。

### 运行期网关设置（v0.20：热生效，落盘 `sap_gateway.toml`）

三层，`GET /api/settings` 如实标注（每项带 `source`：`default` / `env` / `file`）：

- **hot** —— 下一个请求即生效并落盘：`request_timeout_secs`、`rate_limit_rps`（0 = 不限流）、`read_only`（全局；与各系统自己的开关取 OR）、`api_key`（值永不回显；`""` 关闭鉴权）、`drain_secs`、`update_check`、`lang`。
- **apply_on_system_rebuild** —— 烤在系统句柄里的全局默认（`pool_size`、`pool_idle_validate_secs`、`adt_base_url`）：只影响之后新建/编辑的系统；改**运行中**系统用 `PUT /api/setup/system/{name}`。
- **apply_on_restart** —— `listen_addr`、`mode`、`systems_file`、`registry_file`：改 `.env` 后重启（`/setup` 配置台有一键重启按钮）。

```bash
curl http://127.0.0.1:3000/api/settings
# → {"hot":{"request_timeout_secs":{"value":60,"source":"default"},...},
#    "apply_on_system_rebuild":{...},"apply_on_restart":{...},"file":"sap_gateway.toml"}

curl -X PUT http://127.0.0.1:3000/api/settings -H "Content-Type: application/json" \
  -d '{"request_timeout_secs":120,"rate_limit_rps":10,"read_only":true}'
# → {"saved":true,"applied":["request_timeout_secs","rate_limit_rps","read_only"],...}
#    立即生效（下一个请求就按新值），同时落盘。
```

- 写路径与系统配置同一道安全门（未鉴权 → 仅本机回环可写）。
- 持久化在独立文件 `sap_gateway.toml`（`SAP_GATEWAY_FILE`，0600——存 API key 明文，与 `.env` 同信任级）；优先级 **文件 > env > 默认**；删掉文件里的键即回落 env。
- 改 API key 立即生效——**下一个请求**（包括你自己的）就要带新 Bearer token。小心把自己锁在外面。

## 标准操作流程

绝大多数任务遵循 **查注册表 → 搜索 → 查接口 → 查文档 → 看源码 → 调用** 六步：

```
0. 查注册表  GET  /api/registry             复用之前会话建好的接口（空表可跳过）
1. 搜函数    POST /api/functions/search     找到目标函数名
2. 查接口    GET  /api/functions/{name}     看清楚参数名、类型、方向
3. 查文档    GET  /api/functions/{name}/doc 理解用途、约束、示例
4. 看源码    GET  /api/functions/{name}/source  理解实现（可选）
5. 调用      POST /api/rfc                  按 interface 填参执行
```

> 不要跳过第 2 步直接调用——SAP 参数名区分大小写且必须大写，类型（CHAR/INT/BCD...）决定如何传值。先查接口能避免 90% 的传参错误。

## 多系统

网关可以**同时连接多个 SAP 系统**（部署方通过 `sap_systems.toml` 配置）。所有 `/api/*` 业务端点不写前缀 = **默认系统**（行为与单系统完全一致）；每个端点都有**作用域形态**——在 `/api` 前缀的位置插入 `/api/s/{系统名}`：

```bash
curl http://127.0.0.1:3000/api/s/prd/rfc                  # POST → 在 prd 系统上调用
curl http://127.0.0.1:3000/api/s/prd/functions/BAPI_USER_GETLIST
curl http://127.0.0.1:3000/api/s/a4h/dumps/grouped
curl http://127.0.0.1:3000/api/s/prd/ready                # 该系统的 readiness
```

- 会话第一步 `GET /api/systems`：列出各系统（`name`、是否 `default`、`read_only`、`adt`、`sysid`、`quarantined`，以及连接状态 `connected`/`connect_error`——来自 60s 周期探测）。单系统部署只返回一条，忽略作用域即可。
- **SAP 连不上网关照样启动**（v0.19：所有系统一律惰性连接，取消启动期 fail-fast）。连接问题以降级状态呈现而不是进程死亡：`GET /api/systems` 显示 `connected:false` + `connect_error`，`/ready` 返回 503（保持严格，供负载均衡摘流），首页显示琥珀色横幅并列出失败系统的错误，对不可达系统的业务调用返回 **503 `SYSTEM_UNREACHABLE`**（附原始 RFC 详情）。恢复全自动：60s 探测翻回状态，业务调用按需重试。只有真正的配置形状错误（toml 语法、缺默认系统）仍会让启动中止。
- 未知系统名 → `404 SYSTEM_UNKNOWN`（错误里附可用清单）。**跨系统对比对象**：同一读取端点换两个作用域各调一次（如 `/api/functions/ZFOO/source` 分别在 a4h 与 prd 下），对结果做 diff。
- 平坦交付端口 `/api/invokes/{alias}` **按条目记录的 system 路由**（别名是全局命名空间）；显式作用域与条目不符 → `409 SYSTEM_MISMATCH`。`GET /api/registry?system=prd` 按系统过滤。
- MCP 工具接受可选 `system` 参数（缺省 = 默认系统）。
- 系统也可以**运行期管理**：`POST /api/setup/system` 新增，`PUT/DELETE /api/setup/system/{name}` 编辑/删除，`PUT /api/setup/default` 切默认——全部先试连再落盘 `sap_systems.toml` 并热生效（见「首次启动引导」节）。任一操作都无需重启。
- SAP 端**改了 RFC 接口**（SE37 里增删改参数）会被缓存三层——SAP 侧函数组加载按**连接**缓存、SDK 的函数/类型描述符按 **SYSID 进程级**缓存（sapnwrfc.h 明言 "cache for every R/3 System ID"——**换连接刷不掉这一层**）、网关按**函数名**缓存元数据。`POST /api/admin/reload` 把三层一起作废：连接池换代（空闲连接立刻关闭，借出中的归还即弃）+ 清空函数/DDIC 元数据缓存 + 按 `RfcRemoveFunctionDesc` **点名移除** SDK 缓存条目。可选请求体 `{"func_names":["Z_FOO"]}`——显式点名移除"调用过但从未自省过"的函数。**Agent 或前端要做"重启"就调这个**——它不重启进程，也不重读 `.env` / `sap_systems.toml`。作用域形态 `POST /api/s/{system}/admin/reload`。响应：`system`、`connections_closed`、`func_metadata_cleared`、`type_metadata_cleared`、`sdk_desc_removed`、`sdk_type_removed`。
- 当改动落在系统运行态**之外**（`.env` 里的值、换了二进制）时，需要 `POST /api/admin/restart`。它**立刻返回 202**，重启异步发生：`/ready` 在 `SAP_DRAIN_SECS`（默认 5 秒）窗口内一律 **503 + `Retry-After`**，监听继续把在途请求跑完，窗口结束进程 `exit(0)`。**必须有 supervisor 才能自动恢复**（`start.sh` 守护循环 / launchd `KeepAlive` / systemd `Restart=always` / docker `--restart`）；裸 `exec` 启动的进程退出后不会回来。调用方的正确串法：POST → 收 202 → 轮询 `/ready` → 见 503（此时别发新业务）→ 见 200 → 恢复。`SIGTERM`/`SIGINT` 走同一条排空路径，所以普通 `kill` 也不会再甩给调用方 `ECONNREFUSED`，而是可重试的 503。
- `/api/admin/reload` 与 `/api/admin/restart` 与配置写**同一道安全门**（v0.20）：未鉴权部署仅本机回环可调（Host 头也须回环——防 DNS rebinding）；远程调用设 `SAP_API_KEY` 走 Bearer。`/setup` 配置台自带一键重启按钮。
- **同一系统不同 client**（同 ashost/sysnr、不同 `client`）完全支持：SAP 仓库对象跨 client、签名必然一致，各条目仍有独立的池 / ADT / 注册表命名空间。
- 硬性限制：**不同后端**共用同一 SYSID（如经不同主机访问的克隆开发容器）会被隔离（`503 SYSTEM_SYSID_CONFLICT`）——SDK 描述符缓存按 SYSID 分区，跨后端的 SYSID 孪生会静默共享错误签名。

## 端点速查（含可复制的示例）

### 0. 注册表——Agent 的跨会话记忆 **兼** 交付端口

注册表是网关的持久记忆：凡是**经本网关**成功写入的 remote-enabled 函数，都会自动登记为 `draft` 条目（幂等；你写过的 intent/notes 不会被清掉）；删除该函数对象时条目转为墓碑。存储为本地 JSON 文件（`SAP_REGISTRY_FILE`，默认 `./registry.json`）——跨网关重启存活、不依赖 SAP。

```bash
# 之前会话都建了什么？（每个会话的第 0 步）
curl http://127.0.0.1:3000/api/registry
# → {"count":1,"version":1,"entries":[{"alias":"z_calc","func_name":"Z_CALC",
#     "intent":"...","notes":"...","example":{...},"status":"draft",...}]}

# 按子串过滤（alias / func_name / intent，大小写不敏感）
curl 'http://127.0.0.1:3000/api/registry?q=customer'

# 完善自动登记的 draft（PUT 为全量替换：先 GET、改完再 PUT 回来）
curl -X PUT http://127.0.0.1:3000/api/registry/z_calc \
  -H "Content-Type: application/json" \
  -d '{"func_name":"Z_CALC","group":"ZMATH","intent":"给前端用的客户列表",
       "notes":"必须传 MAX_ROWS；只读接口无需 commit",
       "example":{"inputs":{"IV_A":20,"IV_B":22}},"status":"published"}'

# 单条 · 删除（默认墓碑——记录留着可考）
curl http://127.0.0.1:3000/api/registry/z_calc
curl -X DELETE 'http://127.0.0.1:3000/api/registry/z_calc'            # → status "deleted"
curl -X DELETE 'http://127.0.0.1:3000/api/registry/z_calc?purge=true' # 物理删除
```

- 条目字段：`alias`（唯一、小写、URL 安全，支持 `team/name` 前缀）、`func_name`、`group`、`intent`（干什么的）、`notes`（踩坑记录/know-how）、`doc`（面向消费方的 Markdown 文档，展示在 OpenAPI 目录）、`example`（调用示例体）、`status`（`draft`/`published`；墓碑显示 `deleted`，默认列表隐藏——`?include_deleted=true` 可见）、`max_rows`（平坦调用的表行封顶，默认 100）。
- 删除 SAP 对象时条目**转墓碑**（绝不静默消失）；再次写入同名函数自动复活。删除 fugr 不会级联处理其中 FM 的条目——整组删除后需手动清理。
- 同名函数重复登记会保留你的 `intent`/`notes`/`example`——只刷新 status/group/时间戳。登记失败绝不影响 SAP 写入本身（仅在写响应里降级为 warning）。
- MCP 客户端：`list_registry_apis` 工具就是这份目录。

#### 交付端口：`POST /api/invokes/{alias}`（扁平 JSON 进 / 扁平 JSON 出）

published（或 draft）条目可被**外部系统零 SAP 方言**调用——不用大写参数约定、不用声明怎么读输出。契约每次调用**实时读 SAP 服务器接口**（`FUNCTION_IMPORT_INTERFACE`）派生——无论漂移来自哪里（SE37 手改、经本网关删库重建）都立即跟随。执行侧按 SDK 的 **SYSID 进程级**描述符缓存编组：**经本网关**做的签名变更，写后钩子会自动移除过期缓存条目，新签名立即可调；只有**网关之外**的变更（SE37 直改）会让 SDK 陈旧——触碰这类参数返回 `409 SIGNATURE_STALE`，提示调 `POST /api/admin/reload`，全程无需重启进程（见写入章节）：

```bash
# 前端/其他语言的消费方调用注册接口——这就是全部的 API：
curl -X POST http://127.0.0.1:3000/api/invokes/z_calc \
  -H "Content-Type: application/json" \
  -d '{"iv_a": 20, "iv_b": 22}'
# → {"EV_SUM": 42}          ← 扁平响应，真实类型（INT→整数、BYTE→Base64）

# 输出表行数封顶：?limit= > 条目 max_rows > 默认 100
curl -X POST 'http://127.0.0.1:3000/api/invokes/bapi-users?limit=50' -d '{}' -H "Content-Type: application/json"
```

- 请求体键**大小写不敏感**（`requtext` 等价 `REQUTEXT`）；未知键 → `400 INVOKE_PARAM_UNKNOWN`（带合法参数清单——拼写错误当场暴露而非被吞掉）。结构体传 JSON 对象、表传行对象数组。
- 所有输出按真实类型返回；输出表按封顶截断（`?limit=` > 条目 `max_rows` > 默认 100），发生截断时响应带 `"_truncated": [...]`。
- `GET /openapi.json`（公开）为每个 published 条目携带**类型化 operation**（intent/notes/example 流入）——把这个 URL 交给 openapi-generator，消费方就能生成客户端 SDK。`GET /api/openapi` 无参调用返回同一目录（现算）。公开规范由后台 watcher 预热缓存——SAP 宕机时照样秒回。
- 墓碑条目 → 404（其函数已删除）。draft 条目可调用但不进公开目录，置 `status:"published"` 后进入。
- `GET /api/invokes/audit` —— 最近约 500 次调用（别名、函数、成败/状态码、耗时、来源 IP；最新在前），免翻日志的快速排障。

### 1. 搜索函数

```bash
curl -X POST http://127.0.0.1:3000/api/functions/search \
  -H "Content-Type: application/json" \
  -d '{"pattern":"BAPI_USER_*","max_results":10}'
```

- `pattern`：函数名通配符，`*` 匹配任意。如 `BAPI_*`、`RFC_*`。
- 返回 `functions` 数组，每项含 `name` / `group` / `description`。
- 无匹配返回 `200 {"count":0,"functions":[]}`（**不是错误**）。

### 2. 查函数接口

```bash
curl http://127.0.0.1:3000/api/functions/BAPI_USER_GETLIST
```

返回该函数的**全部参数**，每个参数含：
- `name`：参数名（**传入时必须用这个原样大写名**）
- `type`：`CHAR` / `INT` / `STRUCTURE` / `TABLE` / `BCD` / `DATE` ... 或 `ELEMENT`（见下）
- `direction`：`IMPORT`（你要填）/ `EXPORT`（返回值）/ `TABLES`（可进可出）
- `length`：字符长度（CHAR/NUM/DATE 等）
- `optional`：是否可省略
- `description`：参数说明
- `fields`：若为 STRUCTURE/TABLE，列出嵌套字段

接口元数据**实时读 SAP 服务器**（`FUNCTION_IMPORT_INTERFACE`，响应字段 `interface_via: "fii"`）——无论签名是经本网关改的还是别人在 SE37 手改的，都**立即**反映。SDK 描述符已认识的参数带精确类型/长度/字段；**新增**参数（SDK 缓存慢一步）做尽力映射：ABAP 内建字面量（`I`→`INT`、`STRING`、`D`→`DATE`…）精确对应，结构体/表经 DDIC 解析出字段清单，其余显示 `type: "ELEMENT"`、引用对象写进 `description`（如 `TYPE BAPIEXTUIDGET`）。网关重启后 SDK 视图跟上，精确类型自然出现。

> 支持带命名空间的函数名（含 `/`，如 `/SDF/EWA_GET_ABAP_DUMPS`）。
> URL 路径中原始形式（`/api/functions//SDF/EWA_GET_ABAP_DUMPS`）与
> 百分号编码形式（`/api/functions/%2FSDF%2FEWA_GET_ABAP_DUMPS`）均可；
> JSON 请求体（如 `/api/rfc` 的 `func_name`）直接原样传入即可。

### 3. 查函数文档

```bash
curl 'http://127.0.0.1:3000/api/functions/BAPI_USER_GETLIST/doc?lang=EN'
```

返回 `short_text`（短说明）、`long_text`（SE37 完整文档，可能很长）、`parameter_docs`（各参数描述）。`lang` 不传则用 `SAP_LANG` 环境变量（默认 EN）。

> 不是所有函数都有长文档。`long_text` 为空属正常，看 `parameter_docs` 即可。

### 4. 查 DDIC 表/结构字段

```bash
curl http://127.0.0.1:3000/api/ddic/type/BAPIRET2
```

返回该 DDIC 对象的全部字段定义。⚠️ 对**结构**（如 `BAPIRET2`）普遍可用；对**透明表**（如 `MARA`）取决于目标系统 DDIC 配置，部分系统会返回 `NOT_FOUND`。

### 5. 查字段语义（数据元素/域/合法取值）

```bash
curl 'http://127.0.0.1:3000/api/ddic/field/BAPIRET2/TYPE?lang=EN'
```

返回 `data_element`（数据元素）、`domain`（域）、`description`、`fixed_values`（域的固定值，对状态码/类型字段特别有用——告诉你这个字段能填哪些合法值）。

### 6. 调用 SAP 函数

```bash
curl -X POST http://127.0.0.1:3000/api/rfc \
  -H "Content-Type: application/json" \
  -d '{
    "func_name": "STFC_CONNECTION",
    "inputs": {"REQUTEXT": "hello"},
    "string_outputs": {"ECHOTEXT": 255, "RESPTEXT": 255}
  }'
```

请求体字段：
- `func_name`：**必填**，函数名（大写）
- `inputs`：IMPORT 标量参数 → 值。字符串直接传，整数直接传数字
- `table_inputs`：TABLES 输入参数 → 行数组（每行是 `{字段: 值}`）
- `struct_inputs`：顶层 IMPORT 结构体参数 → `{字段: 值}`
- `string_outputs`：要读的 EXPORT 字符串参数 → 最大长度（`null` 表示自动发现）
- `int_outputs`：要读的 EXPORT 整型参数名数组
- `auto_outputs`：要按元数据真实类型读的 EXPORT 标量参数名（INT→整数、FLOAT→浮点、INT8→i64、BCD→字符串、BYTE/XSTRING→Base64）
- `table_outputs`：要遍历的 EXPORT 表 → 字段列表。字段项 `{"name":"FIELD"}` 或 `{"name":"FIELD","max_len":12}`；加 `"auto":true` 让该字段按真实类型读（INT→整数、FLOAT→浮点、INT8→i64、BYTE/XSTRING→Base64、其余→字符串）
- `struct_outputs`：顶层结构体输出 → 字段列表（规则同 `table_outputs`）
- `read_return`：是否自动读 BAPI 的 RETURN 消息表
- `timeout_secs`：本次调用超时秒数（可选，≥1）。不传用全局默认 60s；慢接口（批量 BAPI、大表查询）可自主放宽。超时返回 504
- `retry`：通信类错误是否自动换新连接重试（可选，默认 `true`，最多 3 轮）。⚠️ 重试发生在「函数可能已执行、响应丢失」的场景——过账类写 BAPI 传 `false` 防双执行（重试发生与否看 `rfc_call_retries_total` 指标）。扁平端口用 `?retry=false`

响应体：
- `scalars`：标量输出（参数名 → 值，值类型由读取方式决定）
- `tables`：表输出（表名 → 行数组，每行 `{字段: 值}`）。默认字段值为字符串；`auto:true` 的字段按真实类型返回（整数/浮点/Base64 字符串）
- `structs`：顶层结构体输出（同 `tables` 的值类型规则）
- `return_table`：RETURN 消息（若有，字段统一为字符串）

> ⚠️ **表/结构输出默认按字符串读**。需要保留数值语义时，给字段加 `"auto":true`，服务端按 DDIC 真实类型（INT/FLOAT/INT8/BYTE）选择对应 getter。

### 7. ADT REST 代理（dump、类源码等 Eclipse 工具链资源）

`ANY /api/adt/{path}` 把 SAP 系统的 **ADT REST API**（ICF 上的 `/sap/bc/adt/**`，Eclipse 用的同一套）透传出去。凭证由网关统一持有，写方法的 CSRF token 自动处理，调用方只管发 HTTP。

```bash
# ABAP 短转储列表（Atom feed：错误 ID、终止程序、用户、时间）
curl -H "Accept: */*" http://127.0.0.1:3000/api/adt/runtime/dumps

# 某条 dump 的完整 ST22 正文（key 取自 feed 条目 rel="self" 链接）
curl -H "Accept: text/plain" \
  "http://127.0.0.1:3000/api/adt/runtime/dump/<key>/formatted"

# 读 ABAP 类源码（命名空间/长类名同样可用）
curl -H "Accept: */*" http://127.0.0.1:3000/api/adt/oo/classes/cl_runtime_error/source/main

# ADT 服务目录（注意它要求 Accept: application/atomsvc+xml）
curl -H "Accept: application/atomsvc+xml" http://127.0.0.1:3000/api/adt/discovery
```

行为说明：
- `/api/adt/` 之后的路径 1:1 映射 ADT 路径（`/api/adt/runtime/dumps` → `/sap/bc/adt/runtime/dumps`）；`%20` 等百分号编码（dump key 里常见）正常工作。
- 请求侧转发 `Accept`、`Content-Type`、`If-Match`/`If-None-Match` 与 body；响应侧把 ADT 的状态码、`Content-Type`、`ETag`、`Last-Modified` 与 body **原样透传**（多为 XML）——此处的 404/406 来自 ADT 本身，不是网关错误。
- 写方法（POST/PUT/DELETE/PATCH）：网关自动获取并携带 `X-CSRF-Token` 与会话 Cookie，遇 403（token 过期）自动刷新重试一次。
- 网关侧故障走 JSON 错误契约：400 `ADT_PATH_INVALID`、502 `ADT_UNREACHABLE`、503 `ADT_DISABLED`（`SAP_ADT_BASE_URL` 为空）、504 `ADT_TIMEOUT`。
- 需要目标系统的 ADT ICF 服务处于激活状态（SICF）。基地址默认 `http://<SAP_ASHOST>:50000`，可用 `SAP_ADT_BASE_URL` 覆盖（设为空串禁用）。

### 8. 短转储结构化分析（ST22）

对第 7 节同一份 ADT 数据的**解析视图**：拿到的是结构化 JSON，而不是原始 Atom feed / 45KB–1MB 的 ST22 文本：

```bash
# 结构化列表（最新在前）：error_type、program、user、at、message、key
curl http://127.0.0.1:3000/api/dumps

# 什么在「反复」失败：按（错误类型 + 终止程序）聚合
curl http://127.0.0.1:3000/api/dumps/grouped

# 单个转储的解析详情：头表、终止点（include/行号/过程）、调用栈（最内层在前）。
# key 取列表/聚合响应里的 key 字段
curl http://127.0.0.1:3000/api/dumps/20260824012009%20a4h/detail
```

- `GET /api/dumps` —— 查询参数：`from`/`to`（`yyyyMMddHHmmss`，UTC，透传给 ADT 服务端翻页）、`limit`（默认 100，上限 1000）。
- `GET /api/dumps/grouped` —— 参数同上；返回组按条数降序（同数按最新在前），每组含 `count` / `first` / `last` / `users` / `latest_key` / `latest_message`，`latest_key` 可直接接详情端点。
- `GET /api/dumps/{key}/detail` —— 网关内部拉英文渲染并解析：`error_type`、`exception`、`program`（头表值，RAISE_EXCEPTION 时可能与 feed 的终止程序不同——feed 指抛异常的标准类，头表指调用它的类）、`component`（"Not assigned" 归一化为空）、`include`/`line`/`procedure`/`main_program`、`stack[]`（position/type/program/include/line/name）与头表原始 `header` 标签→值。
- 典型排查流：`grouped` → 挑最上面的组 → 用 `latest_key` 拉 `detail` → 用 `/api/programs/{program}/source` 读出错行的源码。
- 详情里的 `program` 对类转储是 **class pool 名**（`ZCL_X=========CP`），类本身是 `ZCL_X`。
- 需要启用 ADT（同第 7 节）；未启用返回 503 `ADT_DISABLED`。详情解析按英文标签匹配——非英文系统上字段返回空而非错值，需要原文时走第 7 节的 `/formatted`。详情 404 = 转储已过期或该 release 无详情资源（7.50 有 feed 无详情）。

`GET /api/functions/{name}/source?prologue=true`（同一「省往返」思路）：返回源码 + 依赖签名前言——扫描源码里的 `CALL FUNCTION 'X'`，逐个解析成紧凑签名块（`prologue.text`，ABAP 注释风格），一次调用同时拿到「代码 + 它调用的东西的契约」。读取失败的依赖保留 `FUNCTION X -- 接口读取失败` 占位行，缺口可见而非静默丢弃。

### 9. 代码修改（写入编排）

修改函数 / 类 / 程序 / 接口 / include / 函数组 / CDS 视图。网关在一次 HTTP 请求内跑完 ADT 写序列：**建立 stateful 会话 → LOCK → PUT 源码 → UNLOCK → 激活**；lockHandle 绝不跨请求（ADT 锁绑定 ABAP 会话，跨请求的柄必然失效）。

`{type}` 取 `prog`（程序/报表）、`incl`（include）、`class`、`intf`（接口）、`func`（函数模块，组名自动经 RFC 搜索反解，也可显式传 `"group"`）、`fugr`（函数组）、`cds`（CDS/DDLS 视图，source 即 DDL 文本）、`tabl`（DDIC 透明表，source 为 `define table` DDL）/ `stru`（DDIC 结构，source 为 `define structure` DDL）——两者均无锁，见下方说明、`package`（仅创建/删除——包没有源码）。

```bash
# AI 式编辑（推荐）：唯一匹配查找替换 + 激活
curl -X POST http://127.0.0.1:3000/api/objects/prog/ZMY_REPORT/replace \
  -H "Content-Type: application/json" \
  -d '{"old_string":"WRITE '\''old'\''.","new_string":"WRITE '\''new'\''."}'

# 全量写源码
curl -X PUT http://127.0.0.1:3000/api/objects/class/ZCL_FOO/source \
  -H "Content-Type: application/json" \
  -d '{"source":"CLASS zcl_foo DEFINITION ... ENDCLASS."}'

# 读回对象源码（保留原文大小写；所有带源码类型可用）
curl http://127.0.0.1:3000/api/objects/intf/ZIF_FOO/source

# 一步：创建 + 填源码 + 设为 remote-enabled，随后直接经 /api/rfc 调用
curl -X POST http://127.0.0.1:3000/api/objects/func/Z_CALC/create \
  -H "Content-Type: application/json" \
  -d '{"description":"calculator","group":"ZMATH","rfc_enabled":true,
       "source":"FUNCTION z_calc IMPORTING VALUE(iv_a) TYPE i VALUE(iv_b) TYPE i EXPORTING VALUE(ev_sum) TYPE i.\n  ev_sum = iv_a + iv_b.\nENDFUNCTION."}'
curl -X POST http://127.0.0.1:3000/api/rfc \
  -H "Content-Type: application/json" \
  -d '{"func_name":"Z_CALC","inputs":{"IV_A":20,"IV_B":22},"auto_outputs":["EV_SUM"]}'
# → {"scalars":{"EV_SUM":42},...}

# 老版本（7.52）上的函数模块：先建「组」，再建「函数」
# （建组必须带 devclass/transport——漏了组就落在 $TMP）
curl -X POST http://127.0.0.1:3000/api/objects/fugr/ZMATH/create \
  -H "Content-Type: application/json" \
  -d '{"description":"math helpers","devclass":"ZMATH","transport":"A4HK900123"}'
curl -X POST http://127.0.0.1:3000/api/objects/func/Z_CALC/create \
  -H "Content-Type: application/json" \
  -d '{"description":"calculator","group":"ZMATH","devclass":"ZMATH","transport":"A4HK900123",
       "rfc_enabled":true,"source":"FUNCTION z_calc.\nENDFUNCTION."}'
# 新名字是 SDK 缓存未命中、现场拉取，写入钩子同时移除同名旧缓存条目——立即经 /api/rfc 可调。

# 删除对象（删 fugr 会连带其中的函数模块）
curl -X DELETE "http://127.0.0.1:3000/api/objects/func/Z_CALC?group=ZMATH"

# 语法检查（不写库：源码内嵌提交）
curl -X POST http://127.0.0.1:3000/api/objects/prog/ZMY_REPORT/syntax \
  -H "Content-Type: application/json" \
  -d '{"source":"REPORT zmy_report.\nWRITE 1."}'
```

- **`doc` 字段（仅 func，v0.14）**：`create`/`PUT source`/`replace` 接受 `doc` —— 面向消费方的 Markdown 文档，存入注册表条目并渲染进 OpenAPI 目录。文档随代码走：带新 `doc` 重写即更新条目；不带则保留已有。（当前 release 的 SE37 长文本没有远程写通道——注册表条目就是文档的存放地。）
- **写后一致性（v0.14 + v0.15 + v0.18.1）**：函数写入/删除成功后，网关清元数据缓存、排干空闲 RFC 连接，并（v0.18.1 起）从 SDK 的 **SYSID 进程级**描述符缓存中按名移除该函数的条目（`RfcRemoveFunctionDesc`——唯一一层换连接刷不掉的缓存）。v0.15 起**契约面永远新鲜**：`GET /api/functions/{n}`、`POST /api/invokes/{alias}` 的契约派生、OpenAPI 目录、MCP `get_function_interface` 全部实时读 SAP 服务器接口（`FUNCTION_IMPORT_INTERFACE`）——签名变更（含删除后重建、SE37 手改）立即可见。v0.18.1 起**执行侧对网关内变更即时跟随**：删除重建的新签名带新参数立即就能调用（A4H 端到端实测）。仅剩的缺口是**网关之外**改签名（SE37 直改）且该函数此前已被调用过：SDK 仍持旧编组视图。网关会检测这一点（契约 vs SDK 视图）并拒绝瞎猜：`POST /api/invokes/{alias}` → `409 SIGNATURE_STALE`，`/api/rfc` 的参数错误附带 `POST /api/admin/reload` 提示——reload 点名移除旧条目即可，无需重启进程。正文/逻辑修改不受影响（值走活连接）；被缓存的只有参数清单。tabl/stru 写入/删除也会清网关的 DDIC 字段缓存（`GET /api/ddic/type/{n}`）。
- `replace` 请求体：`old_string` / `new_string`（可选 `transport`、`activate`（默认 true）、`group`、`rfc_enabled`、`doc`）。`old_string` 必须精确匹配**唯一一处**（0 处 → 先读当前源码；多处 → 带更多上下文行；`\r\n`/`\n` 差异自动归一化）。空 `old_string` 仅对空对象有效。
- `PUT /source` 请求体：`source`（全量源码）+ 同上可选字段。
- `syntax` 请求体：`source`。返回 `issues[]`：`severity`（E/W/…）、`line`、`offset`、`text`。
- 响应里的 `activated.success` 是**逻辑结果**：激活失败时 HTTP 仍为 200，带 `activated.messages[]` / `problems[]`（"Line N: 文本"）——读它、改源码、重试。传输层错误（网络/会话）才走 4xx/5xx。
- 锁冲突（他人正在编辑）→ 409 `OBJECT_LOCKED`，消息来自 SAP 原文。另一种冲突长这样：HTTP 500 `ExceptionResourceSaveFailure` + *「… is already locked in request ED1K… of user X」*——该对象已被登记在另一个请求里，**把这个请求号当 `transport` 传**，换新请求接不过去。
- **`tabl`/`stru` 写入无锁（etag 乐观并发）**：源码化 DDIC 对象（透明表与结构）完全不用 ADT 编辑锁——网关按 GET etag → PUT（`If-Match`）→ 激活 写入，没有 stateful 会话，也就没有 423 `InvalidLockHandle` 一族的问题。读与写之间被并发修改会以 412 `ETAG_CONFLICT` 暴露——重读源码再试即可。源码是 DDL 文本（表 `define table ...`，结构 `define structure ...`——结构只需 `@EndUserText.label` + `@AbapCatalog.enhancement.category`，表还需 `@AbapCatalog.tableCategory`、`@AbapCatalog.deliveryClass`、`@AbapCatalog.dataMaintenance`），`syntax` 检查同样适用。缺注解会被以 "Can't save due to errors in source; execute check for details" 拒绝（跑一下 `POST .../syntax` 即可看到缺哪个）。⚠️ 其余 DDIC 类型（域、数据元素、表类型）在现行版本上是**结构编辑器对象、无文本源码**——本组端点不支持；请改用下方小节的裸代理配方创建。
- **函数模块签名可经源码写入（SEDI 形态）**：参数签名**内联写在 FUNCTION 语句里**——`FUNCTION zfm IMPORTING VALUE(iv) TYPE i EXPORTING VALUE(ev) TYPE i.` … `ENDFUNCTION.`——签名会注册进 FM 接口（已端到端实证）。网关也接受经典 `*" IMPORTING ...` 注释块形态（`/api/functions/{n}/source` 的返回形态）并自动转换。⚠️ 例外：**曾/现 rfc_enabled** 的模块接口被冻结——源码写得进、激活也成功，但参数变化被 SAP 忽略；改签名请删除重建（配合 `create` + `rfc_enabled` 成本很低）。删除重建改签名后，接口读取立即新鲜，新签名也**立即可调**（写后钩子已移除 SDK 旧缓存条目，见下方写后一致性）。
- **`rfc_enabled: true`**（create / `PUT source` / `replace`，仅 func）：源码写完后，网关在同一把锁下 PUT 模块元数据（`fmodule:processingType="rfc"`，描述先读回再带上——该 PUT 是整文档替换）。新 FM 立即可经 `POST /api/rfc` 调用。
- **创建走 ADT-first**：`POST /api/objects/{type}/{name}/create`（body：`description` 必填；可选 `devclass`（默认 `$TMP`）、`transport`、`software_component`（仅 package；缺省按 ZLOCAL→LOCAL→HOME 逐个试）、`source` 首版源码一步写入+激活）。网关按标准 ADT objectcreation XML 创建（Eclipse / vscode_abap_remote_fs / vibing-steampunk 同一契约）；`prog`/`func` 在 ADT 失败时回退 RFC RPY 插入路径。`func` 在组缺失时自动建 `fugr`（ADT 路径——ABAP Cloud Trial 上同样有效，绕开了 RFC 建组不写 TADIR 的限制）。更丝滑的形态：`PUT .../source` 与 `POST .../replace` 支持 `"create": true` + `"description"`——对象不存在时网关自动建壳并重试；建错的壳一个 `DELETE` 即可清掉。⚠️ `create: true` 只在 `prog` / `class` / `incl` / `intf` 上验证过；7.52 上对 `func` 的 PUT 恒回 **405 `ResourceMethodNotSupported`**（该路由不存在，重试无意义）——请用 `POST /api/objects/func/{名称}/create`，见下方 7.52 小节。
- 写入需 ADT 已启用；写入失败也会尽力 UNLOCK，不留孤儿锁。
- **replace 匹配容忍阶梯**（依次）：精确 → CRLF/LF 归一 → 末尾 `\n` 修剪（末行锚点）→ **大小写不敏感唯一匹配**。SAP 存储源码原文，而部分读取路径曾返回大写化视图——兜底吸收这个漂移；大小写不敏感多命中仍按命中数报错。
- **读回保留原文大小写**：`/api/programs/{name}/source` 现以 `WITH_LOWERCASE` 请求——读到即存到，从读回内容取的锚点必然匹配。`GET /api/objects/{type}/{name}/source`（ADT 通道）是新类型与函数模块的规范读入口（SEDI 形态，与写入口径一致）。
- **只读部署**：部署者可能以 `SAP_READ_ONLY=1` 运行网关——写端点（`PUT .../source`、`POST .../replace`、`POST .../create`、`DELETE`、`/api/adt` 非读方法）此时返回 403 `READ_ONLY`。这是刻意为之：不要重试写操作，改为只读操作与 `POST .../syntax`（仍可用，不落库）。`POST /api/rfc` 不受该开关影响。

### 写后激活检查清单（必做）

Agent 写 ABAP 最常见的静默失败：写入返回 200、没人读 `activated.success`、对象停留在非激活版本——下一次测试/运行就报出难懂的「inactive version」错误。每次源代码写入（`PUT .../source`、`POST .../replace`、`POST .../create` 带 `source`）之后：

1. **读写响应里的 `activated.success`**——HTTP 200 不代表激活成功。`success:false` → 读 `problems[]` / `activated.messages[]`，改源码，重试写入。
2. **不要走裸 `/api/adt/**` PUT 写源代码**——用 `/api/objects/{type}/{name}/source`（同一请求内自动激活，`activate` 默认 true）。裸 ADT 写入只留给结构编辑器对象（doma/dtel/ttyp，见下方小节）——且其后必须跟激活 POST。
3. **不确定上次写入激活了没有？**继续之前先验证：`GET /api/objects/{type}/{name}/meta` → `active` 必须为 `true`（§10；这正是「我的激活到底成没成」的读侧答案）。
4. **会话收尾清扫**：列出非激活对象，只把自己写入留下的遗留补激活——不要批量激活别人未提交的对象：

```bash
curl http://127.0.0.1:3000/api/adt/activation/inactiveobjects

curl -X POST "http://127.0.0.1:3000/api/adt/activation?method=activate&preauditRequested=true" \
  -H "Content-Type: application/xml" -H "Accept: */*" \
  -d '<?xml version="1.0" encoding="UTF-8"?><adtcore:objectReferences xmlns:adtcore="http://www.sap.com/adt/core"><adtcore:objectReference adtcore:uri="/sap/bc/adt/oo/classes/zcl_mine" adtcore:name="ZCL_MINE"/></adtcore:objectReferences>'
```

### 10. 包导出与对象元数据动词（abapGit 吸收，v0.21）

**包级快照**——把一个包（含子包）序列化为 abapGit 风格文件树，zip 交付：agent 可直接提交 git、做 SAP↔本地 diff、做备份：

```bash
# 先看清单（JSON manifest，不读源码，快）
curl 'http://127.0.0.1:3000/api/export/package/ZAGT_TEST?manifest=true'
# → {"format":"sap-for-agents-export/1","folder_logic":"full","objects":[{type,name,package,path,status,author,...}],"packages":[...]}

# 完整导出（zip：.abapgit.xml + export_manifest.json + src/<包>/ 文件树）
curl 'http://127.0.0.1:3000/api/export/package/ZAGT_TEST' -o zagt_test.zip
# → application/zip；树内：src/<包>/{名}.{prog|incl|clas|intf|func}.abap、{名}.{tabl|stru|cds}.ddl、package.devc.xml

# 递归默认开（TDEVC 子包 BFS，深度 10）；控制范围：
curl 'http://127.0.0.1:3000/api/export/package/ZFOO?recurse=false&max_objects=200'
```

- **文件命名沿用 abapGit 约定**（filename logic + FULL 目录逻辑、小写、命名空间 `/NS/` → `#ns#`）：源码文件对 abapGit **源码兼容**。元数据 XML（TPOOL/SOTR/VSEOCLASS）本版**不生成**——树**尚不可被 abapGit 导入**（导入是二期计划；zip 同时带网关自有的 `export_manifest.json`，逐对象列出作者/状态/跳过原因——缺口可见，绝不静默丢弃）。
- **覆盖范围**：R3TR PROG（include 经 `REPOSRC.SUBC='I'` 与程序分流）、CLAS、INTF、FUGR（经仓库环境服务枚举**全量组 include**——TOP/UXX/U01-99/自定义；文件按 include 名命名：FM↔include 映射在 `ENLFDIR` 里，而它是表池表、`RFC_READ_TABLE` 拒读）、DDLS、TABL（结构经 `DD02L.TABCLASS='INTAB'` 分流）、DEVC（每包一个 `package.devc.xml`）。其余 R3TR 类型在清单里记 `status:"skipped"`。
- 单对象源码读取失败**不会**让导出整体失败——清单条目带 `error`，其余照常交付。
- GET 是读操作：`read_only` 部署可用，继承鉴权/限流/作用域（`/api/s/{system}/export/package/...`）。包不存在 → `404 PACKAGE_NOT_FOUND`；超过 `max_objects`（默认 500，上限 2000）→ `400 EXPORT_TOO_LARGE`。
- zip 根带最小可识别的 `.abapgit.xml`（`FOLDER_LOGIC=FULL`）——仅为前向兼容，不是可导入。

**对象元数据动词**——写前探测，一次往返拿全：

```bash
curl 'http://127.0.0.1:3000/api/objects/prog/ZFOO/meta'
# → {"type":"prog","name":"ZFOO","exists":true,"locked":false,"lock_mode":"adt_lock",
#     "active":true,"active_check":"adt","changed_by":"DEVELOPER","changed_by_via":"adt",
#     "author":"DEVELOPER","author_via":"tadir",...}
```

- `exists`——ADT base GET 探测；`false` 时短路（其余字段为空语义，不报错）。
- `locked`——非破坏性探测（LOCK 成功立刻 UNLOCK，会话随即丢弃）：`false` = 当前可写；`true` = 他人持锁（`lock_message` 带 SAP 一手原因，EU510）。无编辑锁概念的对象返回 `null`：tabl/stru（`lock_mode:"etag"`——走 etag 乐观并发）与 package（`lock_mode:"none"`）。
- `active`——`true` 表示无待激活的未激活版本。主判据是 ADT 对象节点自身（`adtcore:version`，`active_check:"adt"`——与存在性同一次 GET，A4H 实证语义：写后不激活 → `"inactive"`）；属性缺失时按类型回退 REPOSRC（`active_check:"reposrc"`，仅 prog/incl/fugr）；其余返回 `null` + `active_check:"unsupported_type"`。这是"我上次激活是否失败了"的读侧答案（写响应本就带 `activated.success`）。
- `changed_by`/`changed_on`——最后修改者来自 ADT 对象节点（`adtcore:changedBy/changedAt`，全类型可用——包括 REPOSRC 映射在表池表里读不到的 func）；属性缺失时按类型回退 REPOSRC（prog/incl/fugr）。`author`/`created_on`——TADIR 创建信息（全类型可用）。`*_via` 字段标注来源通道（`adt` / `reposrc` / `tadir` / `none`）。
- func：传 `?group=` 可跳过组反解；反解失败按 `exists:false` 应答（FM 找不到），不报错。

### 11. 包导入（abapGit 反序列化模型移植，v0.21）

把导出的 zip 装回系统——第 10 节的逆向。网关把 abapGit 反序列化的四个核心机制移植到自己的写管线上：

```bash
# 先预检（零写入）：解析 + 探测存在性 + 输出计划
curl -X POST 'http://127.0.0.1:3000/api/import/package/ZRESTORE?dry_run=true' \
  -H "Content-Type: application/zip" --data-binary @zagt_test.zip
# → {"format":"sap-for-agents-import/1","dry_run":true,"objects":[{type,name,package,exists,action}],"packages":[...],"skipped_unsupported":[...]}

# 真导入（缺包先建、全部以未激活态写入、再按序激活）
curl -X POST 'http://127.0.0.1:3000/api/import/package/ZRESTORE' \
  -H "Content-Type: application/zip" --data-binary @zagt_test.zip
# → objects:[{action:"created|updated|skipped_exists|skipped_unchanged|failed",
#     activated:"activated|activation_failed|retried|inactive", problems:[...], real_package?}], summary:{...}

# 全部保持未激活，留人工审查 / 分阶段激活
curl -X POST '.../api/import/package/ZRESTORE?activate=false' --data-binary @zagt_test.zip
```

- **输入格式**：网关自有导出 zip（`sap-for-agents-export/1`，第 10 节）——round-trip 保证。任意 abapGit 仓库 XML/AFF 导入（VSEOCLASS/TPOOL/SOTR 元数据通道）是**三期**，不在本端点。
- **包重映射（PREFIX 式）**：manifest 根包 → 路径里的 `{devclass}`；子包 → `{devclass}_{叶名}`（对象名不变）。缺失的包先建（父先于子）。manifest 的 `folder_logic` 必须为 FULL（网关导出恒为 FULL）。
- **依赖序创建**（abapGit korrnum/依赖图模型的类型级简化）：packages → FUGR 壳 → tabl/stru → cds → intf → class → prog → incl → 组内 include。单对象失败不中止整批——结果清单逐对象带错误。
- **两阶段激活**（abapGit"全部建完再批量激活"语义，适配逐对象 ADT 激活）：全部写入 `activate:false`；写完后按 ddic → cds → intf → class → prog → incl → 组内 include 的顺序激活，**失败者还有一轮重试**——同批内的前向引用自愈。`?activate=false` 保持全部未激活，便于分阶段审查。
- **覆盖语义**：`?overwrite=true`（默认）对已存在对象原地覆盖写；`false` 跳过（`action:"skipped_exists"`）。⚠️ **SAP 对象名全局唯一**：同名对象还在包 A 里时往包 B 导入，实际是原地更新 A 的对象（不做包搬移）——结果里的 `real_package` 字段如实回显每个被更新对象的实际所在包。快照→恢复流程请先删原对象（此时 dry_run 会显示全部 `action:"created"`）。
- **dry_run**（`?dry_run=true`）是真预检：零写入（read_only 守卫也豁免它），逐对象报告 exists/action 和将创建的包。
- read_only 部署拦导入（`403 READ_ONLY`，dry_run 除外）；runtime 模式全拦；请求体上限 32 MB。

### 函数组与函数模块（7.52 实测要点）

在 SAP_BASIS 7.52 / EHP8 上验证——当上面那套「纯 ADT」的路子走不通时，实际可行的路径：

- **`func` 无法经「写源码」创建。** `PUT /api/objects/func/{n}/source` 带 `"create": true` 每次都是 405 `ResourceMethodNotSupported`（该 release 的对象级 source 路由不接受 PUT）。用 `POST /api/objects/func/{n}/create`，body 带 `group` + `devclass` + `transport` + `rfc_enabled` + `source`，一次完成建壳 + 写源 + 激活 + 置 RFC。
- **先建 `fugr`，并且必须带 `devclass` 和 `transport`。** 这个 release 上 `POST /api/objects/fugr/{g}/create` 的 ADT 调用会失败，网关回落 RPY 插入——而这条路径才是对的：RPY 会置 `TRDIR-UCCHECK = 'X'`。组若存在但该标记为空，组内每个函数激活都报「程序不与 Unicode 兼容」。建完组请查 `TRDIR-UCCHECK`，为空就 `DELETE` 掉重建，让 RPY 路径跑一遍。
- **这样建出来的模块在 `TADIR` 里没有 `FUNC` 行**——`OBJECT='FUNC' AND DEVCLASS=<包>` 查不到任何记录，连存在多年的模块也一样。这是常态、不是登记失败：abapGit 按 `ENLFDIR` + 组成员序列化函数组，照常能推能拉。别去手工补 `TADIR`。
- **新建模块立即可调；参数清单变更也不需要重启。** 新函数名是 SDK 描述符缓存的未命中、现场从 DDIC 拉取（A4H 实测）；经网关做的签名变更在写入时就移除了旧 SDK 缓存条目（`RfcRemoveFunctionDesc`）。只有**网关之外**（SE37）改签名且该函数已被调用过才会陈旧——`POST /api/admin/reload` 即可修复；无论哪种情况 `GET /api/functions/{n}` 都实时读接口。
- **源码必须是完整的 SEDI 形态**：`FUNCTION z_x.` 要**带句号**，结尾要有 `ENDFUNCTION.`。漏句号时扫描器会把后面第一条语句吞进 FUNCTION 语句；漏 `ENDFUNCTION.` 则报 FUNCTION 未关闭。
- **签名注释块里的 `TABLES x STRUCTURE y` 会踩 7.52 的保存扫描**（`Parameter x declares no type`），改成 `TYPE <表类型>` 又因为 TABLES 形参需要表类型被拒。请写 ADT 规范形态——签名内联 `TABLES OPTIONS LIKE /SAPDS/RFC_DB_OPT`，与 `GET /api/adt/functions/groups/{g}/fmodules/{fm}/source/main` 返回的完全一致；这个形态一次保存激活成功，且 RFC 侧契约不变。
- 函数组的成员 include 不能经 `programs/incl` 资源写（SAP 报「这一语法不能用于对象名称」）——必须走 `func` 资源。

### 结构化 DDIC 类型（doma / dtel / ttyp）——经裸代理创建

域、数据元素、表类型是**结构编辑器对象**（无文本源码），上面的 `/api/objects` 管线不覆盖它们。但可以经裸 ADT 代理创建，哲学与一切相同——**创建文档就是对象文档本身**：POST 到集合 → 激活 → （无锁）DELETE。以下模板全部在 ABAP Platform 816 上端到端实证（创建 → 激活 → `version="active"` → ABAP 可用 → 删除）。

```bash
# 1) 创建——把对象文档 POST 到集合
curl -X POST http://127.0.0.1:3000/api/adt/ddic/domains \
  -H "Content-Type: application/*" -H "Accept: */*" -d @domain.xml

# 2) 激活——与所有对象同一个激活服务
curl -X POST "http://127.0.0.1:3000/api/adt/activation?method=activate&preauditRequested=true" \
  -H "Content-Type: application/xml" -H "Accept: */*" \
  -d '<?xml version="1.0" encoding="UTF-8"?><adtcore:objectReferences xmlns:adtcore="http://www.sap.com/adt/core"><adtcore:objectReference adtcore:uri="/sap/bc/adt/ddic/domains/zmy_dom" adtcore:name="ZMY_DOM"/></adtcore:objectReferences>'

# 3) 删除（无锁，同 tabl/stru）
curl -X DELETE http://127.0.0.1:3000/api/adt/ddic/domains/zmy_dom
```

**域**（`ddic/domains`；CHAR(10)）——根元素 `doma:domain`，content 承载类型定义：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<doma:domain xmlns:doma="http://www.sap.com/dictionary/domain" xmlns:adtcore="http://www.sap.com/adt/core"
  adtcore:description="my domain" adtcore:name="ZMY_DOM" adtcore:type="DOMA/DD">
  <adtcore:packageRef adtcore:name="$TMP"/>
  <doma:content>
    <doma:typeInformation><doma:datatype>CHAR</doma:datatype><doma:length>000010</doma:length><doma:decimals>000000</doma:decimals></doma:typeInformation>
    <doma:outputInformation><doma:length>000010</doma:length><doma:style>00</doma:style><doma:conversionExit/><doma:signExists>false</doma:signExists><doma:lowercase>false</doma:lowercase><doma:ampmFormat>false</doma:ampmFormat></doma:outputInformation>
    <doma:valueInformation><doma:valueTableRef/><doma:appendExists>false</doma:appendExists><doma:fixValues/></doma:valueInformation>
  </doma:content>
</doma:domain>
```

**数据元素**（`ddic/dataelements`；引用域）——根是 `blue:wbobj`（命名空间与域不同！），子元素 `dtel:dataElement`；四组字段标签（short/medium/long/heading，各需 Label + Length + MaxLength）**全部必填**：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<blue:wbobj xmlns:blue="http://www.sap.com/wbobj/dictionary/dtel" xmlns:adtcore="http://www.sap.com/adt/core"
  xmlns:dtel="http://www.sap.com/adt/dictionary/dataelements"
  adtcore:description="my element" adtcore:name="ZMY_ELEM" adtcore:type="DTEL/DE">
  <adtcore:packageRef adtcore:name="$TMP"/>
  <dtel:dataElement>
    <dtel:typeKind>domain</dtel:typeKind><dtel:typeName>ZMY_DOM</dtel:typeName>
    <dtel:dataType>CHAR</dtel:dataType><dtel:dataTypeLength>000010</dtel:dataTypeLength><dtel:dataTypeDecimals>000000</dtel:dataTypeDecimals>
    <dtel:shortFieldLabel>Id</dtel:shortFieldLabel><dtel:shortFieldLength>10</dtel:shortFieldLength><dtel:shortFieldMaxLength>10</dtel:shortFieldMaxLength>
    <dtel:mediumFieldLabel>My Element</dtel:mediumFieldLabel><dtel:mediumFieldLength>15</dtel:mediumFieldLength><dtel:mediumFieldMaxLength>20</dtel:mediumFieldMaxLength>
    <dtel:longFieldLabel>My Element</dtel:longFieldLabel><dtel:longFieldLength>20</dtel:longFieldLength><dtel:longFieldMaxLength>40</dtel:longFieldMaxLength>
    <dtel:headingFieldLabel>My Element</dtel:headingFieldLabel><dtel:headingFieldLength>25</dtel:headingFieldLength><dtel:headingFieldMaxLength>55</dtel:headingFieldMaxLength>
    <dtel:searchHelp/><dtel:searchHelpParameter/><dtel:setGetParameter/><dtel:defaultComponentName/>
    <dtel:deactivateInputHistory>false</dtel:deactivateInputHistory><dtel:changeDocument>false</dtel:changeDocument>
    <dtel:leftToRightDirection>false</dtel:leftToRightDirection><dtel:deactivateBIDIFiltering>false</dtel:deactivateBIDIFiltering>
  </dtel:dataElement>
</blue:wbobj>
```

**表类型**（`ddic/tabletypes`；数据元素的标准表）——`rowType` 与 `primaryKey` 的子元素是全量必填（`builtInType`、`kind`……）：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<ttyp:tableType xmlns:ttyp="http://www.sap.com/dictionary/tabletype" xmlns:adtcore="http://www.sap.com/adt/core"
  adtcore:description="my table type" adtcore:name="ZMY_TT" adtcore:type="TTYP/DA">
  <adtcore:packageRef adtcore:name="$TMP"/>
  <ttyp:rowType>
    <ttyp:typeKind>dataElement</ttyp:typeKind><ttyp:typeName>ZMY_ELEM</ttyp:typeName>
    <ttyp:builtInType><ttyp:dataType>CHAR</ttyp:dataType><ttyp:length>000010</ttyp:length><ttyp:decimals>000000</ttyp:decimals></ttyp:builtInType>
    <ttyp:rangeType/>
  </ttyp:rowType>
  <ttyp:initialRowCount>00000</ttyp:initialRowCount>
  <ttyp:accessType>standard</ttyp:accessType>
  <ttyp:primaryKey ttyp:isVisible="true" ttyp:isEditable="true"><ttyp:definition>standard</ttyp:definition><ttyp:kind>nonUnique</ttyp:kind><ttyp:components ttyp:isVisible="false"/><ttyp:alias/></ttyp:primaryKey>
  <ttyp:secondaryKeys ttyp:isVisible="true" ttyp:isEditable="true"><ttyp:allowed>notSpecified</ttyp:allowed></ttyp:secondaryKeys>
</ttyp:tableType>
```

要点：

- 被拒的文档会**点名缺失项**（"System expected the element '…'"）——补上重试，两三轮即收敛。
- 修改已有对象：`GET` 文档 → 改 XML → `PUT` 回去（etag 约定与 tabl/stru 相同）。
- 想学任何对象的完整形态：`GET /api/adt/ddic/<集合>/<已有名>`，照抄活对象（如 `domains/char10`、`dataelements/mtext_d`、`tabletypes/string_table`）。
- **经典 SE11 视图在此无路可走**：`RPY_VIEW_INSERT` 弹窗（`DYNPRO_SEND_IN_BACKGROUND`，后台不可用），ADT 的 `ddic/views` 只管**外部 HANA 视图**。用 CDS 视图实体（`POST /api/objects/cds/{name}/create`）替代——那是现代正解且已完整支持。

### 12. Agent 运行面（sapcli 借鉴，v0.21）

六个端点移植自 [jfilak/sapcli](https://github.com/jfilak/sapcli)（pyrfc 作者的 CLI——README 原话："coding agents love `sapcli abap run`"）。全部走 ADT 通道：自动继承 `adt` 能力开关、多系统作用域形式（`/api/s/{system}/...`）与鉴权。`POST /api/table/osql` 与 `POST /api/table/read` 同为只读姿态；执行端点（`aunit`/`atc`）与 `/api/rfc` 同哲学——SAP 端授权才是边界；`abap/run` 与 CTS 写操作被 `read_only` 拦截。

```bash
# ① 执行 ABAP 片段：临时 if_oo_adt_classrun 类 → 激活 → 执行 → 删除
curl -X POST http://127.0.0.1:3000/api/abap/run -H "Content-Type: application/json" \
  -d '{"source": "out->write( |Hello, { sy-uname }!| )."}'
# → {"ok":true,"output":"Hello, DEVELOPER!","class":"ZCL_SFA_RUN_DEVELOPER_XY","duration_ms":1820,"deleted":true}
#   业务失败 = HTTP 200 + ok:false + step（"syntax_check"|"activate"|"execute"）+ issues/problems。
#   包默认 $TMP；类名 <prefix>_<user>_<random> 预算 30 字符。

# ② 跑 ABAP Unit 测试（异步三步：建单 → 轮询 → 取结果）
curl -X POST http://127.0.0.1:3000/api/tests/aunit -H "Content-Type: application/json" \
  -d '{"objects":[{"type":"class","name":"ZCL_MINE"}],"format":"json"}'
# → {"run_id":"…","summary":{"tests":5,"alerts":1,…},"programs":[…],"alerts":[…]}
#   format: "json"（解析）| "junit4" | "raw"（结果 XML 原样透传——CI 直连）。
#   objects[].type: class|program|interface|fugr|func|package（package 不与普通类型混用）。

# ③ 跑 ATC 检查（写码后的质量门禁）
curl -X POST http://127.0.0.1:3000/api/atc/run -H "Content-Type: application/json" \
  -d '{"objects":[{"type":"class","name":"ZCL_MINE"}]}'
# → {"variant":"ZABAP_CLOUD_DEVELOPMENT","worklist_id":"…","objects":[{name,findings:[…]}],
#    "total_findings":3,"by_priority":{"1":3}}
#   variant 缺省 → 读 atc/customizing（systemCheckVariant）。小对象实测约 60s。

# ④ CTS 传输请求：创建 / 读取 / 释放 / 删除
curl -X POST http://127.0.0.1:3000/api/cts/requests -H "Content-Type: application/json" \
  -d '{"type":"workbench","description":"my change"}'
# → {"number":"A4HK900158","status":"D",…}
curl http://127.0.0.1:3000/api/cts/requests/A4HK900158          # 含 tasks 与已锁对象
curl -X POST http://127.0.0.1:3000/api/cts/requests/A4HK900158/release?recursive=true
# → {"released":true}   业务失败仍是 200 + released:false（如检查报告错误）
curl -X DELETE http://127.0.0.1:3000/api/cts/requests/A4HK900158?recursive=true
#   ⚠️ 已释放的请求 SAP 拒绝删除；type ∈ workbench|customizing|transport-of-copies|
#   development-correction|repair。read_only 拦 create/release/delete。

# ⑤ 全对象类型快搜（类/表/CDS/…）——补 POST /api/functions/search（只搜函数模块）
curl 'http://127.0.0.1:3000/api/objects/search?q=BAPIRET'
# → {"objects":[{"name":"BAPIRET2","typ":"TABL/DS","package":"SBF_BAPI","uri":"/sap/bc/adt/ddic/structures/bapiret2"}]}
#    uri 可直接喂 ANY /api/adt/{path}。词尾自动补 *。

# ⑥ OpenSQL SELECT 走 ADT 数据预览——读 RFC_READ_TABLE 拒读的池表与 JOIN
#   （ENLFDIR 等），无 512 字节行宽限制
curl -X POST http://127.0.0.1:3000/api/table/osql -H "Content-Type: application/json" \
  -d '{"sql":"select mandt, cccategory from t000","rows":10}'
# → {"columns":[…],"rows":[{…}],"row_count":3,"total_rows":3}   （值都是字符串）
#    只接受 SELECT/WITH；写操作请走 BAPI/RFC/对象端点。
```

实现要点：
- **abap run 在全新 ADT 会话里执行**（classrun 前**强制刷新** CSRF token + cookie）——classrun 服务按 ABAP 会话缓存类元数据，同会话跑刚激活的类会得到难懂的 *"Class does not implement if_oo_adt_classrun~main method!"*（sapcli 用 `connection.new_session()` 规避的是同一个坑）。临时类在 `finally` 里无条件删除——执行失败/超时也不留尸体。
- **aunit 用 1s 短轮询**，不用 `withLongPolling=true`（长轮询可能撞网关的客户端 HTTP 超时）。整体期限：`timeout_secs`（默认 120）。
- 典型闭环：写码（`POST /api/objects/...`）→ `POST /api/tests/aunit` → `POST /api/atc/run` → 迭代；要挂传输就 `POST /api/cts/requests` + objects 写入的 `transport` 字段。

### 13. SAP 侧登录方式（v0.21）

鉴权收敛到每系统的 **`auth` 块**——主通道互斥四选一（basic / sso2 / plugin / oauth，装载期强制「恰一」）+ 可选 `snc` 叠加。密码/票据支持 `${VAR}` 插值；同一块形态同时驱动 RFC 连接与（经继承的）ADT 通道。

```toml
[systems.prd]
ashost = "…"
sysnr = "00"
client = "001"

# 主通道——恰选其一：
[systems.prd.auth.basic]
user = "DEVELOPER"
passwd = "${SAP_PASSWD}"

# [systems.prd.auth.sso2]
# ticket = "${SAP_SSO2_TICKET}"          # SAP 登录票据（身份在票据里）

# [systems.prd.auth.plugin]              # 外部插件（仅 ADT 通道）
# command = "/usr/local/bin/sap-auth-helper"   # 强制绝对路径
# params = { tenant = "a4h" }
# cache_secs = 300

# [systems.prd.auth.oauth]               # BTP/ABAP Cloud——系统级使用会被拒，
# token_url = "https://…/oauth/token"    # 请放到 adt.auth（见下）
# client_id = "sb-…"
# client_secret = "${SAP_BTP_SECRET}"

# 可选 SNC 叠加层（与任一主通道组合）：
[systems.prd.auth.snc]
qop = "8"
partnername = "p:CN=A4H"
lib = "gsskrb5"          # myname 可选
```

ADT 通道有独立的 **`adt` 块**——`base_url`（缺省 `http://<ashost>:50000`，空串 = 禁用）、`tls`（mTLS：`cert`/`key`/`ca`；加密私钥拒收并给解密提示）、`auth` **覆盖**（缺省 = 继承系统级 auth；给出 = 整体替换）：

```toml
[systems.prd.adt]
base_url = "https://host:44300"

[systems.prd.adt.tls]
cert = "/etc/sap/client.pem"    # key 可选（合并单文件）；ca 可选

# 覆盖示例——生产 ADT 走独立账号：
[systems.prd.adt.auth.basic]
user = "adt_account"
passwd = "${SAP_PRD_ADT_PASSWD}"

# BTP/ABAP Cloud（那边拒收 Basic；BTP 无 RFC——oauth 系统只服务 /api/adt/**）：
[systems.btp.adt.auth.oauth]
token_url = "https://acme.authentication.eu10.hana.ondemand.com/oauth/token"
client_id = "sb-…"
client_secret = "${SAP_BTP_SECRET}"
# grant = "authorization_code"            # 可选（缺省 client_credentials）；
# user = "you@example.com"                # 可选——两个一起配走 password grant
# passwd = "${SAP_BTP_PASSWD}"
```

**BTP trial / `binding-secret` 绑定（v0.23）**：broker 客户端只放 `client_credentials`，而 trial 的 ABAP 系统**拒收**这种无用户身份的 token（401 "Anmeldung fehlgeschlagen"）。两条可行路径，选一条：

- **password grant**（最简——sapcli 同款）：XSUAA 客户端放行 password grant 的话，在 oauth 块加 `user` + `passwd`（BTP 登录账号）即可直取带用户身份的 token，refresh token 自动复用。先探针：dummy 密码报 *"User authentication failed"*（而非 *"unauthorized_client"*）= grant 放行。
- **浏览器授权**（通吃）：oauth 块加 `grant = "authorization_code"`，然后跑一次授权流：
  ```bash
  # 1) 起流 → 拿 authorize URL
  curl -X POST http://127.0.0.1:3000/api/setup/system/btp/authorize
  #    → 202 {"authorize_url": "https://<tenant>.authentication.../oauth/authorize?…",
  #           "redirect_uri": "http://localhost:3000/oauth/callback", "state": "…", "expires_in_secs": 600}
  # 2) 浏览器打开 authorize_url 用 BTP 账号登录——回调落在本机网关，
  #    refresh token 落盘 + 热注入。
  #    网关在远程主机（回调不可达）？从浏览器地址栏复制 ?code= 手工补完：
  curl -X POST http://127.0.0.1:3000/api/setup/system/btp/authorize \
    -H "Content-Type: application/json" -d '{"code":"<粘贴>"}'
  # 3) GET /api/setup/system/btp → "oauth": {"grant":"authorization_code","authorized":true,"user":"…"}
  ```
  - refresh token 存 `sap_oauth_state.json`（`SAP_OAUTH_STATE_FILE`，0600），重启不丢；配置台对这类系统显示 **Authorize** 按钮。
  - refresh 失败**快速报错**（`ADT_OAUTH_REAUTH`）——绝不静默回落到 ABAP 必拒收的无用户 `client_credentials` token；重新跑授权流即可。
  - redirect 缺省 `http://localhost:<port>/oauth/callback`（XSUAA 未配 `redirect-uris` 时只放行 `localhost`），所以**浏览器必须与网关同机**；authorize 调用支持 `redirect_uri` 覆盖。

**ADT-only 系统（v0.23）**：无 RFC 坐标（`ashost`/`conn` 全省）+ `adt` 块带 `base_url` + `auth` 的系统即 ADT-only——BTP/ABAP Cloud 形态。不需要占位的系统级 `auth`：

```toml
[systems.btp]
# 不写 ashost / sysnr / conn——BTP 没有 RFC

[systems.btp.adt]
base_url = "https://1bc3fa3b-….abap.ap21.hana.ondemand.com"

[systems.btp.adt.auth.oauth]
token_url = "https://<tenant>.authentication.ap21.hana.ondemand.com/oauth/token"
client_id = "sb-…"
client_secret = "${SAP_BTP_SECRET}"
grant = "authorization_code"        # 配好后跑一次授权流（见上）
```

- `/api/systems` 回显 `auth: "adt-only"`；就绪探针（`/ready`、配置台「测试」、60s 探测循环）改打 **ADT**（轻量 discovery GET）而不是 `RFC_PING`——卡片显示真实健康度，而不是永远的「不可达」。
- RFC 侧调用（`/api/rfc`、`/api/functions/**`、`/api/table/**`…）在入口被干净拒绝：**400 `RFC_NOT_AVAILABLE`**，而不是对着不存在的 RFC 端口超时。该系统的 API 面是 `/api/s/{name}/adt/**`（与 `/api/adt/**` 同构）以及 `/api/s/{name}/dumps/**`、`/api/s/{name}/tests/**`、`/api/s/{name}/atc/**` 这些 ADT 派生端点。

要点：
- `GET /api/systems` 回显每系统 `auth`：`basic` | `sso2` | `plugin` | `oauth`（可 +`+snc`）。
- `conn` 透传在装载期**拒收鉴权键**（`USER`/`PASSWD`/`MYSAPSSO2`/`SNC_*`）——鉴权只认 auth 块（无双重表示）。
- OAuth token 缓存 + refresh_token 复用在进程内存；401 → 强制重新 grant 一次。插件响应缓存 `cache_secs`；401 → 失效重跑一次。
- `.env` 单系统路径不变（`SAP_USER`/`SAP_PASSWD` 映射为 basic；`SAP_ADT_USER`/`SAP_ADT_PASSWD` 仅在与连接凭据不同时生效）。
- 启动期卫生：world-readable 配置含明文凭据（auth 下非 `${VAR}` 值）打 WARN 建议 `chmod 600` / `${VAR}` 形态。
- ⚠️ **配置破坏性变更（v0.21）**：平铺 `user`/`passwd`/`mysapsso2`/`snc_*`/`adt_user`/`adt_passwd`/`adt_cert`… 字段全部移除——迁移到 `auth.*` / `adt.*` 块（形态见上）。

### 14. ABAP 调试器（v0.22）：断点 / 单步 / 变量

经网关自有有状态 ADT 通道做远程 ABAP 调试——不需要 Eclipse。调试面移植自 [oisee/vibing-steampunk](https://github.com/oisee/vibing-steampunk)（MIT）端到端验证过的 ADT 契约（契约笔记见 `docs/DEBUGGER-CONTRACT.zh-CN.md`）。流程：**设断点 → 建会话（listen）→ 触发同用户代码 → 轮询 → 检视 → 步进/放行 → 关会话**。

| 目标 | 端点 |
|------|------|
| 设断点（无需会话；整集合替换语义） | `POST /api/debug/breakpoints` |
| 列断点 | `GET /api/debug/breakpoints` |
| 删一个 / 清全部 | `DELETE /api/debug/breakpoints/{id}` · `DELETE /api/debug/breakpoints?all=true` |
| 建会话（listen 在服务端阻塞；agent 轮询） | `POST /api/debug/sessions` |
| 轮询状态 | `GET /api/debug/sessions/{sid}` |
| 调用栈 | `GET /api/debug/sessions/{sid}/stack` |
| 变量（自动下探 `@ROOT`→`@LOCALS`；`?parent=ID` 展开结构/内表；`?frame=N` 切栈帧读调用者变量） | `GET /api/debug/sessions/{sid}/variables` |
| **写变量**（只读系统 403） | `PUT /api/debug/sessions/{sid}/variables/{name}` |
| 步进/放行（`step_into`/`step_over`/`step_out`/`resume`） | `POST /api/debug/sessions/{sid}/control` |
| 关会话（detach + 摘 listener + 清断点） | `DELETE /api/debug/sessions/{sid}` |

```bash
# 1) 先设断点——targets: kind=prog/class/fm(+group) 或 kind=uri；name 自动小写化
#    （断点 URI-mapping 大小写敏感）
curl -X POST http://127.0.0.1:3000/api/debug/breakpoints -H "Content-Type: application/json" \
  -d '{"targets":[{"kind":"class","name":"ZCL_MINE","line":42}]}'

# 2) 建会话：服务端阻塞等 ADT listener（listen_secs 同时是 debuggee 的等待窗）；
#    agent 轮询 GET /api/debug/sessions/{sid}
curl -X POST http://127.0.0.1:3000/api/debug/sessions -H "Content-Type: application/json" \
  -d '{"listen_secs":60}'
# → 202 {"sid":"dbg-…","state":"listening"}

# 3) 建会话后等 ~1s 再触发同用户代码（listener 服务端登记需要一拍——
#    登记完成前撞断点的 debuggee 会被直接跳过）：
#    /api/abap/run（每次强制新会话——最理想）或 /api/rfc

# 4) 轮询 → state=stopped → 栈/变量 → 步进/放行 → 关会话
curl http://127.0.0.1:3000/api/debug/sessions/{sid}            # {"state":"stopped","position":{...}}
curl http://127.0.0.1:3000/api/debug/sessions/{sid}/stack
curl http://127.0.0.1:3000/api/debug/sessions/{sid}/variables   # LV_X=1 → PUT 99 → 步过 → 100
curl -X PUT http://127.0.0.1:3000/api/debug/sessions/{sid}/variables/LV_X \
  -H "Content-Type: application/json" -d '{"value":"99"}'
curl -X POST http://127.0.0.1:3000/api/debug/sessions/{sid}/control \
  -H "Content-Type: application/json" -d '{"action":"step_over"}'   # 或 "resume"
curl -X DELETE http://127.0.0.1:3000/api/debug/sessions/{sid}
```

关键行为（全部在 A4H/816 实测，见 `docs/DEBUGGER-CONTRACT.zh-CN.md`）：

- **调试器与 debuggee 分属两会话**——用 `/api/abap/run` 触发最理想（它每次强制新会话）。状态轮询纯读内存：零 SAP 往返。
- **`DELETE /api/debug/sessions/{sid}` 会结束该网关用户的外部调试并连带清掉该用户全部外部断点**——这是 SAP 语义，不是可选项。它同时是清场手段：残留监听/断点用 `DELETE /api/debug/breakpoints?all=true` 或关会话清掉。
- **外部断点没有监听会话时直接跳过，不会挂死触发方**。停住的 debuggee 等待 ≈ `listen_secs` 后自行放行。
- **每系统至多一个会话**（调试独占工作进程）：第二个 `POST` → `409 DEBUG_SESSION_EXISTS`。闲置超 30 分钟自动拆除。
- **resume 以 `ended: true` 收尾**（`debuggeeEnded`）——这是正常终局，不是错误。
- **read_only 分级**：观察类（listen/栈/变量读/步进）在只读系统可用；仅变量写 403 READ_ONLY。
- `system_debug: true`（**bp_set 的 body 参数**，不是会话参数）才能让 SAP 标准码断点生效（否则被接受但永不触发）。无语句的行会进 `rejected[]` 并带 SAP 原因。
- **优雅停机会善后**：SIGTERM/排空时网关在退出前 ~20s 内 teardown 全部调试会话（detach + 摘 listener + 清断点）——残留 listener 行会静默吞掉该用户的下一个 debuggee。
- 能力开关：`GET /api/version` → `capabilities.debug`。多系统作用域照常（`/api/s/{system}/debug/**`）；作用域形式的关会话仍校验 sid 归属。
- MCP 镜像工具已随版发布：`debug_manage` + `debug_inspect`（`tools/list` 可见）。

## 关键约束（避坑）

1. **参数名必须大写**：SAP 参数名区分大小写，JSON 里永远用大写（如 `USERNAME` 不是 `username`）。
2. **先查接口再调用**：参数名/类型不要猜，先用端点 2 查准。
3. **CHAR 类型传字符串，INT 传数字**：`{"REQUTEXT":"hi"}`、`{"MAX_ROWS":100}`。
4. **BCD/INT8/二进制**用显式类型标记：`{"type":"BCD","value":"123.45"}`、`{"type":"BYTES","value":"<base64>"}`。
5. **BAPI 要显式提交事务**：写操作的 BAPI（CREATE/UPDATE/DELETE）成功后需调 `BAPI_TRANSACTION_COMMIT`，否则改动不生效。
6. **错误看 RETURN**：BAPI 通常不报 HTTP 错，而是返回 `RETURN` 表里带 `TYPE=E`（错误）的行。`read_return: true` 能自动带出。
7. **HTTP 状态码有语义**：4xx（400/401/403/404/405/429）多为调用方问题，5xx（500/502/504）多为 SAP 系统或网络问题。响应体 `error.code`=HTTP 状态码、`error.key`=机器码（如 `FU_NOT_FOUND`/`AUTH_INVALID`/`RATE_LIMITED`），按二者精确分支。
8. **透明表查询受限**：端点 4/5 对 DDIC 结构普遍可用，透明表（如 MARA）视系统配置可能 `NOT_FOUND`。
9. **调用有超时**：单次 SAP 调用默认 60s 超时（`SAP_REQUEST_TIMEOUT_SECS` 可配），超时返回 `504`。`/api/rfc` 可在请求体传 `timeout_secs` per-request 覆盖（慢接口如批量 BAPI、大表查询可放宽）。
10. **限流**：设了 `SAP_RATE_LIMIT_RPS` 时，`/api` 按调用方 IP 限速；超限返回 `429`（`key=RATE_LIMITED`）。默认不限流。
11. **源码端点自动降级 ADT**：`/api/functions/{name}/source` 与 `/api/programs/{name}/source` 先走 RFC（`RPY_FUNCTIONMODULE_READ` / `RPY_PROGRAM_READ`），失败（NOT_FOUND 除外）自动改走 ADT 重读，响应的 `source_via` 字段标明来源（`rfc` / `adt`）。背景：源码行宽超 72 字符（现代 ABAP 常见）在部分系统上会让 RPY 路径直接报错。
12. **激活失败不是 HTTP 错误**：写入端点在 SAP 拒绝激活时返回 200 + `activated.success=false` + `problems[]`——必须检查响应体里的 `activated` 字段。表/结构写入在 SAP 报告成功后还会**用全量未激活清单反核**（`GET /sap/bc/adt/activation/inactiveobjects`）：SAP 可能对激活返回 200 且无消息、DD02L 里却只有未激活版本（append 结构上实证过）——这种假成功会被改判为 `activated.success=false` 并附问题行。清单读不到（老版本无该端点）时不改判，仅以警告标注激活未经核对。
13. **函数模块签名写在源码里（SEDI 形态）**：新版 ADT 系统把 FM 参数签名**内联存在 FUNCTION 语句里**——`FUNCTION zfm IMPORTING VALUE(iv) TYPE i EXPORTING VALUE(ev) TYPE i.`——并**拒绝**经典 `*" IMPORTING ...` 注释块（400 "Parameter comment blocks are not allowed"）。网关写入时自动转换经典块；RFC 读端点（`/api/functions/{n}/source`）返回的仍是经典形态——做读改写时优先用 `GET /api/objects/func/{n}/source`（ADT 形态）。签名变更**会**注册进 FM 接口——**例外**：曾/现 `rfc_enabled` 的模块接口冻结（SAP 忽略源码级签名变更），改签名请删除重建。改过签名之后，接口读取立即新鲜，且 v0.18.1 起**调用**也立即可用——写后钩子已移除 SDK 旧描述符条目。只有网关之外（SE37）的签名变更才可能让 SDK 陈旧直到 `POST /api/admin/reload`——`409 SIGNATURE_STALE`（交付端口）或 `/api/rfc` 参数错误会指路；签名变更在任何情况下都不需要重启进程。

## 典型任务示例

**任务：列出 SAP 系统里的用户**

```bash
# 1. 搜相关函数
curl -X POST http://127.0.0.1:3000/api/functions/search \
  -H "Content-Type: application/json" \
  -d '{"pattern":"BAPI_USER_GETLIST"}'
# → 确认 BAPI_USER_GETLIST 存在

# 2. 查接口，看返回表叫什么、有哪些字段
curl http://127.0.0.1:3000/api/functions/BAPI_USER_GETLIST
# → 发现 EXPORT 表 USERLIST，含 USERNAME 等字段

# 3. 调用，读 USERLIST 表
curl -X POST http://127.0.0.1:3000/api/rfc \
  -H "Content-Type: application/json" \
  -d '{
    "func_name": "BAPI_USER_GETLIST",
    "table_outputs": {"USERLIST": [{"name": "USERNAME", "max_len": 12}, {"name": "FULLNAME", "max_len": 50}]},
    "read_return": true
  }'
```

## 健康检查

两个探针语义不同：

- `GET /health` —— liveness，**不触碰 SAP**，秒回 `{"status":"ok","version":"..."}`，判断进程是否存活（version 顺带给出，轮询方免费拿版本）。
- `GET /ready` —— readiness，借连接池调 `RFC_PING`（5s 超时）验证 SAP 可达；成功 `{"status":"ready","sap":"ok"}`，失败/超时返回 `503`。
- `GET /metrics` —— Prometheus 指标（免鉴权）：连接池 idle/total/max、RFC 调用计数/耗时。供采集系统抓取。

```bash
curl http://127.0.0.1:3000/health
# → {"status":"ok","version":"0.10.0"}   （不触碰 SAP，仅探活）

curl http://127.0.0.1:3000/ready
# → {"status":"ready","sap":"ok"}   （连 SAP 跑 RFC_PING；失败返回 503）
```

## 版本与能力（自描述）

`GET /api/version`（公开，SAP 挂了也不会失败）——**新会话开局先调它**，不用靠撞 401/403/503/429 来发现部署开关：

```bash
curl http://127.0.0.1:3000/api/version
# → {
#   "name": "sap-for-agents",
#   "version": "0.10.0",
#   "commit": "0fa6d6b",                # 构建所用 git 短哈希；"-dirty" = 构建时有未提交改动
#   "capabilities": {
#     "mode": "full",                  # full = Agent 工作台；runtime = 消费方运行时（SAP_MODE=runtime：仅注册表读 + /api/invokes）
#     "auth": false,                     # true = /api/* 需 Bearer token（本端点除外）
#     "read_only": false,                # true = 写端点返回 403 READ_ONLY
#     "adt": true,                       # true = /api/adt/** 与 /api/dumps* 可用
#     "debug": true,                     # true = /api/debug/**（断点/单步/变量）可用
#     "registry": true,                  # true = /api/registry**（跨会话 API 记忆）可用
#     "rate_limit_rps": null             # 按 IP 每秒上限；null = 不限流
#   },
#   "latest": {                          # 后台 GitHub Release 检查（启动时 + 每 24h）；
#     "version": "0.11.0",               # null = 尚未取到 / 已禁用（SAP_UPDATE_CHECK=off）/
#     "url": "https://github.com/...",   # 网络不可达——该字段绝不阻塞响应
#     "update_available": true
#   },
#   "sap": {                             # 首次调用懒加载，进程生命周期内缓存
#     "sysid": "A4H", "release": "816", "host": "vhcala4h", "os": "Linux",
#     "destination": "vhcala4hci_A4H_00", "client": "001"
#   }
# }
```

- SAP 不可达时 `sap` 为 `null`、`sap_error` 带原因——端点仍返回 200，下次调用自动重试；`latest` 同理（后台任务、缓存）。
- `sap.release` 是内核/Basis 版本号（如 `816`），**不区分** ECC 与 S/4HANA；要判定 S/4，用 `POST /api/table/read` 查 `CVERS` 表有无 `S4CORE` 组件。
- MCP 客户端等价工具：`get_gateway_info`。
