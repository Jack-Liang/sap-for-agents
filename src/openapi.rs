//! `GET /openapi.json` —— 导出本网关的 OpenAPI 3.0.3 规范。
//!
//! 目的与 `/agents.md` 一致：给 AI Agent / 代码生成器 / Postman 等工具一份
//! 机器可读的接口契约，让它们不用读人工文档就能自助构建调用。
//!
//! 设计：
//! - 规范主体是一份**数据驱动的 JSON 字面量**（[`SPEC_JSON`]），启动后
//!   `from_str` 解析再补丁插值（servers / version / 认证方案）。相比在
//!   `json!` 宏里手写 800 行嵌套，字面量没有宏定界符陷阱，且内容可用
//!   任何 JSON 工具直接校验/格式化。
//! - `servers` 按请求 Host 头动态推导（与首页 `index_handler` 同策略），
//!   规范里的示例地址自动匹配用户实际访问的 host:port。
//! - Bearer 认证按 `auth::is_enabled()` 动态出现：部署方设置了 `SAP_API_KEY`
//!   时，规范才声明安全方案，生成出的客户端才带 token。
//! - 每次请求现算（无缓存）：解析 + 组装成本低，换来零失效风险。

use axum::response::IntoResponse;
use serde_json::{json, Value};

/// 内置规范主体（OpenAPI 3.0.3）。占位符由 [`build_spec`] 在运行时替换：
/// - `REPLACE_BASE` → 实际访问地址（Host 头推导）
/// - `info.version` → 编译期 crate 版本
///
/// 内容正确性由单测 `spec_has_core_structure` 与
/// `spec_is_serializable_and_ref_resolvable` 锁定。
const SPEC_JSON: &str = r##"{
  "openapi": "3.0.3",
  "info": {
    "title": "sap-for-agents — SAP NWRFC → REST gateway",
    "description": "Expose SAP RFC/BAPI function modules as HTTP endpoints. Any language or AI agent that can POST JSON can call SAP through this gateway, without installing an SAP client or NWRFC SDK. Machine-readable contract for AI self-service: search → inspect interface → read docs → invoke.",
    "version": "0.0.0"
  },
  "servers": [
    {
      "url": "REPLACE_BASE"
    }
  ],
  "tags": [
    {
      "name": "invoke",
      "description": "Generic RFC/BAPI invocation"
    },
    {
      "name": "discovery",
      "description": "AI-facing metadata endpoints (search / interface / docs / DDIC)"
    },
    {
      "name": "source",
      "description": "ABAP source code readers"
    },
    {
      "name": "objects",
      "description": "ABAP object writes (lock → write → unlock → activate orchestrated)"
    },
    {
      "name": "dumps",
      "description": "ABAP short-dump (ST22) structured reads"
    },
    {
      "name": "adt",
      "description": "ADT REST proxy (Eclipse tooling API pass-through)"
    },
    {
      "name": "table",
      "description": "Transparent table data reader"
    },
    {
      "name": "registry",
      "description": "API registry — cross-session memory of agent-built interfaces (gateway-local, survives restarts)"
    },
    {
      "name": "ops",
      "description": "Health probes and metrics (unauthenticated)"
    }
  ],
  "paths": {
    "/health": {
      "get": {
        "tags": [
          "ops"
        ],
        "summary": "Liveness probe (does not touch SAP)",
        "responses": {
          "200": {
            "description": "Process is alive",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/HealthStatus"
                }
              }
            }
          }
        }
      }
    },
    "/ready": {
      "get": {
        "tags": [
          "ops"
        ],
        "summary": "Readiness probe (pings SAP via RFC_PING, 5s timeout)",
        "description": "Verifies SAP reachability through the connection pool. Returns 503 when SAP is unreachable or times out — orchestrators should remove this instance from load balancing, not restart it.",
        "responses": {
          "200": {
            "description": "SAP reachable",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/ReadyStatus"
                }
              }
            }
          },
          "503": {
            "description": "SAP unreachable / timed out",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/ReadyStatus"
                }
              }
            }
          }
        }
      }
    },
    "/metrics": {
      "get": {
        "tags": [
          "ops"
        ],
        "summary": "Prometheus metrics (connection pool + RFC call counters/latency)",
        "responses": {
          "200": {
            "description": "Prometheus text exposition format",
            "content": {
              "text/plain": {
                "schema": {
                  "type": "string"
                }
              }
            }
          }
        }
      }
    },
    "/agents.md": {
      "get": {
        "tags": [
          "ops"
        ],
        "summary": "Agent documentation (AGENTS.md, markdown)",
        "description": "Human/AI-readable guide for using this gateway. Start here if you don't know which endpoint fits your task.",
        "responses": {
          "200": {
            "description": "Markdown document",
            "content": {
              "text/markdown": {
                "schema": {
                  "type": "string"
                }
              }
            }
          }
        }
      }
    },
    "/api/version": {
      "get": {
        "tags": [
          "ops"
        ],
        "summary": "Gateway version + capability self-description (public, does not fail on SAP outage)",
        "description": "Gateway version/git commit, capability switches (auth, read_only, adt, rate_limit_rps) and SAP system info (sysid, release, host, os, client). Call this first in a new session instead of probing 401/403/503/429 by trial. SAP info is fetched lazily on first call and cached for the process lifetime; when SAP is unreachable, `sap` is null and `sap_error` carries the reason (still HTTP 200).",
        "security": [],
        "responses": {
          "200": {
            "description": "Version and capabilities (sap block may be null)",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/VersionInfo"
                }
              }
            }
          }
        }
      }
    },
    "/api/rfc": {
      "post": {
        "tags": [
          "invoke"
        ],
        "summary": "Invoke any SAP RFC/BAPI function module",
        "description": "Generic invocation: fill `func_name` plus the parameters you care about; outputs you request are returned as JSON. For write BAPIs (CREATE/UPDATE/DELETE) you must call BAPI_TRANSACTION_COMMIT afterwards or the changes won't take effect. Check the RETURN table (read_return: true) for TYPE=E rows instead of relying on HTTP status.",
        "requestBody": {
          "required": true,
          "content": {
            "application/json": {
              "schema": {
                "$ref": "#/components/schemas/InvokeRequest"
              }
            }
          }
        },
        "responses": {
          "200": {
            "description": "Function executed (may still carry SAP-side errors in return_table)",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/InvokeResponse"
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "404": {
            "$ref": "#/components/responses/Error"
          },
          "429": {
            "$ref": "#/components/responses/Error"
          },
          "500": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          },
          "504": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/functions/search": {
      "post": {
        "tags": [
          "discovery"
        ],
        "summary": "Search function modules by wildcard",
        "description": "`*` matches anything, e.g. `BAPI_USER_*`, `RFC_*`. Returns 200 with an empty list when nothing matches (not an error). At least one of pattern / group must be non-empty.",
        "requestBody": {
          "required": true,
          "content": {
            "application/json": {
              "schema": {
                "$ref": "#/components/schemas/SearchRequest"
              }
            }
          }
        },
        "responses": {
          "200": {
            "description": "Search result",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/SearchResponse"
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "429": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/functions/{name}": {
      "get": {
        "tags": [
          "discovery"
        ],
        "summary": "Inspect a function's interface (all parameters)",
        "description": "Returns parameter names (case-sensitive, always UPPERCASE), types (CHAR/INT/TABLE/...), directions (IMPORT/EXPORT/TABLES), lengths and nested fields. Inspect this before invoking to avoid 90% of parameter mistakes.",
        "parameters": [
          {
            "name": "name",
            "in": "path",
            "required": true,
            "description": "Function module name, e.g. BAPI_USER_GETLIST. Namespaced names (containing /) are supported via the raw or percent-encoded form.",
            "schema": {
              "type": "string"
            }
          }
        ],
        "responses": {
          "200": {
            "description": "Function interface",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/FunctionInterface"
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "404": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/functions/{name}/doc": {
      "get": {
        "tags": [
          "discovery"
        ],
        "summary": "Read a function's documentation (SE37)",
        "description": "Returns short_text, long_text (full SE37 documentation, may be empty) and per-parameter docs. Empty long_text is normal — read parameter_docs instead.",
        "parameters": [
          {
            "name": "name",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string"
            }
          },
          {
            "name": "lang",
            "in": "query",
            "required": false,
            "schema": {
              "type": "string",
              "default": "EN"
            },
            "description": "Documentation language (e.g. EN, DE, ZH). Defaults to SAP_LANG env."
          }
        ],
        "responses": {
          "200": {
            "description": "Function documentation",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/FunctionDocResponse"
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "404": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/functions/{name}/source": {
      "get": {
        "tags": [
          "source"
        ],
        "summary": "Read a function module's ABAP source",
        "description": "Reads via RPY_FUNCTIONMODULE_READ; on non-404 failures it falls back to the ADT channel automatically (response `source_via` tells you which path served the lines: \"rfc\" or \"adt\"). `?prologue=true` appends a `prologue` section: signatures of the function's CALL FUNCTION dependencies.",
        "parameters": [
          {
            "name": "name",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string"
            }
          },
          {
            "name": "prologue",
            "in": "query",
            "required": false,
            "schema": {
              "type": "string",
              "enum": [
                "true",
                "1"
              ]
            },
            "description": "true = include dependency-signature prologue"
          }
        ],
        "responses": {
          "200": {
            "description": "ABAP source lines",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/SourceResponse"
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "404": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/programs/{name}/source": {
      "get": {
        "tags": [
          "source"
        ],
        "summary": "Read an ABAP program/report/include's source",
        "description": "Reads via RPY_PROGRAM_READ; falls back to the ADT channel on failure. The `source_via` field tells which path served the lines (\"rfc\" or \"adt\").",
        "parameters": [
          {
            "name": "name",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string"
            }
          }
        ],
        "responses": {
          "200": {
            "description": "ABAP source lines",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/SourceResponse"
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "404": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/ddic/type/{name}": {
      "get": {
        "tags": [
          "discovery"
        ],
        "summary": "Inspect DDIC structure/table field definitions",
        "description": "Widely available for structures (e.g. BAPIRET2); for transparent tables (e.g. MARA) it depends on the target system's DDIC configuration — some systems return NOT_FOUND.",
        "parameters": [
          {
            "name": "name",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string"
            }
          }
        ],
        "responses": {
          "200": {
            "description": "Type definition",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/DdicTypeResponse"
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "404": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/ddic/field/{table}/{field}": {
      "get": {
        "tags": [
          "discovery"
        ],
        "summary": "Inspect a field's semantics (data element / domain / fixed values)",
        "description": "Especially useful for status-code / type fields: fixed_values tells you which values are legal.",
        "parameters": [
          {
            "name": "table",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string"
            }
          },
          {
            "name": "field",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string"
            }
          },
          {
            "name": "lang",
            "in": "query",
            "required": false,
            "schema": {
              "type": "string",
              "default": "EN"
            }
          }
        ],
        "responses": {
          "200": {
            "description": "Field semantics",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/FieldSemanticsResponse"
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "404": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/table/read": {
      "post": {
        "tags": [
          "table"
        ],
        "summary": "Read transparent table data (no RFC_READ_TABLE plumbing)",
        "description": "Wraps RFC_READ_TABLE with ET_DATA (avoids the 512-byte truncation). rowcount defaults to 1000, max 10000. WHERE clauses are ABAP Open SQL fragments, one per array element.",
        "requestBody": {
          "required": true,
          "content": {
            "application/json": {
              "schema": {
                "$ref": "#/components/schemas/TableReadRequest"
              }
            }
          }
        },
        "responses": {
          "200": {
            "description": "Table rows",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/TableReadResponse"
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "404": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/registry": {
      "get": {
        "tags": [
          "registry"
        ],
        "summary": "List registered APIs (agent cross-session memory)",
        "description": "Every remote-enabled function module successfully written through this gateway is auto-registered here as a draft entry; entries carry intent / pitfalls / example invocations and survive restarts. Check this first in a new session before searching SAP. Tombstoned entries (their FM was deleted) are hidden unless include_deleted=true.",
        "parameters": [
          {
            "name": "q",
            "in": "query",
            "required": false,
            "schema": {
              "type": "string"
            },
            "description": "Substring filter on alias / func_name / intent (case-insensitive)"
          },
          {
            "name": "include_deleted",
            "in": "query",
            "required": false,
            "schema": {
              "type": "boolean",
              "default": false
            },
            "description": "Also list tombstoned entries"
          }
        ],
        "responses": {
          "200": {
            "description": "Registry entries (alias-sorted)",
            "content": {
              "application/json": {
                "schema": {
                  "type": "object",
                  "required": [
                    "count",
                    "version",
                    "entries"
                  ],
                  "properties": {
                    "count": {
                      "type": "integer"
                    },
                    "version": {
                      "type": "integer",
                      "description": "Storage schema version"
                    },
                    "entries": {
                      "type": "array",
                      "items": {
                        "$ref": "#/components/schemas/RegistryEntry"
                      }
                    }
                  }
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "503": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/registry/{alias}": {
      "get": {
        "tags": [
          "registry"
        ],
        "summary": "Read one registry entry",
        "parameters": [
          {
            "name": "alias",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string",
              "pattern": "^[a-z0-9_/-]{1,60}$"
            },
            "description": "Unique lowercase id (team/name prefixes allowed)"
          }
        ],
        "responses": {
          "200": {
            "description": "The entry",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/RegistryEntry"
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "404": {
            "$ref": "#/components/responses/Error"
          }
        }
      },
      "put": {
        "tags": [
          "registry"
        ],
        "summary": "Create/fully replace a registry entry",
        "description": "PUT is a FULL replace (GET first, merge, PUT back). Auto-registered drafts keep their origin. status accepts draft/published only (deletion goes through DELETE).",
        "parameters": [
          {
            "name": "alias",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string",
              "pattern": "^[a-z0-9_/-]{1,60}$"
            }
          }
        ],
        "requestBody": {
          "required": true,
          "content": {
            "application/json": {
              "schema": {
                "$ref": "#/components/schemas/RegistryPutRequest"
              }
            }
          }
        },
        "responses": {
          "200": {
            "description": "Entry after write + created flag",
            "content": {
              "application/json": {
                "schema": {
                  "type": "object",
                  "properties": {
                    "created": {
                      "type": "boolean"
                    },
                    "entry": {
                      "$ref": "#/components/schemas/RegistryEntry"
                    }
                  }
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          }
        }
      },
      "delete": {
        "tags": [
          "registry"
        ],
        "summary": "Tombstone (default) or physically remove an entry",
        "parameters": [
          {
            "name": "alias",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string"
            }
          },
          {
            "name": "purge",
            "in": "query",
            "required": false,
            "schema": {
              "type": "boolean",
              "default": false
            },
            "description": "true = physical removal (default tombstones for the record)"
          }
        ],
        "responses": {
          "200": {
            "description": "deleted / purged",
            "content": {
              "application/json": {
                "schema": {
                  "type": "object",
                  "properties": {
                    "alias": {
                      "type": "string"
                    },
                    "status": {
                      "type": "string",
                      "enum": [
                        "deleted",
                        "purged"
                      ]
                    }
                  }
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "404": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/invokes/audit": {
      "get": {
        "tags": [
          "registry"
        ],
        "summary": "Recent invoke audit trail (last ~500, newest first)",
        "description": "Ring buffer of the most recent calls across /api/rfc, typed invokes and flat invokes: timestamp, source endpoint (via), registry alias when applicable, function name, ok/status, duration in ms, caller IP. For quick troubleshooting without log access.",
        "responses": {
          "200": {
            "description": "Audit records",
            "content": {
              "application/json": {
                "schema": {
                  "type": "object",
                  "properties": {
                    "count": {
                      "type": "integer"
                    },
                    "records": {
                      "type": "array",
                      "items": {
                        "type": "object",
                        "properties": {
                          "at": { "type": "string", "description": "RFC3339 UTC" },
                          "via": { "type": "string", "enum": ["rfc", "functions-invoke", "invokes-alias"] },
                          "alias": { "type": "string", "nullable": true },
                          "func": { "type": "string" },
                          "ok": { "type": "boolean" },
                          "status": { "type": "integer" },
                          "ms": { "type": "integer" },
                          "ip": { "type": "string" }
                        }
                      }
                    }
                  }
                }
              }
            }
          },
          "401": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/invokes/{alias}": {
      "post": {
        "tags": [
          "registry"
        ],
        "summary": "Flat invoke of a registered API (consumer-facing delivery port)",
        "description": "Flat JSON in / flat JSON out — no SAP dialect needed. The contract is derived from the function's live interface metadata: input keys map to parameters (case-insensitive; structures are objects, tables are arrays of row objects), all outputs are returned by true type, output tables are row-capped (?limit= overrides; default 100, entry max_rows overrides the default). Response carries \"_truncated\" listing capped tables when truncation occurred. Unknown body keys are rejected with 400 (allowed keys listed). Alias comes from GET /api/registry; published entries also appear as fully typed operations in this spec.",
        "parameters": [
          {
            "name": "alias",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string"
            },
            "description": "Registry alias (unique lowercase id)"
          },
          {
            "name": "limit",
            "in": "query",
            "required": false,
            "schema": {
              "type": "integer",
              "default": 100,
              "maximum": 10000
            },
            "description": "Row cap for output tables"
          },
          {
            "name": "timeout_secs",
            "in": "query",
            "required": false,
            "schema": {
              "type": "integer",
              "minimum": 1
            },
            "description": "Per-call timeout override in seconds"
          }
        ],
        "requestBody": {
          "required": true,
          "content": {
            "application/json": {
              "schema": {
                "type": "object",
                "description": "Flat body: parameter name → value"
              }
            }
          }
        },
        "responses": {
          "200": {
            "description": "Flat result: output parameter name → value",
            "content": {
              "application/json": {
                "schema": {
                  "type": "object"
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "404": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          },
          "504": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/dumps": {
      "get": {
        "tags": [
          "dumps"
        ],
        "summary": "Structured short-dump list (parsed from the ADT Atom feed)",
        "description": "ST22 entries newest-first, parsed into structured items (no 45KB–1MB raw text). Each item's `key` plugs into /api/dumps/{key}/detail.",
        "parameters": [
          {
            "name": "from",
            "in": "query",
            "required": false,
            "schema": {
              "type": "string",
              "pattern": "^[0-9]{14}$"
            },
            "description": "Start time, UTC yyyyMMddHHmmss (server-side paging in ADT)"
          },
          {
            "name": "to",
            "in": "query",
            "required": false,
            "schema": {
              "type": "string",
              "pattern": "^[0-9]{14}$"
            },
            "description": "End time, same format"
          },
          {
            "name": "limit",
            "in": "query",
            "required": false,
            "schema": {
              "type": "integer",
              "default": 100,
              "minimum": 1,
              "maximum": 1000
            },
            "description": "Max entries processed"
          }
        ],
        "responses": {
          "200": {
            "description": "Dump list",
            "content": {
              "application/json": {
                "schema": {
                  "type": "object",
                  "required": [
                    "count",
                    "dumps"
                  ],
                  "properties": {
                    "count": {
                      "type": "integer"
                    },
                    "dumps": {
                      "type": "array",
                      "items": {
                        "$ref": "#/components/schemas/DumpEntry"
                      }
                    }
                  }
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          },
          "503": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/dumps/grouped": {
      "get": {
        "tags": [
          "dumps"
        ],
        "summary": "Dumps grouped by (error type, terminated program)",
        "description": "Answers \"what keeps failing\". Groups sorted by count desc; `latest_key` plugs straight into the detail endpoint.",
        "parameters": [
          {
            "name": "from",
            "in": "query",
            "required": false,
            "schema": {
              "type": "string",
              "pattern": "^[0-9]{14}$"
            }
          },
          {
            "name": "to",
            "in": "query",
            "required": false,
            "schema": {
              "type": "string",
              "pattern": "^[0-9]{14}$"
            }
          },
          {
            "name": "limit",
            "in": "query",
            "required": false,
            "schema": {
              "type": "integer",
              "default": 100,
              "minimum": 1,
              "maximum": 1000
            }
          }
        ],
        "responses": {
          "200": {
            "description": "Grouped dumps",
            "content": {
              "application/json": {
                "schema": {
                  "type": "object",
                  "required": [
                    "count",
                    "groups"
                  ],
                  "properties": {
                    "count": {
                      "type": "integer"
                    },
                    "groups": {
                      "type": "array",
                      "items": {
                        "$ref": "#/components/schemas/DumpGroup"
                      }
                    }
                  }
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          },
          "503": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/dumps/{key}/detail": {
      "get": {
        "tags": [
          "dumps"
        ],
        "summary": "One dump's structured detail (header / termination point / call stack)",
        "description": "Take `key` from the list endpoint's items. Raw %20-encoded or decoded keys both work. Note: OpenAPI cannot express a multi-segment path parameter — {key} may contain slashes.",
        "parameters": [
          {
            "name": "key",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string"
            }
          }
        ],
        "responses": {
          "200": {
            "description": "Dump detail",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/DumpDetail"
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "404": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          },
          "503": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/objects/{type}/{name}/source": {
      "get": {
        "tags": [
          "objects"
        ],
        "summary": "Read an object's source (ADT channel, original casing)",
        "description": "type ∈ prog|incl|class|intf|func|fugr|cds|tabl (package has no source; tabl returns the DDIC table DDL 'define table ...' form; stru the structure DDL 'define structure ...'). For func the group is auto-resolved when not given. Returns lines[] plus the joined source string.",
        "parameters": [
          {
            "name": "type",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string",
              "enum": [
                "prog",
                "incl",
                "class",
                "intf",
                "func",
                "fugr",
                "cds",
                "tabl",
                "stru"
              ]
            }
          },
          {
            "name": "name",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string"
            }
          }
        ],
        "responses": {
          "200": {
            "description": "Source lines",
            "content": {
              "application/json": {
                "schema": {
                  "type": "object",
                  "properties": {
                    "type": { "type": "string" },
                    "name": { "type": "string" },
                    "lines": { "type": "array", "items": { "type": "string" } },
                    "source": { "type": "string" },
                    "source_via": { "type": "string", "enum": ["adt"] }
                  }
                }
              }
            }
          },
          "400": { "$ref": "#/components/responses/Error" },
          "401": { "$ref": "#/components/responses/Error" },
          "404": { "$ref": "#/components/responses/Error" },
          "502": { "$ref": "#/components/responses/Error" }
        }
      },
      "put": {
        "tags": [
          "objects"
        ],
        "summary": "Write an object's full source (lock → put → unlock → activate, orchestrated)",
        "description": "type ∈ prog|incl|class|intf|func|fugr|cds|tabl (tabl/stru = DDIC table/structure DDL sources, written lock-free with etag optimistic concurrency); name may contain / (namespaced objects, e.g. /UI5/CL_X — OpenAPI cannot express multi-segment path params). Activation failure is a logical result (HTTP 200 + activated.messages), not a transport error. For func objects the function group is auto-resolved when not given; FM parameter signatures are part of the source (inline in the FUNCTION statement; classic *\" comment blocks are converted automatically); rfc_enabled:true additionally marks the module remote-enabled.",
        "parameters": [
          {
            "name": "type",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string",
              "enum": [
                "prog",
                "incl",
                "class",
                "intf",
                "func",
                "fugr",
                "cds",
                "tabl",
                "stru"
              ]
            }
          },
          {
            "name": "name",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string"
            }
          }
        ],
        "requestBody": {
          "required": true,
          "content": {
            "application/json": {
              "schema": {
                "$ref": "#/components/schemas/ObjectWriteBody"
              }
            }
          }
        },
        "responses": {
          "200": {
            "description": "Write outcome (check activated.success / activated.messages)",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/WriteOutcome"
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "404": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/objects/{type}/{name}/replace": {
      "post": {
        "tags": [
          "objects"
        ],
        "summary": "AI-style unique find-and-replace + activate",
        "description": "Reads the current source, replaces `old_string` with `new_string` (must match exactly once and change the content), then runs the same write orchestration as PUT /source.",
        "parameters": [
          {
            "name": "type",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string",
              "enum": [
                "prog",
                "incl",
                "class",
                "intf",
                "func",
                "fugr",
                "cds",
                "tabl",
                "stru"
              ]
            }
          },
          {
            "name": "name",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string"
            }
          }
        ],
        "requestBody": {
          "required": true,
          "content": {
            "application/json": {
              "schema": {
                "$ref": "#/components/schemas/ObjectReplaceBody"
              }
            }
          }
        },
        "responses": {
          "200": {
            "description": "Write outcome (replaced=true)",
            "content": {
              "application/json": {
                "schema": {
                  "$ref": "#/components/schemas/WriteOutcome"
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "404": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/objects/{type}/{name}/syntax": {
      "post": {
        "tags": [
          "objects"
        ],
        "summary": "Syntax-check source without writing it",
        "description": "Submits the given source for a syntax check (nothing is persisted).",
        "parameters": [
          {
            "name": "type",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string",
              "enum": [
                "prog",
                "incl",
                "class",
                "intf",
                "func",
                "fugr",
                "cds",
                "tabl",
                "stru"
              ]
            }
          },
          {
            "name": "name",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string"
            }
          }
        ],
        "requestBody": {
          "required": true,
          "content": {
            "application/json": {
              "schema": {
                "type": "object",
                "required": [
                  "source"
                ],
                "properties": {
                  "source": {
                    "type": "string"
                  }
                }
              }
            }
          }
        },
        "responses": {
          "200": {
            "description": "Syntax issues (empty list = clean)",
            "content": {
              "application/json": {
                "schema": {
                  "type": "object",
                  "required": [
                    "type",
                    "name",
                    "count",
                    "issues"
                  ],
                  "properties": {
                    "type": {
                      "type": "string"
                    },
                    "name": {
                      "type": "string"
                    },
                    "count": {
                      "type": "integer"
                    },
                    "issues": {
                      "type": "array",
                      "items": {
                        "$ref": "#/components/schemas/SyntaxIssue"
                      }
                    }
                  }
                }
              }
            }
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "404": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    },
    "/api/objects/{type}/{name}/create": {
      "post": {
        "tags": [
          "objects"
        ],
        "summary": "Create an object shell (ADT-first; prog/func fall back to RFC)",
        "description": "type ∈ prog|incl|class|intf|func|fugr|cds|tabl|stru|package. Creates the object via the standard ADT objectcreation XML (Eclipse/vsp/abapfs contract); prog/func fall back to the RFC RPY insert path when ADT fails. func auto-creates its function group when missing. package: devclass = parent package, software_component optional (candidates ZLOCAL→LOCAL→HOME). Optional `source` writes+activates the first version in one call; rfc_enabled marks a func remote-enabled.",
        "parameters": [
          {
            "name": "type",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string",
              "enum": [
                "prog",
                "incl",
                "class",
                "intf",
                "func",
                "fugr",
                "cds",
                "tabl",
                "stru",
                "package"
              ]
            }
          },
          {
            "name": "name",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string"
            }
          }
        ],
        "requestBody": {
          "required": true,
          "content": {
            "application/json": {
              "schema": {
                "type": "object",
                "required": [
                  "description"
                ],
                "properties": {
                  "description": {
                    "type": "string",
                    "description": "Object title / short text (required by SAP)"
                  },
                  "devclass": {
                    "type": "string",
                    "description": "Package (default $TMP); for package type: parent package"
                  },
                  "transport": {
                    "type": "string"
                  },
                  "software_component": {
                    "type": "string",
                    "description": "package only: software component"
                  },
                  "source": {
                    "type": "string",
                    "description": "Optional first source (written + activated in one call)"
                  },
                  "activate": {
                    "type": "boolean",
                    "default": true
                  },
                  "rfc_enabled": {
                    "type": "boolean",
                    "description": "func only: mark remote-enabled after the first write"
                  }
                }
              }
            }
          }
        },
        "responses": {
          "200": {
            "description": "created=true (+write when source given)",
            "content": {
              "application/json": {
                "schema": {
                  "type": "object",
                  "properties": {
                    "created": { "type": "boolean" },
                    "type": { "type": "string" },
                    "name": { "type": "string" },
                    "write": { "$ref": "#/components/schemas/WriteOutcome" }
                  }
                }
              }
            }
          },
          "400": { "$ref": "#/components/responses/Error" },
          "401": { "$ref": "#/components/responses/Error" },
          "409": { "$ref": "#/components/responses/Error" },
          "502": { "$ref": "#/components/responses/Error" }
        }
      }
    },
    "/api/objects/{type}/{name}": {
      "delete": {
        "tags": [
          "objects"
        ],
        "summary": "Delete an object (lock → DELETE → done)",
        "description": "type ∈ prog|incl|class|intf|func|fugr|cds|tabl|stru|package. Deleting a fugr removes its function modules too. Query params: group (func only), transport. Irreversible.",
        "parameters": [
          {
            "name": "type",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string",
              "enum": [
                "prog",
                "incl",
                "class",
                "intf",
                "func",
                "fugr",
                "cds",
                "tabl",
                "stru",
                "package"
              ]
            }
          },
          {
            "name": "name",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string"
            }
          },
          {
            "name": "group",
            "in": "query",
            "required": false,
            "schema": {
              "type": "string"
            },
            "description": "Function group (func only; auto-resolved when omitted)"
          },
          {
            "name": "transport",
            "in": "query",
            "required": false,
            "schema": {
              "type": "string"
            }
          }
        ],
        "responses": {
          "200": {
            "description": "deleted=true",
            "content": {
              "application/json": {
                "schema": {
                  "type": "object",
                  "properties": {
                    "type": { "type": "string" },
                    "name": { "type": "string" },
                    "deleted": { "type": "boolean" }
                  }
                }
              }
            }
          },
          "400": { "$ref": "#/components/responses/Error" },
          "401": { "$ref": "#/components/responses/Error" },
          "409": { "$ref": "#/components/responses/Error" },
          "502": { "$ref": "#/components/responses/Error" }
        }
      }
    },
    "/api/adt/{path}": {
      "get": {
        "tags": [
          "adt"
        ],
        "summary": "ADT REST proxy (any method: GET/POST/PUT/DELETE/PATCH)",
        "description": "Transparently proxies the SAP system's ADT REST API (/sap/bc/adt/** on ICF — the same API Eclipse uses). The URL after /api/adt/ maps 1:1 to the ADT path; Accept/Content-Type/If-Match headers and body are forwarded, responses passed through verbatim (mostly XML). Write methods get X-CSRF-Token + session handling automatically (retries once on 403). Gateway-side failures use the JSON error contract (400 ADT_PATH_INVALID / 502 ADT_UNREACHABLE / 503 ADT_DISABLED / 504 ADT_TIMEOUT); other statuses come from ADT itself. Note: OpenAPI cannot express a multi-segment path parameter — {path} here may contain slashes (e.g. runtime/dumps).",
        "parameters": [
          {
            "name": "path",
            "in": "path",
            "required": true,
            "schema": {
              "type": "string"
            },
            "description": "ADT sub-path, e.g. runtime/dumps or oo/classes/CL_RUNTIME_ERROR/source/main"
          }
        ],
        "responses": {
          "200": {
            "description": "ADT response passed through verbatim"
          },
          "400": {
            "$ref": "#/components/responses/Error"
          },
          "401": {
            "$ref": "#/components/responses/Error"
          },
          "502": {
            "$ref": "#/components/responses/Error"
          },
          "503": {
            "$ref": "#/components/responses/Error"
          },
          "504": {
            "$ref": "#/components/responses/Error"
          }
        }
      }
    }
  },
  "components": {
    "schemas": {
      "HealthStatus": {
        "type": "object",
        "properties": {
          "status": {
            "type": "string",
            "example": "ok"
          },
          "version": {
            "type": "string",
            "description": "Gateway version (same as /api/version, convenience for pollers)"
          }
        }
      },
      "VersionInfo": {
        "type": "object",
        "properties": {
          "name": {
            "type": "string",
            "example": "sap-for-agents"
          },
          "version": {
            "type": "string",
            "example": "0.10.0"
          },
          "commit": {
            "type": "string",
            "description": "Git short commit the binary was built from; '-dirty' suffix when the working tree had uncommitted changes, 'unknown' when built without git",
            "example": "0fa6d6b"
          },
          "capabilities": {
            "type": "object",
            "description": "Deployment switches that change agent behavior",
            "properties": {
              "auth": {
                "type": "boolean",
                "description": "true = SAP_API_KEY is set; /api/* requires Bearer token (this endpoint excepted)"
              },
              "read_only": {
                "type": "boolean",
                "description": "true = SAP_READ_ONLY; gateway write endpoints return 403 READ_ONLY"
              },
              "adt": {
                "type": "boolean",
                "description": "true = ADT proxy (/api/adt/**, /api/dumps*) enabled"
              },
              "rate_limit_rps": {
                "type": "integer",
                "nullable": true,
                "description": "Per-IP requests/second cap; null = unlimited"
              }
            }
          },
          "latest": {
            "nullable": true,
            "type": "object",
            "description": "Latest GitHub release, from a background check (startup + every 24h, one anonymous GET — disable with SAP_UPDATE_CHECK=off). null when not fetched / disabled / unreachable; never blocks the response",
            "properties": {
              "version": {
                "type": "string",
                "example": "0.11.0"
              },
              "url": {
                "type": "string",
                "description": "Release page URL"
              },
              "update_available": {
                "type": "boolean",
                "description": "true = the running version is older than this release"
              }
            }
          },
          "sap": {
            "nullable": true,
            "type": "object",
            "description": "Target SAP system info, cached from the first successful fetch; null when SAP is currently unreachable",
            "properties": {
              "sysid": {
                "type": "string",
                "example": "A4H"
              },
              "release": {
                "type": "string",
                "description": "SAP release (RFCSI SAPRL, e.g. 753/816). Reflects the kernel/Basis level only — to distinguish ECC vs S/4HANA, check the CVERS table for the S4CORE component (POST /api/table/read)",
                "example": "816"
              },
              "host": {
                "type": "string",
                "description": "Application server hostname"
              },
              "os": {
                "type": "string",
                "example": "Linux"
              },
              "destination": {
                "type": "string",
                "description": "RFC destination name (e.g. vhcala4hci_A4H_00)"
              },
              "client": {
                "type": "string",
                "description": "Login client (SAP_CLIENT config)",
                "example": "001"
              }
            }
          },
          "sap_error": {
            "type": "string",
            "description": "Present when sap is null: why the SAP info fetch failed (retryable on next call)"
          }
        }
      },
      "ReadyStatus": {
        "type": "object",
        "properties": {
          "status": {
            "type": "string",
            "enum": [
              "ready",
              "unavailable",
              "timeout"
            ]
          },
          "sap": {
            "type": "string",
            "description": "Present (\"ok\") on success"
          },
          "code": {
            "type": "integer",
            "description": "SAP error code on failure"
          },
          "message": {
            "type": "string"
          },
          "timeout_ms": {
            "type": "integer",
            "description": "Present on probe timeout"
          }
        }
      },
      "ScalarValue": {
        "description": "Scalar parameter value. Implicit dispatch by JSON type: string → CHAR/NUM/DATE/TIME/BCD-as-string, integer → INT, float → FLOAT. Explicit type markers for BCD/INT8/binary: {\"type\":\"BCD\",\"value\":\"123.45\"}, {\"type\":\"INT8\",\"value\":123}, {\"type\":\"BYTES\",\"value\":\"<base64>\"}.",
        "oneOf": [
          {
            "type": "string"
          },
          {
            "type": "integer",
            "format": "int32"
          },
          {
            "type": "integer",
            "format": "int64"
          },
          {
            "type": "number",
            "format": "double"
          },
          {
            "$ref": "#/components/schemas/TypedScalar"
          }
        ]
      },
      "TypedScalar": {
        "type": "object",
        "required": [
          "type",
          "value"
        ],
        "properties": {
          "type": {
            "type": "string",
            "enum": [
              "BCD",
              "INT8",
              "BYTES"
            ]
          },
          "value": {
            "description": "BCD → numeric string; INT8 → integer; BYTES → Base64 string",
            "oneOf": [
              {
                "type": "string"
              },
              {
                "type": "integer",
                "format": "int64"
              }
            ]
          }
        }
      },
      "MaxLen": {
        "description": "String-output max length. Legacy form: plain integer. Detailed form: {\"max_len\": 255} (null = auto-discover from metadata).",
        "oneOf": [
          {
            "type": "integer",
            "minimum": 0
          },
          {
            "type": "object",
            "properties": {
              "max_len": {
                "type": "integer",
                "nullable": true,
                "minimum": 0
              }
            }
          }
        ]
      },
      "FieldSpec": {
        "type": "object",
        "required": [
          "name"
        ],
        "properties": {
          "name": {
            "type": "string",
            "description": "Field name (uppercase)"
          },
          "max_len": {
            "type": "integer",
            "nullable": true,
            "description": "Omit for auto-discovery from metadata"
          },
          "auto": {
            "type": "boolean",
            "default": false,
            "description": "true = read by the field's true DDIC type (INT→integer, FLOAT→float, INT8→i64, BYTE/XSTRING→Base64), false (default) = string"
          }
        }
      },
      "FieldDef": {
        "type": "object",
        "description": "Nested field definition (recursive for structures/tables)",
        "required": [
          "name",
          "type",
          "length"
        ],
        "properties": {
          "name": {
            "type": "string"
          },
          "type": {
            "type": "string",
            "enum": [
              "CHAR",
              "DATE",
              "BCD",
              "TIME",
              "BYTE",
              "TABLE",
              "NUM",
              "FLOAT",
              "INT",
              "INT2",
              "INT1",
              "STRUCTURE",
              "STRING",
              "XSTRING",
              "INT8",
              "UNKNOWN"
            ]
          },
          "length": {
            "type": "integer"
          },
          "decimals": {
            "type": "integer"
          },
          "description": {
            "type": "string"
          },
          "fields": {
            "type": "array",
            "nullable": true,
            "items": {
              "$ref": "#/components/schemas/FieldDef"
            }
          }
        }
      },
      "InvokeRequest": {
        "type": "object",
        "required": [
          "func_name"
        ],
        "properties": {
          "func_name": {
            "type": "string",
            "description": "RFC function module name, UPPERCASE, e.g. BAPI_USER_GETLIST. Max 30 chars, [A-Za-z0-9_], optional /NS/ namespace prefix."
          },
          "inputs": {
            "type": "object",
            "description": "Scalar IMPORT/CHANGING parameters: name → value",
            "additionalProperties": {
              "$ref": "#/components/schemas/ScalarValue"
            }
          },
          "table_inputs": {
            "type": "object",
            "description": "TABLES input parameters: name → array of rows; each row is {field: value}. Max 100000 rows per table.",
            "additionalProperties": {
              "type": "array",
              "items": {
                "type": "object",
                "additionalProperties": {
                  "$ref": "#/components/schemas/ScalarValue"
                }
              }
            }
          },
          "struct_inputs": {
            "type": "object",
            "description": "Top-level IMPORTING structure parameters: name → {field: value}, e.g. BAPI_USER_CREATE.ADDRESS",
            "additionalProperties": {
              "type": "object",
              "additionalProperties": {
                "$ref": "#/components/schemas/ScalarValue"
              }
            }
          },
          "int_outputs": {
            "type": "array",
            "description": "EXPORT integer parameter names to read",
            "items": {
              "type": "string"
            }
          },
          "string_outputs": {
            "type": "object",
            "description": "EXPORT string parameter names to read → max length. A STRUCTURE-typed parameter is read into structs by its sub-fields automatically.",
            "additionalProperties": {
              "$ref": "#/components/schemas/MaxLen"
            }
          },
          "auto_outputs": {
            "type": "array",
            "description": "EXPORT scalar parameter names to read by their true metadata type (INT→integer, FLOAT→float, INT8→i64, BCD→string, BYTE/XSTRING→Base64)",
            "items": {
              "type": "string"
            }
          },
          "table_outputs": {
            "type": "object",
            "description": "EXPORT/TABLES tables to traverse: name → field list. Output rows are capped at 10000 (use /api/table/read for pagination).",
            "additionalProperties": {
              "type": "array",
              "items": {
                "$ref": "#/components/schemas/FieldSpec"
              }
            }
          },
          "struct_outputs": {
            "type": "object",
            "description": "Top-level structure outputs: name → field list (same rules as table_outputs)",
            "additionalProperties": {
              "type": "array",
              "items": {
                "$ref": "#/components/schemas/FieldSpec"
              }
            }
          },
          "read_return": {
            "type": "boolean",
            "default": false,
            "description": "Automatically read the BAPI RETURN message table (rows with TYPE=E indicate errors)"
          },
          "timeout_secs": {
            "type": "integer",
            "minimum": 1,
            "maximum": 1800,
            "description": "Per-call timeout in seconds. Omit to use the global default (60s); relax it for slow endpoints (batch BAPIs, large tables). 504 on timeout."
          }
        }
      },
      "InvokeResponse": {
        "type": "object",
        "required": [
          "func",
          "scalars",
          "tables"
        ],
        "properties": {
          "func": {
            "type": "string",
            "description": "Echoed function name"
          },
          "scalars": {
            "type": "object",
            "description": "Scalar outputs; value type depends on the read method",
            "additionalProperties": {
              "$ref": "#/components/schemas/ScalarValue"
            }
          },
          "tables": {
            "type": "object",
            "description": "Table outputs: name → array of rows ({field: value}); auto:true fields keep native types, others are strings",
            "additionalProperties": {
              "type": "array",
              "items": {
                "type": "object",
                "additionalProperties": {
                  "$ref": "#/components/schemas/ScalarValue"
                }
              }
            }
          },
          "structs": {
            "type": "object",
            "description": "Top-level structure outputs (same value-type rules as tables)",
            "additionalProperties": {
              "type": "object",
              "additionalProperties": {
                "$ref": "#/components/schemas/ScalarValue"
              }
            }
          },
          "return_table": {
            "type": "array",
            "nullable": true,
            "description": "RETURN messages (when read_return=true and the table exists). TYPE=E rows are errors.",
            "items": {
              "type": "object",
              "properties": {
                "TYPE": {
                  "type": "string"
                },
                "ID": {
                  "type": "string"
                },
                "NUMBER": {
                  "type": "string"
                },
                "MESSAGE": {
                  "type": "string"
                }
              }
            }
          }
        }
      },
      "SearchRequest": {
        "type": "object",
        "properties": {
          "pattern": {
            "type": "string",
            "description": "Function name wildcard, * matches anything, e.g. BAPI_USER_*"
          },
          "group": {
            "type": "string",
            "description": "Function group filter (optional)"
          },
          "max_results": {
            "type": "integer",
            "default": 50,
            "maximum": 500,
            "description": "Max entries returned (default 50, cap 500)"
          }
        }
      },
      "SearchResponse": {
        "type": "object",
        "required": [
          "pattern",
          "count",
          "functions"
        ],
        "properties": {
          "pattern": {
            "type": "string"
          },
          "count": {
            "type": "integer"
          },
          "functions": {
            "type": "array",
            "items": {
              "$ref": "#/components/schemas/SearchFunctionEntry"
            }
          }
        }
      },
      "SearchFunctionEntry": {
        "type": "object",
        "required": [
          "name"
        ],
        "properties": {
          "name": {
            "type": "string"
          },
          "group": {
            "type": "string"
          },
          "description": {
            "type": "string"
          }
        }
      },
      "FunctionParam": {
        "type": "object",
        "required": [
          "name",
          "type",
          "direction",
          "length",
          "optional"
        ],
        "properties": {
          "name": {
            "type": "string",
            "description": "Use this exact UPPERCASE name when invoking"
          },
          "type": {
            "type": "string",
            "enum": [
              "CHAR",
              "DATE",
              "BCD",
              "TIME",
              "BYTE",
              "TABLE",
              "NUM",
              "FLOAT",
              "INT",
              "INT2",
              "INT1",
              "STRUCTURE",
              "STRING",
              "XSTRING",
              "INT8",
              "UNKNOWN"
            ]
          },
          "direction": {
            "type": "string",
            "enum": [
              "IMPORT",
              "EXPORT",
              "CHANGING",
              "TABLES",
              "UNKNOWN"
            ]
          },
          "length": {
            "type": "integer"
          },
          "decimals": {
            "type": "integer"
          },
          "optional": {
            "type": "boolean"
          },
          "default": {
            "type": "string"
          },
          "description": {
            "type": "string"
          },
          "fields": {
            "type": "array",
            "nullable": true,
            "items": {
              "$ref": "#/components/schemas/FieldDef"
            }
          }
        }
      },
      "FunctionInterface": {
        "type": "object",
        "required": [
          "name",
          "params"
        ],
        "properties": {
          "name": {
            "type": "string"
          },
          "params": {
            "type": "array",
            "items": {
              "$ref": "#/components/schemas/FunctionParam"
            }
          },
          "interface_via": {
            "type": "string",
            "enum": ["fii", "sdk"],
            "description": "Which channel served the interface: fii = live server-side read (always fresh, default), sdk = SDK descriptor cache (fallback when FII is unavailable)"
          }
        }
      },
      "ParamDoc": {
        "type": "object",
        "properties": {
          "name": {
            "type": "string"
          },
          "text": {
            "type": "string"
          }
        }
      },
      "FunctionDocResponse": {
        "type": "object",
        "required": [
          "name",
          "parameter_docs"
        ],
        "properties": {
          "name": {
            "type": "string"
          },
          "short_text": {
            "type": "string"
          },
          "long_text": {
            "type": "string",
            "description": "Full SE37 documentation; may be empty (normal)"
          },
          "warning": {
            "type": "string",
            "nullable": true,
            "description": "Present when documentation could not be read"
          },
          "parameter_docs": {
            "type": "array",
            "items": {
              "$ref": "#/components/schemas/ParamDoc"
            }
          }
        }
      },
      "SourceResponse": {
        "type": "object",
        "required": [
          "name",
          "count",
          "lines",
          "source_via"
        ],
        "properties": {
          "name": {
            "type": "string"
          },
          "count": {
            "type": "integer",
            "description": "Number of source lines"
          },
          "lines": {
            "type": "array",
            "items": {
              "type": "string"
            }
          },
          "source_via": {
            "type": "string",
            "enum": [
              "rfc",
              "adt"
            ],
            "description": "Which channel served the lines: rfc (RPY_*_READ) or adt (fallback after RPY failure)"
          },
          "prologue": {
            "description": "Only with /api/functions/{name}/source?prologue=true: signatures of the function's CALL FUNCTION dependencies",
            "nullable": true
          }
        }
      },
      "DdicTypeResponse": {
        "type": "object",
        "required": [
          "name",
          "fields"
        ],
        "properties": {
          "name": {
            "type": "string"
          },
          "fields": {
            "type": "array",
            "items": {
              "$ref": "#/components/schemas/FieldDef"
            }
          }
        }
      },
      "FixedValueDto": {
        "type": "object",
        "properties": {
          "value": {
            "type": "string"
          },
          "text": {
            "type": "string"
          }
        }
      },
      "FieldSemanticsResponse": {
        "type": "object",
        "required": [
          "table",
          "field"
        ],
        "properties": {
          "table": {
            "type": "string"
          },
          "field": {
            "type": "string"
          },
          "data_element": {
            "type": "string"
          },
          "domain": {
            "type": "string"
          },
          "check_table": {
            "type": "string"
          },
          "description": {
            "type": "string"
          },
          "medium_label": {
            "type": "string"
          },
          "fixed_values": {
            "type": "array",
            "items": {
              "$ref": "#/components/schemas/FixedValueDto"
            }
          }
        }
      },
      "TableReadRequest": {
        "type": "object",
        "required": [
          "table",
          "fields"
        ],
        "properties": {
          "table": {
            "type": "string",
            "description": "Transparent table name, e.g. T000, USR01"
          },
          "fields": {
            "type": "array",
            "items": {
              "type": "string"
            },
            "description": "Fields to select (determines column order and field-name mapping)"
          },
          "where": {
            "type": "array",
            "items": {
              "type": "string"
            },
            "description": "WHERE conditions (ABAP Open SQL fragments, one per element; empty = no filter)"
          },
          "rowcount": {
            "type": "integer",
            "default": 1000,
            "maximum": 10000,
            "description": "Max rows returned"
          },
          "delimiter": {
            "type": "string",
            "description": "Field delimiter for parsing (default \\u0001; change only if values contain it)"
          }
        }
      },
      "RegistryEntry": {
        "type": "object",
        "required": [
          "alias",
          "func_name",
          "status",
          "origin",
          "created_at",
          "updated_at"
        ],
        "properties": {
          "alias": {
            "type": "string",
            "description": "Unique lowercase URL-safe id (team/name prefixes allowed)"
          },
          "func_name": {
            "type": "string",
            "description": "SAP function module name (uppercase, remote-enabled)"
          },
          "group": {
            "type": "string",
            "description": "Function group (informational)"
          },
          "intent": {
            "type": "string",
            "description": "What this API is for, who consumes it (agent-written)"
          },
          "notes": {
            "type": "string",
            "description": "Pitfalls / know-how from previous sessions (agent-written)"
          },
          "doc": {
            "type": "string",
            "description": "Consumer-facing Markdown documentation; rendered in the OpenAPI catalog's operation description"
          },
          "example": {
            "type": "object",
            "description": "Sample invoke body for POST /api/functions/{func_name}/invoke"
          },
          "status": {
            "type": "string",
            "enum": [
              "draft",
              "published",
              "deleted"
            ],
            "description": "deleted = tombstone (FM was deleted; record kept)"
          },
          "origin": {
            "type": "string",
            "enum": [
              "auto",
              "manual"
            ],
            "description": "auto = created by the write hook on rfc_enabled FM writes"
          },
          "created_at": {
            "type": "string",
            "description": "RFC3339 UTC"
          },
          "updated_at": {
            "type": "string",
            "description": "RFC3339 UTC"
          }
        }
      },
      "RegistryPutRequest": {
        "type": "object",
        "required": [
          "func_name"
        ],
        "properties": {
          "func_name": {
            "type": "string",
            "description": "SAP function module name (uppercase)"
          },
          "group": {
            "type": "string",
            "description": "Function group (optional)"
          },
          "intent": {
            "type": "string",
            "description": "What this API is for (default empty)"
          },
          "notes": {
            "type": "string",
            "description": "Pitfalls / usage notes (default empty)"
          },
          "doc": {
            "type": "string",
            "description": "Consumer-facing Markdown documentation (default empty)"
          },
          "example": {
            "type": "object",
            "description": "Sample invoke body (default null = cleared)"
          },
          "status": {
            "type": "string",
            "enum": [
              "draft",
              "published"
            ],
            "default": "draft"
          }
        }
      },
      "TableReadResponse": {
        "type": "object",
        "required": [
          "table",
          "fields",
          "count",
          "rows"
        ],
        "properties": {
          "table": {
            "type": "string"
          },
          "fields": {
            "type": "array",
            "items": {
              "type": "string"
            }
          },
          "count": {
            "type": "integer"
          },
          "rows": {
            "type": "array",
            "items": {
              "type": "object",
              "description": "Field name → value (all values are strings)",
              "additionalProperties": {
                "type": "string"
              }
            }
          }
        }
      },
      "DumpEntry": {
        "type": "object",
        "required": [
          "key",
          "at"
        ],
        "properties": {
          "key": {
            "type": "string",
            "description": "Plug into /api/dumps/{key}/detail"
          },
          "at": {
            "type": "string",
            "description": "Timestamp"
          },
          "user": {
            "type": "string",
            "nullable": true
          },
          "error_type": {
            "type": "string",
            "nullable": true
          },
          "program": {
            "type": "string",
            "nullable": true
          },
          "message": {
            "type": "string",
            "nullable": true
          },
          "id": {
            "type": "string",
            "nullable": true
          },
          "at_secs": {
            "type": "integer",
            "nullable": true,
            "description": "Unix seconds, when available"
          }
        }
      },
      "DumpGroup": {
        "type": "object",
        "required": [
          "error_type",
          "program",
          "count",
          "first",
          "last",
          "users",
          "latest_key",
          "latest_message"
        ],
        "properties": {
          "error_type": {
            "type": "string"
          },
          "program": {
            "type": "string"
          },
          "count": {
            "type": "integer"
          },
          "first": {
            "type": "string"
          },
          "last": {
            "type": "string"
          },
          "users": {
            "type": "array",
            "items": {
              "type": "string"
            }
          },
          "latest_key": {
            "type": "string"
          },
          "latest_message": {
            "type": "string"
          }
        }
      },
      "DumpFrame": {
        "type": "object",
        "properties": {
          "position": {
            "type": "integer"
          },
          "type_name": {
            "type": "string"
          },
          "program": {
            "type": "string"
          },
          "include": {
            "type": "string"
          },
          "line": {
            "type": "integer"
          },
          "name": {
            "type": "string"
          }
        }
      },
      "DumpDetail": {
        "type": "object",
        "required": [
          "key",
          "error_type",
          "exception",
          "program",
          "component",
          "include",
          "line",
          "procedure",
          "main_program",
          "stack",
          "header"
        ],
        "properties": {
          "key": {
            "type": "string"
          },
          "error_type": {
            "type": "string"
          },
          "exception": {
            "type": "string"
          },
          "program": {
            "type": "string"
          },
          "component": {
            "type": "string"
          },
          "include": {
            "type": "string"
          },
          "line": {
            "type": "integer"
          },
          "procedure": {
            "type": "string"
          },
          "main_program": {
            "type": "string"
          },
          "stack": {
            "type": "array",
            "items": {
              "$ref": "#/components/schemas/DumpFrame"
            }
          },
          "header": {
            "type": "object",
            "additionalProperties": {
              "type": "string"
            },
            "description": "Full ST22 header table (field → value)"
          }
        }
      },
      "ObjectWriteBody": {
        "type": "object",
        "required": [
          "source"
        ],
        "properties": {
          "source": {
            "type": "string",
            "description": "Full source text"
          },
          "transport": {
            "type": "string",
            "nullable": true,
            "description": "Transport request number; omitted = reuse the object's bound request"
          },
          "activate": {
            "type": "boolean",
            "default": true
          },
          "group": {
            "type": "string",
            "nullable": true,
            "description": "Function group (func objects only); omitted = auto-resolved via RFC_FUNCTION_SEARCH"
          },
          "rfc_enabled": {
            "type": "boolean",
            "default": false,
            "description": "func only: mark the function module remote-enabled (processingType=rfc) under the same lock"
          }
        }
      },
      "ObjectReplaceBody": {
        "type": "object",
        "required": [
          "old_string",
          "new_string"
        ],
        "properties": {
          "old_string": {
            "type": "string",
            "description": "Must match exactly once in the current source"
          },
          "new_string": {
            "type": "string"
          },
          "transport": {
            "type": "string",
            "nullable": true
          },
          "activate": {
            "type": "boolean",
            "default": true
          },
          "group": {
            "type": "string",
            "nullable": true,
            "description": "Function group hint (func objects only)"
          },
          "rfc_enabled": {
            "type": "boolean",
            "default": false,
            "description": "func only: mark remote-enabled after the write"
          }
        }
      },
      "ActivationMessage": {
        "type": "object",
        "properties": {
          "severity": {
            "type": "string"
          },
          "line": {
            "type": "integer"
          },
          "text": {
            "type": "string"
          }
        }
      },
      "ActivationOutcome": {
        "type": "object",
        "properties": {
          "success": {
            "type": "boolean"
          },
          "activation_executed": {
            "type": "boolean"
          },
          "messages": {
            "type": "array",
            "items": {
              "$ref": "#/components/schemas/ActivationMessage"
            }
          },
          "inactive": {
            "type": "array",
            "items": {
              "type": "string"
            }
          },
          "problems": {
            "type": "array",
            "items": {
              "type": "string"
            }
          }
        }
      },
      "WriteOutcome": {
        "type": "object",
        "required": [
          "obj_type",
          "name",
          "source_url",
          "written",
          "warnings"
        ],
        "properties": {
          "obj_type": {
            "type": "string"
          },
          "name": {
            "type": "string"
          },
          "group": {
            "type": "string",
            "nullable": true
          },
          "source_url": {
            "type": "string",
            "description": "ADT source URL of the object"
          },
          "written": {
            "type": "boolean"
          },
          "transport_used": {
            "type": "string",
            "nullable": true
          },
          "activated": {
            "$ref": "#/components/schemas/ActivationOutcome"
          },
          "warnings": {
            "type": "array",
            "items": {
              "type": "string"
            }
          }
        }
      },
      "SyntaxIssue": {
        "type": "object",
        "properties": {
          "severity": {
            "type": "string"
          },
          "line": {
            "type": "integer"
          },
          "offset": {
            "type": "integer"
          },
          "text": {
            "type": "string"
          }
        }
      },
      "ErrorBody": {
        "type": "object",
        "properties": {
          "error": {
            "type": "object",
            "required": [
              "code",
              "message"
            ],
            "properties": {
              "code": {
                "type": "integer",
                "description": "Equals the HTTP status code"
              },
              "message": {
                "type": "string"
              },
              "key": {
                "type": "string",
                "description": "Machine-readable code, e.g. FU_NOT_FOUND / AUTH_INVALID / RATE_LIMITED / JSON_INVALID"
              }
            }
          }
        }
      }
    },
    "responses": {
      "Error": {
        "description": "Error (4xx = caller-side, 5xx = SAP/network). Branch on error.code (HTTP status) and error.key (machine code).",
        "content": {
          "application/json": {
            "schema": {
              "$ref": "#/components/schemas/ErrorBody"
            }
          }
        }
      }
    }
  }
}
"##;

/// GET /openapi.json —— 返回 OpenAPI 3.0.3 规范（免鉴权，公开页）。
/// v0.13 起并入注册表 published 条目的平坦调用 operation（服务目录）——
/// 读的是后台 watcher 预热的缓存，不碰 SAP：公开页永远秒回、SAP 宕机不受影响。
pub async fn openapi_handler(
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    // 与 index_handler 同策略：从 Host 头推导访问地址（servers 字段）
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("127.0.0.1:3000");
    let base = format!("http://{host}");
    let auth_enabled = crate::auth::is_enabled();
    let mut spec = build_spec(&base, auth_enabled);
    if let Some(ops) = registry_ops_cached().await {
        if !ops.is_empty() {
            if let Some(paths) = spec["paths"].as_object_mut() {
                for (path, op) in ops.iter() {
                    paths.insert(path.clone(), op.clone());
                }
            }
            if auth_enabled {
                mark_api_security(&mut spec);
            }
            spec["info"]["description"] = json!(format!(
                "{} — This document additionally contains the API registry service catalog \
                 (each published entry maps to POST /api/invokes/{{alias}}, flat JSON in/out).",
                spec["info"]["description"].as_str().unwrap_or_default()
            ));
        }
    }
    axum::Json(spec).into_response()
}

/// 构建完整规范。`base_url` 进 servers；`auth_enabled` 决定是否声明 Bearer 安全方案。
/// pub 供单元测试直接断言结构。
pub fn build_spec(base_url: &str, auth_enabled: bool) -> Value {
    let mut spec: Value =
        serde_json::from_str(SPEC_JSON).expect("内置 SPEC_JSON 必须是合法 JSON（单测锁定）");
    // 运行时插值：访问地址 + crate 版本
    spec["servers"][0]["url"] = json!(base_url);
    spec["info"]["version"] = json!(env!("CARGO_PKG_VERSION"));

    // 认证方案 + /api/* 操作的安全标记（合并进来的动态 operation 也走同一段，
    // 见 mark_api_security——两个调用方共用）
    if auth_enabled {
        spec["components"]["securitySchemes"] = json!({
            "bearerAuth": {
                "type": "http",
                "scheme": "bearer",
                "description": "Authorization: Bearer <SAP_API_KEY>. Applies to /api/* endpoints only; probes and public pages stay open."
            }
        });
        mark_api_security(&mut spec);
    }

    spec
}

/// 给 spec 里全部 `/api/*` operation 打上 bearerAuth 标记（`/api/version` 公开例外）。
/// build_spec 与动态合并（/api/openapi、注册表目录）共用，保证鉴权模式下
/// 生成器产出的客户端会带 token。
fn mark_api_security(spec: &mut Value) {
    if let Some(paths) = spec["paths"].as_object_mut() {
        for (path, item) in paths.iter_mut() {
            if !path.starts_with("/api/") {
                continue;
            }
            // /api/version 刻意公开：Agent 需要在拿到 token 之前知道
            // capabilities.auth（要不要 token）
            if path == "/api/version" {
                continue;
            }
            if let Some(ops) = item.as_object_mut() {
                for (_method, op) in ops.iter_mut() {
                    op["security"] = json!([{ "bearerAuth": [] }]);
                }
            }
        }
    }
}

// ========================================================================
// 动态规范：按函数生成类型化 operation（GET /api/openapi?functions=A,B）
// ========================================================================

use crate::api::{FieldDef, FunctionParam};

/// RFCTYPE 名称（静态规范同款命名，如 "CHAR"/"INT"/"TABLE"）→ JSON Schema type。
/// 与 /api/rfc 的取值语义一致：BCD 进出都是字符串，BYTE/XSTRING 是 Base64 字符串。
fn schema_type_for(type_name: &str) -> &'static str {
    match type_name {
        "INT" | "INT1" | "INT2" | "INT8" => "integer",
        "FLOAT" => "number",
        _ => "string",
    }
}

/// 字段列表 → (properties, required)。required 只收 SAP 标记为必填的输入字段。
fn field_defs_to_properties(fields: &[FieldDef]) -> (serde_json::Map<String, Value>, Vec<String>) {
    let mut props = serde_json::Map::new();
    let mut required = Vec::new();
    for f in fields {
        let mut schema = match f.type_name {
            "STRUCTURE" => {
                let (sub, _) = field_defs_to_properties(f.fields.as_deref().unwrap_or(&[]));
                json!({ "type": "object", "properties": sub })
            }
            "TABLE" => {
                let (sub, _) = field_defs_to_properties(f.fields.as_deref().unwrap_or(&[]));
                json!({ "type": "array", "items": { "type": "object", "properties": sub } })
            }
            t => {
                let mut s = json!({ "type": schema_type_for(t) });
                if f.length > 0 {
                    s["maxLength"] = json!(f.length);
                }
                s
            }
        };
        if !f.description.is_empty() {
            schema["description"] = json!(f.description);
        }
        props.insert(f.name.clone(), schema);
        // SAP 字段必填信息在 ParamInfo 层；FieldDef 层没有，输入 required 由参数级决定
        let _ = &mut required;
    }
    (props, required)
}

/// 参数级 schema（含 maxLength/description），STRUCTURE/TABLE 展开子字段。
fn param_schema(p: &FunctionParam) -> Value {
    match p.type_name {
        "STRUCTURE" => {
            let (sub, _) = field_defs_to_properties(p.fields.as_deref().unwrap_or(&[]));
            json!({ "type": "object", "properties": sub })
        }
        "TABLE" => {
            let (sub, _) = field_defs_to_properties(p.fields.as_deref().unwrap_or(&[]));
            json!({ "type": "array", "items": { "type": "object", "properties": sub } })
        }
        t => {
            let mut s = json!({ "type": schema_type_for(t) });
            if p.length > 0 {
                s["maxLength"] = json!(p.length);
            }
            s
        }
    }
}

/// 为一个函数生成 `POST /api/functions/{name}/invoke` 的 operation。
/// 返回 (路径, operation)。纯函数（输入来自接口元数据），单测直接覆盖。
///
/// 请求体结构与 /api/rfc 完全同构；差异只在：func_name 由路径注入（不出现），
/// 各参数按真实类型/嵌套展开，输出参数以 auto_outputs/table_outputs 的
/// enum/示例形式提示。agent 拿到 operation 即等于拿到该 BAPI 的调用说明。
pub fn function_operation(name: &str, params: &[FunctionParam]) -> (String, Value) {
    let is_input = |p: &FunctionParam| p.direction == "IMPORT" || p.direction == "CHANGING";
    let is_output = |p: &FunctionParam| {
        p.direction == "EXPORT" || p.direction == "CHANGING" || p.direction == "TABLES"
    };

    // 输入分组
    let mut inputs_props = serde_json::Map::new();
    let mut inputs_required: Vec<String> = Vec::new();
    let mut struct_inputs_props = serde_json::Map::new();
    let mut table_inputs_props = serde_json::Map::new();
    for p in params.iter().filter(|p| is_input(p)) {
        let mut schema = param_schema(p);
        if !p.description.is_empty() {
            schema["description"] = json!(p.description);
        }
        if !p.default.is_empty() {
            schema["default"] = json!(p.default);
        }
        match p.type_name {
            "STRUCTURE" => {
                struct_inputs_props.insert(p.name.clone(), schema);
            }
            "TABLE" => {
                table_inputs_props.insert(p.name.clone(), schema);
            }
            _ => {
                if !p.optional {
                    inputs_required.push(p.name.clone());
                }
                inputs_props.insert(p.name.clone(), schema);
            }
        }
    }

    // 输出分组（提示性：标量 enum 进 auto_outputs，表/结构体给示例）
    let scalar_out: Vec<&str> = params
        .iter()
        .filter(|p| is_output(p) && p.type_name != "STRUCTURE" && p.type_name != "TABLE")
        .map(|p| p.name.as_str())
        .collect();
    let table_out: Vec<&str> = params
        .iter()
        .filter(|p| is_output(p) && p.type_name == "TABLE")
        .map(|p| p.name.as_str())
        .collect();
    let struct_out: Vec<&str> = params
        .iter()
        .filter(|p| is_output(p) && p.type_name == "STRUCTURE")
        .map(|p| p.name.as_str())
        .collect();
    let has_return_table = params
        .iter()
        .any(|p| is_output(p) && p.type_name == "TABLE" && p.name == "RETURN");

    // 组 requestBody schema
    let mut req_props = serde_json::Map::new();
    let mut example = serde_json::Map::new();
    if !inputs_props.is_empty() {
        req_props.insert(
            "inputs".into(),
            json!({
                "type": "object",
                "properties": inputs_props,
                "required": inputs_required,
            }),
        );
    }
    if !struct_inputs_props.is_empty() {
        req_props.insert(
            "struct_inputs".into(),
            json!({ "type": "object", "properties": struct_inputs_props }),
        );
    }
    if !table_inputs_props.is_empty() {
        req_props.insert(
            "table_inputs".into(),
            json!({ "type": "object", "properties": table_inputs_props }),
        );
    }
    if !scalar_out.is_empty() {
        req_props.insert(
            "auto_outputs".into(),
            json!({
                "type": "array",
                "items": { "type": "string", "enum": scalar_out },
                "description": "Output scalar parameters to read by their true DDIC type",
            }),
        );
        example.insert("auto_outputs".into(), json!(scalar_out));
    }
    if !table_out.is_empty() || !struct_out.is_empty() {
        let mut ex = serde_json::Map::new();
        for t in &table_out {
            ex.insert(
                (*t).to_string(),
                json!([{ "name": "<field>", "auto": true }]),
            );
        }
        for s in &struct_out {
            ex.insert((*s).to_string(), json!([{ "name": "<field>" }]));
        }
        req_props.insert(
            "table_outputs".into(),
            json!({
                "type": "object",
                "description": "Output tables (and top-level structures) to traverse; keys are parameter names",
                "example": ex,
            }),
        );
    }
    if has_return_table {
        req_props.insert(
            "read_return".into(),
            json!({ "type": "boolean", "default": true, "description": "Read the RETURN message table" }),
        );
        example.insert("read_return".into(), json!(true));
    }
    req_props.insert(
        "timeout_secs".into(),
        json!({ "type": "integer", "minimum": 1, "description": "Per-call timeout override (seconds)" }),
    );

    let path = format!("/api/functions/{name}/invoke");
    let operation = json!({
        "post": {
            "tags": ["invoke"],
            "operationId": format!("call_{name}"),
            "summary": format!("Invoke {name} (typed; func name comes from the path)"),
            "description": format!(
                "Typed operation generated from the DDIC interface metadata of {name}. \
    The body is structurally identical to POST /api/rfc (func_name is injected from the path; \
    a func_name key in the body is ignored). Inspect GET /api/functions/{name} for the full interface."
            ),
            "requestBody": {
                "required": true,
                "content": {
                    "application/json": {
                        "schema": {
                            "type": "object",
                            "properties": req_props,
                            "example": example,
                        }
                    }
                }
            },
            "responses": {
                "200": {
                    "description": "Invocation result (scalars/tables/structs/return_table)",
                    "content": { "application/json": { "schema": { "$ref": "#/components/schemas/InvokeResponse" } } }
                },
                "default": { "$ref": "#/components/responses/Error" }
            }
        }
    });
    (path, operation)
}

/// 接口读取失败时的占位 operation（缺口可见，不吞错——与 prologue 同哲学）。
pub fn function_operation_failed(name: &str, err_msg: &str) -> (String, Value) {
    let path = format!("/api/functions/{name}/invoke");
    let operation = json!({
        "post": {
            "tags": ["invoke"],
            "operationId": format!("call_{name}"),
            "summary": format!("Invoke {name} (interface metadata unavailable)"),
            "description": format!("Interface metadata could not be read: {err_msg}. \
    The operation itself still works — inspect GET /api/functions/{name} and call it via POST /api/rfc."),
            "responses": {
                "200": { "description": "Invocation result", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/InvokeResponse" } } } },
                "default": { "$ref": "#/components/responses/Error" }
            }
        }
    });
    (path, operation)
}

// ========================================================================
// 注册表驱动的服务目录（v0.13）：published 条目 → /api/invokes/{alias} 类型化
// operation，并入公开的 /openapi.json。契约按需派生（与 invoke 模块同一套
// 输入/输出分类），SAP 侧签名漂移由 TTL 重建自动跟随。
// ========================================================================

use crate::registry::Entry;
use std::sync::Arc;
use std::time::Duration as CacheTtl;

/// 一个 published 条目的平坦调用 operation。纯函数（输入 = 条目 + 接口元数据）。
/// 请求/响应 schema 全部扁平展开——消费方拿到的就是调用说明本身。
pub fn flat_invoke_operation(entry: &Entry, params: &[FunctionParam]) -> (String, Value) {
    // TABLES 双向：可输入（BAPI 选择表惯例）也是输出——与 invoke 模块同口径
    let is_input = |p: &FunctionParam| {
        p.direction == "IMPORT" || p.direction == "CHANGING" || p.direction == "TABLES"
    };
    let is_output = |p: &FunctionParam| {
        p.direction == "EXPORT" || p.direction == "CHANGING" || p.direction == "TABLES"
    };
    // 请求体：输入参数平铺（required = SAP 标记非可选）
    let mut req_props = serde_json::Map::new();
    let mut required = Vec::new();
    for p in params.iter().filter(|p| is_input(p)) {
        if !p.description.is_empty() {
            let mut s = param_schema(p);
            s["description"] = json!(p.description);
            req_props.insert(p.name.clone(), s);
        } else {
            req_props.insert(p.name.clone(), param_schema(p));
        }
        if !p.optional {
            required.push(p.name.clone());
        }
    }
    // 响应：输出参数平铺 + 截断标记
    let mut resp_props = serde_json::Map::new();
    for p in params.iter().filter(|p| is_output(p)) {
        resp_props.insert(p.name.clone(), param_schema(p));
    }
    resp_props.insert(
        "_truncated".into(),
        json!({
            "type": "array", "items": { "type": "string" },
            "description": "Present only when an output table exceeded its row cap and was truncated"
        }),
    );
    // summary/description：intent 与 notes 是条目的灵魂；example 原样内嵌
    let summary = if entry.intent.is_empty() {
        format!("Invoke {} (registered API)", entry.func_name)
    } else {
        entry.intent.clone()
    };
    let mut description = format!(
        "Flat invoke of SAP function {}. Response/output tables are row-capped ({} by default; \
         override per call with ?limit=).",
        entry.func_name,
        entry.max_rows.unwrap_or(crate::invoke::DEFAULT_TABLE_CAP),
    );
    if !entry.notes.is_empty() {
        description.push_str(&format!("\n\nNotes: {}", entry.notes));
    }
    // doc 是面向消费方的完整文档正文（Markdown；Redoc 会按 Markdown 渲染）
    if !entry.doc.is_empty() {
        description.push_str(&format!("\n\n---\n{}", entry.doc));
    }
    if let Some(ex) = &entry.example {
        description.push_str(&format!("\n\nExample request body: `{}`", ex));
    }
    let op_id: String = format!(
        "invoke_{}",
        entry
            .alias
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
    );
    let path = format!("/api/invokes/{}", entry.alias);
    let mut op = json!({
        "post": {
            "tags": ["registry"],
            "operationId": op_id,
            "summary": summary,
            "description": description,
            "parameters": [
                { "name": "limit", "in": "query", "required": false,
                  "schema": { "type": "integer", "default": entry.max_rows.unwrap_or(crate::invoke::DEFAULT_TABLE_CAP), "maximum": 10000 },
                  "description": "Row cap for output tables" },
                { "name": "timeout_secs", "in": "query", "required": false,
                  "schema": { "type": "integer", "minimum": 1 },
                  "description": "Per-call timeout override in seconds" }
            ],
            "requestBody": {
                "required": true,
                "content": { "application/json": { "schema": {
                    "type": "object",
                    "properties": req_props,
                    "required": required,
                    "description": "Flat body: parameter name → value (case-insensitive keys; \
                                    structures are objects, tables are arrays of row objects)"
                } } }
            },
            "responses": {
                "200": { "description": "Flat result: output parameter name → value", "content": {
                    "application/json": { "schema": { "type": "object", "properties": resp_props } } } },
                "400": { "$ref": "#/components/responses/Error" },
                "401": { "$ref": "#/components/responses/Error" },
                "404": { "$ref": "#/components/responses/Error" },
                "502": { "$ref": "#/components/responses/Error" },
                "504": { "$ref": "#/components/responses/Error" }
            }
        }
    });
    if crate::auth::is_enabled() {
        op["post"]["security"] = json!([{ "bearerAuth": [] }]);
    }
    (path, op)
}

/// 注册表 operation 缓存。generation 对齐注册表变更；TTL 兜底签名漂移
///（FM 接口变了但注册表没动的场景）。
struct RegistryOpsCache {
    generation: u64,
    built_at: std::time::Instant,
    ops: Arc<Vec<(String, Value)>>,
}

static REGISTRY_OPS: tokio::sync::RwLock<Option<RegistryOpsCache>> =
    tokio::sync::RwLock::const_new(None);

/// 缓存 TTL：签名漂移跟随的上限（正常变更走 generation 立即失效）。
const REGISTRY_OPS_TTL: CacheTtl = CacheTtl::from_secs(600);

/// 重建全部 published 条目的 operation（借连接池逐个拉接口元数据）。
/// 单条目失败 → 占位 operation（缺口可见，不吞错，不拖垮整个目录）。
async fn rebuild_registry_ops(pool: &crate::server::SharedPool) -> Vec<(String, Value)> {
    let entries = match crate::registry::published_entries() {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(key = %e.key, "注册表目录重建：读取 published 条目失败");
            return Vec::new();
        }
    };
    let mut ops = Vec::with_capacity(entries.len());
    for entry in entries {
        let func = entry.func_name.clone();
        let r = crate::server::run_blocking(Arc::clone(pool), move |conn| {
            crate::server::collect_function_params(conn, &func)
        })
        .await;
        let op = match r {
            Ok(view) => flat_invoke_operation(&entry, &view.params),
            Err(e) => {
                let path = format!("/api/invokes/{}", entry.alias);
                let placeholder = json!({
                    "post": {
                        "tags": ["registry"],
                        "operationId": format!("invoke_{}", entry.alias.replace('/', "_")),
                        "summary": format!("{} (interface metadata unavailable)", entry.intent),
                        "description": format!(
                            "Interface metadata could not be read for {}: {}. \
                             The endpoint still works; see GET /api/registry/{}.",
                            entry.func_name, e.message, entry.alias
                        ),
                        "responses": { "200": { "description": "Flat result" } }
                    }
                });
                (path, placeholder)
            }
        };
        ops.push(op);
    }
    ops
}

/// 取注册表 operation（缓存优先：代际一致且未过 TTL 直接复用）。
/// 重建失败（如 SAP 不可达）时退回旧缓存——公开规范永远可用，最多旧一点。
pub async fn registry_ops(pool: &crate::server::SharedPool) -> Arc<Vec<(String, Value)>> {
    let gen = crate::registry::generation();
    if let Some(cached) = REGISTRY_OPS.read().await.as_ref() {
        if cached.generation == gen && cached.built_at.elapsed() < REGISTRY_OPS_TTL {
            return Arc::clone(&cached.ops);
        }
    }
    let ops = Arc::new(rebuild_registry_ops(pool).await);
    *REGISTRY_OPS.write().await = Some(RegistryOpsCache {
        generation: gen,
        built_at: std::time::Instant::now(),
        ops: Arc::clone(&ops),
    });
    ops
}

/// 只读缓存快照（公开 /openapi.json handler 用——不触碰 SAP，永远秒回；
/// 缓存由后台 watcher 预热）。
async fn registry_ops_cached() -> Option<Arc<Vec<(String, Value)>>> {
    REGISTRY_OPS
        .read()
        .await
        .as_ref()
        .map(|c| Arc::clone(&c.ops))
}

/// 后台 watcher（main spawn）：代际变化、缓存为空或 TTL 到期时重建。
/// 让公开的 /openapi.json 永远命中缓存，不被 SAP 的健康状况绑架。
pub async fn registry_ops_watcher(pool: crate::server::SharedPool) {
    loop {
        tokio::time::sleep(CacheTtl::from_secs(2)).await;
        let gen = crate::registry::generation();
        let need = match REGISTRY_OPS.read().await.as_ref() {
            None => true, // 尚未预热
            Some(c) => c.generation != gen || c.built_at.elapsed() >= REGISTRY_OPS_TTL,
        };
        if need {
            let ops = rebuild_registry_ops(&pool).await;
            tracing::info!(count = ops.len(), generation = gen, "注册表服务目录已重建");
            *REGISTRY_OPS.write().await = Some(RegistryOpsCache {
                generation: gen,
                built_at: std::time::Instant::now(),
                ops: Arc::new(ops),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_has_core_structure() {
        let spec = build_spec("http://127.0.0.1:3000", false);
        assert_eq!(spec["openapi"], "3.0.3");
        assert!(spec["info"]["version"].as_str().is_some());
        assert_eq!(spec["servers"][0]["url"], "http://127.0.0.1:3000");
        // 主要端点齐全
        let paths = spec["paths"].as_object().unwrap();
        for p in [
            "/health",
            "/ready",
            "/metrics",
            "/agents.md",
            "/api/version",
            "/api/rfc",
            "/api/functions/search",
            "/api/functions/{name}",
            "/api/functions/{name}/doc",
            "/api/functions/{name}/source",
            "/api/programs/{name}/source",
            "/api/ddic/type/{name}",
            "/api/ddic/field/{table}/{field}",
            "/api/table/read",
            "/api/registry",
            "/api/registry/{alias}",
            "/api/invokes/audit",
            "/api/invokes/{alias}",
            "/api/dumps",
            "/api/dumps/grouped",
            "/api/dumps/{key}/detail",
            "/api/objects/{type}/{name}/source",
            "/api/objects/{type}/{name}/replace",
            "/api/objects/{type}/{name}/syntax",
            "/api/objects/{type}/{name}/create",
            "/api/objects/{type}/{name}",
            "/api/adt/{path}",
        ] {
            assert!(paths.contains_key(p), "缺少端点 {p}");
        }
        // 组件 schema 齐全
        let schemas = spec["components"]["schemas"].as_object().unwrap();
        for s in [
            "ScalarValue",
            "TypedScalar",
            "MaxLen",
            "VersionInfo",
            "FieldSpec",
            "FieldDef",
            "InvokeRequest",
            "InvokeResponse",
            "SearchRequest",
            "SearchResponse",
            "FunctionInterface",
            "FunctionParam",
            "FunctionDocResponse",
            "SourceResponse",
            "DdicTypeResponse",
            "FieldSemanticsResponse",
            "TableReadRequest",
            "TableReadResponse",
            "DumpEntry",
            "DumpGroup",
            "DumpFrame",
            "DumpDetail",
            "ObjectWriteBody",
            "ObjectReplaceBody",
            "ActivationOutcome",
            "WriteOutcome",
            "SyntaxIssue",
            "ErrorBody",
        ] {
            assert!(schemas.contains_key(s), "缺少 schema {s}");
        }
    }

    #[test]
    fn auth_disabled_has_no_security() {
        let spec = build_spec("http://127.0.0.1:3000", false);
        // 未启用认证：无 securitySchemes，操作不带 security 标记
        assert!(spec["components"]["securitySchemes"].is_null());
        assert!(spec["paths"]["/api/rfc"]["post"]["security"].is_null());
    }

    #[test]
    fn auth_enabled_marks_api_ops_only() {
        let spec = build_spec("http://127.0.0.1:3000", true);
        // 启用认证：声明 Bearer 方案
        assert_eq!(
            spec["components"]["securitySchemes"]["bearerAuth"]["scheme"],
            "bearer"
        );
        // /api/* 操作带 security
        assert_eq!(
            spec["paths"]["/api/rfc"]["post"]["security"][0]["bearerAuth"],
            json!([])
        );
        assert_eq!(
            spec["paths"]["/api/functions/search"]["post"]["security"][0]["bearerAuth"],
            json!([])
        );
        // 探针/公开页不带 security
        assert!(spec["paths"]["/health"]["get"]["security"].is_null());
        assert!(spec["paths"]["/agents.md"]["get"]["security"].is_null());
        assert!(spec["paths"]["/metrics"]["get"]["security"].is_null());
        // /api/version 虽在 /api/* 下，但刻意公开（Agent 需在拿到 token 前
        // 知道 capabilities.auth）——显式 security: [] 而非缺省
        assert_eq!(spec["paths"]["/api/version"]["get"]["security"], json!([]));
    }

    #[test]
    fn spec_is_serializable_and_ref_resolvable() {
        // 序列化往返（保证无不可序列化值）；$ref 指向的 schema 都存在
        let spec = build_spec("http://h:1", true);
        let round: Value = serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        let schemas = round["components"]["schemas"].as_object().unwrap();
        let text = round.to_string();
        // 粗校验：每个 "#/components/schemas/X" 引用都有对应定义
        for part in text.split("\"#/components/schemas/").skip(1) {
            let name: String = part
                .chars()
                .take_while(|c| *c != '"' && *c != '\\')
                .collect();
            assert!(schemas.contains_key(&name), "引用了不存在的 schema: {name}");
        }
    }

    /// 构造测试用 FunctionParam（含嵌套结构/表字段）
    fn fp(
        name: &str,
        type_name: &'static str,
        direction: &'static str,
        optional: bool,
        fields: Option<Vec<FieldDef>>,
    ) -> FunctionParam {
        FunctionParam {
            name: name.into(),
            type_name,
            direction,
            length: if type_name == "CHAR" { 255 } else { 0 },
            decimals: 0,
            optional,
            default: String::new(),
            description: String::new(),
            fields,
        }
    }

    #[test]
    fn function_operation_groups_inputs_and_outputs() {
        let params = vec![
            fp("REQUTEXT", "CHAR", "IMPORT", false, None),
            fp("ECHOTEXT", "CHAR", "EXPORT", false, None),
            fp("MAX_ROW", "INT", "IMPORT", true, None),
        ];
        let (path, op) = function_operation("STFC_CONNECTION", &params);
        assert_eq!(path, "/api/functions/STFC_CONNECTION/invoke");
        let post = &op["post"];
        assert_eq!(post["operationId"], "call_STFC_CONNECTION");
        // 输入标量：必填 REQUTEXT、可选 MAX_ROW（整数类型）
        let inputs =
            &post["requestBody"]["content"]["application/json"]["schema"]["properties"]["inputs"];
        assert_eq!(inputs["properties"]["REQUTEXT"]["type"], "string");
        assert_eq!(inputs["properties"]["REQUTEXT"]["maxLength"], 255);
        assert_eq!(inputs["properties"]["MAX_ROW"]["type"], "integer");
        assert_eq!(inputs["required"][0], "REQUTEXT");
        // 输出标量进 auto_outputs enum
        let auto = &post["requestBody"]["content"]["application/json"]["schema"]["properties"]
            ["auto_outputs"];
        assert_eq!(auto["items"]["enum"][0], "ECHOTEXT");
        // func_name 不出现在请求体（由路径注入）
        let text = post.to_string();
        assert!(!text.contains("\"func_name\""), "func_name 应由路径注入");
    }

    /// USERLIST/RETURN 共用的行字段构造器（FieldDef 未实现 Clone，各自构造）
    fn row_fields() -> Option<Vec<FieldDef>> {
        Some(vec![FieldDef {
            name: "USERNAME".into(),
            type_name: "CHAR",
            length: 12,
            decimals: 0,
            description: String::new(),
            fields: None,
        }])
    }

    #[test]
    fn function_operation_expands_structures_and_tables() {
        let params = vec![
            fp("USERLIST", "TABLE", "EXPORT", false, row_fields()),
            fp("RETURN", "TABLE", "EXPORT", false, row_fields()),
        ];
        let (_path, op) = function_operation("BAPI_USER_GETLIST", &params);
        let text = op.to_string();
        // 表输出给了 table_outputs 提示 + RETURN 触发 read_return
        assert!(text.contains("table_outputs"));
        assert!(text.contains("read_return"));
        // TABLE 行字段展开为数组元素对象
        assert!(
            op["post"]["requestBody"]["content"]["application/json"]["schema"]["properties"]
                ["table_outputs"]["example"]["USERLIST"][0]["name"]
                == "<field>"
        );
    }

    #[test]
    fn function_operation_failed_is_visible_placeholder() {
        let (path, op) = function_operation_failed("Z_MISSING", "FU_NOT_FOUND: ID:FL");
        assert_eq!(path, "/api/functions/Z_MISSING/invoke");
        let desc = op["post"]["description"].as_str().unwrap();
        assert!(desc.contains("FU_NOT_FOUND"), "占位应携带错误信息: {desc}");
    }

    /// v0.13：published 条目 → /api/invokes/{alias} 的平坦 operation。
    #[test]
    fn flat_invoke_operation_builds_typed_flat_contract() {
        use crate::registry::{Entry, EntryOrigin, EntryStatus};
        let entry = Entry {
            alias: "z_calc".into(),
            func_name: "Z_CALC".into(),
            group: Some("ZMATH".into()),
            intent: "calculator for frontend".into(),
            notes: "integers only".into(),
            doc: "## Usage\n\nPass two integers; get their sum.".into(),
            example: Some(json!({"iv_a": 1})),
            status: EntryStatus::Published,
            origin: EntryOrigin::Manual,
            max_rows: Some(500),
            created_at: "t".into(),
            updated_at: "t".into(),
        };
        let params = vec![
            FunctionParam {
                name: "IV_A".into(),
                type_name: "INT",
                direction: "IMPORT",
                length: 4,
                decimals: 0,
                optional: false,
                default: String::new(),
                description: String::new(),
                fields: None,
            },
            FunctionParam {
                name: "EV_SUM".into(),
                type_name: "INT",
                direction: "EXPORT",
                length: 4,
                decimals: 0,
                optional: true,
                default: String::new(),
                description: String::new(),
                fields: None,
            },
        ];
        let (path, op) = flat_invoke_operation(&entry, &params);
        assert_eq!(path, "/api/invokes/z_calc");
        assert_eq!(op["post"]["operationId"], "invoke_z_calc");
        assert_eq!(op["post"]["summary"], "calculator for frontend");
        let desc = op["post"]["description"].as_str().unwrap();
        assert!(
            desc.contains("integers only") && desc.contains("500"),
            "{desc}"
        );
        assert!(
            desc.contains("## Usage"),
            "doc 正文应流入 description: {desc}"
        );
        // 请求体：IV_A 必填且为 integer
        let body = &op["post"]["requestBody"]["content"]["application/json"]["schema"];
        assert_eq!(body["properties"]["IV_A"]["type"], "integer");
        assert_eq!(body["required"][0], "IV_A");
        // 响应：EV_SUM 平铺 + _truncated 说明
        let resp = &op["post"]["responses"]["200"]["content"]["application/json"]["schema"];
        assert_eq!(resp["properties"]["EV_SUM"]["type"], "integer");
        assert!(resp["properties"].get("_truncated").is_some());
        // limit 默认值来自条目 max_rows
        assert_eq!(op["post"]["parameters"][0]["schema"]["default"], 500);
    }

    #[test]
    fn schema_type_maps_numeric_families() {
        assert_eq!(schema_type_for("INT"), "integer");
        assert_eq!(schema_type_for("INT8"), "integer");
        assert_eq!(schema_type_for("FLOAT"), "number");
        assert_eq!(schema_type_for("BCD"), "string"); // BCD 走字符串语义
        assert_eq!(schema_type_for("CHAR"), "string");
        assert_eq!(schema_type_for("DATE"), "string");
    }
}
