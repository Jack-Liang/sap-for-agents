# AGENTS.md

[English](./AGENTS.md) | [简体中文](./AGENTS.zh-CN.md)

This file guides AI/Agents on how to use the sap-for-agents service.

## What it is

sap-for-agents is a **SAP NWRFC → REST gateway**: it exposes SAP RFC/BAPI function modules as HTTP endpoints. Through it, you (the AI) can search, inspect, and invoke function modules in an SAP system without installing an SAP client.

- **Project**: https://github.com/Jack-Liang/sap-for-agents
- **Issues**: https://github.com/Jack-Liang/sap-for-agents/issues

The service listens on `http://127.0.0.1:3000` by default (override with `SAP_LISTEN_ADDR`).

## Authentication (optional)

By default the service is **unauthenticated** (local access). If the deployer sets the `SAP_API_KEY` environment variable, all `/api/*` endpoints require the `Authorization: Bearer <token>` request header:

```bash
curl -H "Authorization: Bearer <SAP_API_KEY>" http://127.0.0.1:3000/api/functions/BAPI_USER_GETLIST
```

- Missing or wrong token → `401 {"code":401,"message":"..."}`.
- The probes `/health`, `/ready` and the public pages `/`, `/agents.md`, `/openapi.json` are **always unauthenticated** (no token required).
- Whether auth is enabled is decided by the deployer. The default local environment is usually unauthenticated — try without a token first, and request one from the deployer if you receive a 401.

## What you can do

| Goal | Endpoint |
|------|-----------|
| Don't know which functions exist → fuzzy search by name | `POST /api/functions/search` |
| Know the function name, want to know how to fill parameters | `GET /api/functions/{name}` |
| Want full function documentation (purpose, examples) | `GET /api/functions/{name}/doc` |
| Want the fields of a table/structure | `GET /api/ddic/type/{name}` |
| Want to understand a field's meaning and valid values | `GET /api/ddic/field/{table}/{field}` |
| Want the function's ABAP source (how it's implemented) | `GET /api/functions/{name}/source` |
| Want function source + signatures of the functions it calls (one round trip) | `GET /api/functions/{name}/source?prologue=true` |
| Want the source of a program/report/include | `GET /api/programs/{name}/source` |
| Want to read transparent table data (without calling RFC_READ_TABLE directly) | `POST /api/table/read` |
| Want to list ABAP short dumps (structured: error type, program, user, time) | `GET /api/dumps` |
| Want to know **what keeps failing** (dumps grouped by error type + program) | `GET /api/dumps/grouped` |
| Want one dump's call stack / failing line / component (without the 45KB–1MB ST22 text) | `GET /api/dumps/{key}/detail` |
| Want the raw full ST22 text (What happened/Error analysis) | `GET /api/adt/runtime/dump/{key}/formatted` |
| Want to **modify** ABAP code (function / class / program) | `PUT /api/objects/{type}/{name}/source` |
| Want AI-style editing (unique find-and-replace, verified) | `POST /api/objects/{type}/{name}/replace` |
| Want to syntax-check source **without** writing it | `POST /api/objects/{type}/{name}/syntax` |
| Want to read/write ABAP class sources and other ADT (Eclipse tooling) resources | `ANY /api/adt/{path}` |
| **Actually invoke an SAP function** | `POST /api/rfc` |
| Want to invoke a function **without** filling `func_name` (typed endpoint) | `POST /api/functions/{name}/invoke` |
| Want a machine-readable OpenAPI spec of this gateway | `GET /openapi.json` (public) |
| Want a spec with **typed operations per BAPI** (params/types/fields expanded) | `GET /api/openapi?functions=BAPI_X,BAPI_Y` |
| Want to know **who calls a function** before editing it (where-used) | `GET /api/functions/{name}/where-used` |
| You are an MCP client (Claude etc.) — mount the gateway as an MCP tool server | `POST /mcp` (Streamable HTTP, stateless) |
| Human-friendly interactive API docs | `GET /docs` (Redoc rendering of the spec) |

## Standard workflow

Most tasks follow five steps: **search → inspect interface → read docs → view source → invoke**:

```
1. Search functions  POST /api/functions/search      Find the target function name
2. Inspect interface GET  /api/functions/{name}      See parameter names, types, directions
3. Read docs         GET  /api/functions/{name}/doc  Understand purpose, constraints, examples
4. View source       GET  /api/functions/{name}/source  Understand the implementation (optional)
5. Invoke            POST /api/rfc                   Fill parameters per the interface and execute
```

> Do not skip step 2 and invoke directly — SAP parameter names are case-sensitive and must be uppercase, and the type (CHAR/INT/BCD...) determines how to pass values. Inspecting the interface first avoids 90% of parameter mistakes.

## Endpoint quick reference (copyable examples)

### 1. Search functions

```bash
curl -X POST http://127.0.0.1:3000/api/functions/search \
  -H "Content-Type: application/json" \
  -d '{"pattern":"BAPI_USER_*","max_results":10}'
```

- `pattern`: function name wildcard; `*` matches anything. E.g. `BAPI_*`, `RFC_*`.
- Returns a `functions` array; each item has `name` / `group` / `description`.
- No match returns `200 {"count":0,"functions":[]}` (**not an error**).

### 2. Inspect function interface

```bash
curl http://127.0.0.1:3000/api/functions/BAPI_USER_GETLIST
```

Returns **all parameters** of the function; each parameter has:
- `name`: parameter name (**always use this exact uppercase name when passing it**)
- `type`: `CHAR` / `INT` / `STRUCTURE` / `TABLE` / `BCD` / `DATE` ...
- `direction`: `IMPORT` (you fill) / `EXPORT` (return value) / `TABLES` (in or out)
- `length`: character length (for CHAR/NUM/DATE etc.)
- `optional`: whether it can be omitted
- `description`: parameter description
- `fields`: for STRUCTURE/TABLE, lists the nested fields

> Namespaced function names (containing `/`, e.g. `/SDF/EWA_GET_ABAP_DUMPS`) are supported.
> In URL paths, use either the raw form (`/api/functions//SDF/EWA_GET_ABAP_DUMPS`) or the
> percent-encoded form (`/api/functions/%2FSDF%2FEWA_GET_ABAP_DUMPS`); in JSON bodies
> (e.g. `func_name` of `/api/rfc`), pass the name as-is.

### 3. Read function documentation

```bash
curl 'http://127.0.0.1:3000/api/functions/BAPI_USER_GETLIST/doc?lang=EN'
```

Returns `short_text` (short description), `long_text` (full SE37 documentation, may be long), and `parameter_docs` (per-parameter descriptions). If `lang` is omitted, the `SAP_LANG` environment variable is used (default EN).

> Not all functions have long documentation. An empty `long_text` is normal — read `parameter_docs` instead.

### 4. Inspect DDIC table/structure fields

```bash
curl http://127.0.0.1:3000/api/ddic/type/BAPIRET2
```

Returns all field definitions of the DDIC object. ⚠️ Widely available for **structures** (e.g. `BAPIRET2`); for **transparent tables** (e.g. `MARA`) it depends on the target system's DDIC configuration — some systems return `NOT_FOUND`.

### 5. Inspect field semantics (data element / domain / valid values)

```bash
curl 'http://127.0.0.1:3000/api/ddic/field/BAPIRET2/TYPE?lang=EN'
```

Returns `data_element` (data element), `domain` (domain), `description`, and `fixed_values` (the domain's fixed values — especially useful for status code / type fields, telling you which values are legal).

### 6. Invoke an SAP function

```bash
curl -X POST http://127.0.0.1:3000/api/rfc \
  -H "Content-Type: application/json" \
  -d '{
    "func_name": "STFC_CONNECTION",
    "inputs": {"REQUTEXT": "hello"},
    "string_outputs": {"ECHOTEXT": 255, "RESPTEXT": 255}
  }'
```

Request body fields:
- `func_name`: **required**, function name (uppercase)
- `inputs`: IMPORT scalar parameters → value. Pass strings directly, integers as numbers
- `table_inputs`: TABLES input parameters → array of rows (each row is `{field: value}`)
- `struct_inputs`: top-level IMPORT structure parameters → `{field: value}`
- `string_outputs`: EXPORT string parameters to read → max length (`null` means auto-discover)
- `int_outputs`: array of EXPORT integer parameter names to read
- `auto_outputs`: EXPORT scalar parameter names to read by their true metadata type (INT→integer, FLOAT→float, INT8→i64, BCD→string, BYTE/XSTRING→Base64)
- `table_outputs`: EXPORT tables to traverse → field list. Field item `{"name":"FIELD"}` or `{"name":"FIELD","max_len":12}`; add `"auto":true` to read that field by its true type (INT→integer, FLOAT→float, INT8→i64, BYTE/XSTRING→Base64, others→string)
- `struct_outputs`: top-level structure outputs → field list (same rules as `table_outputs`)
- `read_return`: whether to automatically read the BAPI's RETURN message table
- `timeout_secs`: timeout in seconds for this call (optional, ≥1). Omit to use the global default of 60s; relax it for slow endpoints (batch BAPIs, large table queries). On timeout returns 504

Response body:
- `scalars`: scalar outputs (parameter name → value; value type depends on the read method)
- `tables`: table outputs (table name → array of rows, each row `{field: value}`). Field values are strings by default; fields with `auto:true` are returned by their true type (integer/float/Base64 string)
- `structs`: top-level structure outputs (same value-type rules as `tables`)
- `return_table`: RETURN messages (if any; fields uniformly strings)

> ⚠️ **Table/structure outputs are read as strings by default.** To preserve numeric semantics, add `"auto":true` to the field; the server then selects the appropriate getter by the DDIC true type (INT/FLOAT/INT8/BYTE).

### 7. ADT REST proxy (dumps, class sources, anything Eclipse ADT exposes)

`ANY /api/adt/{path}` transparently proxies the SAP system's **ADT REST API** (`/sap/bc/adt/**` on ICF — the same API Eclipse uses). The gateway holds the credentials and handles CSRF tokens for write methods; you just call HTTP.

```bash
# List ABAP short dumps (Atom feed: error id, terminated program, user, time)
curl -H "Accept: */*" http://127.0.0.1:3000/api/adt/runtime/dumps

# Full ST22 text of a dump (take the key from the feed entry's rel="self" link)
curl -H "Accept: text/plain" \
  "http://127.0.0.1:3000/api/adt/runtime/dump/<key>/formatted"

# Read an ABAP class source (works for namespaced/long names too)
curl -H "Accept: */*" http://127.0.0.1:3000/api/adt/oo/classes/cl_runtime_error/source/main

# ADT service discovery (note: requires Accept: application/atomsvc+xml)
curl -H "Accept: application/atomsvc+xml" http://127.0.0.1:3000/api/adt/discovery
```

Behavior:
- The URL path after `/api/adt/` maps 1:1 to the ADT path (`/api/adt/runtime/dumps` → `/sap/bc/adt/runtime/dumps`). Percent-encoded characters (e.g. `%20` in dump keys) work.
- Requests: `Accept`, `Content-Type`, `If-Match`/`If-None-Match` and the body are forwarded. Responses: ADT's HTTP status, `Content-Type`, `ETag`, `Last-Modified` and body are passed through **verbatim** (mostly XML) — a 404/406 here comes from ADT itself, not the gateway.
- Write methods (POST/PUT/DELETE/PATCH): the gateway fetches and attaches the `X-CSRF-Token` + session cookie automatically and retries once on 403 (token expiry).
- Gateway-side failures use the JSON error contract: 400 `ADT_PATH_INVALID`, 502 `ADT_UNREACHABLE`, 503 `ADT_DISABLED` (empty `SAP_ADT_BASE_URL`), 504 `ADT_TIMEOUT`.
- Requires the ADT ICF service to be active in the target system (SICF). Base URL defaults to `http://<SAP_ASHOST>:50000`, override with `SAP_ADT_BASE_URL` (empty string disables).

### 8. Structured short-dump analysis (ST22)

Parsed views over the same ADT data as section 7 — you get structured JSON instead of raw XML feed / 45KB–1MB of ST22 text:

```bash
# Structured list, newest first: error_type, program, user, at, message, key
curl http://127.0.0.1:3000/api/dumps

# What KEEPS failing: dumps collapsed by (error type, terminated program)
curl http://127.0.0.1:3000/api/dumps/grouped

# One dump's parsed detail: header, termination point (include/line/procedure),
# call stack (innermost first). Use the `key` from the list/grouped response.
curl http://127.0.0.1:3000/api/dumps/20260824012009%20a4h/detail
```

- `GET /api/dumps` — query params: `from`/`to` (`yyyyMMddHHmmss`, UTC, passed through to ADT), `limit` (default 100, max 1000).
- `GET /api/dumps/grouped` — same params; returns groups sorted by count desc (ties: most recent first), each with `count` / `first` / `last` / `users` / `latest_key` / `latest_message`. A group's `latest_key` plugs straight into the detail endpoint.
- `GET /api/dumps/{key}/detail` — the gateway fetches the English rendering of the dump and parses it: `error_type`, `exception`, `program` (header table's, which can differ from the feed's terminated program on RAISE_EXCEPTION), `component` (empty when "Not assigned"), `include`/`line`/`procedure`/`main_program`, `stack[]` (position/type/program/include/line/name), and the raw `header` label→value map.
- Typical triage flow: `grouped` → pick the top group → `detail` on `latest_key` → read the failing line's source via `/api/programs/{program}/source`.
- `program` in the detail is a **class pool name** for class dumps (`ZCL_X=========CP`) — the class itself is `ZCL_X`.
- Requires ADT enabled (like section 7); these endpoints return 503 `ADT_DISABLED` when `SAP_ADT_BASE_URL` is empty. Detail parsing matches English labels — on a non-English system fields come back empty rather than wrong; fall back to the raw `/formatted` text via section 7 if you need it. A 404 on detail means the dump is gone or the release has no detail resource (7.50 has the feed but not per-dump details).

`GET /api/functions/{name}/source?prologue=true` (also under this "save round trips" theme) returns the source plus a dependency prologue: every `CALL FUNCTION 'X'` target resolved to a compact signature block (`prologue.text`, ABAP-comment style), so one call gives you the code *and* the contracts of what it calls. Failures stay visible as `FUNCTION X -- 接口读取失败` lines rather than being dropped.

### 9. Code modification (write orchestration)

Edit functions / classes / programs. The gateway runs the full ADT write sequence — **establish stateful session → LOCK → PUT source → UNLOCK → activate** — inside one HTTP request; the lock handle never crosses requests (ADT locks are bound to the ABAP session, so a cross-request handle is dead on arrival).

`{type}` is `prog` (program/report), `class`, or `func` (function module; the group is resolved automatically via RFC search, or pass `"group"` explicitly).

```bash
# AI-style editing (recommended): unique find-and-replace + activate
curl -X POST http://127.0.0.1:3000/api/objects/prog/ZMY_REPORT/replace \
  -H "Content-Type: application/json" \
  -d '{"old_string":"WRITE 'old'.","new_string":"WRITE 'new'."}'

# Full-source write
curl -X PUT http://127.0.0.1:3000/api/objects/class/ZCL_FOO/source \
  -H "Content-Type: application/json" \
  -d '{"source":"CLASS zcl_foo DEFINITION ... ENDCLASS."}'

# Syntax check WITHOUT writing (source is sent inline, nothing is stored)
curl -X POST http://127.0.0.1:3000/api/objects/prog/ZMY_REPORT/syntax \
  -H "Content-Type: application/json" \
  -d '{"source":"REPORT zmy_report.\nWRITE 1."}'
```

- `replace` body: `old_string` / `new_string` (+ optional `transport`, `activate` (default true), `group`). `old_string` must match **exactly one** place (0 → read the current source first; >1 → include more context lines; `\r\n`/`\n` differences are normalized automatically). Empty `old_string` only works on an empty object.
- `PUT /source` body: `source` (full text), plus the same optional fields.
- `syntax` body: `source`. Returns `issues[]` with `severity` (E/W/…), `line`, `offset`, `text`.
- Response `activated.success` is the **logical** result: an activation failure is HTTP 200 with `activated.messages[]` / `problems[]` ("Line N: text") — read them, fix the source, retry. Transport errors (network, session) are 4xx/5xx as usual.
- Lock conflict (someone else editing) → 409 `OBJECT_LOCKED` with SAP's own message.
- **Function modules**: in the FM source the parameter block (`FUNCTION name.` down to `EXCEPTIONS ... .`) is **regenerated from parameter metadata** — edits anchored there are silently dropped. Anchor `old_string` in the function **body**. Parameter changes need the metadata API (not built yet).
- Writes need the object to exist (creation is not built yet) and ADT enabled; a failed write still attempts UNLOCK so no orphan lock is left behind.
- **Read-only deployments**: the deployer may run the gateway with `SAP_READ_ONLY=1` — write endpoints (`PUT .../source`, `POST .../replace`, non-read `/api/adt` methods) then return 403 `READ_ONLY`. This is intentional: don't retry writes, stick to reads and `POST .../syntax` (still allowed, nothing is stored). `POST /api/rfc` is unaffected by the switch.

## Key constraints (pitfalls to avoid)

1. **Parameter names must be uppercase**: SAP parameter names are case-sensitive; in JSON always use uppercase (e.g. `USERNAME`, not `username`).
2. **Inspect the interface before invoking**: don't guess parameter names/types — confirm them with endpoint 2 first.
3. **Pass strings for CHAR, numbers for INT**: `{"REQUTEXT":"hi"}`, `{"MAX_ROWS":100}`.
4. **Use explicit type markers for BCD/INT8/binary**: `{"type":"BCD","value":"123.45"}`, `{"type":"BYTES","value":"<base64>"}`.
5. **Commit transactions explicitly for BAPIs**: after a write BAPI (CREATE/UPDATE/DELETE) succeeds, you must call `BAPI_TRANSACTION_COMMIT`, otherwise the changes do not take effect.
6. **Check RETURN for errors**: BAPIs usually do not raise HTTP errors; instead they return rows with `TYPE=E` (error) in the `RETURN` table. `read_return: true` brings it out automatically.
7. **HTTP status codes are semantic**: 4xx (400/401/403/404/405/429) are mostly caller-side problems; 5xx (500/502/504) are mostly SAP system or network problems. The response body's `error.code` = HTTP status code, `error.key` = machine code (e.g. `FU_NOT_FOUND` / `AUTH_INVALID` / `RATE_LIMITED`); branch precisely on both.
8. **Transparent table queries are limited**: endpoints 4/5 are generally available for DDIC structures; transparent tables (e.g. MARA) may return `NOT_FOUND` depending on system configuration.
9. **Calls have timeouts**: a single SAP call times out after 60s by default (configurable via `SAP_REQUEST_TIMEOUT_SECS`); timeout returns `504`. `/api/rfc` accepts a per-request `timeout_secs` in the body to override it (relax it for slow endpoints like batch BAPIs or large table queries).
10. **Rate limiting**: when `SAP_RATE_LIMIT_RPS` is set, `/api` is rate-limited per caller IP; exceeding the limit returns `429` (`key=RATE_LIMITED`). No rate limit by default.
11. **Source endpoints fall back to ADT automatically**: `/api/functions/{name}/source` and `/api/programs/{name}/source` try the RFC path (`RPY_FUNCTIONMODULE_READ` / `RPY_PROGRAM_READ`) first; on failure (except NOT_FOUND) they re-read via ADT. The response's `source_via` field says which channel served it (`rfc` / `adt`). This matters because sources with lines wider than 72 chars (common in modern ABAP) fail the RPY path on some systems.
12. **Activation failure is not an HTTP error**: write endpoints return 200 with `activated.success=false` + `problems[]` when SAP refuses to activate — always check `activated` in the response body.
13. **FM parameter block is metadata-owned**: edits to the `FUNCTION ... EXCEPTIONS x.` header region of a function module's source are silently dropped by SAP; anchor edits in the function body.

## Typical task example

**Task: list users in the SAP system**

```bash
# 1. Search for the relevant function
curl -X POST http://127.0.0.1:3000/api/functions/search \
  -H "Content-Type: application/json" \
  -d '{"pattern":"BAPI_USER_GETLIST"}'
# → Confirm BAPI_USER_GETLIST exists

# 2. Inspect the interface; see what the return table is called and its fields
curl http://127.0.0.1:3000/api/functions/BAPI_USER_GETLIST
# → Find the EXPORT table USERLIST, with fields like USERNAME

# 3. Invoke, reading the USERLIST table
curl -X POST http://127.0.0.1:3000/api/rfc \
  -H "Content-Type: application/json" \
  -d '{
    "func_name": "BAPI_USER_GETLIST",
    "table_outputs": {"USERLIST": [{"name": "USERNAME", "max_len": 12}, {"name": "FULLNAME", "max_len": 50}]},
    "read_return": true
  }'
```

## Health checks

The two probes have different semantics:

- `GET /health` — liveness, **does not touch SAP**, returns `{"status":"ok"}` instantly; indicates whether the process is alive.
- `GET /ready` — readiness, uses the connection pool to call `RFC_PING` (5s timeout) to verify SAP reachability; on success `{"status":"ready","sap":"ok"}`, on failure/timeout returns `503`.
- `GET /metrics` — Prometheus metrics (unauthenticated): connection pool idle/total/max, RFC call counts/latency. For scraping by collection systems.

```bash
curl http://127.0.0.1:3000/health
# → {"status":"ok"}   (does not touch SAP; liveness only)

curl http://127.0.0.1:3000/ready
# → {"status":"ready","sap":"ok"}   (connects to SAP and runs RFC_PING; returns 503 on failure)
```
