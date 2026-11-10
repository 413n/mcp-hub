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
        Ok(ListToolsResult::with_all_items(tools.to_vec()))
    }

    async fn call_tool(
        &self,
        req: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if req.name == "fail" {
            return Err(ErrorData::invalid_params("upstream says no", None));
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
            "fake": {"url": format!("http://127.0.0.1:{fake_port}/mcp"), "description": "Echoes things back"},
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
    tokio::spawn(mcp_hub::connect::run(port, store.clone(), input, output));

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
    tokio::spawn(mcp_hub::connect::run(port, store.clone(), input, output));
    let err = mcp_hub::upstream::client_info().serve(agent).await.err();
    assert!(
        err.as_ref()
            .is_some_and(|e| format!("{e:?}").contains("refused the token")),
        "{err:?}"
    );

    // A token rotated after the bridge started is read again.
    let (agent, bridge) = tokio::io::duplex(1 << 16);
    let (input, output) = tokio::io::split(bridge);
    tokio::spawn(mcp_hub::connect::run(port, store.clone(), input, output));
    tokio::time::sleep(Duration::from_millis(100)).await;
    store.set(mcp_hub::TOKEN_KEY, TOKEN).await.unwrap();
    let client = mcp_hub::upstream::client_info().serve(agent).await.unwrap();
    assert_eq!(client.list_all_tools().await.unwrap().len(), 3);
    client.cancel().await.unwrap();
}
