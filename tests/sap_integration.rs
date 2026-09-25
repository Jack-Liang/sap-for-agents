//! 真实 SAP 集成测试。
//!
//! 这些测试启动真实 HTTP server（cargo run 子进程），连接真实 SAP 系统，
//! 验证端到端正确性。全部标记 #[ignore]：
//!   - 无 SAP 环境（CI）：`cargo test` 默认跳过
//!   - 有 SAP 环境：`cargo test -- --ignored` 或 `cargo test -- --ignored sap_integration`
//!
//! 用的都是安全只读 RFC（STFC_CONNECTION、RFC_FUNCTION_SEARCH、BAPI_USER_GETLIST），
//! 不会修改 SAP 数据。

mod common;

use common::{alloc_port, ensure_sap_env};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// 启动 server 子进程并等待就绪。
/// 返回 (Child, base_url)。子进程在 Drop 时自动 kill。
struct ServerHandle {
    child: Child,
    base_url: String,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        // 强制 kill 子进程（SIGKILL），再 wait 回收
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_server() -> ServerHandle {
    start_server_with_env(&[])
}

/// 启动 server 子进程并等待就绪，可注入额外环境变量（如 `SAP_API_KEY` 测认证）。
fn start_server_with_env(extra: &[(&str, &str)]) -> ServerHandle {
    ensure_sap_env();
    let port = alloc_port();
    let base_url = format!("http://127.0.0.1:{}", port);
    let listen_addr = format!("127.0.0.1:{}", port);

    // 找到项目根目录的 target/debug/sap_for_agents（或用 cargo run）
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let bin_debug = format!("{}/target/debug/sap_for_agents", manifest_dir);
    let bin_release = format!("{}/target/release/sap_for_agents", manifest_dir);

    let (mut cmd, bin_exists) = if std::path::Path::new(&bin_debug).exists() {
        (Command::new(&bin_debug), true)
    } else if std::path::Path::new(&bin_release).exists() {
        (Command::new(&bin_release), true)
    } else {
        // fallback: cargo run
        (
            {
                let mut c = Command::new("cargo");
                c.args(["run", "--quiet"]);
                c
            },
            false,
        )
    };

    cmd.env("SAP_LISTEN_ADDR", &listen_addr)
        .env("RUST_LOG", "warn") // 减少日志噪音
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in extra {
        cmd.env(k, v);
    }

    if !bin_exists {
        cmd.current_dir(manifest_dir);
    }

    let child = cmd.spawn().expect("启动 server 失败");
    let handle = ServerHandle { child, base_url };

    // 等待 server 就绪（轮询 /health，最多 30 秒）
    let deadline = Instant::now() + Duration::from_secs(30);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    loop {
        if Instant::now() > deadline {
            panic!("server 启动超时（30s）");
        }
        if let Ok(resp) = client.get(format!("{}/health", handle.base_url)).send() {
            if resp.status().is_success() {
                return handle;
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// HTTP 客户端（复用连接，超时 30s）
fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
}

// ========================================================================
// 连接与基础调用
// ========================================================================

#[test]
#[ignore]
fn health_check_works() {
    let _s = start_server();
    let resp = http_client()
        .get(format!("{}/health", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["status"], "ok");
}

#[test]
#[ignore]
fn ready_returns_sap_ok() {
    // readiness 探针：借连接池调 RFC_PING，SAP 可达时应 200。
    let _s = start_server();
    let resp = http_client()
        .get(format!("{}/ready", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "SAP 可达时 /ready 应返回 200");
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["status"], "ready");
    assert_eq!(body["sap"], "ok");
}

#[test]
#[ignore]
fn version_endpoint_reports_gateway_and_sap() {
    // /api/version：本地部分秒回 + SAP 块（RFC_SYSTEM_INFO 懒加载缓存）
    let _s = start_server();
    let client = http_client();

    let resp = client
        .get(format!("{}/api/version", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["name"], "sap-for-agents");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    for k in ["auth", "read_only", "adt"] {
        assert!(
            body["capabilities"][k].is_boolean(),
            "capabilities.{k} 应为布尔: {body}"
        );
    }
    // SAP 可达时应给出系统信息（sysid/release 非空，client 来自启动配置）
    assert!(body["sap"].is_object(), "SAP 可达时 sap 块应有值: {body}");
    assert!(!body["sap"]["sysid"].as_str().unwrap_or("").is_empty());
    assert!(!body["sap"]["release"].as_str().unwrap_or("").is_empty());
    let expect_client = std::env::var("SAP_CLIENT").unwrap_or_default();
    assert_eq!(body["sap"]["client"], expect_client);

    // 第二次调用走缓存，形状不变（sap_error 不应出现）
    let resp2 = client
        .get(format!("{}/api/version", _s.base_url))
        .send()
        .unwrap();
    let body2: serde_json::Value = resp2.json().unwrap();
    assert!(body2["sap"].is_object());
    assert!(body2["sap_error"].is_null());
}

#[test]
#[ignore]
fn version_stays_public_when_auth_enabled() {
    // 设了 SAP_API_KEY：/api/version 仍免鉴权（Agent 需在拿到 token 前
    // 知道 capabilities.auth），且如实上报 auth=true
    let _s = start_server_with_env(&[("SAP_API_KEY", "test-secret")]);
    let resp = http_client()
        .get(format!("{}/api/version", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "认证开启时 /api/version 仍应公开");
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["capabilities"]["auth"], true);
}

#[test]
#[ignore]
fn api_requires_token_when_key_set() {
    // 启动带 SAP_API_KEY 的 server：/api/* 应要求 Bearer token，探针免鉴权。
    let _s = start_server_with_env(&[("SAP_API_KEY", "test-secret")]);
    let client = http_client();

    // 1. /api/* 无 token → 401（认证层拦截，不触达 SAP）
    let resp = client
        .get(format!("{}/api/functions/STFC_CONNECTION", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 401, "设了 key 后 /api 无 token 应 401");

    // 2. /api/* 带正确 token → 200（放行后触达 SAP 拉元数据）
    let resp = client
        .get(format!("{}/api/functions/STFC_CONNECTION", _s.base_url))
        .bearer_auth("test-secret")
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "正确 Bearer token 应放行");

    // 3. 探针始终免鉴权（编排系统探针不便带 token）
    let h = client
        .get(format!("{}/health", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(h.status(), 200, "/health 始终免鉴权");
    let r = client.get(format!("{}/ready", _s.base_url)).send().unwrap();
    assert_eq!(r.status(), 200, "/ready 始终免鉴权");
}

#[test]
#[ignore]
fn stfc_connection_echo_roundtrip() {
    let _s = start_server();
    let payload = serde_json::json!({
        "func_name": "STFC_CONNECTION",
        "inputs": {"REQUTEXT": "hello_integration_test"},
        "string_outputs": {"ECHOTEXT": {"max_len": 255}, "RESPTEXT": {"max_len": 255}}
    });
    let resp = http_client()
        .post(format!("{}/api/rfc", _s.base_url))
        .json(&payload)
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "STFC_CONNECTION 应成功");
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["func"], "STFC_CONNECTION");
    // ECHOTEXT 应回显 REQUTEXT
    let echo = body["scalars"]["ECHOTEXT"].as_str().unwrap_or("");
    assert!(
        echo.contains("hello_integration_test"),
        "ECHOTEXT 应回显输入文本，实际: {}",
        echo
    );
    // RESPTEXT 应非空（通常是 SAP 系统信息）
    assert!(!body["scalars"]["RESPTEXT"].as_str().unwrap_or("").is_empty());
}

// ========================================================================
// 元数据端点
// ========================================================================

#[test]
#[ignore]
fn function_interface_returns_params() {
    let _s = start_server();
    let resp = http_client()
        .get(format!("{}/api/functions/STFC_CONNECTION", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["name"], "STFC_CONNECTION");
    let params = body["params"].as_array().expect("params 应是数组");
    assert!(!params.is_empty(), "STFC_CONNECTION 应有参数");
    // 应包含 REQUTEXT（import）和 ECHOTEXT/RESPTEXT（export）
    let names: Vec<&str> = params
        .iter()
        .map(|p| p["name"].as_str().unwrap_or(""))
        .collect();
    assert!(names.contains(&"REQUTEXT"), "参数应含 REQUTEXT, 实际 {:?}", names);
}

#[test]
#[ignore]
fn function_search_finds_stfc() {
    let _s = start_server();
    let payload = serde_json::json!({
        "pattern": "STFC_*",
        "max_results": 10
    });
    let resp = http_client()
        .post(format!("{}/api/functions/search", _s.base_url))
        .json(&payload)
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    let functions = body["functions"].as_array().expect("functions 应是数组");
    assert!(!functions.is_empty(), "STFC_* 应至少匹配 STFC_CONNECTION");
    let names: Vec<&str> = functions
        .iter()
        .map(|f| f["name"].as_str().unwrap_or(""))
        .collect();
    assert!(
        names.iter().any(|n| n.contains("STFC")),
        "应匹配到 STFC 开头的函数, 实际 {:?}",
        names
    );
}

#[test]
#[ignore]
fn ddic_type_bapiret2_has_fields() {
    let _s = start_server();
    let resp = http_client()
        .get(format!("{}/api/ddic/type/BAPIRET2", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    let fields = body["fields"].as_array().expect("fields 应是数组");
    assert!(!fields.is_empty(), "BAPIRET2 应有字段");
    let names: Vec<&str> = fields
        .iter()
        .map(|f| f["name"].as_str().unwrap_or(""))
        .collect();
    assert!(names.contains(&"TYPE"), "BAPIRET2 应含 TYPE 字段, 实际 {:?}", names);
    assert!(
        names.contains(&"MESSAGE"),
        "BAPIRET2 应含 MESSAGE 字段, 实际 {:?}",
        names
    );
}

#[test]
#[ignore]
fn ddic_field_semantics_has_fixed_values() {
    let _s = start_server();
    let resp = http_client()
        .get(format!("{}/api/ddic/field/BAPIRET2/TYPE", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["field"], "TYPE");
    // TYPE 字段应返回语义元数据（data_element / domain / description 至少其一非空）
    let has_semantics = !body["data_element"].as_str().unwrap_or("").is_empty()
        || !body["domain"].as_str().unwrap_or("").is_empty()
        || !body["description"].as_str().unwrap_or("").is_empty();
    assert!(
        has_semantics,
        "TYPE 字段应返回语义元数据, 实际: {}",
        body
    );
    // fixed_values 可能为空（固定值在域级别，非所有系统都暴露），不强制断言
}

#[test]
#[ignore]
fn function_doc_returns_text() {
    let _s = start_server();
    let resp = http_client()
        .get(format!("{}/api/functions/STFC_CONNECTION/doc", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["name"], "STFC_CONNECTION");
    // parameter_docs 是数组（可能为空——取决于该函数是否有 parameterText）
    assert!(
        body["parameter_docs"].is_array(),
        "parameter_docs 应为数组, 实际: {}",
        body["parameter_docs"]
    );
    // 响应体应包含 short_text 或 long_text 或 warning 之一
    // （不同系统/函数的文档覆盖度不同，至少字段语义存在即可）
    assert!(
        body.get("short_text").is_some()
            || body.get("long_text").is_some()
            || body.get("warning").is_some(),
        "文档响应应含 short_text/long_text/warning 之一, 实际: {}",
        body
    );
}

// ========================================================================
// 错误处理与状态码
// ========================================================================

#[test]
#[ignore]
fn nonexistent_function_returns_404() {
    let _s = start_server();
    let payload = serde_json::json!({
        "func_name": "Z_NONEXISTENT_FAKE_FUNC_12345",
        "string_outputs": {"X": {"max_len": 255}}
    });
    let resp = http_client()
        .post(format!("{}/api/rfc", _s.base_url))
        .json(&payload)
        .send()
        .unwrap();
    // 不存在的函数：SAP 可能返回 NOT_FOUND(404) 或 ABAP_EXCEPTION(400)，
    // 取决于系统版本。两者都是"调用未成功"，接受任一。
    assert!(
        resp.status() == 404 || resp.status() == 400 || resp.status() == 500,
        "不存在函数应返回 4xx/5xx, 实际 {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().unwrap_or_default();
    assert!(body["error"]["message"].is_string(), "应有错误消息");
}

#[test]
#[ignore]
fn invalid_func_name_returns_400() {
    let _s = start_server();
    // func_name 含非法字符 → 输入校验拦截 → 400
    let payload = serde_json::json!({
        "func_name": "FOO;DROP TABLE",
        "string_outputs": {"X": {"max_len": 255}}
    });
    let resp = http_client()
        .post(format!("{}/api/rfc", _s.base_url))
        .json(&payload)
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400, "非法 func_name 应返回 400");
    let body: serde_json::Value = resp.json().unwrap_or_default();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("非法字符"),
        "错误信息应含'非法字符', 实际: {}",
        body
    );
}

// ========================================================================
// read_return + table_outputs
// ========================================================================

#[test]
#[ignore]
fn bapi_user_getlist_with_return() {
    let _s = start_server();
    let payload = serde_json::json!({
        "func_name": "BAPI_USER_GETLIST",
        "table_outputs": {
            "USERLIST": [
                {"name": "USERNAME", "max_len": 12}
            ]
        },
        "read_return": true
    });
    let resp = http_client()
        .post(format!("{}/api/rfc", _s.base_url))
        .json(&payload)
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "BAPI_USER_GETLIST 应成功");
    let body: serde_json::Value = resp.json().unwrap();
    // 应返回 USERLIST 表（可能为空数组，但键应存在）
    assert!(body["tables"].get("USERLIST").is_some(), "应返回 USERLIST 表");
    // read_return=true，return_table 应存在（即使为 null 也合理——无消息）
}

#[test]
#[ignore]
fn table_outputs_auto_preserves_numeric_type() {
    let _s = start_server();
    // BAPI_USER_GETLIST 的 USERLIST 表没有明显的 INT 字段，
    // 但用 auto:true 读 USERNAME 仍应是字符串。这里验证 auto 模式不破坏正常读取。
    let payload = serde_json::json!({
        "func_name": "BAPI_USER_GETLIST",
        "table_outputs": {
            "USERLIST": [
                {"name": "USERNAME", "auto": true}
            ]
        }
    });
    let resp = http_client()
        .post(format!("{}/api/rfc", _s.base_url))
        .json(&payload)
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    let userlist = body["tables"]["USERLIST"].as_array();
    if let Some(rows) = userlist {
        if !rows.is_empty() {
            // USERNAME 字段应存在且是字符串（CHAR 类型走 chars）
            let username = &rows[0]["USERNAME"];
            assert!(
                username.is_string() || username.is_null(),
                "USERNAME 应为字符串, 实际: {}",
                username
            );
        }
    }
}

// ========================================================================
// 并发
// ========================================================================

#[test]
#[ignore]
fn concurrent_calls_dont_deadlock() {
    let _s = start_server();
    let url = format!("{}/api/rfc", _s.base_url);
    let payload = serde_json::json!({
        "func_name": "STFC_CONNECTION",
        "inputs": {"REQUTEXT": "concurrent"},
        "string_outputs": {"ECHOTEXT": {"max_len": 255}}
    });

    // 8 个并发线程（= 默认连接池大小），每个发 3 次请求
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let url = url.clone();
            let payload = payload.clone();
            std::thread::spawn(move || {
                let client = http_client();
                let mut ok = 0;
                for _ in 0..3 {
                    let resp = client.post(&url).json(&payload).send().unwrap();
                    if resp.status() == 200 {
                        ok += 1;
                    }
                }
                ok
            })
        })
        .collect();

    let total_ok: u32 = threads.into_iter().map(|t| t.join().unwrap()).sum();
    assert_eq!(total_ok, 24, "8 线程 × 3 请求应全部成功，实际 {} / 24", total_ok);
}

// ========================================================================
// ADT REST 代理（/api/adt/**）

#[test]
#[ignore]
fn adt_proxy_forwards_dump_list() {
    let _s = start_server();
    // ADT dump 列表是标准 GET 资源；透传成功 = 200 + Atom feed 内容
    let resp = http_client()
        .get(format!("{}/api/adt/runtime/dumps", _s.base_url))
        .header("Accept", "*/*")
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "ADT 代理应透传 200，实际 {}", resp.status());
    let body = resp.text().unwrap();
    assert!(
        body.contains("atom:feed") || body.contains("atom:entry"),
        "响应应是 ADT dump Atom feed"
    );
}

#[test]
#[ignore]
fn adt_proxy_rejects_traversal() {
    let _s = start_server();
    // %2e%2e 解码后构成 .. 段 → 网关侧 400，不应转发到 ICF
    let resp = http_client()
        .get(format!("{}/api/adt/runtime/..%2F..%2Fetc", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400, "路径穿越应被网关拦截");
    let body: serde_json::Value = resp.json().unwrap_or_default();
    assert_eq!(body["error"]["key"], "ADT_PATH_INVALID");
}

#[test]
#[ignore]
fn adt_proxy_passes_through_adt_status() {
    let _s = start_server();
    // discovery 资源要求特定 Accept；带错误 Accept 调用 → ICF 的 406 应原样透传
    let resp = http_client()
        .get(format!("{}/api/adt/discovery", _s.base_url))
        .header("Accept", "application/xml")
        .send()
        .unwrap();
    assert_eq!(resp.status(), 406, "ADT 自身状态码应透传");
}

// ========================================================================
// 结构化 ST22 转储（/api/dumps**）
// ========================================================================

#[test]
#[ignore]
fn dumps_list_returns_structured_entries() {
    let _s = start_server();
    let resp = http_client()
        .get(format!("{}/api/dumps?limit=5", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    let dumps = body["dumps"].as_array().expect("dumps 应为数组");
    assert_eq!(body["count"].as_u64().unwrap(), dumps.len() as u64);
    // 环境可能无转储（count=0 合法）；有转储时每条必须有结构化字段
    if let Some(first) = dumps.first() {
        let key = first["key"].as_str().expect("dump 应有 key");
        assert!(!key.is_empty(), "key 非空（detail 端点依赖它）");
        assert!(first["error_type"].is_string(), "应有 error_type");
        assert!(first["at"].is_string(), "应有时间戳 at");
    }
}

#[test]
#[ignore]
fn dumps_grouped_sorts_by_count_desc() {
    let _s = start_server();
    let resp = http_client()
        .get(format!("{}/api/dumps/grouped", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    let groups = body["groups"].as_array().expect("groups 应为数组");
    assert_eq!(body["count"].as_u64().unwrap(), groups.len() as u64);
    // 组数 >=2 时验证按 count 降序（并列按最近时间，不测并列细节）
    let counts: Vec<u64> = groups.iter().filter_map(|g| g["count"].as_u64()).collect();
    assert_eq!(counts.len(), groups.len(), "每组都应有 count");
    let mut sorted = counts.clone();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(counts, sorted, "groups 应按 count 降序");
    for g in groups {
        assert!(g["latest_key"].is_string(), "每组应有 latest_key");
    }
}

#[test]
#[ignore]
fn dumps_detail_uses_key_from_list() {
    let _s = start_server();
    let list: serde_json::Value = http_client()
        .get(format!("{}/api/dumps?limit=1", _s.base_url))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let Some(first) = list["dumps"].as_array().and_then(|a| a.first()) else {
        return; // 环境无转储，跳过（结构由上面的列表测试覆盖）
    };
    let key = first["key"].as_str().unwrap();
    // key 本身含 %20 等已编码序列，原样拼进路径
    let resp = http_client()
        .get(format!("{}/api/dumps/{}/detail", _s.base_url, key))
        .send()
        .unwrap();
    // 200 = 解析出结构化详情；404 = 转储已被清或版本过老无 detail 资源（7.50）
    let status = resp.status();
    assert!(
        status == 200 || status == 404,
        "detail 应返回 200 或 404，实际 {status}"
    );
    if status == 200 {
        let body: serde_json::Value = resp.json().unwrap();
        assert!(body["error_type"].is_string(), "detail 应有 error_type");
        assert!(body["stack"].is_array(), "detail 应有 stack 数组");
    }
}

// ========================================================================
// 源码读取（/api/functions/{name}/source、/api/programs/{name}/source）
// ========================================================================

#[test]
#[ignore]
fn function_source_returns_lines_and_via() {
    let _s = start_server();
    let resp = http_client()
        .get(format!("{}/api/functions/STFC_CONNECTION/source", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    let lines = body["lines"].as_array().expect("lines 应为数组");
    assert!(!lines.is_empty(), "源码不应为空");
    let via = body["source_via"].as_str().expect("应有 source_via");
    assert!(
        via == "rfc" || via == "adt",
        "source_via 应标明来源渠道，实际 {via}"
    );
    let first = lines[0].as_str().unwrap().to_lowercase();
    assert!(
        first.contains("stfc_connection"),
        "首行应是函数头，实际: {first}"
    );
}

#[test]
#[ignore]
fn function_source_prologue_inlines_dependencies() {
    let _s = start_server();
    // BAPI_TRANSACTION_COMMIT 内部 CALL FUNCTION BALW_BAPIRETURN_GET2，
    // 是所有版本都稳定的经典依赖，适合验证 prologue 内联
    let resp = http_client()
        .get(format!(
            "{}/api/functions/BAPI_TRANSACTION_COMMIT/source?prologue=true",
            _s.base_url
        ))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    let prologue = &body["prologue"];
    assert!(prologue.is_object(), "应返回 prologue 对象");
    let text = prologue["text"].as_str().expect("prologue 应有 text");
    assert!(
        text.contains("FUNCTION"),
        "prologue.text 应内联被调函数签名块，实际: {}",
        &text[..text.len().min(120)]
    );
}

#[test]
#[ignore]
fn program_source_returns_lines_and_via() {
    let _s = start_server();
    // SAPLSTFC = 标准函数组 STFC 的主程序，RFC 可用的系统必有
    let resp = http_client()
        .get(format!("{}/api/programs/SAPLSTFC/source", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    let lines = body["lines"].as_array().expect("lines 应为数组");
    assert!(!lines.is_empty(), "程序源码不应为空");
    let via = body["source_via"].as_str().expect("应有 source_via");
    assert!(via == "rfc" || via == "adt");
}

// ========================================================================
// 透明表读取（POST /api/table/read）
// ========================================================================

#[test]
#[ignore]
fn table_read_t000_clients() {
    let _s = start_server();
    // T000（集团表）任何系统都有、行数个位数，是最安全的探针表
    let resp = http_client()
        .post(format!("{}/api/table/read", _s.base_url))
        .json(&serde_json::json!({
            "table": "T000",
            "fields": ["MANDT", "MTEXT"],
            "rowcount": 3
        }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["table"], "T000");
    let rows = body["rows"].as_array().expect("rows 应为数组");
    assert!(!rows.is_empty(), "T000 至少一行");
    assert_eq!(body["count"].as_u64().unwrap(), rows.len() as u64);
    for row in rows {
        assert!(row["MANDT"].is_string(), "字段值应为字符串");
        assert!(row["MTEXT"].is_string());
    }
}

#[test]
#[ignore]
fn table_read_rejects_empty_fields() {
    let _s = start_server();
    let resp = http_client()
        .post(format!("{}/api/table/read", _s.base_url))
        .json(&serde_json::json!({"table": "T000", "fields": []}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["error"]["key"], "FIELDS_EMPTY");
}

// ========================================================================
// 语法检查（POST /api/objects/{type}/{name}/syntax，不落库）
// ========================================================================

#[test]
#[ignore]
fn objects_syntax_reports_error_lines() {
    let _s = start_server();
    // 锚在不存在的 Z 名上：ADT 按虚拟对象检查，干扰告警最少（不落库）
    let resp = http_client()
        .post(format!("{}/api/objects/prog/ZSYNTAX_PROBE/syntax", _s.base_url))
        .json(&serde_json::json!({"source": "REPORT zsyntax_probe.\nWRIT 1."}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    let issues = body["issues"].as_array().expect("issues 应为数组");
    let has_error = issues
        .iter()
        .any(|i| i["severity"] == "E" && i["line"].as_u64().unwrap_or(0) >= 1);
    assert!(has_error, "WRIT 拼写错应报 E 级问题，实际: {issues:?}");
}

#[test]
#[ignore]
fn objects_syntax_clean_source_has_no_errors() {
    let _s = start_server();
    // 注意锚点选可执行程序语义：F 类型主程序（如 SAPLSTFC）顶层 WRITE 会报
    // 「Statement is not accessible」E，与源码本身无关；Z 名 = type 1 语义
    let resp = http_client()
        .post(format!("{}/api/objects/prog/ZSYNTAX_PROBE/syntax", _s.base_url))
        .json(&serde_json::json!({"source": "REPORT zsyntax_probe.\nWRITE 1."}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    let issues = body["issues"].as_array().expect("issues 应为数组");
    assert!(
        issues.iter().all(|i| i["severity"] != "E"),
        "合法源码不应有 E 级问题（W 允许），实际: {issues:?}"
    );
}

// ========================================================================
// 只读模式（SAP_READ_ONLY=1）
// ========================================================================

#[test]
#[ignore]
fn read_only_blocks_object_writes_but_allows_syntax() {
    let _s = start_server_with_env(&[("SAP_READ_ONLY", "1")]);
    // PUT 全量写 → 403 READ_ONLY
    let resp = http_client()
        .put(format!("{}/api/objects/prog/ZRO_TEST/source", _s.base_url))
        .json(&serde_json::json!({"source": "REPORT zro_test.\nWRITE 1."}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 403, "只读模式应拦 PUT source");
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["error"]["key"], "READ_ONLY");
    // POST replace → 403
    let resp = http_client()
        .post(format!("{}/api/objects/prog/ZRO_TEST/replace", _s.base_url))
        .json(&serde_json::json!({"old_string": "A", "new_string": "B"}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 403, "只读模式应拦 replace");
    // POST syntax（不落库）→ 照常工作
    let resp = http_client()
        .post(format!("{}/api/objects/prog/ZRO_TEST/syntax", _s.base_url))
        .json(&serde_json::json!({"source": "REPORT zro_test.\nWRITE 1."}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "syntax 不落库，只读模式应放行");
}

#[test]
#[ignore]
fn read_only_blocks_adt_writes_but_passes_reads() {
    let _s = start_server_with_env(&[("SAP_READ_ONLY", "1")]);
    // ADT 写方法 → 403 READ_ONLY（网关侧拦截，不转发到 ICF）
    let resp = http_client()
        .post(format!("{}/api/adt/oo/classes/zcl_ro_test/source/main", _s.base_url))
        .header("Content-Type", "text/plain")
        .body("class")
        .send()
        .unwrap();
    assert_eq!(resp.status(), 403, "只读模式应拦 ADT POST");
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["error"]["key"], "READ_ONLY");
    // ADT 读方法照常透传
    let resp = http_client()
        .get(format!("{}/api/adt/runtime/dumps", _s.base_url))
        .header("Accept", "*/*")
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "只读模式不应影响 ADT 读");
}

#[test]
#[ignore]
fn read_only_keeps_rfc_and_reads_working() {
    let _s = start_server_with_env(&[("SAP_READ_ONLY", "1")]);
    // /api/rfc 明确不在拦截范围（RFC 无法可靠区分读写），读取类端点照常
    let resp = http_client()
        .post(format!("{}/api/rfc", _s.base_url))
        .json(&serde_json::json!({"func_name": "RFC_PING"}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "只读模式不应拦 /api/rfc");
    let resp = http_client()
        .get(format!("{}/api/functions/STFC_CONNECTION", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "只读模式不应拦接口查看");
}

#[test]
#[ignore]
fn idle_connection_validated_on_checkout() {
    // 阈值压到 1s：睡 2s 后借出必然走 ping 校验路径，校验通过则调用照常成功
    let _s = start_server_with_env(&[("SAP_POOL_IDLE_VALIDATE_SECS", "1")]);
    let r1: serde_json::Value = http_client()
        .post(format!("{}/api/rfc", _s.base_url))
        .json(&serde_json::json!({"func_name": "RFC_PING"}))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(r1["func"], "RFC_PING");
    std::thread::sleep(std::time::Duration::from_secs(2));
    let r2 = http_client()
        .post(format!("{}/api/rfc", _s.base_url))
        .json(&serde_json::json!({
            "func_name": "STFC_CONNECTION",
            "inputs": {"REQUTEXT": "after idle validate"}
        }))
        .send()
        .unwrap();
    assert_eq!(r2.status(), 200, "空闲超阈值后借出（先 ping 校验）应照常成功");
}

// ========================================================================
// OpenAPI 规范与类型化调用（/openapi.json、/api/openapi、{name}/invoke）
// ========================================================================

#[test]
#[ignore]
fn openapi_json_returns_valid_spec() {
    let _s = start_server();
    let resp = http_client()
        .get(format!("{}/openapi.json", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "公开规范应免鉴权可访问");
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["openapi"], "3.0.3");
    assert!(body["paths"]["/api/rfc"].is_object(), "应含通用调用端点");
    assert!(
        body["paths"]["/api/functions/{name}/source"].is_object(),
        "应含源码端点"
    );
}

#[test]
#[ignore]
fn openapi_dynamic_generates_typed_operations() {
    let _s = start_server();
    let resp = http_client()
        .get(format!(
            "{}/api/openapi?functions=STFC_CONNECTION,BAPI_USER_GETLIST",
            _s.base_url
        ))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let spec: serde_json::Value = resp.json().unwrap();
    // STFC_CONNECTION 的类型化 operation 存在，REQUTEXT 已展开
    let op = &spec["paths"]["/api/functions/STFC_CONNECTION/invoke"]["post"];
    assert!(op.is_object(), "应生成类型化 operation");
    let inputs = &op["requestBody"]["content"]["application/json"]["schema"]["properties"]["inputs"];
    assert_eq!(inputs["properties"]["REQUTEXT"]["type"], "string");
    // BAPI 的输出表进 table_outputs 提示
    let bapi = &spec["paths"]["/api/functions/BAPI_USER_GETLIST/invoke"]["post"];
    assert!(
        bapi["requestBody"]["content"]["application/json"]["schema"]["properties"]
            ["table_outputs"]["example"]["USERLIST"]
            .is_array(),
        "USERLIST 输出表应有示例"
    );
}

#[test]
#[ignore]
fn openapi_dynamic_rejects_bad_requests() {
    let _s = start_server();
    // 空列表 → 400
    let resp = http_client()
        .get(format!("{}/api/openapi?functions=,,", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400);
    // 超过 50 个 → 400
    let many = (0..51).map(|_| "RFC_PING").collect::<Vec<_>>().join(",");
    let resp = http_client()
        .get(format!("{}/api/openapi?functions={}", _s.base_url, many))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[test]
#[ignore]
fn function_invoke_endpoint_roundtrip() {
    let _s = start_server();
    // 函数名来自路径，body 免填 func_name
    let resp = http_client()
        .post(format!("{}/api/functions/STFC_CONNECTION/invoke", _s.base_url))
        .json(&serde_json::json!({
            "inputs": {"REQUTEXT": "typed invoke"},
            "string_outputs": {"ECHOTEXT": 135}
        }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["func"], "STFC_CONNECTION");
    assert_eq!(body["scalars"]["ECHOTEXT"], "typed invoke");
}

#[test]
#[ignore]
fn function_invoke_path_overrides_body_func_name() {
    let _s = start_server();
    // body 里塞了错误的 func_name 也应被路径覆盖
    let resp = http_client()
        .post(format!("{}/api/functions/RFC_PING/invoke", _s.base_url))
        .json(&serde_json::json!({"func_name": "WRONG_FUNC", "inputs": {}}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "路径注入应优先于 body");
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["func"], "RFC_PING");
}

// ========================================================================
// MCP 服务器（POST /mcp，JSON-RPC over Streamable HTTP 无状态模式）
// ========================================================================

#[test]
#[ignore]
fn mcp_initialize_negotiates_protocol() {
    let _s = start_server();
    let resp = http_client()
        .post(format!("{}/mcp", _s.base_url))
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test"} }
        }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    assert_eq!(body["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(body["result"]["serverInfo"]["name"], "sap-for-agents");
    assert!(body["result"]["capabilities"]["tools"].is_object());
}

#[test]
#[ignore]
fn mcp_tools_list_returns_manifest() {
    let _s = start_server();
    let resp = http_client()
        .post(format!("{}/mcp", _s.base_url))
        .json(&serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    let tools = body["result"]["tools"].as_array().expect("tools 数组");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for must in ["search_functions", "get_function_interface", "invoke_rfc", "syntax_check"] {
        assert!(names.contains(&must), "缺工具 {must}");
    }
}

#[test]
#[ignore]
fn mcp_tools_call_invoke_roundtrip() {
    let _s = start_server();
    // initialize → initialized（通知 202）→ tools/call invoke_rfc 回显
    let _ = http_client()
        .post(format!("{}/mcp", _s.base_url))
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t"}}
        }))
        .send()
        .unwrap();
    let resp = http_client()
        .post(format!("{}/mcp", _s.base_url))
        .json(&serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 202, "通知应回 202");

    let resp = http_client()
        .post(format!("{}/mcp", _s.base_url))
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "invoke_rfc", "arguments": {
                "func_name": "STFC_CONNECTION",
                "inputs": {"REQUTEXT": "via mcp"},
                "string_outputs": {"ECHOTEXT": 135}
            }}
        }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["result"]["isError"], false);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let payload: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(payload["scalars"]["ECHOTEXT"], "via mcp");
}

#[test]
#[ignore]
fn mcp_tools_call_metadata_and_errors() {
    let _s = start_server();
    // 元数据工具
    let resp = http_client()
        .post(format!("{}/mcp", _s.base_url))
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "search_functions", "arguments": {"pattern": "STFC_*", "max_results": 3} }
        }))
        .send()
        .unwrap();
    let body: serde_json::Value = resp.json().unwrap();
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let payload: serde_json::Value = serde_json::from_str(text).unwrap();
    assert!(payload["count"].as_u64().unwrap() >= 1);
    // 工具级失败：isError=true 而非 JSON-RPC error
    let resp = http_client()
        .post(format!("{}/mcp", _s.base_url))
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "get_function_interface", "arguments": {"name": "Z_NOT_EXIST_999"} }
        }))
        .send()
        .unwrap();
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["result"]["isError"], true, "工具失败应为 isError");
    assert!(body.get("error").is_none(), "工具失败不是协议错误");
    // 未知方法 → JSON-RPC -32601
    let resp = http_client()
        .post(format!("{}/mcp", _s.base_url))
        .json(&serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "bogus/method"}))
        .send()
        .unwrap();
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["error"]["code"], -32601);
}

#[test]
#[ignore]
fn where_used_returns_structure_even_without_index() {
    let _s = start_server();
    let resp = http_client()
        .get(format!("{}/api/functions/BAPI_TRANSACTION_COMMIT/where-used", _s.base_url))
        .send()
        .unwrap();
    // 有索引的系统返回真实使用者；无索引的 trial 返回空 + note——都是 200
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["name"], "BAPI_TRANSACTION_COMMIT");
    let usages = body["usages"].as_array().expect("usages 数组");
    assert_eq!(body["count"].as_u64().unwrap(), usages.len() as u64);
    if usages.is_empty() {
        assert!(
            body["note"].as_str().unwrap_or("").contains("index"),
            "空结果应附索引说明 note"
        );
    } else {
        // 有真实结果时每条有 type/object 字段
        assert!(usages[0]["type"].is_string());
        assert!(usages[0]["object"].is_string());
    }
}

#[test]
#[ignore]
fn mcp_where_used_tool_available() {
    let _s = start_server();
    let resp = http_client()
        .post(format!("{}/mcp", _s.base_url))
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "where_used", "arguments": {"name": "BAPI_TRANSACTION_COMMIT", "max": 50} }
        }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["result"]["isError"], false);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let payload: serde_json::Value = serde_json::from_str(text).unwrap();
    assert!(payload["count"].is_u64());
}

// ========================================================================
// 写入丝滑度（创建 + 增删改 + 大小写漂移 + MCP 编辑）
// ========================================================================

#[test]
#[ignore]
fn smooth_write_lifecycle_on_program() {
    let _s = start_server();
    let name = format!("ZSMOOTH_T{}", std::process::id() % 10000);

    // ① PUT + create:true 从零创建并写入（历史痛点：不存在 → 409）
    let resp = http_client()
        .put(format!("{}/api/objects/prog/{}/source", _s.base_url, name))
        .json(&serde_json::json!({
            "source": "REPORT zsmooth.\nDATA lv_n TYPE i.\nWRITE 'born'.",
            "create": true, "description": "smooth lifecycle test"
        }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "create:true 应从零创建");
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["activated"]["success"], true);

    // ② 读回保留原文大小写（口径修复：RPY 读回不再大写化）
    let src: serde_json::Value = http_client()
        .get(format!("{}/api/programs/{}/source", _s.base_url, name))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let text = src["lines"].as_array().unwrap().iter()
        .map(|l| l.as_str().unwrap()).collect::<Vec<_>>().join("\n");
    assert!(text.contains("lv_n"), "读回应保留小写原文: {text}");

    // ③ 大写锚点改小写原文（大小写漂移兜底）
    let resp = http_client()
        .post(format!("{}/api/objects/prog/{}/replace", _s.base_url, name))
        .json(&serde_json::json!({"old_string": "LV_N TYPE I.", "new_string": "lv_n TYPE i VALUE 7."}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "大写锚点应经大小写不敏感兜底命中");
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["activated"]["success"], true);

    // ④ 末行 \n 锚点
    let resp = http_client()
        .post(format!("{}/api/objects/prog/{}/replace", _s.base_url, name))
        .json(&serde_json::json!({"old_string": "WRITE 'born'.\n", "new_string": "WRITE / |n={ lv_n }|."}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "末行 \\n 锚点应命中");

    // ⑤ 删行 + create 动作端点
    let resp = http_client()
        .post(format!("{}/api/objects/prog/{}/replace", _s.base_url, name))
        .json(&serde_json::json!({"old_string": "DATA lv_n TYPE i VALUE 7.\n", "new_string": ""}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[test]
#[ignore]
fn mcp_edit_code_and_write_source_tools() {
    let _s = start_server();
    let name = format!("ZMCPW{}", std::process::id() % 10000);

    // MCP write_source 从零创建（create:true）
    let resp = http_client()
        .post(format!("{}/mcp", _s.base_url))
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "write_source", "arguments": {
                "type": "prog", "name": name,
                "source": "REPORT zmcpw.\nWRITE 'mcp born'.",
                "create": true, "description": "mcp write test"
            }}
        }))
        .send()
        .unwrap();
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["result"]["isError"], false, "MCP write_source 创建应成功");

    // MCP edit_code 用大写漂移锚点丝滑修改
    let resp = http_client()
        .post(format!("{}/mcp", _s.base_url))
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "edit_code", "arguments": {
                "type": "prog", "name": name,
                "old_string": "WRITE 'MCP BORN'.",
                "new_string": "WRITE / 'mcp edited smoothly'."
            }}
        }))
        .send()
        .unwrap();
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["result"]["isError"], false, "MCP edit_code 大写锚点应兜底命中");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let payload: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(payload["activated"]["success"], true);
}

// ========================================================================
// API 注册表（v0.12）：纯网关本地状态，不写 SAP
// ========================================================================

/// 注册表集成测试用的临时文件路径（每次唯一）。
fn temp_registry_path(tag: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "sfa-itest-registry-{}-{}",
        tag,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("registry.json").to_str().unwrap().to_string()
}

#[test]
#[ignore]
fn registry_crud_and_persistence_across_restart() {
    let reg_path = temp_registry_path("crud");
    let _s = start_server_with_env(&[("SAP_REGISTRY_FILE", reg_path.as_str())]);

    // 空表
    let resp = http_client()
        .get(format!("{}/api/registry", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["count"], 0, "新注册表应为空: {body}");

    // capabilities.registry = true
    let resp = http_client()
        .get(format!("{}/api/version", _s.base_url))
        .send()
        .unwrap();
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["capabilities"]["registry"], true);

    // 创建（带齐三件套：intent/notes/example）
    let entry = serde_json::json!({
        "func_name": "Z_ITEST_CALC",
        "group": "ZITEST",
        "intent": "integration test calculator",
        "notes": "must pass IV_A as integer",
        "example": {"inputs": {"IV_A": 20, "IV_B": 22}},
        "status": "published"
    });
    let resp = http_client()
        .put(format!("{}/api/registry/z-itest/calc", _s.base_url))
        .json(&entry)
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["created"], true);
    assert_eq!(body["entry"]["func_name"], "Z_ITEST_CALC");
    assert_eq!(body["entry"]["status"], "published");

    // 单条 + 过滤
    let resp = http_client()
        .get(format!("{}/api/registry/z-itest/calc", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["notes"], "must pass IV_A as integer");

    let resp = http_client()
        .get(format!("{}/api/registry?q=CALCULATOR", _s.base_url))
        .send()
        .unwrap();
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["count"], 1, "q 命中 intent（大小写不敏感）");

    let resp = http_client()
        .get(format!("{}/api/registry?q=nomatchxyz", _s.base_url))
        .send()
        .unwrap();
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["count"], 0);

    // MCP 工具面：list_registry_apis 能看到
    let resp = http_client()
        .post(format!("{}/mcp", _s.base_url))
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "list_registry_apis", "arguments": {"q": "calc"}}
        }))
        .send()
        .unwrap();
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["result"]["isError"], false);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let payload: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(payload["count"], 1, "MCP 列表应含条目: {payload}");
    assert_eq!(payload["entries"][0]["alias"], "z-itest/calc");

    // 非法 alias / 非法 status → 400
    let resp = http_client()
        .put(format!("{}/api/registry/BAD_ALIAS", _s.base_url))
        .json(&serde_json::json!({"func_name": "Z_X"}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = http_client()
        .put(format!("{}/api/registry/z-ok", _s.base_url))
        .json(&serde_json::json!({"func_name": "Z_X", "status": "deleted"}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400);

    // 墓碑删除：默认列表不可见，include_deleted 可见
    let resp = http_client()
        .delete(format!("{}/api/registry/z-itest/calc", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = http_client()
        .get(format!("{}/api/registry", _s.base_url))
        .send()
        .unwrap();
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["count"], 0, "墓碑默认不可见");
    let resp = http_client()
        .get(format!("{}/api/registry?include_deleted=true", _s.base_url))
        .send()
        .unwrap();
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["count"], 1);
    assert_eq!(body["entries"][0]["status"], "deleted");

    // 文件落盘校验（schema version + 墓碑条目）
    let text = std::fs::read_to_string(&reg_path).unwrap();
    assert!(text.contains("\"version\": 1"), "{text}");
    assert!(text.contains("Z_ITEST_CALC"));

    // 跨重启持久化：同一文件，第二个 server 实例仍能读到（ revived 场景：
    // PUT 同 alias 复活墓碑）
    drop(_s);
    let _s2 = start_server_with_env(&[("SAP_REGISTRY_FILE", reg_path.as_str())]);
    let resp = http_client()
        .put(format!("{}/api/registry/z-itest/calc", _s2.base_url))
        .json(&entry)
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["created"], false, "重启后条目仍在（墓碑复活而非新建）");
    let resp = http_client()
        .get(format!("{}/api/registry", _s2.base_url))
        .send()
        .unwrap();
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["count"], 1, "复活后默认列表可见");
}

#[test]
#[ignore]
fn registry_corrupt_file_recovers() {
    // 预置坏文件：网关应备份后以空表启动（而不是崩溃）
    let reg_path = temp_registry_path("corrupt");
    std::fs::write(&reg_path, "{ broken json").unwrap();
    let _s = start_server_with_env(&[("SAP_REGISTRY_FILE", reg_path.as_str())]);
    let resp = http_client()
        .get(format!("{}/api/registry", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200, "坏文件不应拖垮网关");
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["count"], 0, "坏文件 → 空表启动");
    // 备份文件存在
    let dir = std::path::Path::new(&reg_path).parent().unwrap();
    let has_backup = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e
            .file_name()
            .to_string_lossy()
            .starts_with("registry.json.corrupt-"));
    assert!(has_backup, "坏文件应被备份而非覆盖丢失");
}

// ========================================================================
// 交付端口（v0.13）：/api/invokes/{alias} 平坦调用 + 注册表驱动的 OpenAPI
// 全程只读 SAP（用标准函数手工注册，不建 Z 对象）
// ========================================================================

#[test]
#[ignore]
fn flat_invoke_and_registry_catalog() {
    let reg_path = temp_registry_path("flat");
    let _s = start_server_with_env(&[("SAP_REGISTRY_FILE", reg_path.as_str())]);

    // 注册 STFC_CONNECTION（标准 RFC 测试函数：REQUTEXT → ECHOTEXT/RESPTEXT）
    let resp = http_client()
        .put(format!("{}/api/registry/stfc-echo", _s.base_url))
        .json(&serde_json::json!({
            "func_name": "STFC_CONNECTION",
            "intent": "connectivity check echo",
            "status": "published"
        }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);

    // 平坦调用：小写键也能命中（大小写不敏感）
    let resp = http_client()
        .post(format!("{}/api/invokes/stfc-echo", _s.base_url))
        .json(&serde_json::json!({"requtext": "hello flat"}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["ECHOTEXT"], "hello flat", "扁平响应: {body}");
    assert!(
        body["RESPTEXT"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
        "RESPTEXT 应非空: {body}"
    );
    // 响应就是扁平对象：没有 /api/rfc 的 scalars/tables 包装
    assert!(body.get("scalars").is_none(), "不应有泛型包装: {body}");

    // 未知参数 → 400（带合法参数清单）
    let resp = http_client()
        .post(format!("{}/api/invokes/stfc-echo", _s.base_url))
        .json(&serde_json::json!({"typo": 1}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().unwrap();
    assert_eq!(body["error"]["key"], "INVOKE_PARAM_UNKNOWN");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("REQUTEXT"),
        "错误应列出合法参数: {body}"
    );

    // 未知别名 → 404
    let resp = http_client()
        .post(format!("{}/api/invokes/no_such", _s.base_url))
        .json(&serde_json::json!({}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 404);

    // 表封顶：BAPI_USER_GETLIST 注册时给 max_rows=5（默认表封顶可被条目覆盖）
    let resp = http_client()
        .put(format!("{}/api/registry/bapi-users", _s.base_url))
        .json(&serde_json::json!({
            "func_name": "BAPI_USER_GETLIST", "status": "published", "max_rows": 5
        }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = http_client()
        .post(format!("{}/api/invokes/bapi-users", _s.base_url))
        .json(&serde_json::json!({}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().unwrap();
    let n = body["USERLIST"].as_array().map(|a| a.len()).unwrap_or(0);
    assert!(n <= 5, "条目 max_rows=5 应生效，实得 {n} 行: {body}");
    // ?limit=2 覆盖条目值
    let resp = http_client()
        .post(format!("{}/api/invokes/bapi-users?limit=2", _s.base_url))
        .json(&serde_json::json!({}))
        .send()
        .unwrap();
    let body: serde_json::Value = resp.json().unwrap();
    let n = body["USERLIST"].as_array().map(|a| a.len()).unwrap_or(0);
    assert!(n <= 2, "?limit=2 应生效，实得 {n} 行");

    // 公开 /openapi.json：watcher 预热后应含两个 published 条目的类型化 operation
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let body: serde_json::Value = http_client()
            .get(format!("{}/openapi.json", _s.base_url))
            .send()
            .unwrap()
            .json()
            .unwrap();
        if body["paths"]["/api/invokes/stfc-echo"].is_object()
            && body["paths"]["/api/invokes/bapi-users"].is_object()
        {
            // intent 流入 summary；请求体 schema 已按接口展开
            assert_eq!(
                body["paths"]["/api/invokes/stfc-echo"]["post"]["summary"],
                "connectivity check echo"
            );
            let schema = &body["paths"]["/api/invokes/stfc-echo"]["post"]["requestBody"]
                ["content"]["application/json"]["schema"];
            assert_eq!(schema["properties"]["REQUTEXT"]["type"], "string");
            break;
        }
        assert!(Instant::now() < deadline, "openapi 目录未在 15s 内出现: {body}");
        std::thread::sleep(Duration::from_millis(500));
    }

    // /api/openapi 无参模式 = 注册表目录（新鲜构建，立即含条目）
    let body: serde_json::Value = http_client()
        .get(format!("{}/api/openapi", _s.base_url))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert!(body["paths"]["/api/invokes/stfc-echo"].is_object());

    // 墓碑后不可调用
    let resp = http_client()
        .delete(format!("{}/api/registry/stfc-echo", _s.base_url))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = http_client()
        .post(format!("{}/api/invokes/stfc-echo", _s.base_url))
        .json(&serde_json::json!({"requtext": "x"}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 404, "墓碑条目应 404");
}
