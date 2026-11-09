//! `~/.config/mcp-hub/config.json`: the hub's port and its upstream servers.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use url::Url;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub login_port: u16,
    /// In config-file order.
    pub servers: Vec<ServerConfig>,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub name: String,
    pub url: Url,
    /// Shown to agents in the hub's MCP instructions so they know what each server is for.
    pub description: Option<String>,
    pub auth: Auth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    None,
    /// OAuth: dynamic client registration, or a fixed `client_id` (Home Assistant).
    OAuth { client_id: Option<String>, scope: Option<String> },
    /// A static token kept in the keyring, set with `mcp-hub set-token`.
    Bearer,
}

impl Config {
    pub fn server(&self, name: &str) -> Result<&ServerConfig> {
        self.servers
            .iter()
            .find(|s| s.name == name)
            .with_context(|| format!("no server named \"{name}\" in the config"))
    }
}

pub fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("MCP_HUB_CONFIG") {
        return PathBuf::from(path);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".config").join("mcp-hub").join("config.json")
}

pub fn load_config() -> Result<Config> {
    let path = config_path();
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    parse_config(&text).with_context(|| format!("invalid config {}", path.display()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawConfig {
    port: Option<u16>,
    login_port: Option<u16>,
    #[serde(default)]
    servers: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    url: String,
    description: Option<String>,
    oauth: Option<RawOAuth>,
    bearer: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawOAuth {
    client_id: Option<String>,
    scope: Option<String>,
}

fn check_port(port: u16, what: &str) -> Result<u16> {
    if port < 1024 {
        bail!("{what}: expected a port number between 1024 and 65535");
    }
    Ok(port)
}

/// No underscores, so "<server>__<tool>" always splits unambiguously.
fn valid_server_name(name: &str) -> bool {
    (1..=24).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

pub fn parse_config(text: &str) -> Result<Config> {
    let raw: RawConfig = serde_json::from_str(text)?;
    let port = check_port(raw.port.unwrap_or(3050), "port")?;
    let login_port = check_port(raw.login_port.unwrap_or(3051), "loginPort")?;
    if port == login_port {
        bail!("port and loginPort must differ");
    }

    let mut servers = Vec::new();
    for (name, value) in raw.servers {
        let at = format!("servers.{name}");
        if !valid_server_name(&name) {
            bail!("{at}: name must be 1-24 characters of a-z, 0-9 and -");
        }
        let s: RawServer = serde_json::from_value(value).with_context(|| at.clone())?;
        let url = Url::parse(&s.url).with_context(|| format!("{at}.url"))?;
        if !(url.scheme() == "https" || (url.scheme() == "http" && is_loopback(&url))) {
            bail!("{at}: url must be https, or http on a loopback address");
        }
        if let Some(d) = &s.description
            && d.chars().count() > 300
        {
            bail!("{at}.description: expected a string of at most 300 characters");
        }
        let auth = match (s.oauth, s.bearer) {
            (Some(_), Some(true)) => bail!("{at}: use either oauth or bearer, not both"),
            (_, Some(false)) => bail!("{at}.bearer: expected true"),
            (Some(o), None) => Auth::OAuth { client_id: o.client_id, scope: o.scope },
            (None, Some(true)) => Auth::Bearer,
            (None, None) => Auth::None,
        };
        servers.push(ServerConfig { name, url, description: s.description, auth });
    }
    Ok(Config { port, login_port, servers })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_order() {
        let c = parse_config(
            r#"{"servers":{"b":{"url":"https://b.example/mcp"},"a":{"url":"http://127.0.0.1:1/mcp","oauth":{}}}}"#,
        )
        .unwrap();
        assert_eq!((c.port, c.login_port), (3050, 3051));
        assert_eq!(c.servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(c.servers[1].auth, Auth::OAuth { client_id: None, scope: None });
    }

    #[test]
    fn empty_config() {
        assert!(parse_config(r#"{"servers":{}}"#).unwrap().servers.is_empty());
    }

    #[test]
    fn rejects_bad_input() {
        for (json, why) in [
            (r#"{"servers":{"a_b":{"url":"https://x/"}}}"#, "underscore in name"),
            (r#"{"servers":{"a":{"url":"http://example.com/"}}}"#, "plain http off loopback"),
            (r#"{"servers":{"a":{"url":"https://x/","oauth":{},"bearer":true}}}"#, "oauth and bearer"),
            (r#"{"servers":{"a":{"url":"https://x/","bearer":false}}}"#, "bearer false"),
            (r#"{"servers":{"a":{"url":"https://x/","token":"x"}}}"#, "unknown server key"),
            (r#"{"servers":{},"host":"0.0.0.0"}"#, "unknown top-level key"),
            (r#"{"port":3050,"loginPort":3050}"#, "same ports"),
            (r#"{"port":80}"#, "privileged port"),
        ] {
            assert!(parse_config(json).is_err(), "accepted: {why}");
        }
    }
}
