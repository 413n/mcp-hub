//! The inbound endpoint: http://127.0.0.1:<port>/mcp with a bearer token.
//!
//! Checks, in order: loopback peer, exact Host (DNS rebinding), no Origin
//! header (agents never send one; browsers always do on POST), path, token,
//! method. Then rmcp's stateless Streamable HTTP handler takes over.
//!
//! Two more paths, for `mcp-hub` itself and with the token too: [`RELOAD_PATH`] has the hub
//! read its token from the keyring again (`mcp-hub token --rotate`), [`STATUS_PATH`] has it
//! ask every server for its tools (`mcp-hub status`).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{
    Request, Response, StatusCode, body::Incoming, header, server::conn::http1, service::service_fn,
};
use hyper_util::rt::TokioIo;
use rmcp::transport::{
    StreamableHttpServerConfig, StreamableHttpService,
    streamable_http_server::session::local::LocalSessionManager,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::TOKEN_KEY;
use crate::hub::Hub;
use crate::secrets::SecretStore;

type Body = BoxBody<Bytes, Infallible>;

/// POST here with the current token, and the hub takes the one in the keyring instead.
pub const RELOAD_PATH: &str = "/reload-token";
/// POST here, and the hub answers how each server is, as [`ServerStatus`]es.
pub const STATUS_PATH: &str = "/status";
/// On every answer: changes whenever the tools the agent gets change.
pub const TOOLS_HEADER: &str = "x-mcp-hub-tools";
/// On an answer while some servers have yet to say what tools they have: how many.
pub const PENDING_HEADER: &str = "x-mcp-hub-pending";
/// How long the requests running when the hub is told to stop get to finish.
const GRACE: Duration = Duration::from_secs(3);

struct Endpoint {
    port: u16,
    expected: RwLock<[u8; 32]>,
    /// Where to read the token again; `None` for a hub that never does.
    store: Option<SecretStore>,
    hub: Hub,
    mcp: StreamableHttpService<Hub, LocalSessionManager>,
}

fn digest(value: &[u8]) -> [u8; 32] {
    Sha256::digest(value).into()
}

fn reject(status: StatusCode, message: &str) -> Response<Body> {
    let mut res = Response::new(Full::new(Bytes::from(format!("{message}\n"))).boxed());
    *res.status_mut() = status;
    res.headers_mut()
        .insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
    if status == StatusCode::METHOD_NOT_ALLOWED {
        res.headers_mut()
            .insert(header::ALLOW, "POST".parse().unwrap());
    }
    res
}

impl Endpoint {
    async fn handle(&self, req: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
        if !peer.ip().is_loopback() {
            return reject(StatusCode::FORBIDDEN, "forbidden");
        }
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        if host != format!("127.0.0.1:{}", self.port) && host != format!("localhost:{}", self.port)
        {
            return reject(StatusCode::FORBIDDEN, "invalid Host header");
        }
        if req.headers().contains_key(header::ORIGIN) {
            return reject(StatusCode::FORBIDDEN, "browser requests are not allowed");
        }
        let reload = req.uri().path() == RELOAD_PATH && self.store.is_some();
        let status = req.uri().path() == STATUS_PATH;
        if req.uri().path() != "/mcp" && !reload && !status {
            return reject(StatusCode::NOT_FOUND, "not found");
        }
        let auth = req
            .headers()
            .get(header::AUTHORIZATION)
            .map(|v| v.as_bytes())
            .unwrap_or(b"");
        let expected = *self.expected.read().unwrap();
        if !bool::from(digest(auth).ct_eq(&expected)) {
            return reject(StatusCode::UNAUTHORIZED, "unauthorized");
        }
        if req.method() != hyper::Method::POST {
            return reject(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        if reload {
            return self.reload().await;
        }
        if status {
            let servers = self.hub.status().await.into_iter();
            let servers: Vec<_> = servers
                .map(|(name, listed)| match listed {
                    Ok(tools) => ServerStatus {
                        name,
                        tools: Some(tools),
                        error: None,
                    },
                    Err(error) => ServerStatus {
                        name,
                        tools: None,
                        error: Some(error),
                    },
                })
                .collect();
            let body = serde_json::to_vec(&servers).unwrap_or_default();
            let mut res = Response::new(Full::new(Bytes::from(body)).boxed());
            res.headers_mut()
                .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            return res;
        }
        // Taken before the request is handled: a list made after it has at least these tools.
        let (version, pending) = self.hub.tools_version(&crate::hub::skipped(req.headers()));
        let mut res = self.mcp.handle(req).await;
        let headers = res.headers_mut();
        if let Ok(version) = version.parse() {
            headers.insert(TOOLS_HEADER, version);
        }
        if pending > 0 {
            headers.insert(PENDING_HEADER, pending.into());
        }
        res
    }

    /// Take the token in the keyring from now on.
    async fn reload(&self) -> Response<Body> {
        let Some(store) = &self.store else {
            return reject(StatusCode::NOT_FOUND, "not found");
        };
        match store.get(TOKEN_KEY).await {
            Ok(Some(token)) if token.len() >= 32 => {
                *self.expected.write().unwrap() = digest(format!("Bearer {token}").as_bytes());
                tracing::info!("took the new inbound token from the keyring");
                let mut res = Response::new(Full::new(Bytes::new()).boxed());
                *res.status_mut() = StatusCode::NO_CONTENT;
                res
            }
            Ok(_) => reject(
                StatusCode::INTERNAL_SERVER_ERROR,
                "no usable token in the keyring",
            ),
            Err(e) => reject(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("cannot read the keyring: {e:#}"),
            ),
        }
    }
}

/// Serve until `shutdown` is cancelled. Bind with [`bind`] first.
pub async fn serve(
    listener: TcpListener,
    hub: Hub,
    token: &str,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    serve_with(listener, hub, token, None, shutdown).await
}

/// [`serve`], and read the token from `store` again when asked at [`RELOAD_PATH`].
///
/// Once `shutdown` is cancelled no new connection is taken, connections with no request
/// running are closed, and the requests running get a few seconds to finish before they
/// are dropped.
pub async fn serve_with(
    listener: TcpListener,
    hub: Hub,
    token: &str,
    store: Option<SecretStore>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    anyhow::ensure!(token.len() >= 32, "inbound token missing or too short");
    let port = listener.local_addr()?.port();
    let mut config = StreamableHttpServerConfig::default();
    config.legacy_session_mode = false; // stateless: no per-client state to leak
    config.json_response = true;
    config.sse_keep_alive = None;
    config.allowed_hosts = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    // Not `shutdown`: the requests running then get some time to finish first.
    let handlers = CancellationToken::new();
    config.cancellation_token = handlers.clone();
    let config = config.enforce_origin_validation();
    let mcp_hub = hub.clone();
    let endpoint = Arc::new(Endpoint {
        port,
        expected: RwLock::new(digest(format!("Bearer {token}").as_bytes())),
        store,
        hub,
        mcp: StreamableHttpService::new(move || Ok(mcp_hub.clone()), Arc::default(), config),
    });

    let connections = TaskTracker::new();
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(conn) => conn,
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    continue;
                }
            },
            _ = shutdown.cancelled() => break,
        };
        let endpoint = endpoint.clone();
        let shutdown = shutdown.clone();
        connections.spawn(async move {
            let service = service_fn(move |req| {
                let endpoint = endpoint.clone();
                async move { Ok::<_, Infallible>(endpoint.handle(req, peer).await) }
            });
            let conn = http1::Builder::new().serve_connection(TokioIo::new(stream), service);
            let mut conn = std::pin::pin!(conn);
            let served = tokio::select! {
                served = conn.as_mut() => served,
                _ = shutdown.cancelled() => {
                    // Closes the connection now if it is idle, else once its request is answered.
                    conn.as_mut().graceful_shutdown();
                    conn.await
                }
            };
            if let Err(e) = served {
                tracing::debug!("connection error: {e}");
            }
        });
    }
    drop(listener);
    connections.close();
    if tokio::time::timeout(GRACE, connections.wait())
        .await
        .is_err()
    {
        tracing::info!(
            "{} requests still running after {}s: dropping them",
            connections.len(),
            GRACE.as_secs()
        );
    }
    handlers.cancel();
    Ok(())
}

/// How one server is, as the running hub found it.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ServerStatus {
    pub name: String,
    /// How many tools it has, if it answered.
    pub tools: Option<usize>,
    /// Why not, if it did not.
    pub error: Option<String>,
}

/// Ask the hub on `port` how its servers are. `None` if no hub that can answer this runs
/// there (none at all, or an older version).
pub async fn status(port: u16, token: &str) -> anyhow::Result<Option<Vec<ServerStatus>>> {
    let url = format!("http://127.0.0.1:{port}{STATUS_PATH}");
    // A server may take up to a minute and a half to connect and list its tools.
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(100))
        .build()?;
    let res = match client.post(url).bearer_auth(token).send().await {
        Ok(res) => res,
        Err(e) if e.is_connect() => return Ok(None),
        Err(e) => anyhow::bail!("the hub did not answer: {e}"),
    };
    match res.status().as_u16() {
        200 => Ok(Some(res.json().await?)),
        404 => Ok(None),
        401 => anyhow::bail!("the hub did not take the token in the keyring"),
        status => anyhow::bail!("the hub answered {status}"),
    }
}

/// What came of asking the running hub to take the token in the keyring.
#[derive(Debug, PartialEq, Eq)]
pub enum Reloaded {
    Yes,
    /// Nothing listens on the port.
    NotRunning,
    /// The hub there is a version that cannot do it: it needs a restart.
    TooOld,
}

fn local_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("HTTP client")
}

/// Ask the hub on `port`, which still takes `old`, to take the token in the keyring instead.
pub async fn reload_token(port: u16, old: &str) -> anyhow::Result<Reloaded> {
    let url = format!("http://127.0.0.1:{port}{RELOAD_PATH}");
    let sent = local_client().post(url).bearer_auth(old).send().await;
    let res = match sent {
        Ok(res) => res,
        Err(e) if e.is_connect() => return Ok(Reloaded::NotRunning),
        Err(e) => anyhow::bail!("the hub did not answer: {e}"),
    };
    match res.status().as_u16() {
        204 => Ok(Reloaded::Yes),
        404 => Ok(Reloaded::TooOld),
        401 => anyhow::bail!("the hub did not take the old token either"),
        status => {
            let text = res.text().await.unwrap_or_default();
            anyhow::bail!("the hub answered {status}: {}", text.trim())
        }
    }
}

/// What is on a port where a hub should be.
#[derive(Debug, PartialEq, Eq)]
pub enum Probe {
    /// A hub: it turns down a request without the token.
    Hub,
    /// Some other program, which answered with this HTTP status.
    Other(u16),
    /// Something takes connections but does not answer: a hub that waits for its token.
    Silent,
    /// Nothing listens.
    Nothing,
}

pub async fn probe(port: u16) -> Probe {
    let url = format!("http://127.0.0.1:{port}/mcp");
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("HTTP client");
    match client.get(url).send().await {
        Ok(res) if res.status() == StatusCode::UNAUTHORIZED => Probe::Hub,
        Ok(res) => Probe::Other(res.status().as_u16()),
        Err(e) if e.is_connect() => Probe::Nothing,
        Err(_) => Probe::Silent,
    }
}

/// Whether a hub answers on `port`.
pub async fn answers(port: u16) -> bool {
    probe(port).await == Probe::Hub
}

/// Hardcoded loopback: the bind address is deliberately not configurable.
pub async fn bind(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port)).await
}
