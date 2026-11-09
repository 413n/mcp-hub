//! End to end: a fake upstream MCP server, the hub in front of it, and real MCP clients.

use std::sync::Arc;

use mcp_hub::{config::parse_config, http, hub::Hub, secrets::SecretStore, upstream::Upstream};
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
#[derive(Clone)]
struct Fake;

impl ServerHandler for Fake {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = ["echo", "bad.name"]
            .map(|name| serde_json::from_value(json!({"name": name, "inputSchema": {"type": "object"}})).unwrap());
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
        Ok(CallToolResponse::Complete(CallToolResult::success(vec![ContentBlock::text(text)])))
    }
}

async fn start_fake() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut config = StreamableHttpServerConfig::default();
    config.legacy_session_mode = false;
    config.json_response = true;
    config.allowed_hosts = vec![format!("127.0.0.1:{port}")];
    let service = Arc::new(StreamableHttpService::<Fake, rmcp::transport::streamable_http_server::session::local::LocalSessionManager>::new(|| Ok(Fake), Arc::default(), config));
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
    let fake_port = start_fake().await;
    let config = parse_config(
        &json!({"servers": {
            "fake": {"url": format!("http://127.0.0.1:{fake_port}/mcp"), "description": "Echoes things back"},
            "sentry": {"url": "https://mcp.sentry.dev/mcp", "oauth": {}},
        }})
        .to_string(),
    )
    .unwrap();
    let store = SecretStore::memory();
    let hub = Hub::new(config.servers.iter().map(|s| Upstream::new(s.clone(), store.clone())).collect());
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
    assert_eq!(raw(port, |r| r.header("authorization", "Bearer wrong")).await, 401, "wrong token");
    assert_eq!(raw(port, |r| r.header("authorization", &bearer).header("host", "evil.example")).await, 403, "host");
    assert_eq!(
        raw(port, |r| r.header("authorization", &bearer).header("origin", "http://127.0.0.1:1")).await,
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
    let status = reqwest::Client::new().post(format!("http://127.0.0.1:{port}/other")).send().await.unwrap().status();
    assert_eq!(status, 404, "path");
}

#[tokio::test]
async fn proxies_tools() {
    let port = start_hub().await;
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://127.0.0.1:{port}/mcp")).auth_header(TOKEN),
    );
    let client = mcp_hub::upstream::client_info().serve(transport).await.unwrap();

    let instructions = client.peer_info().unwrap().instructions.clone().unwrap();
    assert!(instructions.contains("- fake: Echoes things back"), "{instructions}");
    assert!(instructions.contains("- sentry"), "{instructions}");

    // sentry is not logged in and bad.name is unusable: only fake__echo remains.
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.iter().map(|t| t.name.as_ref()).collect::<Vec<_>>(), ["fake__echo"]);

    let mut params = CallToolRequestParams::new("fake__echo");
    params.arguments = Some(json!({"x": 1}).as_object().unwrap().clone());
    let result = client.call_tool(params).await.unwrap();
    assert_eq!(result.content[0].as_text().unwrap().text, r#"echo:{"x":1}"#);

    // Upstream JSON-RPC errors pass through.
    let err = client.call_tool(CallToolRequestParams::new("fake__fail")).await.unwrap_err();
    assert!(err.to_string().contains("upstream says no"), "{err}");

    // Unknown server or tool name.
    let err = client.call_tool(CallToolRequestParams::new("nope__x")).await.unwrap_err();
    assert!(err.to_string().contains("Unknown tool"), "{err}");

    // Not logged in: a tool error the agent can read, not a protocol error.
    let result = client.call_tool(CallToolRequestParams::new("sentry__whoami")).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    let text = &result.content[0].as_text().unwrap().text;
    assert!(text.contains("mcp-hub login sentry"), "{text}");

    client.cancel().await.unwrap();
}
