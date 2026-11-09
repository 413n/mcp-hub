//! The inbound endpoint: http://127.0.0.1:<port>/mcp with a bearer token.
//!
//! Checks, in order: loopback peer, exact Host (DNS rebinding), no Origin
//! header (agents never send one; browsers always do on POST), path, token,
//! method. Then rmcp's stateless Streamable HTTP handler takes over.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{Request, Response, StatusCode, body::Incoming, header, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use rmcp::transport::{
    StreamableHttpServerConfig, StreamableHttpService, streamable_http_server::session::local::LocalSessionManager,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::hub::Hub;

type Body = BoxBody<Bytes, Infallible>;

struct Endpoint {
    port: u16,
    expected: [u8; 32],
    mcp: StreamableHttpService<Hub, LocalSessionManager>,
}

fn digest(value: &[u8]) -> [u8; 32] {
    Sha256::digest(value).into()
}

fn reject(status: StatusCode, message: &str) -> Response<Body> {
    let mut res = Response::new(Full::new(Bytes::from(format!("{message}\n"))).boxed());
    *res.status_mut() = status;
    res.headers_mut().insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
    if status == StatusCode::METHOD_NOT_ALLOWED {
        res.headers_mut().insert(header::ALLOW, "POST".parse().unwrap());
    }
    res
}

impl Endpoint {
    async fn handle(&self, req: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
        if !peer.ip().is_loopback() {
            return reject(StatusCode::FORBIDDEN, "forbidden");
        }
        let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
        if host != format!("127.0.0.1:{}", self.port) && host != format!("localhost:{}", self.port) {
            return reject(StatusCode::FORBIDDEN, "invalid Host header");
        }
        if req.headers().contains_key(header::ORIGIN) {
            return reject(StatusCode::FORBIDDEN, "browser requests are not allowed");
        }
        if req.uri().path() != "/mcp" {
            return reject(StatusCode::NOT_FOUND, "not found");
        }
        let auth = req.headers().get(header::AUTHORIZATION).map(|v| v.as_bytes()).unwrap_or(b"");
        if !bool::from(digest(auth).ct_eq(&self.expected)) {
            return reject(StatusCode::UNAUTHORIZED, "unauthorized");
        }
        if req.method() != hyper::Method::POST {
            return reject(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        self.mcp.handle(req).await
    }
}

/// Serve until `shutdown` is cancelled. Bind with [`bind`] first.
pub async fn serve(listener: TcpListener, hub: Hub, token: &str, shutdown: CancellationToken) -> anyhow::Result<()> {
    anyhow::ensure!(token.len() >= 32, "inbound token missing or too short");
    let port = listener.local_addr()?.port();
    let mut config = StreamableHttpServerConfig::default();
    config.legacy_session_mode = false; // stateless: no per-client state to leak
    config.json_response = true;
    config.sse_keep_alive = None;
    config.allowed_hosts = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    config.cancellation_token = shutdown.child_token();
    let config = config.enforce_origin_validation();
    let endpoint = Arc::new(Endpoint {
        port,
        expected: digest(format!("Bearer {token}").as_bytes()),
        mcp: StreamableHttpService::new(move || Ok(hub.clone()), Arc::default(), config),
    });

    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(conn) => conn,
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    continue;
                }
            },
            _ = shutdown.cancelled() => return Ok(()),
        };
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let endpoint = endpoint.clone();
                async move { Ok::<_, Infallible>(endpoint.handle(req, peer).await) }
            });
            if let Err(e) = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await {
                tracing::debug!("connection error: {e}");
            }
        });
    }
}

/// Hardcoded loopback: the bind address is deliberately not configurable.
pub async fn bind(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port)).await
}
