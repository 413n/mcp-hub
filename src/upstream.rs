//! One upstream MCP server, connected lazily and reconnected after failures.
//!
//! An HTTP upstream is reached over streamable HTTP. A command upstream is a child
//! process speaking MCP over stdin/stdout; "reconnecting" starts a new process.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use std::time::{Duration, Instant};

use rmcp::{
    ClientHandler, Peer, RoleClient, ServiceError, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, ClientCapabilities, ClientConfig, ClientRequest,
        Implementation, Tool,
    },
    service::{NotificationContext, RunningService},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        auth::{AuthClient, AuthorizationManager},
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStderr;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

use crate::config::{Auth, Endpoint, ServerConfig, StdioCommand};
use crate::hub::hub_tools;
use crate::oauth::{KeyringCredentialStore, secret_env_key};
use crate::secrets::SecretStore;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const LIST_TIMEOUT: Duration = Duration::from_secs(60);
/// How many of a command server's last stderr lines to keep for a failed connect's error.
const STDERR_TAIL: usize = 20;
/// The longest an upstream that keeps failing is left alone between attempts.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(300);

/// The hub as an upstream's MCP client: it only listens for the tools changing.
struct Handler {
    upstream: Weak<Upstream>,
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        client_info()
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        if let Some(upstream) = self.upstream.upgrade() {
            tracing::debug!("[{}] tools changed", upstream.name());
            upstream.refresh_again.store(true, Ordering::Release);
            upstream.refresh();
        }
    }
}

type Client = RunningService<RoleClient, Handler>;

/// A live connection to the upstream; for a command server, dropping it kills the process.
struct Conn {
    /// Tells this connection from the ones before and after it.
    id: u64,
    service: Client,
    /// A command server's process. It leads its own process group, which holds all it started.
    pid: Option<u32>,
}

impl Conn {
    /// Close the connection. A command server is asked to exit (its stdin is closed, then
    /// SIGTERM, then SIGKILL, a few seconds apart), and then whatever is left of its process
    /// group is killed: a server that exits at once may leave behind what it started.
    async fn close(self) {
        let _ = self.service.cancel().await;
        if let Some(pid) = self.pid.filter(|pid| *pid > 1) {
            // SAFETY: this only sends a signal. The group is the server's own, never the
            // hub's: the server was started as the leader of a new one.
            unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

/// A connection lent to one request. While it exists the upstream counts as busy, so the
/// connection is not closed for being idle.
struct Lease<'a> {
    upstream: &'a Upstream,
    peer: Peer<RoleClient>,
    id: u64,
    /// The connection was made for this request.
    fresh: bool,
    /// A tool call: the idle time starts again when it ends.
    in_use: bool,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if self.in_use {
            *self.upstream.last_used.lock().unwrap() = Instant::now();
        }
        self.upstream.busy.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    /// Credentials are missing or expired: the user has to run a command.
    #[error("{0}")]
    NeedsLogin(String),
    /// A JSON-RPC error from the upstream, passed through to the agent unchanged.
    #[error("{0}")]
    Mcp(rmcp::ErrorData),
    #[error("{0}")]
    Other(String),
}

/// The HTTP client for an upstream at `url`. There are two, shared by all upstreams.
///
/// Remote (https) servers get one that keeps connections open and speaks HTTP/2: requests to
/// a server then share a connection instead of each doing a TLS handshake. Local (http)
/// servers get one that does not keep connections, as in rmcp's own default: a new loopback
/// connection costs nothing, and reusing one can stall a request for 40 ms (delayed ACK).
fn http_client(url: &str) -> reqwest::Client {
    fn build(keep_connections: bool) -> reqwest::Client {
        // As rmcp's default client: only follow redirects that keep the request as it is
        // (307, 308) and stay on the same origin, so credentials never go elsewhere.
        let redirects = reqwest::redirect::Policy::custom(|attempt| {
            let keeps_request = matches!(attempt.status().as_u16(), 307 | 308);
            let same_origin = attempt
                .previous()
                .first()
                .is_some_and(|first| first.origin() == attempt.url().origin());
            if !keeps_request || !same_origin {
                attempt.stop()
            } else if attempt.previous().len() > 10 {
                attempt.error("too many redirects")
            } else {
                attempt.follow()
            }
        });
        let mut builder = reqwest::Client::builder().redirect(redirects);
        if !keep_connections {
            builder = builder.pool_max_idle_per_host(0);
        }
        builder.build().expect("HTTP client")
    }
    static REMOTE: LazyLock<reqwest::Client> = LazyLock::new(|| build(true));
    static LOCAL: LazyLock<reqwest::Client> = LazyLock::new(|| build(false));
    if url.starts_with("https:") {
        REMOTE.clone()
    } else {
        LOCAL.clone()
    }
}

pub fn client_info() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("mcp-hub", env!("CARGO_PKG_VERSION")),
    )
}

/// Timing knobs. Fixed in production; tests shorten them.
#[derive(Debug, Clone)]
pub struct Timings {
    /// A cached tool list is served as it is for this long, then refreshed in the background.
    pub list_ttl: Duration,
    /// How long a tools/list waits for an upstream whose tools are not cached yet.
    pub cold_list_wait: Duration,
    /// How long an upstream is left alone after it failed once; doubles with every failure in
    /// a row, up to 5 minutes.
    pub retry_delay: Duration,
    /// How long a tool call may take.
    pub call_timeout: Duration,
    /// How long a server has to answer a ping after one of its calls timed out.
    pub ping_timeout: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            list_ttl: Duration::from_secs(300),
            cold_list_wait: Duration::from_secs(5),
            retry_delay: Duration::from_secs(5),
            call_timeout: Duration::from_secs(300),
            ping_timeout: Duration::from_secs(5),
        }
    }
}

/// An upstream's tools under their hub names, as last fetched.
struct CachedTools {
    tools: Arc<Vec<Tool>>,
    fetched: Instant,
}

/// An upstream that failed, and until when it is not tried again.
struct Failing {
    /// Failures in a row.
    count: u32,
    until: Instant,
    error: String,
}

pub struct Upstream {
    pub server: ServerConfig,
    store: SecretStore,
    timings: Timings,
    /// For the background tasks, which must not keep a dropped upstream alive.
    me: Weak<Upstream>,
    conn: Mutex<Option<Conn>>,
    connections: AtomicU64,
    /// Requests in flight. Goes up only under the `conn` lock, where the idle check reads it.
    busy: AtomicUsize,
    /// When the last tool call ended, or the connection was made if that is later.
    last_used: std::sync::Mutex<Instant>,
    cache: std::sync::Mutex<Option<CachedTools>>,
    failing: std::sync::Mutex<Option<Failing>>,
    /// Set while a background refresh of `cache` runs, so that there is only one.
    refreshing: AtomicBool,
    /// Bumped when a background refresh ends, whatever its outcome.
    refreshed: watch::Sender<u64>,
    /// The tools changed while a refresh was running: it has to fetch them once more.
    refresh_again: AtomicBool,
}

/// Marks the background refresh as over, also if it panicked.
struct RefreshDone(Arc<Upstream>);

impl Drop for RefreshDone {
    fn drop(&mut self) {
        self.0.refreshing.store(false, Ordering::Release);
        self.0.refreshed.send_modify(|n| *n = n.wrapping_add(1));
    }
}

impl Upstream {
    pub fn new(server: ServerConfig, store: SecretStore) -> Arc<Self> {
        Self::with_timings(server, store, Timings::default())
    }

    pub fn with_timings(server: ServerConfig, store: SecretStore, timings: Timings) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            server,
            store,
            timings,
            me: me.clone(),
            conn: Mutex::new(None),
            connections: AtomicU64::new(0),
            busy: AtomicUsize::new(0),
            last_used: std::sync::Mutex::new(Instant::now()),
            cache: std::sync::Mutex::new(None),
            failing: std::sync::Mutex::new(None),
            refreshing: AtomicBool::new(false),
            refreshed: watch::channel(0).0,
            refresh_again: AtomicBool::new(false),
        })
    }

    pub fn name(&self) -> &str {
        &self.server.name
    }

    fn handler(&self) -> Handler {
        Handler {
            upstream: self.me.clone(),
        }
    }

    /// Connect; for a command server, start it and say which process it is.
    async fn connect(&self) -> Result<(Client, Option<u32>), UpstreamError> {
        match &self.server.endpoint {
            Endpoint::Http(url) => Ok((self.connect_http(url.as_str()).await?, None)),
            Endpoint::Stdio(stdio) => self.connect_stdio(stdio).await,
        }
    }

    /// "stopped" or "disconnected": what a closed connection means for this kind of server.
    fn gone(&self) -> &'static str {
        match self.server.endpoint {
            Endpoint::Stdio(_) => "stopped",
            Endpoint::Http(_) => "disconnected",
        }
    }

    async fn connect_stdio(
        &self,
        stdio: &StdioCommand,
    ) -> Result<(Client, Option<u32>), UpstreamError> {
        let name = &self.server.name;
        let mut cmd = tokio::process::Command::new(&stdio.command);
        cmd.args(&stdio.args).envs(&stdio.env);
        if let Some(dir) = &stdio.cwd {
            // Checked here, as a missing cwd would otherwise be reported as a missing command.
            if !dir.is_dir() {
                return Err(UpstreamError::Other(format!(
                    "cwd {} is not a directory",
                    dir.display()
                )));
            }
            cmd.current_dir(dir);
        }
        for var in &stdio.secret_env {
            let value = self
                .store
                .get(&secret_env_key(name, var))
                .await
                .map_err(|e| UpstreamError::Other(format!("{e:#}")))?;
            let Some(value) = value else {
                return Err(UpstreamError::NeedsLogin(format!(
                    "{var} is not set, run \"mcp-hub set-token {name} {var}\""
                )));
            };
            cmd.env(var, value);
        }
        // Its own process group, so that killing it also stops anything it started (npx, uvx).
        let mut cmd = process_wrap::tokio::CommandWrap::from(cmd);
        cmd.wrap(process_wrap::tokio::ProcessGroup::leader());
        let (transport, stderr) = TokioChildProcess::builder(cmd)
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                let hint =
                    if e.kind() == std::io::ErrorKind::NotFound && !stdio.command.contains('/') {
                        " (not in PATH: use an absolute path, or set PATH in the server's env)"
                    } else {
                        ""
                    };
                UpstreamError::Other(format!("cannot start {}: {e}{hint}", stdio.command))
            })?;
        let pid = transport.id();
        let stderr = stderr.map(|s| tokio::spawn(log_stderr(name.clone(), s)));
        match self.handler().serve(transport).await {
            Ok(client) => Ok((client, pid)),
            Err(e) => {
                // The failed handshake dropped the transport, which kills the process; what it
                // printed last usually says why it did not start.
                let mut message = e.to_string();
                if let Some(task) = stderr
                    && let Ok(Ok(tail)) = tokio::time::timeout(Duration::from_secs(2), task).await
                    && !tail.is_empty()
                {
                    message = format!("{message}; stderr: {}", stderr_summary(tail));
                }
                Err(UpstreamError::Other(message))
            }
        }
    }

    async fn connect_http(&self, url: &str) -> Result<Client, UpstreamError> {
        let name = &self.server.name;
        let other = |e: &dyn std::fmt::Display| UpstreamError::Other(e.to_string());
        let config = StreamableHttpClientTransportConfig::with_uri(url);
        let serving = match &self.server.auth {
            Auth::None => {
                let transport =
                    StreamableHttpClientTransport::with_client(http_client(url), config);
                self.handler().serve(transport).await
            }
            Auth::Bearer => {
                let token = self
                    .store
                    .get(&format!("{name}:bearer"))
                    .await
                    .map_err(|e| other(&e))?;
                let Some(token) = token else {
                    return Err(UpstreamError::NeedsLogin(format!(
                        "no token stored, run \"mcp-hub set-token {name}\""
                    )));
                };
                let transport = StreamableHttpClientTransport::with_client(
                    http_client(url),
                    config.auth_header(token),
                );
                self.handler().serve(transport).await
            }
            Auth::OAuth { .. } => {
                // Read the keyring fresh on every connect: `mcp-hub login` may have run meanwhile.
                let mut manager = AuthorizationManager::new(url)
                    .await
                    .map_err(|e| other(&e))?;
                manager.set_credential_store(KeyringCredentialStore::new(self.store.clone(), name));
                if !manager
                    .initialize_from_store()
                    .await
                    .map_err(|e| other(&e))?
                {
                    return Err(UpstreamError::NeedsLogin(format!(
                        "not logged in, run \"mcp-hub login {name}\""
                    )));
                }
                let http = AuthClient::new(http_client(url), manager);
                self.handler()
                    .serve(StreamableHttpClientTransport::with_client(http, config))
                    .await
            }
        };
        serving.map_err(|e| {
            if e.is_authorization_required() {
                UpstreamError::NeedsLogin(format!("login expired, run \"mcp-hub login {name}\""))
            } else {
                other(&e)
            }
        })
    }

    /// Borrow the connection for one request, making it now if there is none. `in_use`
    /// says the request is a tool call, the only kind that keeps an idle upstream connected.
    async fn lease(&self, in_use: bool) -> Result<Lease<'_>, UpstreamError> {
        let mut slot = self.conn.lock().await;
        let lease = |conn: &Conn, fresh| {
            self.busy.fetch_add(1, Ordering::AcqRel);
            Lease {
                upstream: self,
                peer: conn.service.peer().clone(),
                id: conn.id,
                fresh,
                in_use,
            }
        };
        if let Some(conn) = slot.as_ref()
            && !conn.service.is_closed()
        {
            return Ok(lease(conn, false));
        }
        if let Some(conn) = slot.take() {
            tracing::info!("[{}] {} by itself", self.name(), self.gone());
            tokio::spawn(conn.close());
        }
        if let Some(e) = self.failing() {
            return Err(e);
        }
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, self.connect())
            .await
            .unwrap_or_else(|_| Err(UpstreamError::Other("connection timed out".into())));
        match connected {
            Ok((service, pid)) => {
                match pid {
                    Some(pid) => tracing::info!("[{}] started, pid {pid}", self.name()),
                    None => tracing::debug!("[{}] connected", self.name()),
                }
                let id = self.connections.fetch_add(1, Ordering::Relaxed);
                let conn = slot.insert(Conn { id, service, pid });
                *self.last_used.lock().unwrap() = Instant::now();
                if let Some(idle) = self.server.idle_timeout {
                    tokio::spawn(close_when_idle(self.me.clone(), id, idle));
                }
                Ok(lease(conn, true))
            }
            Err(e) => {
                self.failed(&e);
                Err(e)
            }
        }
    }

    /// Give up connection `id`, if it is still the current one, so the next request
    /// reconnects. It is closed in the background: that can take a few seconds.
    async fn reset(&self, id: u64) {
        let mut slot = self.conn.lock().await;
        if slot.as_ref().is_some_and(|conn| conn.id == id)
            && let Some(conn) = slot.take()
        {
            tokio::spawn(conn.close());
        }
    }

    /// Whether the upstream is connected right now (a command server: running).
    fn connected(&self) -> bool {
        self.conn
            .try_lock()
            .is_ok_and(|slot| slot.as_ref().is_some_and(|c| !c.service.is_closed()))
    }

    fn convert(&self, err: ServiceError) -> UpstreamError {
        match err {
            ServiceError::McpError(e) => UpstreamError::Mcp(e),
            e => UpstreamError::Other(e.to_string()),
        }
    }

    /// Note a failure: the upstream is not tried again for a while, longer each time in a
    /// row, so that agents asking for it do not turn one failure into a stream of attempts.
    fn failed(&self, error: &UpstreamError) {
        // A missing or expired login is fixed by a command, which must work at once.
        if matches!(error, UpstreamError::NeedsLogin(_)) {
            return;
        }
        let mut failing = self.failing.lock().unwrap();
        let count = failing.as_ref().map_or(0, |f| f.count) + 1;
        let delay = self.timings.retry_delay * 2u32.saturating_pow(count - 1);
        *failing = Some(Failing {
            count,
            until: Instant::now() + delay.min(MAX_RETRY_DELAY),
            error: error.to_string(),
        });
    }

    fn succeeded(&self) {
        self.failing.lock().unwrap().take();
    }

    /// The last failure, while the upstream is being left alone because of it.
    fn failing(&self) -> Option<UpstreamError> {
        let failing = self.failing.lock().unwrap();
        let f = failing.as_ref()?;
        let left = f.until.checked_duration_since(Instant::now())?;
        Some(UpstreamError::Other(format!(
            "{}; next attempt in {}s",
            f.error,
            left.as_secs_f32().ceil()
        )))
    }

    /// Ask the upstream for its tools, under their own names. Also updates the cached list.
    pub async fn list_tools(&self) -> Result<Vec<Tool>, UpstreamError> {
        for attempt in 0..2 {
            let lease = self.lease(false).await?;
            let listing = tokio::time::timeout(LIST_TIMEOUT, lease.peer.list_all_tools()).await;
            let error = match listing {
                Ok(Ok(tools)) => {
                    self.succeeded();
                    *self.cache.lock().unwrap() = Some(CachedTools {
                        tools: Arc::new(hub_tools(self.name(), &tools)),
                        fetched: Instant::now(),
                    });
                    return Ok(tools);
                }
                Ok(Err(ServiceError::McpError(e))) => UpstreamError::Mcp(e),
                Ok(Err(e)) => {
                    self.reset(lease.id).await;
                    // An old connection may just have gone stale (session expired, server
                    // restarted): listing is idempotent, so try once more on a new one.
                    if !lease.fresh && attempt == 0 {
                        continue;
                    }
                    self.convert(e)
                }
                Err(_) => {
                    self.reset(lease.id).await;
                    UpstreamError::Other("tools/list timed out".into())
                }
            };
            self.failed(&error);
            return Err(error);
        }
        unreachable!()
    }

    /// The tools under their hub names, for the hub's own tools/list, from the cache.
    ///
    /// An old list is still returned, and refreshed in the background for the next caller. Only
    /// with nothing cached does this wait for the upstream, and not for long: `None` means it
    /// did not answer in time (its tools appear once it has) or it failed.
    pub async fn tools(&self) -> Option<Arc<Vec<Tool>>> {
        let cached = |cache: &Option<CachedTools>| {
            cache
                .as_ref()
                .map(|c| (c.tools.clone(), c.fetched.elapsed()))
        };
        let found = cached(&self.cache.lock().unwrap());
        if let Some((tools, age)) = found {
            // Not if that would start a process that was stopped for being idle: its list is
            // fetched again whenever it starts.
            if age >= self.timings.list_ttl
                && (self.server.idle_timeout.is_none() || self.connected())
            {
                self.refresh();
            }
            return Some(tools);
        }
        if self.failing().is_some() {
            return None;
        }
        let mut done = self.refreshed.subscribe();
        self.refresh();
        let _ = tokio::time::timeout(self.timings.cold_list_wait, done.changed()).await;
        cached(&self.cache.lock().unwrap()).map(|(tools, _)| tools)
    }

    /// Fetch the tool list in the background, unless that is already happening or the
    /// upstream just failed.
    fn refresh(&self) {
        let Some(me) = self.me.upgrade() else { return };
        if self.failing().is_some() || self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        tokio::spawn(async move {
            let done = RefreshDone(me.clone());
            loop {
                me.refresh_again.store(false, Ordering::Release);
                if let Err(e) = me.list_tools().await {
                    tracing::warn!("[{}] tools/list failed: {e}", me.name());
                    break;
                }
                if !me.refresh_again.load(Ordering::Acquire) {
                    break;
                }
            }
            drop(done);
            // A change that came between the last check and the end.
            if me.refresh_again.load(Ordering::Acquire) {
                me.refresh();
            }
        });
    }

    /// Forward a tools/call. Results pass through unchanged; never retried, as it may have side effects.
    pub async fn call_tool(
        &self,
        params: CallToolRequestParams,
        cancel: CancellationToken,
    ) -> Result<CallToolResponse, UpstreamError> {
        let lease = self.lease(true).await?;
        if lease.fresh {
            // The server just (re)started, maybe as another version: ask for its tools again.
            self.refresh();
        }
        let limit = self.timings.call_timeout;
        let call = tokio::time::timeout(limit, lease.peer.call_tool_once(params));
        tokio::select! {
            result = call => match result {
                Ok(Ok(response)) => {
                    self.succeeded();
                    Ok(response)
                }
                Ok(Err(e)) => {
                    if !matches!(e, ServiceError::McpError(_)) {
                        self.reset(lease.id).await;
                    }
                    Err(self.convert(e))
                }
                Err(_) => {
                    self.check_alive(lease.peer.clone(), lease.id);
                    Err(UpstreamError::Other(format!(
                        "tools/call timed out after {}s",
                        limit.as_secs()
                    )))
                }
            },
            _ = cancel.cancelled() => Err(UpstreamError::Other("cancelled by the client".into())),
        }
    }

    /// After a call on connection `id` timed out. The call was cancelled, but a server that
    /// is stuck does not act on that: if it does not even answer a ping, drop the connection
    /// (for a command server: kill it), so that the next request gets a new one.
    fn check_alive(&self, peer: Peer<RoleClient>, id: u64) {
        let Some(me) = self.me.upgrade() else { return };
        tokio::spawn(async move {
            let ping = peer.send_request(ClientRequest::PingRequest(Default::default()));
            let answer = tokio::time::timeout(me.timings.ping_timeout, ping).await;
            // An error for an answer still shows that it is listening.
            if !matches!(answer, Ok(Ok(_) | Err(ServiceError::McpError(_)))) {
                tracing::warn!("[{}] {}: it no longer answers", me.name(), me.gone());
                me.reset(id).await;
            }
        });
    }

    /// Disconnect; a command server is asked to exit, then killed if it does not.
    pub async fn close(&self) {
        let conn = self.conn.lock().await.take();
        if let Some(conn) = conn {
            conn.close().await;
        }
    }
}

/// Close connection `id` of an upstream once it had no tool call for `idle`. Ends when that
/// connection is gone, whoever closed it.
async fn close_when_idle(upstream: Weak<Upstream>, id: u64, idle: Duration) {
    loop {
        let wait = {
            let Some(upstream) = upstream.upgrade() else {
                return;
            };
            if upstream.busy.load(Ordering::Acquire) > 0 {
                idle
            } else {
                idle.saturating_sub(upstream.last_used.lock().unwrap().elapsed())
            }
        };
        tokio::time::sleep(wait.max(Duration::from_millis(50))).await;
        let Some(upstream) = upstream.upgrade() else {
            return;
        };
        let mut slot = upstream.conn.lock().await;
        if !slot.as_ref().is_some_and(|conn| conn.id == id) {
            return;
        }
        let closed = slot.as_ref().is_some_and(|conn| conn.service.is_closed());
        let unused = upstream.busy.load(Ordering::Acquire) == 0
            && upstream.last_used.lock().unwrap().elapsed() >= idle;
        if closed || unused {
            let Some(conn) = slot.take() else { return };
            drop(slot);
            if closed {
                tracing::info!("[{}] {} by itself", upstream.name(), upstream.gone());
            } else {
                tracing::info!(
                    "[{}] {}: no tool call for {}s",
                    upstream.name(),
                    upstream.gone(),
                    idle.as_secs()
                );
            }
            conn.close().await;
            return;
        }
    }
}

/// The line that most likely says why a process died: the last one mentioning an error
/// (Node prints its stack after the message, Python before), else the last three.
fn stderr_summary(tail: VecDeque<String>) -> String {
    if let Some(line) = tail
        .iter()
        .rev()
        .find(|l| l.to_ascii_lowercase().contains("error"))
    {
        return line.trim().to_owned();
    }
    let skip = tail.len().saturating_sub(3);
    tail.into_iter().skip(skip).collect::<Vec<_>>().join(" | ")
}

/// Log a command server's stderr until it exits, then return its last lines.
async fn log_stderr(name: String, stderr: ChildStderr) -> VecDeque<String> {
    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    let mut tail = VecDeque::with_capacity(STDERR_TAIL);
    // Byte lines, not `lines()`: invalid UTF-8 must not stop the draining, or the child blocks
    // once the pipe is full.
    while let Ok(n) = reader.read_until(b'\n', &mut buf).await
        && n > 0
    {
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end();
        if !line.is_empty() {
            tracing::info!(target: "mcp_hub::stderr", "{name}: {line}");
            if tail.len() == STDERR_TAIL {
                tail.pop_front();
            }
            tail.push_back(line.chars().take(500).collect());
        }
        buf.clear();
    }
    tail
}
