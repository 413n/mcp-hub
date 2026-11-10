//! End to end: a fake upstream MCP server, the hub in front of it, and real MCP clients.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mcp_hub::{
    config::parse_config,
    http,
    hub::Hub,
    secrets::SecretStore,
    upstream::{Timings, Upstream},
};
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
    transport::{
        StreamableHttpClientTransport, StreamableHttpServerConfig, StreamableHttpService,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "test-token-0123456789abcdefghijklmnopqrstuvwxyz";

/// Stands in for a real upstream: one tool that echoes, one with an unusable name.
#[derive(Clone, Default)]
struct Fake {
    /// How many tools/list requests it got.
    lists: Arc<AtomicUsize>,
    list_delay: Duration,
    /// Also list "wordy", a tool that says far too much about itself.
    wordy: bool,
}

/// What "wordy" has to say: `about` it, over and over, and how it ends.
fn wordy_text(about: &str) -> String {
    format!(
        "{}\n\nThe end of {about}.",
        format!("All about {about}. ").repeat(60)
    )
}

impl ServerHandler for Fake {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.list_delay).await;
        let tools = ["echo", "bad.name"].map(|name| {
            serde_json::from_value(json!({"name": name, "inputSchema": {"type": "object"}}))
                .unwrap()
        });
        let mut tools = tools.to_vec();
        if self.wordy {
            let wordy = json!({
                "name": "wordy",
                "description": wordy_text("the tool"),
                "inputSchema": {"type": "object", "properties": {
                    "command": {"type": "string", "description": wordy_text("the command")},
                }},
            });
            tools.push(serde_json::from_value(wordy).unwrap());
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        req: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if req.name == "fail" {
            return Err(ErrorData::invalid_params("upstream says no", None));
        }
        if req.name == "sleep" {
            let ms = req.arguments.as_ref().and_then(|a| a["ms"].as_u64());
            tokio::time::sleep(Duration::from_millis(ms.unwrap_or(0))).await;
        }
        if req.name == "rows" {
            // A large JSON result with empty fields, as text and as structured content.
            let rows: Vec<_> = (0..300)
                .map(|n| json!({"n": n, "note": null, "tags": [], "label": "row"}))
                .collect();
            let data = json!({"rows": rows, "next": null});
            let mut result = CallToolResult::success(vec![ContentBlock::text(data.to_string())]);
            result.structured_content = Some(data);
            return Ok(CallToolResponse::Complete(result));
        }
        let args = serde_json::to_string(&req.arguments).unwrap();
        let text = format!("{}:{args}", req.name);
        Ok(CallToolResponse::Complete(CallToolResult::success(vec![
            ContentBlock::text(text),
        ])))
    }
}

async fn start_fake(fake: Fake) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut config = StreamableHttpServerConfig::default();
    config.legacy_session_mode = false;
    config.json_response = true;
    config.allowed_hosts = vec![format!("127.0.0.1:{port}")];
    let service = Arc::new(StreamableHttpService::<
        Fake,
        rmcp::transport::streamable_http_server::session::local::LocalSessionManager,
    >::new(move || Ok(fake.clone()), Arc::default(), config));
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let service = service.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req| {
                    let service = service.clone();
                    async move { Ok::<_, std::convert::Infallible>(service.handle(req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    port
}

async fn start_hub() -> u16 {
    let fake_port = start_fake(Fake::default()).await;
    let config = parse_config(
        &json!({"servers": {
            "fake": {"url": format!("http://127.0.0.1:{fake_port}/mcp"), "description": "Echoes things back", "skipWhenEnv": "INSIDE_FAKE"},
            "sentry": {"url": "https://mcp.sentry.dev/mcp", "oauth": {}},
            "local": {
                "command": "/bin/sh",
                "args": [concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stdio-server.sh")],
                "env": {"FAKE_SECRET": "from-env"},
            },
        }})
        .to_string(),
    )
    .unwrap();
    let store = SecretStore::memory();
    let hub = Hub::new(
        config
            .servers
            .iter()
            .map(|s| Upstream::new(s.clone(), store.clone()))
            .collect(),
    );
    let listener = http::bind(0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { http::serve(listener, hub, TOKEN, CancellationToken::new()).await });
    port
}

async fn raw(port: u16, f: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder) -> u16 {
    let req = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    f(req).send().await.unwrap().status().as_u16()
}

#[tokio::test]
async fn inbound_checks() {
    let port = start_hub().await;
    let bearer = format!("Bearer {TOKEN}");
    assert_eq!(raw(port, |r| r).await, 401, "no token");
    assert_eq!(
        raw(port, |r| r.header("authorization", "Bearer wrong")).await,
        401,
        "wrong token"
    );
    assert_eq!(
        raw(port, |r| r
            .header("authorization", &bearer)
            .header("host", "evil.example"))
        .await,
        403,
        "host"
    );
    assert_eq!(
        raw(port, |r| r
            .header("authorization", &bearer)
            .header("origin", "http://127.0.0.1:1"))
        .await,
        403,
        "origin"
    );
    let status = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/mcp"))
        .header("authorization", &bearer)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 405, "GET");
    let status = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/other"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 404, "path");
}

#[tokio::test]
async fn proxies_tools() {
    let port = start_hub().await;
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://127.0.0.1:{port}/mcp"))
            .auth_header(TOKEN),
    );
    let client = mcp_hub::upstream::client_info()
        .serve(transport)
        .await
        .unwrap();

    let instructions = client.peer_info().unwrap().instructions.clone().unwrap();
    assert!(
        instructions.contains("- fake: Echoes things back"),
        "{instructions}"
    );
    assert!(instructions.contains("- sentry"), "{instructions}");

    // sentry is not logged in and bad.name is unusable; local is a child process.
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<_> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, ["fake__echo", "local__env", "local__crash"]);

    let mut params = CallToolRequestParams::new("fake__echo");
    params.arguments = Some(json!({"x": 1}).as_object().unwrap().clone());
    let result = client.call_tool(params).await.unwrap();
    assert_eq!(result.content[0].as_text().unwrap().text, r#"echo:{"x":1}"#);

    let result = client
        .call_tool(CallToolRequestParams::new("local__env"))
        .await
        .unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    assert!(text.ends_with(" secret=from-env"), "{text}");

    // Upstream JSON-RPC errors pass through.
    let err = client
        .call_tool(CallToolRequestParams::new("fake__fail"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("upstream says no"), "{err}");

    // Unknown server or tool name.
    let err = client
        .call_tool(CallToolRequestParams::new("nope__x"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Unknown tool"), "{err}");

    // Not logged in: a tool error the agent can read, not a protocol error.
    let result = client
        .call_tool(CallToolRequestParams::new("sentry__whoami"))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    let text = &result.content[0].as_text().unwrap().text;
    assert!(text.contains("mcp-hub login sentry"), "{text}");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn caches_tool_lists() {
    let fast = Fake::default();
    let slow = Fake {
        list_delay: Duration::from_millis(800),
        ..Fake::default()
    };
    let config = parse_config(
        &json!({"servers": {
            "fast": {"url": format!("http://127.0.0.1:{}/mcp", start_fake(fast.clone()).await)},
            "slow": {"url": format!("http://127.0.0.1:{}/mcp", start_fake(slow.clone()).await)},
        }})
        .to_string(),
    )
    .unwrap();
    let timings = Timings {
        list_ttl: Duration::from_millis(1500),
        cold_list_wait: Duration::from_millis(300),
        ..Timings::default()
    };
    let store = SecretStore::memory();
    let hub = Hub::new(
        config
            .servers
            .iter()
            .map(|s| Upstream::with_timings(s.clone(), store.clone(), timings.clone()))
            .collect(),
    );
    let names = |tools: Vec<rmcp::model::Tool>| {
        tools.iter().map(|t| t.name.to_string()).collect::<Vec<_>>()
    };
    let counts = || {
        (
            fast.lists.load(Ordering::SeqCst),
            slow.lists.load(Ordering::SeqCst),
        )
    };

    // Many agents at once: each upstream is asked once, and the slow one is not waited for.
    let lists = futures::future::join_all((0..20).map(|_| hub.list_tools())).await;
    for list in lists {
        assert_eq!(names(list), ["fast__echo"]);
    }
    assert_eq!(counts(), (1, 1));

    // The slow one has answered meanwhile, and nothing is asked again.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(names(hub.list_tools().await), ["fast__echo", "slow__echo"]);
    assert_eq!(counts(), (1, 1));

    // Past the TTL the old list is served at once, slow upstream included, and refreshed once.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let started = std::time::Instant::now();
    let lists = futures::future::join_all((0..20).map(|_| hub.list_tools())).await;
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(names(lists[0].clone()), ["fast__echo", "slow__echo"]);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(counts(), (2, 2));
}

/// `mcp-hub connect`: an agent talks MCP over a pipe, and the token comes from the keyring.
#[tokio::test]
async fn connects_over_stdio() {
    let port = start_hub().await;
    let store = SecretStore::memory();
    store.set(mcp_hub::TOKEN_KEY, TOKEN).await.unwrap();
    let (agent, bridge) = tokio::io::duplex(1 << 16);
    let (input, output) = tokio::io::split(bridge);
    tokio::spawn(mcp_hub::connect::run(
        port,
        store.clone(),
        Vec::new(),
        input,
        output,
    ));

    let client = mcp_hub::upstream::client_info().serve(agent).await.unwrap();
    let instructions = client.peer_info().unwrap().instructions.clone().unwrap();
    assert!(instructions.contains("- fake"), "{instructions}");
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 3);
    let mut params = CallToolRequestParams::new("fake__echo");
    params.arguments = Some(json!({"x": 1}).as_object().unwrap().clone());
    let result = client.call_tool(params).await.unwrap();
    assert_eq!(result.content[0].as_text().unwrap().text, r#"echo:{"x":1}"#);
    let err = client
        .call_tool(CallToolRequestParams::new("fake__fail"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("upstream says no"), "{err}");

    client.cancel().await.unwrap();

    // A wrong token is reported, not hung on.
    store
        .set(mcp_hub::TOKEN_KEY, "not-the-token")
        .await
        .unwrap();
    let (agent, bridge) = tokio::io::duplex(1 << 16);
    let (input, output) = tokio::io::split(bridge);
    tokio::spawn(mcp_hub::connect::run(
        port,
        store.clone(),
        Vec::new(),
        input,
        output,
    ));
    let err = mcp_hub::upstream::client_info().serve(agent).await.err();
    assert!(
        err.as_ref()
            .is_some_and(|e| format!("{e:?}").contains("refused the token")),
        "{err:?}"
    );

    // A token rotated after the bridge started is read again.
    let (agent, bridge) = tokio::io::duplex(1 << 16);
    let (input, output) = tokio::io::split(bridge);
    tokio::spawn(mcp_hub::connect::run(
        port,
        store.clone(),
        Vec::new(),
        input,
        output,
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    store.set(mcp_hub::TOKEN_KEY, TOKEN).await.unwrap();
    let client = mcp_hub::upstream::client_info().serve(agent).await.unwrap();
    assert_eq!(client.list_all_tools().await.unwrap().len(), 3);
    client.cancel().await.unwrap();
}

/// A server with `skipWhenEnv` is left out for an agent started with that variable.
#[tokio::test]
async fn leaves_out_a_skipped_server() {
    let config = parse_config(
        r#"{"servers":{"a":{"url":"https://a/","skipWhenEnv":"INSIDE_A"},"b":{"url":"https://b/"}}}"#,
    )
    .unwrap();
    assert_eq!(
        mcp_hub::connect::skipped(&config, |var| var == "INSIDE_A"),
        ["a"]
    );
    assert!(mcp_hub::connect::skipped(&config, |_| false).is_empty());

    let port = start_hub().await;
    let store = SecretStore::memory();
    store.set(mcp_hub::TOKEN_KEY, TOKEN).await.unwrap();
    let (agent, bridge) = tokio::io::duplex(1 << 16);
    let (input, output) = tokio::io::split(bridge);
    let skip = vec!["fake".to_owned()];
    tokio::spawn(mcp_hub::connect::run(port, store, skip, input, output));
    let client = mcp_hub::upstream::client_info().serve(agent).await.unwrap();

    let instructions = client.peer_info().unwrap().instructions.clone().unwrap();
    assert!(!instructions.contains("- fake"), "{instructions}");
    assert!(instructions.contains("- sentry"), "{instructions}");
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<_> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, ["local__env", "local__crash"]);
    let result = client
        .call_tool(CallToolRequestParams::new("fake__echo"))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    let text = &result.content[0].as_text().unwrap().text;
    assert!(
        text.contains("left out of this session (INSIDE_FAKE is set)"),
        "{text}"
    );
    client.cancel().await.unwrap();
}

/// Calls, lists and what the upstreams did end up in the record, without their content.
#[tokio::test]
async fn records_what_happens() {
    use mcp_hub::stats::{Stats, summary};
    let dir = std::env::temp_dir().join(format!("mcp-hub-record-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let stats = Stats::open(&dir);
    let config = parse_config(
        &json!({"servers": {
            "fake": {"url": format!("http://127.0.0.1:{}/mcp", start_fake(Fake::default()).await)},
            "gone": {"url": "http://127.0.0.1:9/mcp"},
        }})
        .to_string(),
    )
    .unwrap();
    let store = SecretStore::memory();
    store.set(mcp_hub::TOKEN_KEY, TOKEN).await.unwrap();
    let upstreams = config
        .servers
        .iter()
        .map(|s| Upstream::with(s.clone(), store.clone(), Timings::default(), stats.clone()));
    let hub = Hub::with_stats(upstreams.collect(), stats);
    let listener = http::bind(0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { http::serve(listener, hub, TOKEN, CancellationToken::new()).await });

    let (agent, bridge) = tokio::io::duplex(1 << 16);
    let (input, output) = tokio::io::split(bridge);
    tokio::spawn(mcp_hub::connect::run(
        port,
        store,
        Vec::new(),
        input,
        output,
    ));
    let client = mcp_hub::upstream::client_info().serve(agent).await.unwrap();
    client.list_all_tools().await.unwrap();
    let mut params = CallToolRequestParams::new("fake__echo");
    params.arguments = Some(json!({"secret": "hunter2"}).as_object().unwrap().clone());
    client.call_tool(params).await.unwrap();
    let _ = client
        .call_tool(CallToolRequestParams::new("fake__fail"))
        .await;
    client
        .call_tool(CallToolRequestParams::new("gone__x"))
        .await
        .unwrap();
    client.cancel().await.unwrap();

    let s = summary(&dir, 1, None);
    assert_eq!(s["total"]["calls"], 3, "{s}");
    assert_eq!(s["total"]["failed"], 2, "{s}");
    assert_eq!(s["agents"][0]["agent"], "mcp-hub", "{s}");
    assert_eq!(s["agents"][0]["toolLists"], 1, "{s}");
    assert_eq!(s["servers"]["fake"]["connected"], 1, "{s}");
    assert!(s["servers"]["gone"]["failed"].as_u64() >= Some(1), "{s}");
    let record = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
    assert!(record.contains(r#""tool":"echo""#), "{record}");
    assert!(record.contains(r#""error":"code -32602""#), "{record}");
    assert!(
        !record.contains("hunter2") && !record.contains("upstream says no"),
        "{record}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Long descriptions are cut where the hub lists the tools, and `hub__describe` has the rest.
#[tokio::test]
async fn cuts_long_descriptions() {
    let wordy = Fake {
        wordy: true,
        ..Fake::default()
    };
    let config = parse_config(
        &json!({"maxDescription": 300, "servers": {
            "fake": {"url": format!("http://127.0.0.1:{}/mcp", start_fake(wordy).await)},
            "whole": {
                "url": format!("http://127.0.0.1:{}/mcp", start_fake(Fake { wordy: true, ..Fake::default() }).await),
                "maxDescription": 0,
            },
        }})
        .to_string(),
    )
    .unwrap();
    let store = SecretStore::memory();
    store.set(mcp_hub::TOKEN_KEY, TOKEN).await.unwrap();
    let upstreams = config
        .servers
        .iter()
        .map(|s| Upstream::new(s.clone(), store.clone()));
    let hub = Hub::new(upstreams.collect());
    let listener = http::bind(0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { http::serve(listener, hub, TOKEN, CancellationToken::new()).await });
    let (agent, bridge) = tokio::io::duplex(1 << 16);
    let (input, output) = tokio::io::split(bridge);
    tokio::spawn(mcp_hub::connect::run(
        port,
        store,
        Vec::new(),
        input,
        output,
    ));
    let client = mcp_hub::upstream::client_info().serve(agent).await.unwrap();

    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<_> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(
        names,
        [
            "fake__echo",
            "fake__wordy",
            "whole__echo",
            "whole__wordy",
            "hub__describe"
        ]
    );
    let description = tools[1].description.as_deref().unwrap();
    assert!(
        description.starts_with("All about the tool."),
        "{description}"
    );
    assert!(
        description.len() < 500 && !description.contains("The end"),
        "{description}"
    );
    assert!(description.contains(r#"call hub__describe with {"tool": "fake__wordy"}"#));
    let parameter = tools[1].input_schema["properties"]["command"]["description"].as_str();
    assert!(
        parameter.is_some_and(|p| p.len() < 500 && !p.contains("The end")),
        "{parameter:?}"
    );
    // The server that asked to be left alone is.
    assert_eq!(
        tools[3].description.as_deref(),
        Some(wordy_text("the tool").as_str())
    );

    let describe = |tool: &'static str| {
        let mut params = CallToolRequestParams::new("hub__describe");
        params.arguments = Some(json!({"tool": tool}).as_object().unwrap().clone());
        let client = &client;
        async move {
            let result = client.call_tool(params).await.unwrap();
            result.content[0].as_text().unwrap().text.clone()
        }
    };
    // By the name an agent knows the tool under, prefix and all.
    let full = describe("mcp__hub__fake__wordy").await;
    assert!(
        full.contains(&format!("## Description\n\n{}", wordy_text("the tool"))),
        "{full}"
    );
    assert!(
        full.contains(&format!(
            "## Parameter command\n\n{}",
            wordy_text("the command")
        )),
        "{full}"
    );
    assert!(describe("fake__echo").await.contains("Nothing was cut"));
    assert!(describe("whole__wordy").await.contains("Nothing was cut"));
    assert!(describe("nope").await.contains("No tool named"));
    client.cancel().await.unwrap();
}

/// Results larger than `maxResult` are kept in a file; `compactResults` drops empty fields.
#[tokio::test]
async fn shrinks_results() {
    let fake = format!("http://127.0.0.1:{}/mcp", start_fake(Fake::default()).await);
    let config = parse_config(
        &json!({"servers": {
            "big": {"url": fake, "maxResult": 5000},
            "lean": {"url": fake, "compactResults": true},
            "raw": {"url": fake, "maxResult": 0},
        }})
        .to_string(),
    )
    .unwrap();
    let dir = std::env::temp_dir().join(format!("mcp-hub-kept-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = SecretStore::memory();
    let upstreams = config
        .servers
        .iter()
        .map(|s| Upstream::new(s.clone(), store.clone()));
    let hub = Hub::new(upstreams.collect()).keeping_results_in(dir.clone());
    let listener = http::bind(0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { http::serve(listener, hub, TOKEN, CancellationToken::new()).await });
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://127.0.0.1:{port}/mcp"))
            .auth_header(TOKEN),
    );
    let client = mcp_hub::upstream::client_info()
        .serve(transport)
        .await
        .unwrap();
    let call = |name: &'static str| client.call_tool(CallToolRequestParams::new(name));

    let raw = call("raw__rows").await.unwrap();
    let whole = raw.content[0].as_text().unwrap().text.clone();
    assert!(
        whole.len() > 10_000 && raw.structured_content.is_some(),
        "{}",
        whole.len()
    );

    let big = call("big__rows").await.unwrap();
    let note = &big.content[0].as_text().unwrap().text;
    assert!(
        note.len() < 3000 && big.structured_content.is_none(),
        "{note}"
    );
    assert!(
        note.contains(&format!("this result is {} characters", whole.len())),
        "{note}"
    );
    let path = note
        .split("in the file ")
        .nth(1)
        .unwrap()
        .split(", kept")
        .next()
        .unwrap();
    assert!(
        path.starts_with(dir.to_str().unwrap()) && path.contains("big-rows"),
        "{path}"
    );
    let kept: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(kept, raw.structured_content.unwrap());

    let lean = call("lean__rows").await.unwrap();
    let text = &lean.content[0].as_text().unwrap().text;
    assert!(
        text.starts_with(r#"{"rows":[{"n":0,"label":"row"}"#),
        "{}",
        &text[..80]
    );
    assert!(!text.contains("null") && !text.contains("[]"));
    assert_eq!(
        lean.structured_content.unwrap()["rows"][1],
        json!({"n": 1, "label": "row"})
    );
    client.cancel().await.unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A hub in front of `fake`, served until the token returned is cancelled, with `store` to
/// read its token from again.
async fn serve_fake(
    fake: Fake,
    timings: Timings,
    store: &SecretStore,
) -> (
    u16,
    CancellationToken,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let fake_port = start_fake(fake).await;
    let config = parse_config(
        &json!({"servers": {"fake": {"url": format!("http://127.0.0.1:{fake_port}/mcp")}}})
            .to_string(),
    )
    .unwrap();
    let upstream = Upstream::with_timings(config.servers[0].clone(), store.clone(), timings);
    let hub = Hub::new(vec![upstream]);
    let listener = http::bind(0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let shutdown = CancellationToken::new();
    let serving = tokio::spawn(http::serve_with(
        listener,
        hub,
        TOKEN,
        Some(store.clone()),
        shutdown.clone(),
    ));
    (port, shutdown, serving)
}

async fn call_sleep(client: &reqwest::Client, port: u16, ms: u64) -> Result<String, String> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "fake__sleep", "arguments": {"ms": ms}}});
    let res = client
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .bearer_auth(TOKEN)
        .header("accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    res.text().await.map_err(|e| e.to_string())
}

/// Told to stop, the hub closes idle connections at once, lets a short call finish, and
/// gives a long one a few seconds only.
#[tokio::test]
async fn stops_quickly() {
    let store = SecretStore::memory();
    let (port, shutdown, serving) = serve_fake(Fake::default(), Timings::default(), &store).await;
    let client = reqwest::Client::new();
    // A connection kept open, idle, as agents keep them.
    assert!(
        call_sleep(&client, port, 0)
            .await
            .unwrap()
            .contains("sleep:")
    );

    let short = tokio::spawn({
        let client = client.clone();
        async move { call_sleep(&client, port, 1000).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = std::time::Instant::now();
    shutdown.cancel();
    serving.await.unwrap().unwrap();
    let took = started.elapsed();
    assert!(took < Duration::from_millis(2000), "{took:?}");
    let answer = short.await.unwrap().unwrap();
    assert!(answer.contains("sleep:"), "{answer}");
    assert!(
        reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/mcp"))
            .send()
            .await
            .is_err()
    );

    let (port, shutdown, serving) = serve_fake(Fake::default(), Timings::default(), &store).await;
    let long = tokio::spawn(async move { call_sleep(&reqwest::Client::new(), port, 60_000).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = std::time::Instant::now();
    shutdown.cancel();
    serving.await.unwrap().unwrap();
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(2900) && took < Duration::from_millis(4000),
        "{took:?}"
    );
    long.abort();
}

/// `mcp-hub token --rotate`: once the hub is told, the old token is turned down.
#[tokio::test]
async fn rotates_the_token() {
    let store = SecretStore::memory();
    let (port, _shutdown, _serving) = serve_fake(Fake::default(), Timings::default(), &store).await;
    let new = "the-next-token-0123456789abcdefghijklmnopqrstuvwxyz";
    store.set(mcp_hub::TOKEN_KEY, new).await.unwrap();
    let with = |token: &'static str| move |r: reqwest::RequestBuilder| r.bearer_auth(token);
    // Not before it is told.
    assert_eq!(raw(port, with(TOKEN)).await, 200);
    assert_eq!(raw(port, with(new)).await, 401);
    // Only with the token it has.
    assert!(http::reload_token(port, new).await.is_err());
    assert_eq!(
        http::reload_token(port, TOKEN).await.unwrap(),
        http::Reloaded::Yes
    );
    assert_eq!(raw(port, with(TOKEN)).await, 401);
    assert_eq!(raw(port, with(new)).await, 200);
    assert!(http::reload_token(port, TOKEN).await.is_err());

    // A hub that cannot, and none at all.
    let old_hub = start_hub().await;
    assert_eq!(
        http::reload_token(old_hub, TOKEN).await.unwrap(),
        http::Reloaded::TooOld
    );
    let free = http::bind(0).await.unwrap().local_addr().unwrap().port();
    assert_eq!(
        http::reload_token(free, TOKEN).await.unwrap(),
        http::Reloaded::NotRunning
    );
    assert!(http::answers(port).await);
    assert!(!http::answers(free).await);
}

/// An agent that only tells when its tools change.
#[derive(Clone)]
struct Listener(tokio::sync::mpsc::UnboundedSender<()>);

impl rmcp::ClientHandler for Listener {
    async fn on_tool_list_changed(&self, _: rmcp::service::NotificationContext<rmcp::RoleClient>) {
        let _ = self.0.send(());
    }
}

/// A server slower to start than a tools/list waits for: `connect` tells the agent once
/// its tools are there.
#[tokio::test]
async fn tells_the_agent_when_tools_arrive() {
    let store = SecretStore::memory();
    store.set(mcp_hub::TOKEN_KEY, TOKEN).await.unwrap();
    let slow = Fake {
        list_delay: Duration::from_millis(1000),
        ..Fake::default()
    };
    let timings = Timings {
        cold_list_wait: Duration::from_millis(200),
        ..Timings::default()
    };
    let (port, _shutdown, _serving) = serve_fake(slow, timings, &store).await;
    let (agent, bridge) = tokio::io::duplex(1 << 16);
    let (input, output) = tokio::io::split(bridge);
    tokio::spawn(mcp_hub::connect::run(
        port,
        store,
        Vec::new(),
        input,
        output,
    ));

    let (told, mut changes) = tokio::sync::mpsc::unbounded_channel();
    let client = Listener(told).serve(agent).await.unwrap();
    let capabilities = &client.peer_info().unwrap().capabilities;
    assert_eq!(
        capabilities.tools.as_ref().unwrap().list_changed,
        Some(true)
    );
    assert_eq!(client.list_all_tools().await.unwrap().len(), 0);
    tokio::time::timeout(Duration::from_secs(5), changes.recv())
        .await
        .expect("no tools/list_changed")
        .unwrap();
    assert_eq!(client.list_all_tools().await.unwrap().len(), 1);
    // Nothing more changes, nothing more is said.
    let more = tokio::time::timeout(Duration::from_millis(2500), changes.recv()).await;
    assert!(more.is_err(), "{more:?}");
    client.cancel().await.unwrap();
}

/// A restarted hub lists the tools saved by the one before at once, and fetches them again.
#[tokio::test]
async fn remembers_tool_lists() {
    let dir = std::env::temp_dir().join(format!("mcp-hub-tools-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let fake = Fake {
        list_delay: Duration::from_millis(500),
        ..Fake::default()
    };
    let lists = fake.lists.clone();
    let fake_port = start_fake(fake).await;
    let server = |max_description: usize| {
        let config = parse_config(
            &json!({"servers": {"fake": {
                "url": format!("http://127.0.0.1:{fake_port}/mcp"),
                "maxDescription": max_description,
            }}})
            .to_string(),
        )
        .unwrap();
        let timings = Timings {
            cold_list_wait: Duration::from_millis(100),
            ..Timings::default()
        };
        Upstream::with_timings(config.servers[0].clone(), SecretStore::memory(), timings)
    };

    let first = server(2000);
    first.keep_tools_in(&dir);
    assert!(first.pending());
    assert_eq!(first.list_tools().await.unwrap().len(), 2);
    assert!(dir.join("fake.json").exists());

    let restarted = server(2000);
    restarted.keep_tools_in(&dir);
    assert!(!restarted.pending());
    let generation = restarted.tools_generation();
    let started = std::time::Instant::now();
    assert_eq!(restarted.tools().await.unwrap().len(), 1);
    assert!(started.elapsed() < Duration::from_millis(100));
    // And asks the server again, in the background; the same tools change nothing.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(lists.load(Ordering::SeqCst), 2);
    assert_eq!(restarted.tools_generation(), generation);

    // Another config: the saved list does not apply.
    let other = server(500);
    other.keep_tools_in(&dir);
    assert!(other.pending());
    assert!(other.tools().await.is_none());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `mcp-hub status`, asked of the running hub.
#[tokio::test]
async fn reports_status() {
    let port = start_hub().await;
    let servers = http::status(port, TOKEN).await.unwrap().unwrap();
    let names: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["fake", "sentry", "local"]);
    assert_eq!(servers[0].tools, Some(2));
    assert!(servers[1].tools.is_none());
    let error = servers[1].error.as_deref().unwrap();
    assert!(error.contains("mcp-hub login sentry"), "{error}");
    assert!(servers[2].tools.is_some_and(|n| n > 0), "{:?}", servers[2]);

    assert!(http::status(port, "wrong").await.is_err());
    let free = http::bind(0).await.unwrap().local_addr().unwrap().port();
    assert!(http::status(free, TOKEN).await.unwrap().is_none());
}
