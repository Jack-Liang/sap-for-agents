# Server 端模式：被 SAP 调用（反向代理）

[English](./SERVER_MODE.md) | [简体中文](./SERVER_MODE.zh-CN.md)

除 client 模式（HTTP→SAP）外，本服务还支持 **server 模式**：让 SAP 系统通过 RFC 回调本服务，本服务把调用转发到配置的 HTTP webhook。实现「SAP → HTTP」的反向代理，与 client 对称。

典型用途：让 ABAP 程序调用外部微服务（无需 ABAP 写 HTTP 客户端）；把业务事件从 SAP 推送到外部系统。

## 工作原理

```
[SAP 系统]
  1. SM59 配 RFC Destination（Type T, Registration mode, Program ID=ZREST_SERVER）
  2. ABAP: CALL FUNCTION 'Z_REST_PING' DESTINATION 'ZREST'
       │
       ▼ (SAP 主动连本服务注册的 Program ID)
[本服务: RfcListenAndDispatch 循环]
  3. 收到 Z_REST_PING 调用 → 读入参
  4. POST {func, inputs} 到配置的 webhook_url
  5. 收 webhook 响应 {outputs} → 回填 EXPORTING 参数
       │
       ▼
[配置的 webhook 服务（任意语言）]
  收到请求 → 业务处理 → 返回结果
```

## 启用 server 模式

通过环境变量 `SAP_ROLE` 控制：

| 值 | 行为 |
|---|---|
| `client`（默认）| 仅 client 模式（现有 HTTP server） |
| `server` | 仅 server 模式（dispatch 循环，被 SAP 调） |
| `both` | 两个并行（client HTTP + server dispatch） |

```bash
# 1. 配置 servers.toml（cp servers.toml.example servers.toml 后编辑）
#    填 gateway 地址 + program_id + 函数定义 + webhook URL

# 2. 启动（server 模式）
SAP_ROLE=server SERVERS_CONFIG=servers.toml cargo run --release
```

## 配置文件 `servers.toml`

```toml
[gateway]
gwhost = "192.168.0.215"        # SAP Gateway 主机
gwserv = "sapgw00"              # sysnr 00 → sapgw00
program_id = "ZREST_SERVER"     # 必须与 SM59 的 Program ID 一致

[[functions]]
name = "Z_REST_PING"            # SAP 调用的函数名
webhook_url = "http://localhost:9000/ping"

[[functions.params]]
name = "INPUT"
direction = "import"            # import/export/changing/tables
type = "char"                   # char/int/float/bcd/date/time/num/byte/xstring/string
length = 255

[[functions.params]]
name = "OUTPUT"
direction = "export"
type = "char"
length = 1024
```

> **重要**：`program_id` 必须与 SAP 侧 SM59 配置完全一致。函数名建议用 `Z_` 前缀（自定义命名空间）。

## SAP 侧 SM59 配置（你负责）

在 SAP 系统（SE37/SM59）配置 RFC Destination：

1. **T-code `SM59`** → Create
2. **RFC Connection Type**: `T`（TCP/IP Connection）
3. **Activation Type**: **Registered Server Program**
4. **Program ID**: 与 `servers.toml` 的 `program_id` 一致（如 `ZREST_SERVER`）
5. **Gateway Host**: SAP 系统的 gateway（即 `servers.toml` 里 `gwhost` 指向的系统的 gw）
6. **Gateway Service**: `sapgw00`（对应 sysnr）
7. **保存后测试**：点 Connection Test。此时本服务必须已启动并注册，否则测试会失败

ABAP 调用示例：
```abap
DATA: lv_input  TYPE c LENGTH 255,
      lv_output TYPE c LENGTH 1024.

lv_input = 'hello'.
CALL FUNCTION 'Z_REST_PING' DESTINATION 'ZREST'
  IMPORTING
    input  = lv_input
  EXPORTING
    output = lv_output.
" lv_output 现在是 webhook 返回的处理结果
```

## webhook 协议

**请求**（本服务 POST 给 webhook）：
```json
{
  "func": "Z_REST_PING",
  "inputs": { "INPUT": "hello" }
}
```

**响应**（webhook 返回）：
```json
{
  "outputs": { "OUTPUT": "processed: hello" }
}
```

- 请求/响应都是 JSON，Content-Type: application/json
- webhook 必须**在 30 秒内返回**（否则 SAP 端 RFC 超时）
- `outputs` 的键必须与 `[[functions.params]]` 里 direction=export 的参数名一致
- webhook 返回非 2xx 或超时 → 本服务向 SAP 回传 `SYSTEM_FAILURE`

## webhook 示例（Python Flask）

```python
from flask import Flask, request, jsonify
app = Flask(__name__)

@app.post("/ping")
def ping():
    data = request.json
    inp = data["inputs"]["INPUT"]
    # 业务处理
    return jsonify({"outputs": {"OUTPUT": f"processed: {inp}"}})

if __name__ == "__main__":
    app.run(port=9000)
```

仓库里还自带一份零依赖版本：[`mock_webhook.py`](../mock_webhook.py)（标准库
`http.server`，无需 Flask）。`python mock_webhook.py` 同样监听 9000，同时服务
`/ping`（`Z_REST_PING`）与 `/echo`（`Z_REST_ECHO`），并把每个 SAP 请求体原样
回显——接真实业务服务之前，用它调试 dispatch 循环最方便。

## 限制（首版）

| 项 | 说明 |
|---|---|
| 无状态 | 每个 SAP 调用独立处理，不维护 stateful session |
| 串行 dispatch | 单线程 dispatch，SAP 并发调用排队（可后续多线程） |
| 类型简化 | 入参/出参统一按字符串读写（数值类型靠 webhook 侧自行转换） |
| 不支持 tRFC/qRFC | 事务回调首版未实现 |
| webhook 超时 | 30 秒硬编码（后续可配置化） |
