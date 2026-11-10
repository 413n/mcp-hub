//! One upstream MCP server, connected lazily and reconnected after failures.
//!
//! An HTTP upstream is reached over streamable HTTP. A command upstream is a child
//! process speaking MCP over stdin/stdout; "reconnecting" starts a new process.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
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
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStderr;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

use crate::config::{Auth, Endpoint, ServerConfig, StdioCommand};
use crate::hub::hub_tools;
use crate::oauth::{KeyringCredentialStore, secret_env_key};
use crate::secrets::SecretStore;
use crate::stats::Stats;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const LIST_TIMEOUT: Duration = Duration::from_secs(60);
/// How many of a command server's last stderr lines to keep for a failed connect's error.
const STDERR_TAIL: usize = 20;
/// The longest an upstream that keeps failing is left alone between attempts.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(300);
/// How long [`Upstream::close`] waits for a connection to close before what is left of it is
/// killed. rmcp's own way can take several seconds: a command server gets 3 to exit once its
/// stdin is closed and 3 more after SIGTERM, an HTTP session 5 to be deleted.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

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
    since: Instant,
}

impl Conn {
    /// Close the connection. A command server is asked to exit (its stdin is closed, then
    /// SIGTERM, then SIGKILL, a few seconds apart), and then whatever is left of its process
    /// group is killed: a server that exits at once may leave behind what it started.
    async fn close(self) {
        self.close_within(None).await
    }

    /// [`Conn::close`], but kill what is left after `limit`.
    async fn close_within(self, limit: Option<Duration>) {
        let closing = self.service.cancel();
        match limit {
            // A close cut short goes on in the background; the kill below ends it.
            Some(limit) => drop(tokio::time::timeout(limit, closing).await),
            None => drop(closing.await),
        }
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
    /// How often the memory of a command server with `maxMemoryMb` is measured.
    pub memory_check: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            list_ttl: Duration::from_secs(300),
            cold_list_wait: Duration::from_secs(5),
            retry_delay: Duration::from_secs(5),
            call_timeout: Duration::from_secs(300),
            ping_timeout: Duration::from_secs(5),
            memory_check: Duration::from_secs(15),
        }
    }
}

/// An upstream's tools under their hub names, as last fetched.
struct CachedTools {
    tools: Arc<Vec<Tool>>,
    /// The descriptions the hub cut, in full: see [`crate::hub::Listed`].
    full: Arc<HashMap<String, Vec<(String, String)>>>,
    fetched: Instant,
    /// Read from the file an earlier run saved it to, and not fetched since.
    saved: bool,
    /// [`Upstream::tools_generation`] when this list was cached.
    generation: u64,
}

/// An upstream's tools as saved on disk, for a restarted hub to list them at once.
#[derive(Serialize, Deserialize)]
struct SavedTools {
    /// What the list depends on: when it changes, the saved list is not used.
    fingerprint: String,
    tools: Vec<Tool>,
    full: HashMap<String, Vec<(String, String)>>,
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
    stats: Stats,
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
    /// Goes up every time the cached list changes.
    generation: AtomicU64,
    /// The file the tool list is saved to, if it is.
    saved_in: std::sync::OnceLock<PathBuf>,
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
        Self::with(server, store, Timings::default(), Stats::off())
    }

    pub fn with_timings(server: ServerConfig, store: SecretStore, timings: Timings) -> Arc<Self> {
        Self::with(server, store, timings, Stats::off())
    }

    /// An upstream that records in `stats` when it connects, stops and fails.
    pub fn with(
        server: ServerConfig,
        store: SecretStore,
        timings: Timings,
        stats: Stats,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            server,
            store,
            timings,
            stats,
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
            generation: AtomicU64::new(0),
            saved_in: std::sync::OnceLock::new(),
        })
    }

    /// Save the tool list in `dir` whenever it changes, and start from the one an earlier run
    /// saved there: a restarted hub then lists every server's tools at once, without waiting
    /// for the slow ones. A saved list is fetched again when it is first asked for (for a
    /// server stopped for being idle, when it next starts).
    pub fn keep_tools_in(&self, dir: &Path) {
        let path = dir.join(format!("{}.json", self.name()));
        let saved = std::fs::read_to_string(&path).ok();
        let saved = saved.and_then(|text| serde_json::from_str::<SavedTools>(&text).ok());
        if let Some(saved) = saved.filter(|saved| saved.fingerprint == self.fingerprint()) {
            let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
            *self.cache.lock().unwrap() = Some(CachedTools {
                tools: Arc::new(saved.tools),
                full: Arc::new(saved.full),
                fetched: Instant::now(),
                saved: true,
                generation,
            });
        }
        let _ = self.saved_in.set(path);
    }

    /// What the tools the hub lists for this server depend on, apart from the server itself.
    fn fingerprint(&self) -> String {
        let s = &self.server;
        let about = format!(
            "{} {:?} {:?} {:?}",
            env!("CARGO_PKG_VERSION"),
            s.endpoint,
            s.auth,
            s.max_description
        );
        let digest = <sha2::Sha256 as sha2::Digest>::digest(about.as_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn save_tools(&self, cached: &CachedTools) {
        let Some(path) = self.saved_in.get() else {
            return;
        };
        let saved = SavedTools {
            fingerprint: self.fingerprint(),
            tools: cached.tools.to_vec(),
            full: (*cached.full).clone(),
        };
        let write = || -> std::io::Result<()> {
            crate::stats::private_dir(path.parent().unwrap_or(Path::new(".")))?;
            let temporary = path.with_extension("json.new");
            std::fs::write(&temporary, serde_json::to_vec(&saved)?)?;
            std::fs::rename(&temporary, path)
        };
        if let Err(e) = write() {
            tracing::debug!("[{}] cannot save the tool list: {e}", self.name());
        }
    }

    /// Goes up every time the tools this server has change, also when they first arrive.
    pub fn tools_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The server's tools are not known yet, and no attempt to fetch them has ended.
    pub fn pending(&self) -> bool {
        self.cache.lock().unwrap().is_none() && *self.refreshed.borrow() == 0
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

    fn connect_timed_out(&self) -> UpstreamError {
        let secs = CONNECT_TIMEOUT.as_secs();
        UpstreamError::Other(match &self.server.endpoint {
            Endpoint::Http(_) => format!("connection timed out after {secs}s"),
            // On macOS a server started by an updated mcp-hub may wait for the user to allow
            // it something (files in Documents, say): macOS asks on behalf of the hub.
            Endpoint::Stdio(_) if cfg!(target_os = "macos") => format!(
                "started, but did not answer within {secs}s (is macOS showing a dialog asking \
                 to allow mcp-hub access to something?)"
            ),
            Endpoint::Stdio(_) => format!("started, but did not answer within {secs}s"),
        })
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
            self.stopped(&conn, "exited");
            tokio::spawn(conn.close());
        }
        if let Some(e) = self.failing() {
            return Err(e);
        }
        let started = Instant::now();
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, self.connect())
            .await
            .unwrap_or_else(|_| Err(self.connect_timed_out()));
        match connected {
            Ok((service, pid)) => {
                match pid {
                    Some(pid) => tracing::info!("[{}] started, pid {pid}", self.name()),
                    None => tracing::debug!("[{}] connected", self.name()),
                }
                let ms = started.elapsed().as_millis() as u64;
                self.stats.record(
                    "connect",
                    serde_json::json!({"server": self.name(), "pid": pid, "ms": ms}),
                );
                let id = self.connections.fetch_add(1, Ordering::Relaxed);
                let since = Instant::now();
                let conn = slot.insert(Conn {
                    id,
                    service,
                    pid,
                    since,
                });
                *self.last_used.lock().unwrap() = since;
                if let Some(idle) = self.server.idle_timeout {
                    tokio::spawn(close_when_idle(self.me.clone(), id, idle));
                }
                if let Endpoint::Stdio(stdio) = &self.server.endpoint
                    && let (Some(pid), Some(limit)) = (pid, stdio.max_memory_mb)
                {
                    tokio::spawn(stop_when_too_large(self.me.clone(), id, pid, limit));
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
    /// reconnects. It is closed in the background: that can take a few seconds. `why` is
    /// for the record.
    async fn reset(&self, id: u64, why: &str) {
        let mut slot = self.conn.lock().await;
        if slot.as_ref().is_some_and(|conn| conn.id == id)
            && let Some(conn) = slot.take()
        {
            self.stopped(&conn, why);
            tokio::spawn(conn.close());
        }
    }

    /// Record that `conn` is being closed, and why.
    fn stopped(&self, conn: &Conn, why: &str) {
        self.stats.record(
            "stop",
            serde_json::json!({
                "server": self.name(),
                "why": why,
                "upSeconds": conn.since.elapsed().as_secs(),
            }),
        );
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
        let record = |retry: Duration| {
            let message: String = error.to_string().chars().take(160).collect();
            self.stats.record(
                "fail",
                serde_json::json!({
                    "server": self.name(),
                    "error": message,
                    "retrySeconds": retry.as_secs(),
                }),
            );
        };
        // A missing or expired login is fixed by a command, which must work at once.
        if matches!(error, UpstreamError::NeedsLogin(_)) {
            tracing::warn!("[{}] {error}", self.name());
            return record(Duration::ZERO);
        }
        let mut failing = self.failing.lock().unwrap();
        let count = failing.as_ref().map_or(0, |f| f.count) + 1;
        let delay = self.timings.retry_delay * 2u32.saturating_pow(count - 1);
        let delay = delay.min(MAX_RETRY_DELAY);
        tracing::warn!(
            "[{}] {error}; next attempt in {}s at the earliest",
            self.name(),
            delay.as_secs()
        );
        record(delay);
        *failing = Some(Failing {
            count,
            until: Instant::now() + delay,
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
            let started = Instant::now();
            let listing = tokio::time::timeout(LIST_TIMEOUT, lease.peer.list_all_tools()).await;
            let error = match listing {
                Ok(Ok(tools)) => {
                    self.succeeded();
                    let listed = hub_tools(self.name(), &tools, self.server.max_description);
                    let size =
                        |tools: &[Tool]| serde_json::to_vec(tools).map_or(0, |json| json.len());
                    self.stats.record(
                        "tools",
                        serde_json::json!({
                            "server": self.name(),
                            "ms": started.elapsed().as_millis() as u64,
                            "tools": listed.tools.len(),
                            "bytes": size(&listed.tools),
                            "bytesBeforeCuts": (!listed.full.is_empty()).then(|| size(&tools)),
                            "toolsCut": (!listed.full.is_empty()).then_some(listed.full.len()),
                        }),
                    );
                    let mut cached = CachedTools {
                        tools: Arc::new(listed.tools),
                        full: Arc::new(listed.full),
                        fetched: Instant::now(),
                        saved: false,
                        generation: 0,
                    };
                    let mut cache = self.cache.lock().unwrap();
                    let changed = cache
                        .as_ref()
                        .is_none_or(|old| old.tools != cached.tools || old.full != cached.full);
                    cached.generation = if changed {
                        self.save_tools(&cached);
                        self.generation.fetch_add(1, Ordering::AcqRel) + 1
                    } else {
                        self.generation.load(Ordering::Acquire)
                    };
                    *cache = Some(cached);
                    return Ok(tools);
                }
                Ok(Err(ServiceError::McpError(e))) => UpstreamError::Mcp(e),
                Ok(Err(e)) => {
                    self.reset(lease.id, "error").await;
                    // An old connection may just have gone stale (session expired, server
                    // restarted): listing is idempotent, so try once more on a new one.
                    if !lease.fresh && attempt == 0 {
                        continue;
                    }
                    self.convert(e)
                }
                Err(_) => {
                    self.reset(lease.id, "error").await;
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
        self.tools_as_of().await.0
    }

    /// [`Upstream::tools`], and the [`Upstream::tools_generation`] of what it returns.
    pub async fn tools_as_of(&self) -> (Option<Arc<Vec<Tool>>>, u64) {
        let cached = |cache: &Option<CachedTools>| match cache {
            Some(c) => {
                let old = c.saved || c.fetched.elapsed() >= self.timings.list_ttl;
                (Some(c.tools.clone()), c.generation, old)
            }
            None => (None, self.tools_generation(), false),
        };
        let (found, generation, old) = cached(&self.cache.lock().unwrap());
        if let Some(tools) = found {
            // Not if that would start a process that was stopped for being idle: its list is
            // fetched again whenever it starts.
            if old && (self.server.idle_timeout.is_none() || self.connected()) {
                self.refresh();
            }
            return (Some(tools), generation);
        }
        if self.failing().is_some() {
            return (None, generation);
        }
        let mut done = self.refreshed.subscribe();
        self.refresh();
        let _ = tokio::time::timeout(self.timings.cold_list_wait, done.changed()).await;
        let (tools, generation, _) = cached(&self.cache.lock().unwrap());
        (tools, generation)
    }

    /// Whether the cached list has descriptions the hub cut short.
    pub fn cuts_descriptions(&self) -> bool {
        let cache = self.cache.lock().unwrap();
        cache.as_ref().is_some_and(|c| !c.full.is_empty())
    }

    /// What was cut from the tool listed as `hub_name`: each description in full, the
    /// tool's own under "" and a parameter's under its name.
    pub fn full_descriptions(&self, hub_name: &str) -> Option<Vec<(String, String)>> {
        let cache = self.cache.lock().unwrap();
        cache.as_ref()?.full.get(hub_name).cloned()
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
                // A failure is logged where it is recorded.
                if me.list_tools().await.is_err() {
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
                        self.reset(lease.id, "error").await;
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
                me.reset(id, "unresponsive").await;
            }
        });
    }

    /// Disconnect; a command server is asked to exit, then killed if it does not.
    pub async fn close(&self) {
        let conn = self.conn.lock().await.take();
        if let Some(conn) = conn {
            self.stopped(&conn, "shutdown");
            conn.close_within(Some(CLOSE_TIMEOUT)).await;
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
            upstream.stopped(&conn, if closed { "exited" } else { "idle" });
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

/// Stop a command server (connection `id`, process group `pid`) once it uses more than
/// `limit_mb` of memory. Ends when that connection is gone, whoever closed it.
async fn stop_when_too_large(upstream: Weak<Upstream>, id: u64, pid: u32, limit_mb: u64) {
    let mut was_over = false;
    loop {
        let Some(every) = upstream.upgrade().map(|u| u.timings.memory_check) else {
            return;
        };
        tokio::time::sleep(every).await;
        let Some(upstream) = upstream.upgrade() else {
            return;
        };
        let current = (upstream.conn.lock().await.as_ref())
            .is_some_and(|conn| conn.id == id && !conn.service.is_closed());
        if !current {
            return;
        }
        let measured = tokio::time::timeout(Duration::from_secs(10), group_memory_mb(pid)).await;
        let used = match measured {
            Ok(Ok(mb)) => mb,
            Ok(Err(e)) => {
                tracing::warn!(
                    "[{}] maxMemoryMb is not enforced: cannot run ps: {e}",
                    upstream.name()
                );
                return;
            }
            Err(_) => continue,
        };
        // Twice in a row, so that a short peak is let through.
        if used > limit_mb && was_over {
            tracing::warn!(
                "[{}] stopped: it uses {used} MB, more than its maxMemoryMb of {limit_mb}",
                upstream.name()
            );
            upstream.reset(id, "memory").await;
            return;
        }
        was_over = used > limit_mb;
    }
}

/// Resident memory, in MB, of all the processes in process group `pgid`. Asked of `ps`,
/// which answers the same way on Linux and macOS.
async fn group_memory_mb(pgid: u32) -> std::io::Result<u64> {
    let output = tokio::process::Command::new("ps")
        .args(["-A", "-o", "pgid=", "-o", "rss="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;
    let kb: u64 = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let group: u32 = fields.next()?.parse().ok()?;
            let rss: u64 = fields.next()?.parse().ok()?;
            (group == pgid).then_some(rss)
        })
        .sum();
    Ok(kb / 1024)
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
