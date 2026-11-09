//! One upstream MCP server, connected lazily and reconnected after failures.

use std::sync::Arc;
use std::time::Duration;

use rmcp::{
    RoleClient, ServiceError, ServiceExt,
    model::{CallToolRequestParams, CallToolResponse, ClientCapabilities, ClientConfig, Implementation, Tool},
    service::RunningService,
    transport::{
        StreamableHttpClientTransport,
        auth::{AuthClient, AuthorizationManager},
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config::{Auth, ServerConfig};
use crate::oauth::KeyringCredentialStore;
use crate::secrets::SecretStore;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const LIST_TIMEOUT: Duration = Duration::from_secs(60);
const CALL_TIMEOUT: Duration = Duration::from_secs(300);

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
        let name = &self.server.name;
        let url = self.server.url.as_str();
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
