//! Merges the tools of several upstreams under "<server>__<tool>" names.

use std::sync::Arc;

use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, DiscoverResult,
        Implementation, InitializeRequestParams, InitializeResult, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
};

use crate::upstream::{Upstream, UpstreamError};

pub const SEP: &str = "__";

/// Request header naming the servers, separated by commas, that this client is not to get.
/// Sent by `mcp-hub connect` and `mcp-hub headers` for servers with `skipWhenEnv`.
pub const SKIP_HEADER: &str = "x-mcp-hub-skip";

/// What a request says about the client that sent it.
#[derive(Default)]
struct Client {
    /// Names of the servers left out for it.
    skip: Vec<String>,
}

impl Client {
    fn of(context: &RequestContext<RoleServer>) -> Self {
        // rmcp hands over the HTTP request each MCP request came in.
        let Some(http) = context.extensions.get::<hyper::http::request::Parts>() else {
            return Self::default();
        };
        let header = |name| http.headers.get(name).and_then(|value| value.to_str().ok());
        let names = header(SKIP_HEADER).unwrap_or_default().split(',');
        Self {
            skip: names
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
                .collect(),
        }
    }

    fn skips(&self, upstream: &Upstream) -> bool {
        self.skip.iter().any(|name| name == upstream.name())
    }
}

/// What Claude, Codex and Cursor accept as a tool name.
fn usable_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// An upstream's tools under their hub names, without those whose name agents would reject.
pub(crate) fn hub_tools(server: &str, tools: &[Tool]) -> Vec<Tool> {
    let mut renamed = Vec::with_capacity(tools.len());
    for tool in tools {
        let name = format!("{server}{SEP}{}", tool.name);
        if !usable_tool_name(&name) {
            tracing::warn!(
                "[{server}] skipping tool with unusable name: {:?}",
                tool.name
            );
            continue;
        }
        let mut tool = tool.clone();
        tool.name = name.into();
        renamed.push(tool);
    }
    renamed
}

#[derive(Clone)]
pub struct Hub {
    upstreams: Arc<Vec<Arc<Upstream>>>,
}

impl Hub {
    pub fn new(upstreams: Vec<Arc<Upstream>>) -> Self {
        Self {
            upstreams: Arc::new(upstreams),
        }
    }

    pub fn upstreams(&self) -> &[Arc<Upstream>] {
        &self.upstreams
    }

    /// MCP server instructions: what each upstream is for, so clients know when to search its tools.
    pub fn instructions(&self) -> String {
        self.instructions_for(&Client::default())
    }

    fn instructions_for(&self, client: &Client) -> String {
        let mut text = format!(
            "Tools from several MCP servers, named \"<server>{SEP}<tool>\". \
             Look for a server's tools when the task matches it:"
        );
        for u in self.upstreams.iter().filter(|u| !client.skips(u)) {
            text.push_str(&format!("\n- {}", u.name()));
            if let Some(d) = &u.server.description {
                text.push_str(&format!(": {d}"));
            }
        }
        text
    }

    /// Every upstream's tools, from each one's cached list: see [`Upstream::tools`].
    pub async fn list_tools(&self) -> Vec<Tool> {
        self.tools_for(&Client::default()).await
    }

    async fn tools_for(&self, client: &Client) -> Vec<Tool> {
        let upstreams = self.upstreams.iter().filter(|u| !client.skips(u));
        let lists = futures::future::join_all(upstreams.map(|u| u.tools())).await;
        let mut tools = Vec::with_capacity(lists.iter().flatten().map(|l| l.len()).sum());
        for list in lists.into_iter().flatten() {
            tools.extend(list.iter().cloned());
        }
        tools
    }

    fn route(&self, name: &str) -> Option<(&Arc<Upstream>, String)> {
        let (server, tool) = name.split_once(SEP)?;
        let upstream = self.upstreams.iter().find(|u| u.name() == server)?;
        Some((upstream, tool.to_owned()))
    }
}

impl ServerHandler for Hub {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mcp-hub", env!("CARGO_PKG_VERSION")))
            .with_instructions(self.instructions())
    }

    // The two ways a client opens a session; both tell it what the servers are for.
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        context.peer.set_peer_info(request.clone());
        let mut result = self.negotiate_initialize(&request)?;
        result.instructions = Some(self.instructions_for(&Client::of(&context)));
        Ok(result)
    }

    async fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        let mut info = self.get_info();
        info.instructions = Some(self.instructions_for(&Client::of(&context)));
        let versions = self.supported_protocol_versions().into_owned();
        Ok(DiscoverResult::from_server_info(versions, info))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self.tools_for(&Client::of(&context)).await;
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        mut request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let full_name = request.name.to_string();
        let Some((upstream, tool)) = self.route(&full_name) else {
            return Err(ErrorData::invalid_params(
                format!("Unknown tool: {full_name}"),
                None,
            ));
        };
        if Client::of(&context).skips(upstream) {
            let var = upstream.server.skip_when_env.as_deref();
            let why = var
                .map(|var| format!(" ({var} is set)"))
                .unwrap_or_default();
            return Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                ContentBlock::text(format!(
                    "mcp-hub: {} is left out of this session{why}. \
                     Use the tools this session has of its own for it.",
                    upstream.name()
                )),
            ])));
        }
        request.name = tool.into();
        match upstream.call_tool(request, context.ct).await {
            Ok(response) => Ok(response),
            Err(UpstreamError::Mcp(e)) => Err(e),
            Err(e) => {
                tracing::warn!("[{}] tools/call {full_name} failed: {e}", upstream.name());
                Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                    ContentBlock::text(format!("mcp-hub: {e}")),
                ])))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::usable_tool_name;

    #[test]
    fn tool_names() {
        assert!(usable_tool_name("sentry__search_issues"));
        assert!(!usable_tool_name("x__bad.name"));
        assert!(!usable_tool_name(&"a".repeat(65)));
    }
}
