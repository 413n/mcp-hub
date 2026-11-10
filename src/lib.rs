//! mcp-hub: a localhost MCP proxy. One URL for every agent, fanning out to
//! several upstream MCP servers, with credentials in the OS keyring.

pub mod config;
pub mod connect;
pub mod http;
pub mod hub;
pub mod login;
pub mod oauth;
pub mod secrets;
pub mod stats;
pub mod update;
pub mod upstream;

/// Keyring key of the bearer token agents use to reach the hub.
pub const TOKEN_KEY: &str = "hub:inbound-token";
