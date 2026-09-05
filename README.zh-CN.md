# sap-for-agents

[English](./README.md) | [简体中文](./README.zh-CN.md)

把 SAP NWRFC SDK 包装成常驻 HTTP 服务，提供 RESTful 接口调用任意 SAP RFC / BAPI。
其他服务无需安装 SAP SDK，发一个 JSON POST 即可调用。

- **技术栈**：Rust（标准库 FFI 直连 `sapnwrfc.dll`）+ axum + tokio + serde
- **零 SDK 依赖客户端**：调用方只要会发 HTTP POST
- **通用接口**：一个端点 `/api/rfc` 描述任意 BAPI，无需为每个 BAPI 写代码
- **面向 AI**：8 个元数据端点（搜函数/查接口/查文档/查数据字典/读透明表/看源码），Agent 能自服务探索。给 AI 的操作指南见 [`AGENTS.md`](./AGENTS.md)

> ⚠️ **风险提示**：本项目是一个**探索性、实验性项目**，主要用于学习、测试与本地开发场景。它未经生产环境的充分打磨，不保证稳定性与正确性，API 可能随时变更。它以所配置 `SAP_USER` 的完整权限开放 RFC 调用能力——使用前请自行评估风险（数据暴露、未授权调用、合规性等）并自行采取防护措施。对生产 SAP 系统使用本项目的风险由使用者自行承担，作者不对使用本项目造成的任何损失负责。

## 目录

- [Quick Start](#quick-start5-分钟跑起来)
- [§1 适用场景与限制](#1-适用场景与限制)
- [§2 配置参考](#2-配置参考)
- [§3 API 参考](#3-api参考) — 含 [3.3 面向 AI 的元数据 API](#33-面向-ai-的元数据-api)
- [§4 调用示例](#4-调用示例)
- [§5 常见 BAPI 速查](#5-常见-bapi-速查)
- [§6 错误处理](#6-错误处理)
- [§7 部署提示](#7-部署提示)
- [§8 架构与限制](#8-架构与限制)
- [§9 Server 端模式](#9-server-端模式被-sap-调用) — 详见 [docs/SERVER_MODE.md](./docs/SERVER_MODE.md)
- [§10 发布新版本](#10-发布新版本维护者)
- [License](#license)

---

## Quick Start（5 分钟跑起来）

> **两个东西都需要**：
> 1. **预编译二进制 / Rust 源码**：提供 HTTP 服务和 SAP 协议绑定代码
> 2. **SAP NWRFC SDK**：SAP 私有 C 库，提供实际的 SAP 通信实现（受版权限制不能随项目分发）
>
> 预编译二进制**省的是装 Rust 工具链 + 23 秒编译**，但**省不掉 SDK**——运行时仍要链接 SAP 库。
>
> 📦 **省事的做法**：把 SAP 下载的**对应平台** zip（如 macOS 上放 `nwrfcsdk-...-darwin-arm64.zip`，Linux 上放 `...-linux-x86_64.zip`）丢进 `nwrfcsdk/lib/` 下任意子目录，启动脚本会自动解压到 `<os>-<arch>/` 正确路径。详见 §「自动安装 SDK」。
>
> ⚠️ zip 必须与当前系统平台匹配（`.dylib`↔macOS、`.so`↔Linux、`.dll`↔Windows）。脚本不校验平台，放错平台的 zip 会导致解压后库文件无法加载。

### 自动安装 SDK（推荐）

`start.sh` / `start.ps1` 会按以下顺序找 SDK：

1. **环境变量 `SAP_SDK_DIR`** —— 指向已安装的 SDK 根目录（最灵活，Docker/CI 常用）
2. **`nwrfcsdk/lib/<os>-<arch>/`** —— 已放好库文件的默认路径
3. **`nwrfcsdk/lib/<任意>/nwrfcsdk-*.zip`** —— 自动识别并解压到正确路径 ✨
4. 都没有 → 报错并清晰指引

最省事：把 SAP 下载的**对应平台** zip 整个丢到 `nwrfcsdk/lib/` 下任意子目录，启动脚本会自动处理。例如（macOS Apple Silicon）：

```
nwrfcsdk/
└── lib/
    └── incoming/                  ← 随便建一个目录
        └── nwrfcsdk-...-darwin-arm64.zip   ← 必须是当前平台的 SDK
```

然后跑 `./start.sh`，脚本会自动：

- 解压 zip
- 识别 zip 内的库文件位置（SAP SDK zip 通常是 `nwrfcsdk/lib/<file>` 无平台子目录）
- 复制到 `nwrfcsdk/lib/darwin-aarch64/`（或对应平台子目录）
- 清理临时文件

解压后的真实路径仍按 SAP 官方约定保留，便于后续更新 SDK。

### 下载预编译二进制（推荐给最终用户）

不用装 Rust，直接用现成二进制：

1. 打开 [GitHub Releases](../../releases) → 选最新 tag
2. 下载对应平台的压缩包：
   - Linux x86_64: `sap_for_agents-x86_64-unknown-linux-gnu.tar.gz`
   - Linux ARM64: `sap_for_agents-aarch64-unknown-linux-gnu.tar.gz`
   - macOS Intel: `sap_for_agents-x86_64-apple-darwin.tar.gz`
   - macOS Apple Silicon: `sap_for_agents-aarch64-apple-darwin.tar.gz`
   - Windows x86_64: `sap_for_agents-x86_64-pc-windows-msvc.zip`
3. 解压，里面有 `sap_for_agents`（或 `.exe`）+ `README.md` + `.env.example` + `nwrfcsdk/` 目录骨架
4. **下载 SAP NWRFC SDK**：到 [SAP Support Portal](https://launchpad.support.sap.com) 注册账号（需 SAP 客户/合作伙伴身份），搜索 `SAP NW RFC SDK`，按平台下载 zip
5. **把 zip 放到 `nwrfcsdk/lib/` 下任意子目录**（如 `nwrfcsdk/lib/incoming/`），启动脚本会自动解压到 `<os>-<arch>/` 正确路径。**注意 zip 必须匹配当前平台**（macOS→`.dylib`、Linux→`.so`、Windows→`.dll`）
6. `cp .env.example .env` 并填 SAP 连接参数
7. 运行：
   - Linux/macOS: `./sap_for_agents`
   - Windows: 双击 `sap_for_agents.exe` 或 PowerShell 启动

> **Windows 用户**：CI 现已自动构建 Windows x86_64 二进制（通过 `.def` 文件生成 stub 导入库绕过 MSVC 链接限制）。解压 zip 后仍需自行放置 `sapnwrfc.dll`，详见第 4–5 步。

### 方式一：本地运行（开发/调试）

```bash
# 1. 安装 Rust（已装可跳过）
#    Windows: winget install Rustlang.Rustup
#    Linux/macOS: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 2. 放置 SAP SDK（平台对应子目录，详见 nwrfcsdk/README.md）
#    Windows: nwrfcsdk/lib/windows-x86_64/   ← 放 sapnwrfc.dll 等
#    Linux:   nwrfcsdk/lib/linux-x86_64/     ← 放 libsapnwrfc.so 等

# 3. 配置连接信息
cp .env.example .env        # 然后编辑 .env 填入 SAP 连接参数

# 4. 一键检查环境并启动（Windows 用 start.ps1，Linux/macOS 用 start.sh）
./start.sh                  # 或：powershell -File start.ps1

# 5. 验证（新开一个终端）
curl http://127.0.0.1:3000/health
# → {"status":"ok"}
```

### 方式二：Docker（部署）

```bash
# 1. 放置 Linux 版 SDK 到 nwrfcsdk/lib/linux-x86_64/（构建期需要）

# 2. 复制并填写配置
cp .env.example .env        # 编辑：填 SAP 连接参数 + SAP_SDK_HOST_PATH（见下）

# 3. 一键起（docker compose 自动 build + run + 挂载 SDK + 注入配置）
docker compose up -d --build

# 4. 验证
curl http://127.0.0.1:3000/health
```

`.env` 里需额外填一项 `SAP_SDK_HOST_PATH`：宿主机上 Linux SDK 目录的**绝对路径**（如 `C:\Users\you\sap-sdk` 或 `/opt/sap/nwrfcsdk`），compose 会把它挂载进容器的 `/app/nwrfcsdk`。该目录结构需与 `nwrfcsdk/` 一致（含 `lib/linux-x86_64/libsapnwrfc.so`）。

### 第一次调用

服务起来后，任何 HTTP 客户端都能调用：

```bash
curl -X POST http://127.0.0.1:3000/api/rfc \
  -H "Content-Type: application/json" \
  -d '{"func_name":"STFC_CONNECTION","inputs":{"REQUTEXT":"hello"},"string_outputs":{"ECHOTEXT":{"max_len":null}}}'
```

> `max_len` 留 `null` —— 服务会自动从 SAP 元数据发现字段长度，不必手填。



## 1. 适用场景与限制

**适用**

- 微服务架构中，让 Python / Node / Java 服务通过 HTTP 调 SAP
- 自动化脚本、ETL、报表后端
- 本地开发调试 BAPI

**能力边界**

| 能力 | 支持情况 |
|---|---|
| 连接 | ✅ 直连（ASHOST），自动重连，优雅停机 |
| 标量输入 | ✅ string/int/float 自动按 JSON 类型派发；BCD/INT8/二进制用显式 `{"type":"...","value":...}` |
| 标量输出 | ✅ `int_outputs`（按 INT 读）/ `string_outputs`（按字符串读，长度可自动发现）/ `auto_outputs`（按元数据真实类型读，保留 INT/FLOAT/INT8/二进制语义） |
| 表参数（TABLES） | ✅ 输入多行 + 输出遍历；字段加 `"auto":true` 后按真实类型读（INT/FLOAT/INT8/Base64），否则按字符串 |
| 顶层结构体参数 | ✅ `struct_inputs` / `struct_outputs`（如 BAPI_USER_CREATE.ADDRESS），输出字段同样支持 `auto` 按真实类型读 |
| BCD/INT8/二进制 输入 | ✅ `{"type":"BCD",...}` / `{"type":"INT8",...}` / `{"type":"BYTES",...}`（BYTES 用 Base64） |
| 元数据自动发现 | ✅ 字段长度缓存，无需手填 max_len（标量/表/结构体输出均生效） |
| Server 端（被 SAP 回调）| ✅ 配置驱动 webhook 转发（`SAP_ROLE=server`），详见 [§9](#9-server-端模式被-sap-调用) |
| tRFC/qRFC/bgRFC | ❌ 不支持 |
| SSO/SNC 安全登录 | ❌ 仅用户名密码 |

**其他限制**

| 项 | 说明 |
|---|---|
| 并发 | 多连接池（默认 8，`SAP_POOL_SIZE` 可配），不同请求可并行执行 SAP 调用；池耗尽时 acquire 等待上限 120s |
| 字符集 | 通过 UTF-16 桥接 SAP UC，UTF-8 输入输出 |
| 平台 | Windows/Linux/macOS × x86_64/aarch64（`build.rs` 自动选 SDK 子目录） |
| RFC 调用超时 | 连接池层有 acquire 超时（120s）；单次 RFC 调用暂无执行超时 |

---

## 2. 配置参考

所有配置走环境变量，写在项目根目录 `.env`（已被 gitignore，不会提交）。Quick Start 已涵盖基本用法，本节是完整字段参考。

| 变量 | 必填 | 默认 | 说明 |
|---|:---:|---|---|
| `SAP_ASHOST` | ✅ | — | SAP 应用服务器主机名/IP |
| `SAP_SYSNR` | ✅ | — | 系统号，如 `00` |
| `SAP_CLIENT` | ✅ | — | 集团号，如 `001` |
| `SAP_USER` | ✅ | — | 登录账号 |
| `SAP_PASSWD` | ✅ | — | 登录密码 |
| `SAP_LANG` | ❌ | `EN` | 登录语言（也影响文档端点的默认语言） |
| `SAP_LISTEN_ADDR` | ❌ | `127.0.0.1:3000` | HTTP 服务监听地址 |
| `SAP_POOL_SIZE` | ❌ | `8` | SAP 连接池上限（并发调用数），≥1 |
| `SAP_REQUEST_TIMEOUT_SECS` | ❌ | `60` | 单次 SAP 调用全局超时（秒），≥1；超时返回 504。`/api/rfc` 可用请求体 `timeout_secs` per-request 覆盖 |
| `SAP_RATE_LIMIT_RPS` | ❌ | _(不限流)_ | `/api` 按调用方 IP 的每秒请求数，≥1 启用；超限返回 429 |
| `SAP_ROLE` | ❌ | `client` | 运行模式：`client`/`server`/`both`（server 模式见 [§9](#9-server-端模式被-sap-调用)） |
| `SAP_SDK_DIR` | ❌ | `./nwrfcsdk` | SDK 根目录（Docker/CI/自定义路径用） |

> **生产部署提示**：不要把 `.env` 放进镜像层。用容器编排系统的密钥注入（K8s Secret / Docker Swarm secret）替代。

---


## 3. API 参考

### 认证（可选）

设置环境变量 `SAP_API_KEY` 后，所有 `/api/*` 业务端点要求请求头 `Authorization: Bearer <token>`；未设置则免鉴权（本机默认）。探针 `/health`、`/ready` 与公开页 `/`、`/agents.md` 始终免鉴权。

```bash
# 启用认证（生成一个长随机串）
export SAP_API_KEY=$(openssl rand -hex 32)

# 调用时带 token
curl -H "Authorization: Bearer $SAP_API_KEY" \
  http://127.0.0.1:3000/api/functions/BAPI_USER_GETLIST
```

> ⚠️ 一旦把服务暴露到网络（`SAP_LISTEN_ADDR=0.0.0.0` 或 Docker 部署），**务必**设置 `SAP_API_KEY`。否则任何能访问端口的人都能以 `SAP_USER` 的权限调用任意 RFC。失败返回 `401 {"code":401,"message":"..."}` + `WWW-Authenticate: Bearer`。

### 3.1 健康检查与就绪探针

提供两个语义不同的探针端点：

#### `GET /health` —— liveness（进程存活）

不触碰 SAP，秒回，用于判断进程是否存活。

```json
{ "status": "ok" }
```

#### `GET /ready` —— readiness（SAP 可达）

借连接池调用 SAP 标准函数 `RFC_PING`（带 5s 超时）验证后端可达。

- 成功：`200 { "status": "ready", "sap": "ok" }`
- SAP 不可达 / 超时：`503 { "status": "unavailable" | "timeout", ... }`

编排系统（K8s 等）建议：`/health` 作 livenessProbe（进程挂了才重启），`/ready` 作 readinessProbe（连不上 SAP 仅摘流等待恢复）。

#### `GET /metrics` —— Prometheus 指标（免鉴权）

返回 Prometheus 文本格式指标，供 Prometheus / Grafana 等采集系统抓取：

- `pool_idle` / `pool_total` / `pool_max` —— 连接池空闲 / 已建总数 / 上限
- `rfc_calls_total{func,result}` —— RFC 调用计数（按 函数 × 成功/失败）
- `rfc_call_duration_ms{func}` —— 调用耗时直方图（含 p50/p90/p99）

> 免鉴权（运维探针，与 `/health` `/ready` 同类）。若公网部署，需在反向代理层保护。

---

### 3.2 `POST /api/rfc`

通用 RFC 调用。请求体描述要调哪个函数、传什么参数、要读哪些输出。

#### 请求体字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|:---:|---|
| `func_name` | string | ✅ | SAP 函数模块名，如 `BAPI_USER_GETLIST` |
| `inputs` | object | ❌ | 标量输入参数：参数名 → 值（隐式：字符串→CHARS、整数→INT、浮点→FLOAT） |
| `table_inputs` | object | ❌ | 表输入参数：表名 → 行数组；每行是 字段名 → 值 |
| `struct_inputs` | object | ❌ | 顶层结构体输入：结构体名 → {字段名 → 值}（如 `ADDRESS`） |
| `int_outputs` | string[] | ❌ | 要读取的整型输出参数名列表（按 SAP `INT` 读） |
| `string_outputs` | object | ❌ | 要读取的字符串输出参数：参数名 → 最大长度。`max_len` 留 `null` 时由服务端从元数据自动发现 |
| `auto_outputs` | string[] | ❌ | 要按元数据真实类型读的标量输出参数名（INT→整数、FLOAT→浮点、INT8→i64、BCD→字符串、BYTE/XSTRING→Base64） |
| `table_outputs` | object | ❌ | 要读取的输出表：表名 → 字段对象数组 `{"name":"...","max_len":...,"auto":...}`（`auto:true` 时按真实类型读，默认 false） |
| `struct_outputs` | object | ❌ | 要读取的顶层结构体输出：结构体名 → 字段对象数组（同 `table_outputs` 的字段规则） |
| `read_return` | bool | ❌ | 是否自动读取 BAPI 通用 `RETURN` 消息表，默认 `false` |
| `timeout_secs` | u64? | ❌ | 本次调用超时秒数（≥1 生效）；不传/传 0 用全局 `SAP_REQUEST_TIMEOUT_SECS`（默认 60s），超时返回 504。供慢接口（批量 BAPI、大表查询）自主放宽 |

**值类型规则**（`inputs` / `table_inputs` / `struct_inputs` 的字段值）：

- JSON 字符串 → SAP `CHARS`（如 `"X"`, `"D*"`）
- JSON 整数   → SAP `INT`（如 `50`）
- JSON 浮点   → SAP `FLOAT`（如 `123.45`）
- 显式类型（用于 BCD/INT8/二进制）：`{"type":"BCD","value":"999.99"}`、`{"type":"INT8","value":9876543210}`、`{"type":"BYTES","value":"<Base64>"}`

类型由调用方通过 JSON 字面量或显式 `type` 决定，服务端不猜。

#### 响应体

```jsonc
{
  "func": "BAPI_USER_GETLIST",                // 回显函数名
  "scalars": {                                 // 标量输出
    "ROWS": 50,                                //   int_outputs / auto_outputs → JSON 整数
    "ECHOTEXT": "Hello",                       //   string_outputs → JSON 字符串
    "BIG_ID": 9876543210                       //   auto_outputs（INT8）→ JSON 整数
  },
  "tables": {                                  // 表输出：表名 → 行数组
    "USERLIST": [
      // auto:false（默认）→ 字段值为字符串；auto:true → 按真实类型（整数/浮点/Base64）
      { "USERNAME": "DEVELOPER", "ROWCOUNT": 42 }
    ]
  },
  "structs": {                                 // 顶层结构体输出（struct_outputs 声明时出现，类型规则同 tables）
    "ADDRESS": { "FIRSTNAME": "Dev", "LASTNAME": "User" }
  },
  "return_table": [                            // 仅当 read_return=true 且存在 RETURN 表时（字段统一字符串）
    { "TYPE": "S", "ID": "01", "NUMBER": "123", "MESSAGE": "..." }
  ]
}
```

> 💡 **表/结构体输出的类型控制**：字段默认按字符串读（向后兼容）。给字段加 `"auto":true` 后，服务端按 DDIC 真实类型选择 getter（INT→整数、FLOAT→浮点、INT8→i64、BYTE/XSTRING→Base64、其余→字符串），保留数值/二进制语义。

未读取的字段（如没传 `table_outputs`）在响应中对应键**不出现**（`tables` 为空对象）。

---

### 3.3 面向 AI 的元数据 API

11 个端点让 AI/Agent 自服务地发现函数、理解参数、查数据字典、读文档、看源码、读表数据、排查短转储。典型工作流：**搜索 → 查接口 → 查文档 → 看源码 → 调用**。给 AI 的完整操作指南见 [`AGENTS.md`](./AGENTS.md)。

| 端点 | 用途 | 示例 |
|------|------|------|
| `POST /api/functions/search` | 按通配符搜索函数 | `{"pattern":"BAPI_USER_*","max_results":10}` |
| `GET /api/functions/:name` | 查函数完整接口（参数/类型/方向/嵌套字段） | `/api/functions/BAPI_USER_GET_DETAIL` |
| `GET /api/functions/:name/doc` | 查文档（短文本+SE37长文档+参数说明） | `/api/functions/BAPI_USER_GET_DETAIL/doc?lang=EN` |
| `GET /api/functions/:name/source` | 读函数 ABAP 源码（怎么实现的）；`?prologue=true` 附带所调 `CALL FUNCTION` 目标的紧凑签名 | `/api/functions/STFC_CONNECTION/source?prologue=true` |
| `GET /api/programs/:name/source` | 读程序/报表/include 源码 | `/api/programs/RSBDCOS0/source` |
| `POST /api/table/read` | 读透明表数据（封装 RFC_READ_TABLE） | `{"table":"T000","fields":["MANDT","MTEXT"]}` |
| `GET /api/ddic/type/:name` | 查 DDIC 结构/表字段定义 | `/api/ddic/type/BAPIRET2` |
| `GET /api/ddic/field/:table/:field` | 查字段语义（数据元素/域/固定值） | `/api/ddic/field/BAPIRET2/TYPE` |
| `GET /api/dumps` | 短转储结构化列表（解析自 ADT Atom feed） | `/api/dumps?limit=50` |
| `GET /api/dumps/grouped` | 按（错误类型、终止程序）聚合——什么在反复失败 | `/api/dumps/grouped` |
| `GET /api/dumps/:key/detail` | 单个转储的解析详情：头表/终止点/调用栈 | `/api/dumps/<key>/detail` |

端到端示例（列出用户）：
```bash
# 1. 搜函数 → 2. 查接口(发现 EXPORT 表 USERLIST) → 3. 调用
curl http://127.0.0.1:3000/api/functions/BAPI_USER_GETLIST
curl -X POST http://127.0.0.1:3000/api/rfc -H "Content-Type: application/json" \
  -d '{"func_name":"BAPI_USER_GETLIST","table_outputs":{"USERLIST":[{"name":"USERNAME","max_len":12}]},"read_return":true}'
```

> **约束**
> - DDIC 类型查询(端点 4/5)对**结构**普遍可用；**透明表**(如 MARA)视目标系统 DDIC 配置可能 `NOT_FOUND`。
> - 长文档(端点 3)依赖 `DOCU_GET`，个别系统未启用时 `long_text` 为空，但参数描述仍可用。
> - `fixed_values` 对理解状态码/枚举字段的合法取值特别有用。
> - `/api/dumps*` 端点需要启用 ADT（`SAP_ADT_BASE_URL`），否则与 ADT 代理一样返回 503 `ADT_DISABLED`。详情解析按英文标签匹配——非英文 logon 系统上字段返回空而非错值（需要原文走 `/api/adt/runtime/dump/{key}/formatted`）。列表/聚合端点只解析结构化 Atom feed，零详情请求。
> - 源码端点（`/api/functions/:name/source`、`/api/programs/:name/source`）先走 RPY 系 RFC，失败（NOT_FOUND 除外）自动降级 ADT 重读；响应的 `source_via` 字段（`rfc`/`adt`）标明来源。行宽超 72 字符的源码在部分系统上会让 RPY 路径失败——降级即为此而设（v0.5.1 起）。

---


## 4. 调用示例

### 4.1 最小连通测试 — `STFC_CONNECTION`

SAP 标准 ping 函数，回显你发的文本。

```bash
curl -X POST http://127.0.0.1:3000/api/rfc \
  -H "Content-Type: application/json" \
  -d '{
    "func_name": "STFC_CONNECTION",
    "inputs": { "REQUTEXT": "Hello from Rust!" },
    "string_outputs": { "ECHOTEXT": 255, "RESPTEXT": 255 }
  }'
```

响应：

```json
{
  "func": "STFC_CONNECTION",
  "scalars": {
    "ECHOTEXT": "Hello from Rust!",
    "RESPTEXT": "SAP R/3 Rel. ..."
  },
  "tables": {}
}
```

### 4.2 读取用户列表 — `BAPI_USER_GETLIST`

```bash
curl -X POST http://127.0.0.1:3000/api/rfc \
  -H "Content-Type: application/json" \
  -d '{
    "func_name": "BAPI_USER_GETLIST",
    "inputs": { "MAX_ROWS": 50, "WITH_USERNAME": "X" },
    "int_outputs": ["ROWS"],
    "table_outputs": {
      "USERLIST": [
        {"name": "USERNAME", "max_len": 12},
        {"name": "FIRSTNAME", "max_len": 40},
        {"name": "LASTNAME",  "max_len": 40}
      ]
    },
    "read_return": true
  }'
```

> `max_len` 也可省略 → 服务端按 SAP 元数据自动发现。例如 `{"name": "USERNAME"}`。

### 4.3 带选择条件 — `BAPI_USER_GETLIST` + `SELECTION_RANGE`

`table_inputs` 演示：过滤用户名以 `D` 开头。

```bash
curl -X POST http://127.0.0.1:3000/api/rfc \
  -H "Content-Type: application/json" \
  -d '{
    "func_name": "BAPI_USER_GETLIST",
    "inputs": { "MAX_ROWS": 10, "WITH_USERNAME": "X" },
    "table_inputs": {
      "SELECTION_RANGE": [
        {
          "PARAMETER": "USERNAME",
          "SIGN": "I",
          "OPTION": "CP",
          "LOW": "D*"
        }
      ]
    },
    "int_outputs": ["ROWS"],
    "table_outputs": {
      "USERLIST": [
        {"name": "USERNAME",  "max_len": 12},
        {"name": "FIRSTNAME", "max_len": 40},
        {"name": "LASTNAME",  "max_len": 40}
      ]
    }
  }'
```

### 4.4 用其他语言调用

**Python (requests)**

```python
import requests
resp = requests.post("http://127.0.0.1:3000/api/rfc", json={
    "func_name": "STFC_CONNECTION",
    "inputs": {"REQUTEXT": "from python"},
    "string_outputs": {"ECHOTEXT": 255},
})
print(resp.json())
```

**Node.js (fetch)**

```js
const r = await fetch("http://127.0.0.1:3000/api/rfc", {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({
    func_name: "STFC_CONNECTION",
    inputs: { REQUTEXT: "from node" },
    string_outputs: { ECHOTEXT: 255 },
  }),
});
console.log(await r.json());
```

---

## 5. 常见 BAPI 速查

> 下表帮你快速知道哪些字段名该填哪里。具体可用字段需查 SE37 / SAP 官方文档。

| BAPI | inputs | table_inputs | 输出 |
|---|---|---|---|
| `STFC_CONNECTION` | `REQUTEXT` | — | `ECHOTEXT` / `RESPTEXT` (string) |
| `BAPI_USER_GETLIST` | `MAX_ROWS`(int) / `WITH_USERNAME` | `SELECTION_RANGE` | `ROWS`(int) / `USERLIST`(table) |
| `BAPI_USER_GET_DETAIL` | `USERNAME` | — | `ADDRESS`(struct→string) / `RETURN`(table) |
| `BAPI_MATERIAL_GETLIST` | `MAXROWS`(int) | `MATNRSELECTION` | `MATNRLIST`(table) |

调用模式：

- **结构体输入**（如 `ADDRESS`）当前不支持嵌套结构，仅支持表行里的扁平字段。
- **`RETURN` 表**：大多数 BAPI 用 `BAPIRET2` 结构，开启 `read_return: true` 自动解析
  `TYPE` / `ID` / `NUMBER` / `MESSAGE` 四个字段。

---

## 6. 错误处理

### 6.1 HTTP 状态码

服务端按错误来源映射 HTTP 状态码，让调用方能按状态码区分「调用方错误」(4xx) 与「上游错误」(5xx)：

| 状态码 | 触发场景 | 来源 |
|---|---|---|
| `200 OK` | 调用成功；搜索无匹配（`count:0`） | `RFC_OK` (0) |
| `400 Bad Request` | 请求 JSON 不合法；ABAP 消息/异常；参数无效/转换失败；空 pattern | SAP 4/5/20/23；网关 `PATTERN_EMPTY` |
| `401 Unauthorized` | 未带 / 错 token（设了 `SAP_API_KEY` 时） | 网关认证层 `AUTH_INVALID` |
| `403 Forbidden` | SAP 授权检查失败 | SAP 25 |
| `404 Not Found` | 函数 / DDIC 不存在；路由不存在 | SAP 17，或 SAP 5 + key `FU_NOT_FOUND`/`NOT_FOUND`；网关 `ROUTE_NOT_FOUND` |
| `405 Method Not Allowed` | 方法不匹配（如 POST 到 GET 端点） | 网关路由层 `METHOD_NOT_ALLOWED` |
| `422 Unprocessable Entity` | 请求体缺必填字段 | axum 反序列化 `JSON_INVALID` |
| `429 Too Many Requests` | 限流超限（`SAP_RATE_LIMIT_RPS`） | 网关限流层 `RATE_LIMITED` |
| `500 Internal Server Error` | ABAP 运行时失败、内存不足、未知 | SAP 3/11 等 |
| `502 Bad Gateway` | 通信失败、连接被对端关闭 | SAP 1/6 |
| `504 Gateway Timeout` | SAP 侧超时；网关全局 / per-request 超时 | SAP 9；网关超时 |

### 6.2 错误响应体

所有错误统一为：

```json
{
  "error": {
    "code": 404,
    "key": "FU_NOT_FOUND",
    "message": "..."
  }
}
```

| 字段 | 含义 |
|---|---|
| `code` | **HTTP 状态码**（同响应状态行；调用方按此做粗粒度分流）。注意：不是 SAP 内部 RC |
| `key` | 机器码：SAP 错误 key（如 `FU_NOT_FOUND`、`RFC_COMMUNICATION_FAILURE`）或网关 key（`AUTH_INVALID` / `JSON_INVALID` / `RATE_LIMITED` / `METHOD_NOT_ALLOWED` / `ROUTE_NOT_FOUND` / `PATTERN_EMPTY`） |
| `message` | 人读描述（可能含 SAP 原始消息文本） |

> 💡 `code` = HTTP 状态码（SAP 内部 RC 不暴露给调用方）。按 `code` 做粗粒度处理，按 `key` 做精细分支。

**特例：搜索无匹配不是错误。** `POST /api/functions/search` 无结果时返回 `200 {"count":0,"functions":[]}`，不报错。

### 6.3 错误排查思路

1. **看 HTTP 状态码**：4xx 多半是请求参数/ABAP 业务问题（改请求可恢复）；5xx 是 SAP 系统/网络问题
2. **看 `key`**：`RFC_COMMUNICATION_FAILURE` 多半是网络/连接；`RFC_ABAP_EXCEPTION` 是 ABAP 抛错
3. **看 `code` 对照 SDK 头文件 `sapnwrfc.h` 中的 `RFC_RC` 枚举**
4. **本地复现**：把同一个请求体直接对 SAP 系统用 SE37 跑一遍，验证参数名/类型

---

## 7. 部署提示

> **快速部署**：项目提供 `docker-compose.yml`，配好 `.env`（含 `SAP_SDK_HOST_PATH`）后
> `docker compose up -d --build` 即可。详见 [Quick Start](#quick-start5-分钟跑起来)。

### 7.1 `sapnwrfc.dll` 找不到

运行期 Windows 必须能加载 `sapnwrfc.dll`。两种方式：

- **PATH**：把 `nwrfcsdk\lib` 加入系统 `PATH`
- **同目录**：把 `sapnwrfc.dll` 复制到 exe 同级目录

启动时若报「找不到 DLL入口」之类，多半是 PATH 问题。

### 7.2 启动失败：连接相关

```
配置加载失败: 缺少必填环境变量: SAP_ASHOST
```
→ `.env` 没配全，看 [§3](#3-配置)。

```
RFC调用错误(代码: 2)：...
```
连不上 SAP：检查 `ASHOST`/`SYSNR` 网络可达、账号密码、`CLIENT` 集团号。

### 7.3 服务化部署

- 用 systemd / NSSM / Windows Service 把二进制注册为开机自启服务
- 监听 `0.0.0.0:3000` 仅在内网；对外请加反向代理（Nginx）+ 鉴权 + HTTPS
- 建议加 `SAP_LISTEN_ADDR` 限制到内网网卡

### 7.4 信任边界

本服务**不做鉴权**。任何能访问监听端口的调用方都能用配置的 SAP 账号执行任意 RFC。
务必放在受控网络或加一层网关鉴权。

---

## 8. 架构与限制

### 8.1 模块结构

```
src/
├── main.rs           启动入口：.env → 按角色(client/server/both)启动对应模式
├── config.rs         从环境变量组装 client 模式连接参数 + 监听地址
├── server_config.rs  server 模式配置：解析 servers.toml（gateway/函数/webhook）
├── server.rs         axum Router + handler + 鉴权/限流中间件（run_blocking_with_timeout 收敛 spawn_blocking + 超时模板）
├── auth.rs           /api/* 的可选 Bearer Token 鉴权（SAP_API_KEY，常量时间比对）；探针与文档页始终免鉴权
├── server_rfc.rs     server 模式：注册到 Gateway + dispatch 回调 + webhook 转发
├── adt.rs            ADT REST 代理 /api/adt/**：透传到 /sap/bc/adt/**，Basic 认证 + CSRF token/会话管理（写方法遇 403 自动重试）
├── dumps.rs          ST22 结构化分析 /api/dumps**：Atom feed 解析、（错误类型×程序）聚合、/formatted 文本 → 头表/终止点/调用栈
├── api.rs            请求/响应 DTO（serde）+ execute_invoke 执行核心 + 输入校验
├── executor.rs       execute_collect：注入元数据解析后委托 execute_invoke
├── connection.rs     RfcConnection：建连/关闭/取函数/拉参数元数据（unsafe impl Send）
├── function.rs       RfcFunction/RfcTable/RfcRow：参数读写、表操作 + ScalarReader trait
├── pool.rs           RfcConnectionPool：多连接池 + 自动重连 + acquire 超时
├── metadata.rs       函数/DDIC 元数据缓存（RwLock，自动发现字段长度与类型）
├── discovery.rs      面向 AI 的元数据封装（RFC_FUNCTION_SEARCH/DDIF_FIELDINFO_GET/DOCU_GET）
├── error.rs          RfcError + 按 SAP RC 映射的语义化 HTTP 状态码 + JSON 错误体
├── ffi.rs            底层 C FFI 绑定（sapnwrfc 函数签名 + RFCTYPE/方向常量）
├── string_utils.rs   UTF-8 ↔ UTF-16(SAP UC) 转换
├── index.html        首页 HTML 模板，英文（include_str! 编译期嵌入，{{BASE_URL}} 占位符）
└── index.zh.html     首页 HTML 模板，中文（按客户端 Accept-Language: zh 选择）
```

### 8.2 并发模型

```
[HTTP 请求 N] ─▶ axum handler（async）
                  │
                  ├─▶ run_blocking ─▶ spawn_blocking ─▶ [连接池抢一个空闲连接] ─▶ RfcInvoke (FFI)
                  │   (server.rs)                         (pool.rs)                      │
                  └─◀──────── await JoinHandle ◀──────────────────────────────────────────┘
```

- **`run_blocking_with_timeout`（`server.rs`）**：把 `spawn_blocking + with_connection + Join 错误映射 + tokio::time::timeout` 收敛到一处，所有 RFC 业务 handler 共用
- **为什么用 `spawn_blocking`**：SAP 调用是阻塞 FFI，直接在 tokio worker 上跑会卡住整个运行时
- **连接池（`pool.rs`）**：`RfcConnectionPool` 维护一组可复用连接，空闲时 pop、借出执行、通信类错误（RC=1/2/3/22）丢弃并自动重连。`acquire` 有 120s 总超时上限，池耗尽时不会永久挂起
- **为什么需要 `unsafe impl Send`**：`RfcConnection` 持裸指针非 Send；在 `Mutex` 串行化保护下（每次 `with_connection` 独占一个连接），NWRFC SDK 允许跨线程串行使用同一连接，故 sound
- **池大小**：默认 `SAP_POOL_SIZE=8`，可在 `.env` 调整。请求从池里抢空闲连接，未抢到则等待；连接失败时按需自动重连
- **`/api/adt/**` 路径不走连接池**：完全不碰 RFC FFI 与连接池——纯 async HTTP 透传到 ADT 服务（Basic 认证 + CSRF token/会话管理见 `adt.rs`），与 RFC 端点仅共享鉴权/限流中间件

### 8.3 升级路径

| 需求 | 状态 / 改造方向 |
|---|---|
| 鉴权 | ✅ 已实现（可选 `SAP_API_KEY` Bearer 鉴权，常量时间比对，`auth.rs`；探针与文档页始终免鉴权） |
| 连接池 acquire 超时 | ✅ 已实现（`ACQUIRE_TIMEOUT=120s`，池耗尽时调用方不再永久挂起） |
| 表/结构体输出按真实类型读 | ✅ 已实现（`FieldSpec.auto=true` 时按 INT/FLOAT/INT8/Base64 读） |
| HTTP 错误码语义化 | ✅ 已实现（按 SAP RC 映射 400/403/404/500/502/504，见 [§6.1](#61-http-状态码)） |
| HTTP 输入校验 + DoS 防护 | ✅ 已实现（`validate_func_name` 格式/长度、`max_len` clamp、`table_inputs` 行数上界、`table_outputs`/`read_return` 输出行数上限 10000） |
| FFI 句柄防御 | ✅ 已实现（OpenConnection/CreateFunction/AppendNewRow 等返回值 null 检查） |
| 命名空间函数 `/NS/NAME` | ✅ 已实现（校验放行 + 通配路由分发，v0.4.10 起） |
| ADT REST 代理 | ✅ 已实现（`/api/adt/**` 透传 + 写方法 CSRF 自动处理，v0.4.11 起） |
| ST22 结构化分析 | ✅ 已实现（`/api/dumps` 列表/聚合/详情，解析自 ADT——免去几十万字节原始文本，v0.5.0 起） |
| 源码依赖前言 | ✅ 已实现（`/api/functions/:name/source?prologue=true` 内联 `CALL FUNCTION` 目标紧凑签名，v0.5.0 起） |
| 按 IP 限流 | ✅ 已实现（可选 `SAP_RATE_LIMIT_RPS`，governor 键控限流器，超限 429） |
| 单次 RFC 执行超时 | ✅ 已实现（`run_blocking_with_timeout` 用 `tokio::time::timeout` 包 `spawn_blocking`；默认 60s / `SAP_REQUEST_TIMEOUT_SECS`，单请求 `timeout_secs`，超时 504） |
| tRFC/qRFC | 暂不支持 |

---

## 9. Server 端模式（被 SAP 调用）

除 client 模式（HTTP→SAP）外，本服务还支持 **server 模式**：让 SAP 通过 RFC 回调本服务，转发到配置的 HTTP webhook，实现「SAP → HTTP」反向代理。适合让 ABAP 调用外部微服务、或把业务事件从 SAP 推送出去。

启用方式：`SAP_ROLE=server`，配合 `servers.toml` 配置 gateway/program_id/函数/webhook。

完整说明（工作原理、SM59 配置、webhook 协议、示例）见 **[`docs/SERVER_MODE.md`](./docs/SERVER_MODE.md)**。

---


## 10. 发布新版本（维护者）

1. 提交所有改动，本地构建确认通过：
   ```bash
   cargo test                  # 单元测试（无需 SAP，CI 默认跑这个）
   cargo build --release
   # 有真实 SAP 环境时，额外跑集成测试（tests/，标记 #[ignore]）：
   # DYLD_LIBRARY_PATH=./nwrfcsdk/lib/darwin-aarch64 cargo test -- --ignored
   ```
2. 更新 `Cargo.toml` 里的 `version` 字段（如 `0.2.0` → `0.3.0`）
3. 打 tag 并推送，CI 自动构建 Linux/macOS/Windows 二进制并上传到 GitHub Release：
   ```bash
   git tag v0.3.0
   git push origin v0.3.0
   ```
4. Windows 二进制由 CI 自动产出（`.def` + `lib.exe` 生成 stub 导入库绕过 MSVC 链接限制），
   无需维护者手工构建。如需本地复现 CI 的 stub 链接，可在「x64 Native Tools Command Prompt」中：
   ```powershell
   lib /def:sapnwrfc.def /machine:x64 /out:nwrfcsdk\lib\windows-x86_64\sapnwrfc.lib
   cargo build --release
   ```

> CI 工作流：[`.github/workflows/release.yml`](./.github/workflows/release.yml)。改动 [build.rs](./build.rs) 让 `SAP_SDK_DIR` 环境变量可指向任意 SDK 安装目录，方便 Docker / CI / 自定义路径使用。

## License

本项目基于 [MIT License](LICENSE) 开源。

> **关于 SAP NWRFC SDK**：本项目链接了 SAP 私有 SDK（[`build.rs`](./build.rs)），其使用受你与 SAP 之间的协议约束。MIT 协议仅适用于本仓库源码，不延伸至 SDK 本身。

> **商标声明**：本项目为独立维护的社区项目，与 SAP SE 无隶属、认可或赞助关系。文中提及的 "SAP" 及其他 SAP 产品名称为 SAP SE 的商标或注册商标，仅用于描述互操作性。
