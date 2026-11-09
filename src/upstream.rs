//! One upstream MCP server, connected lazily and reconnected after failures.
//!
//! An HTTP upstream is reached over streamable HTTP. A command upstream is a child
//! process speaking MCP over stdin/stdout; "reconnecting" starts a new process.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use rmcp::{
    RoleClient, ServiceError, ServiceExt,
    model::{CallToolRequestParams, CallToolResponse, ClientCapabilities, ClientConfig, Implementation, Tool},
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        auth::{AuthClient, AuthorizationManager},
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStderr;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config::{Auth, Endpoint, ServerConfig, StdioCommand};
use crate::oauth::{KeyringCredentialStore, secret_env_key};
use crate::secrets::SecretStore;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const LIST_TIMEOUT: Duration = Duration::from_secs(60);
const CALL_TIMEOUT: Duration = Duration::from_secs(300);
/// How many of a command server's last stderr lines to keep for a failed connect's error.
const STDERR_TAIL: usize = 20;

type Client = RunningService<RoleClient, ClientConfig>;

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

pub fn client_info() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("mcp-hub", env!("CARGO_PKG_VERSION")),
    )
}

pub struct Upstream {
    pub server: ServerConfig,
    store: SecretStore,
    client: Mutex<Option<Arc<Client>>>,
}

impl Upstream {
    pub fn new(server: ServerConfig, store: SecretStore) -> Self {
        Self { server, store, client: Mutex::new(None) }
    }

    pub fn name(&self) -> &str {
        &self.server.name
    }

    async fn connect(&self) -> Result<Client, UpstreamError> {
        match &self.server.endpoint {
            Endpoint::Http(url) => self.connect_http(url.as_str()).await,
            Endpoint::Stdio(stdio) => self.connect_stdio(stdio).await,
        }
    }

    async fn connect_stdio(&self, stdio: &StdioCommand) -> Result<Client, UpstreamError> {
        let name = &self.server.name;
        let mut cmd = tokio::process::Command::new(&stdio.command);
        cmd.args(&stdio.args).envs(&stdio.env);
        if let Some(dir) = &stdio.cwd {
            // Checked here, as a missing cwd would otherwise be reported as a missing command.
            if !dir.is_dir() {
                return Err(UpstreamError::Other(format!("cwd {} is not a directory", dir.display())));
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
                let hint = if e.kind() == std::io::ErrorKind::NotFound && !stdio.command.contains('/') {
                    " (not in PATH: use an absolute path, or set PATH in the server's env)"
                } else {
                    ""
                };
                UpstreamError::Other(format!("cannot start {}: {e}{hint}", stdio.command))
            })?;
        let stderr = stderr.map(|s| tokio::spawn(log_stderr(name.clone(), s)));
        match client_info().serve(transport).await {
            Ok(client) => Ok(client),
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
            Auth::None => client_info().serve(StreamableHttpClientTransport::from_config(config)).await,
            Auth::Bearer => {
                let token = self.store.get(&format!("{name}:bearer")).await.map_err(|e| other(&e))?;
                let Some(token) = token else {
                    return Err(UpstreamError::NeedsLogin(format!(
                        "no token stored, run \"mcp-hub set-token {name}\""
                    )));
                };
                let transport = StreamableHttpClientTransport::from_config(config.auth_header(token));
                client_info().serve(transport).await
            }
            Auth::OAuth { .. } => {
                // Read the keyring fresh on every connect: `mcp-hub login` may have run meanwhile.
                let mut manager = AuthorizationManager::new(url).await.map_err(|e| other(&e))?;
                manager.set_credential_store(KeyringCredentialStore::new(self.store.clone(), name));
                if !manager.initialize_from_store().await.map_err(|e| other(&e))? {
                    return Err(UpstreamError::NeedsLogin(format!(
                        "not logged in, run \"mcp-hub login {name}\""
                    )));
                }
                let http = AuthClient::new(reqwest::Client::default(), manager);
                client_info().serve(StreamableHttpClientTransport::with_client(http, config)).await
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

    async fn client(&self) -> Result<Arc<Client>, UpstreamError> {
        let mut slot = self.client.lock().await;
        if let Some(client) = slot.as_ref()
            && !client.is_closed()
        {
            return Ok(client.clone());
        }
        let client = tokio::time::timeout(CONNECT_TIMEOUT, self.connect())
            .await
            .map_err(|_| UpstreamError::Other("connection timed out".into()))??;
        let client = Arc::new(client);
        *slot = Some(client.clone());
        Ok(client)
    }

    /// Drop the connection so the next request reconnects.
    async fn reset(&self) {
        self.client.lock().await.take();
    }

    fn convert(&self, err: ServiceError) -> UpstreamError {
        match err {
            ServiceError::McpError(e) => UpstreamError::Mcp(e),
            e => UpstreamError::Other(e.to_string()),
        }
    }

    pub async fn list_tools(&self) -> Result<Vec<Tool>, UpstreamError> {
        // Listing is idempotent, so retry once on a fresh connection.
        for attempt in 0..2 {
            let client = self.client().await?;
            let result = tokio::time::timeout(LIST_TIMEOUT, client.list_all_tools()).await;
            match result {
                Ok(Ok(tools)) => return Ok(tools),
                Ok(Err(ServiceError::McpError(e))) => return Err(UpstreamError::Mcp(e)),
                Ok(Err(e)) => {
                    self.reset().await;
                    if attempt == 1 {
                        return Err(self.convert(e));
                    }
                }
                Err(_) => {
                    self.reset().await;
                    return Err(UpstreamError::Other("tools/list timed out".into()));
                }
            }
        }
        unreachable!()
    }

    /// Forward a tools/call. Results pass through unchanged; never retried, as it may have side effects.
    pub async fn call_tool(
        &self,
        params: CallToolRequestParams,
        cancel: CancellationToken,
    ) -> Result<CallToolResponse, UpstreamError> {
        let client = self.client().await?;
        let call = tokio::time::timeout(CALL_TIMEOUT, client.peer().call_tool_once(params));
        tokio::select! {
            result = call => match result {
                Ok(Ok(response)) => Ok(response),
                Ok(Err(e)) => {
                    if !matches!(e, ServiceError::McpError(_)) {
                        self.reset().await;
                    }
                    Err(self.convert(e))
                }
                Err(_) => Err(UpstreamError::Other("tools/call timed out".into())),
            },
            _ = cancel.cancelled() => Err(UpstreamError::Other("cancelled by the client".into())),
        }
    }

    pub async fn close(&self) {
        if let Some(client) = self.client.lock().await.take()
            && let Ok(client) = Arc::try_unwrap(client)
        {
            let _ = client.cancel().await;
        }
    }
}

/// The line that most likely says why a process died: the last one mentioning an error
/// (Node prints its stack after the message, Python before), else the last three.
fn stderr_summary(tail: VecDeque<String>) -> String {
    if let Some(line) = tail.iter().rev().find(|l| l.to_ascii_lowercase().contains("error")) {
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
