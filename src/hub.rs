//! Merges the tools of several upstreams under "<server>__<tool>" names.

use std::sync::Arc;

use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
};

use crate::upstream::{Upstream, UpstreamError};

pub const SEP: &str = "__";

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
        let mut text = format!(
            "Tools from several MCP servers, named \"<server>{SEP}<tool>\". \
             Look for a server's tools when the task matches it:"
        );
        for u in self.upstreams.iter() {
            text.push_str(&format!("\n- {}", u.name()));
            if let Some(d) = &u.server.description {
                text.push_str(&format!(": {d}"));
            }
        }
        text
    }

    /// Every upstream's tools, from each one's cached list: see [`Upstream::tools`].
    pub async fn list_tools(&self) -> Vec<Tool> {
        let lists = futures::future::join_all(self.upstreams.iter().map(|u| u.tools())).await;
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

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.list_tools().await))
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
