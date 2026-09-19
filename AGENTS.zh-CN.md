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
- 探针 `/health`、`/ready` 与公开页 `/`、`/agents.md`、`/openapi.json` **始终免鉴权**（不需要 token）。
- 是否启用由部署方决定。本机默认环境通常免鉴权——你可先不带 token 试，收到 401 再向部署方索取。

## 你能做什么

| 目标 | 用哪个端点 |
|------|-----------|
| 不知道有哪些函数 → 按名字模糊搜索 | `POST /api/functions/search` |
| 知道函数名，想知道参数怎么填 | `GET /api/functions/{name}` |
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
| 想**修改** ABAP 代码（函数/类/程序） | `PUT /api/objects/{type}/{name}/source` |
| AI 式编辑（唯一匹配查找替换 + 自动激活） | `POST /api/objects/{type}/{name}/replace` |
| 语法检查（**不写库**，源码内嵌提交） | `POST /api/objects/{type}/{name}/syntax` |
| 想读/写 ABAP 类源码等 ADT（Eclipse 工具链）资源 | `ANY /api/adt/{path}` |
| **实际调用一个 SAP 函数** | `POST /api/rfc` |
| 想调用函数但**免填** `func_name`（类型化端点） | `POST /api/functions/{name}/invoke` |
| 想要本网关的机器可读 OpenAPI 规范 | `GET /openapi.json`（公开免鉴权） |
| 想要**按 BAPI 生成类型化 operation** 的规范（参数/类型/字段全展开） | `GET /api/openapi?functions=BAPI_X,BAPI_Y` |
| 改函数前想知道**谁在调用它**（where-used 引用查询） | `GET /api/functions/{name}/where-used` |
| 你是 MCP 客户端（Claude 等）——把网关挂载为 MCP 工具服务器 | `POST /mcp`（Streamable HTTP 无状态） |
| 人类友好的交互式 API 文档 | `GET /docs`（Redoc 渲染规范） |

## 标准操作流程

绝大多数任务遵循 **搜索 → 查接口 → 查文档 → 看源码 → 调用** 五步：

```
1. 搜函数    POST /api/functions/search     找到目标函数名
2. 查接口    GET  /api/functions/{name}     看清楚参数名、类型、方向
3. 查文档    GET  /api/functions/{name}/doc 理解用途、约束、示例
4. 看源码    GET  /api/functions/{name}/source  理解实现（可选）
5. 调用      POST /api/rfc                  按 interface 填参执行
```

> 不要跳过第 2 步直接调用——SAP 参数名区分大小写且必须大写，类型（CHAR/INT/BCD...）决定如何传值。先查接口能避免 90% 的传参错误。

## 端点速查（含可复制的示例）

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
- `type`：`CHAR` / `INT` / `STRUCTURE` / `TABLE` / `BCD` / `DATE` ...
- `direction`：`IMPORT`（你要填）/ `EXPORT`（返回值）/ `TABLES`（可进可出）
- `length`：字符长度（CHAR/NUM/DATE 等）
- `optional`：是否可省略
- `description`：参数说明
- `fields`：若为 STRUCTURE/TABLE，列出嵌套字段

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

修改函数 / 类 / 程序。网关在一次 HTTP 请求内跑完 ADT 写序列：**建立 stateful 会话 → LOCK → PUT 源码 → UNLOCK → 激活**；lockHandle 绝不跨请求（ADT 锁绑定 ABAP 会话，跨请求的柄必然失效）。

`{type}` 取 `prog`（程序/报表）、`class`、`func`（函数模块，组名自动经 RFC 搜索反解，也可显式传 `"group"`）。

```bash
# AI 式编辑（推荐）：唯一匹配查找替换 + 激活
curl -X POST http://127.0.0.1:3000/api/objects/prog/ZMY_REPORT/replace \
  -H "Content-Type: application/json" \
  -d '{"old_string":"WRITE '\''old'\''.","new_string":"WRITE '\''new'\''."}'

# 全量写源码
curl -X PUT http://127.0.0.1:3000/api/objects/class/ZCL_FOO/source \
  -H "Content-Type: application/json" \
  -d '{"source":"CLASS zcl_foo DEFINITION ... ENDCLASS."}'

# 语法检查（不写库：源码内嵌提交）
curl -X POST http://127.0.0.1:3000/api/objects/prog/ZMY_REPORT/syntax \
  -H "Content-Type: application/json" \
  -d '{"source":"REPORT zmy_report.\nWRITE 1."}'
```

- `replace` 请求体：`old_string` / `new_string`（可选 `transport`、`activate`（默认 true）、`group`）。`old_string` 必须精确匹配**唯一一处**（0 处 → 先读当前源码；多处 → 带更多上下文行；`\r\n`/`\n` 差异自动归一化）。空 `old_string` 仅对空对象有效。
- `PUT /source` 请求体：`source`（全量源码）+ 同上可选字段。
- `syntax` 请求体：`source`。返回 `issues[]`：`severity`（E/W/…）、`line`、`offset`、`text`。
- 响应里的 `activated.success` 是**逻辑结果**：激活失败时 HTTP 仍为 200，带 `activated.messages[]` / `problems[]`（"Line N: 文本"）——读它、改源码、重试。传输层错误（网络/会话）才走 4xx/5xx。
- 锁冲突（他人正在编辑）→ 409 `OBJECT_LOCKED`，消息来自 SAP 原文。
- **函数模块**：FM 源码的参数块（`FUNCTION 名.` 到 `EXCEPTIONS … .`）由参数元数据再生成——锚定在此区域的编辑会被**静默丢弃**，请把 `old_string` 锚定在函数**体**内；参数增删需元数据接口（尚未提供）。
- 写入要求对象已存在（对象创建尚未提供）且 ADT 已启用；写入失败也会尽力 UNLOCK，不留孤儿锁。
- **只读部署**：部署者可能以 `SAP_READ_ONLY=1` 运行网关——写端点（`PUT .../source`、`POST .../replace`、`/api/adt` 非读方法）此时返回 403 `READ_ONLY`。这是刻意为之：不要重试写操作，改为只读操作与 `POST .../syntax`（仍可用，不落库）。`POST /api/rfc` 不受该开关影响。

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
12. **激活失败不是 HTTP 错误**：写入端点在 SAP 拒绝激活时返回 200 + `activated.success=false` + `problems[]`——必须检查响应体里的 `activated` 字段。
13. **函数模块的参数块归元数据管**：FM 源码里 `FUNCTION 头 … EXCEPTIONS x.` 这段是从参数元数据再生成的，锚定在这里的编辑会被 SAP 静默丢弃——编辑请锚定函数体；参数增删要走元数据接口（尚未提供）。

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

- `GET /health` —— liveness，**不触碰 SAP**，秒回 `{"status":"ok"}`，判断进程是否存活。
- `GET /ready` —— readiness，借连接池调 `RFC_PING`（5s 超时）验证 SAP 可达；成功 `{"status":"ready","sap":"ok"}`，失败/超时返回 `503`。
- `GET /metrics` —— Prometheus 指标（免鉴权）：连接池 idle/total/max、RFC 调用计数/耗时。供采集系统抓取。

```bash
curl http://127.0.0.1:3000/health
# → {"status":"ok"}   （不触碰 SAP，仅探活）

curl http://127.0.0.1:3000/ready
# → {"status":"ready","sap":"ok"}   （连 SAP 跑 RFC_PING；失败返回 503）
```
