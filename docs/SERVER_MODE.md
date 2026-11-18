# Server Mode: Called by SAP (Reverse Proxy)

[English](./SERVER_MODE.md) | [简体中文](./SERVER_MODE.zh-CN.md)

Beyond client mode (HTTP→SAP), this service also supports **server mode**: SAP calls back into the service over RFC, which then forwards each call to a configured HTTP webhook. This realizes a "SAP → HTTP" reverse proxy, symmetric to the client.

Typical uses: let ABAP programs invoke external microservices (without writing an HTTP client in ABAP); push business events from SAP out to external systems.

## How It Works

```
[SAP system]
  1. Configure RFC Destination via SM59 (Type T, Registration mode, Program ID=ZREST_SERVER)
  2. ABAP: CALL FUNCTION 'Z_REST_PING' DESTINATION 'ZREST'
       │
       ▼ (SAP connects to the Program ID registered by this service)
[This service: RfcListenAndDispatch loop]
  3. Receives the Z_REST_PING call → reads the input parameters
  4. POSTs {func, inputs} to the configured webhook_url
  5. Receives the webhook response {outputs} → fills the EXPORTING parameters
       │
       ▼
[Configured webhook service (any language)]
  Receives the request → runs business logic → returns the result
```

## Enabling Server Mode

Controlled by the environment variable `SAP_ROLE`:

| Value | Behavior |
|---|---|
| `client` (default) | Client mode only (the existing HTTP server) |
| `server` | Server mode only (dispatch loop, called by SAP) |
| `both` | Both run in parallel (client HTTP + server dispatch) |

```bash
# 1. Configure servers.toml (run `cp servers.toml.example servers.toml`, then edit)
#    Fill in the gateway address, program_id, function definitions, and webhook URL

# 2. Start (server mode)
SAP_ROLE=server SERVERS_CONFIG=servers.toml cargo run --release
```

## Configuration File `servers.toml`

```toml
[gateway]
gwhost = "192.168.0.215"        # SAP Gateway host
gwserv = "sapgw00"              # sysnr 00 → sapgw00
program_id = "ZREST_SERVER"     # Must match the SM59 Program ID

[[functions]]
name = "Z_REST_PING"            # Function name called by SAP
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

> **Important**: `program_id` must match the SM59 configuration on the SAP side exactly. Function names should use the `Z_` prefix (custom namespace).

## SAP-Side SM59 Configuration (Your Responsibility)

Configure the RFC Destination in the SAP system (SE37/SM59):

1. **T-code `SM59`** → Create
2. **RFC Connection Type**: `T` (TCP/IP Connection)
3. **Activation Type**: **Registered Server Program**
4. **Program ID**: matches `program_id` in `servers.toml` (e.g. `ZREST_SERVER`)
5. **Gateway Host**: the gateway of the SAP system (the gateway of the host pointed to by `gwhost` in `servers.toml`)
6. **Gateway Service**: `sapgw00` (matching the sysnr)
7. **Test after saving**: click Connection Test. This service must be started and registered beforehand, or the test will fail

ABAP call example:
```abap
DATA: lv_input  TYPE c LENGTH 255,
      lv_output TYPE c LENGTH 1024.

lv_input = 'hello'.
CALL FUNCTION 'Z_REST_PING' DESTINATION 'ZREST'
  IMPORTING
    input  = lv_input
  EXPORTING
    output = lv_output.
" lv_output now holds the result returned by the webhook
```

## Webhook Protocol

**Request** (POSTed to the webhook by this service):
```json
{
  "func": "Z_REST_PING",
  "inputs": { "INPUT": "hello" }
}
```

**Response** (returned by the webhook):
```json
{
  "outputs": { "OUTPUT": "processed: hello" }
}
```

- Both request and response are JSON, with Content-Type: application/json
- The webhook must **return within 30 seconds** (otherwise the SAP-side RFC times out)
- Keys in `outputs` must match the parameter names with direction=export in `[[functions.params]]`
- If the webhook returns a non-2xx status or times out, this service reports a `SYSTEM_FAILURE` back to SAP

## Webhook Example (Python Flask)

```python
from flask import Flask, request, jsonify
app = Flask(__name__)

@app.post("/ping")
def ping():
    data = request.json
    inp = data["inputs"]["INPUT"]
    # Business logic
    return jsonify({"outputs": {"OUTPUT": f"processed: {inp}"}})

if __name__ == "__main__":
    app.run(port=9000)
```

A dependency-free equivalent ships in the repo: [`mock_webhook.py`](../mock_webhook.py) (stdlib `http.server`, no Flask needed). `python mock_webhook.py` listens on 9000 and serves both `/ping` (`Z_REST_PING`) and `/echo` (`Z_REST_ECHO`), echoing back every SAP request body — handy for debugging the dispatch loop before wiring up the real business service.

## Limitations (Initial Release)

| Item | Notes |
|---|---|
| Stateless | Each SAP call is handled independently; no stateful session is maintained |
| Serial dispatch | Single-threaded dispatch; concurrent SAP calls queue up (multi-threading may come later) |
| Simplified types | Input and output parameters are read/written uniformly as strings (numeric conversion is left to the webhook) |
| No tRFC/qRFC | Transactional callbacks are not implemented in the initial release |
| Webhook timeout | Hard-coded at 30 seconds (to be made configurable later) |
