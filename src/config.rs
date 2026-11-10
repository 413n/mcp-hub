//! `~/.config/mcp-hub/config.json`: the hub's port and its upstream servers.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

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
    pub endpoint: Endpoint,
    /// Shown to agents in the hub's MCP instructions so they know what each server is for.
    pub description: Option<String>,
    /// Always `Auth::None` for a command server: its secrets are in `StdioCommand::secret_env`.
    pub auth: Auth,
    /// Disconnect (for a command server: stop the process) after this long without a tool
    /// call. `None` keeps it connected.
    pub idle_timeout: Option<Duration>,
    /// An environment variable: agents started with it set do not get this server. For a
    /// server that an application also gives its own agents directly.
    pub skip_when_env: Option<String>,
    /// Descriptions of this server's tools and their parameters are cut to about this many
    /// characters. `None` leaves them as they are.
    pub max_description: Option<usize>,
}

/// Unless `maxDescription` says otherwise. Few descriptions are longer; one that is can
/// cost an agent thousands of tokens each time it loads the tool.
const DEFAULT_MAX_DESCRIPTION: usize = 2000;

/// A command server nobody has called for this long is stopped, unless `idleTimeout` says otherwise.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// Streamable HTTP.
    Http(Url),
    /// A local process the hub starts and talks to over stdin/stdout.
    Stdio(StdioCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdioCommand {
    /// An absolute path, or a program name looked up in the hub's `PATH`.
    pub command: String,
    pub args: Vec<String>,
    /// Added to the hub's own environment.
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    /// Environment variables whose values live in the keyring (`mcp-hub set-token <server> <VAR>`).
    pub secret_env: Vec<String>,
    /// Stop the server when it and the processes it started use more memory than this.
    pub max_memory_mb: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    None,
    /// OAuth: dynamic client registration, or a fixed `client_id` (Home Assistant).
    OAuth {
        client_id: Option<String>,
        scope: Option<String>,
    },
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
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".config").join("mcp-hub").join("config.json")
}

pub fn load_config() -> Result<Config> {
    let path = config_path();
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    parse_config(&text).with_context(|| format!("invalid config {}", path.display()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawConfig {
    port: Option<u16>,
    login_port: Option<u16>,
    /// Characters; 0 for no limit. Each server can set its own.
    max_description: Option<usize>,
    #[serde(default)]
    servers: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawServer {
    /// Accepted so entries copied from an agent's config work; must match url or command.
    #[serde(rename = "type")]
    kind: Option<String>,
    url: Option<String>,
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    cwd: Option<String>,
    secret_env: Option<Vec<String>>,
    max_memory_mb: Option<u64>,
    /// Seconds; 0 for never.
    idle_timeout: Option<u64>,
    skip_when_env: Option<String>,
    max_description: Option<usize>,
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

/// No underscores, so "<server>__<tool>" always splits unambiguously. "hub" is the hub's
/// own: see [`crate::hub::DESCRIBE`].
fn valid_server_name(name: &str) -> bool {
    name != "hub"
        && (1..=24).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Conventional names only, so each `secretEnv` maps to its own lowercase keyring key.
fn valid_secret_env_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn parse_stdio(at: &str, s: RawServer, command: String) -> Result<StdioCommand> {
    if s.oauth.is_some() || s.bearer.is_some() {
        bail!(
            "{at}: oauth and bearer only apply to url servers; give a command server its secrets with secretEnv"
        );
    }
    if command.is_empty() || (command.contains('/') && !command.starts_with('/')) {
        bail!("{at}.command: expected an absolute path, or a program name looked up in PATH");
    }
    let cwd = match s.cwd {
        Some(dir) if !dir.starts_with('/') => bail!("{at}.cwd: expected an absolute path"),
        dir => dir.map(PathBuf::from),
    };
    let env = s.env.unwrap_or_default();
    if let Some(name) = env.keys().find(|k| k.is_empty() || k.contains(['=', '\0'])) {
        bail!("{at}.env: invalid variable name \"{name}\"");
    }
    let secret_env = s.secret_env.unwrap_or_default();
    for (i, name) in secret_env.iter().enumerate() {
        if !valid_secret_env_name(name) {
            bail!(
                "{at}.secretEnv: \"{name}\" must be 1-64 characters of A-Z, a-z, 0-9 and _, not starting with a digit"
            );
        }
        if secret_env[..i].iter().any(|n| n.eq_ignore_ascii_case(name)) {
            bail!("{at}.secretEnv: \"{name}\" is listed twice");
        }
        if env.contains_key(name) {
            bail!("{at}: \"{name}\" is in both env and secretEnv");
        }
    }
    if s.max_memory_mb == Some(0) {
        bail!(
            "{at}.maxMemoryMb: expected a number of megabytes above 0 (leave it out for no limit)"
        );
    }
    Ok(StdioCommand {
        command,
        args: s.args.unwrap_or_default(),
        env,
        cwd,
        secret_env,
        max_memory_mb: s.max_memory_mb,
    })
}

pub fn parse_config(text: &str) -> Result<Config> {
    let raw: RawConfig = serde_json::from_str(text)?;
    let port = check_port(raw.port.unwrap_or(3050), "port")?;
    let login_port = check_port(raw.login_port.unwrap_or(3051), "loginPort")?;
    if port == login_port {
        bail!("port and loginPort must differ");
    }

    // Shorter than this and little of a description would be left to go by.
    let max_description = |at: &str, max: Option<usize>| match max {
        Some(1..200) => bail!("{at}maxDescription: expected 0 (no limit) or at least 200"),
        max => Ok(max),
    };
    let default_max = max_description("", raw.max_description)?;

    let mut servers = Vec::new();
    for (name, value) in raw.servers {
        let at = format!("servers.{name}");
        if !valid_server_name(&name) {
            bail!("{at}: name must be 1-24 characters of a-z, 0-9 and -, and not \"hub\"");
        }
        let mut s: RawServer = serde_json::from_value(value).with_context(|| at.clone())?;
        if let Some(d) = &s.description
            && d.chars().count() > 300
        {
            bail!("{at}.description: expected a string of at most 300 characters");
        }
        let description = s.description.take();
        let max = max_description(&format!("{at}."), s.max_description.take())?;
        let max_description = Some(max.or(default_max).unwrap_or(DEFAULT_MAX_DESCRIPTION));
        let max_description = max_description.filter(|max| *max > 0);
        let skip_when_env = s.skip_when_env.take();
        if let Some(var) = &skip_when_env
            && !valid_secret_env_name(var)
        {
            bail!("{at}.skipWhenEnv: expected the name of an environment variable");
        }
        // A process costs memory while it waits; a session with an HTTP server costs nothing.
        let idle_timeout = match s.idle_timeout.take() {
            Some(0) => None,
            Some(secs) => Some(Duration::from_secs(secs)),
            None if s.command.is_some() => Some(DEFAULT_IDLE_TIMEOUT),
            None => None,
        };
        match (s.kind.as_deref(), &s.url) {
            (None | Some("stdio"), None) | (None | Some("http"), Some(_)) => {}
            (Some(kind), _) => {
                bail!(
                    "{at}.type: \"{kind}\" does not fit; use \"http\" with url, \"stdio\" with command, or leave it out"
                )
            }
        }
        let (endpoint, auth) = match (s.url.take(), s.command.take()) {
            (Some(_), Some(_)) => bail!("{at}: use either url or command, not both"),
            (None, None) => {
                bail!("{at}: expected url (an HTTP server) or command (a local stdio server)")
            }
            (None, Some(command)) => (Endpoint::Stdio(parse_stdio(&at, s, command)?), Auth::None),
            (Some(url), None) => {
                if s.args.is_some()
                    || s.env.is_some()
                    || s.cwd.is_some()
                    || s.secret_env.is_some()
                    || s.max_memory_mb.is_some()
                {
                    bail!(
                        "{at}: args, env, cwd, secretEnv and maxMemoryMb only apply to command servers"
                    );
                }
                let url = Url::parse(&url).with_context(|| format!("{at}.url"))?;
                if !(url.scheme() == "https" || (url.scheme() == "http" && is_loopback(&url))) {
                    bail!("{at}: url must be https, or http on a loopback address");
                }
                let auth = match (s.oauth, s.bearer) {
                    (Some(_), Some(true)) => bail!("{at}: use either oauth or bearer, not both"),
                    (_, Some(false)) => bail!("{at}.bearer: expected true"),
                    (Some(o), None) => Auth::OAuth {
                        client_id: o.client_id,
                        scope: o.scope,
                    },
                    (None, Some(true)) => Auth::Bearer,
                    (None, None) => Auth::None,
                };
                (Endpoint::Http(url), auth)
            }
        };
        servers.push(ServerConfig {
            name,
            endpoint,
            description,
            auth,
            idle_timeout,
            skip_when_env,
            max_description,
        });
    }
    Ok(Config {
        port,
        login_port,
        servers,
    })
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
        assert_eq!(
            c.servers
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
        assert_eq!(
            c.servers[1].auth,
            Auth::OAuth {
                client_id: None,
                scope: None
            }
        );
    }

    #[test]
    fn stdio_server() {
        let c = parse_config(
            r#"{"servers":{"p":{
                "command":"/usr/bin/node","args":["--import","x.mjs","main.ts"],
                "cwd":"/home/me/p","env":{"MODE":"dev"},"secretEnv":["API_TOKEN"],"description":"d",
                "maxMemoryMb":512
            },"q":{"type":"stdio","command":"uvx"},"r":{"type":"http","url":"https://r/"}}}"#,
        )
        .unwrap();
        let p = &c.servers[0];
        assert_eq!(p.auth, Auth::None);
        assert_eq!(p.idle_timeout, Some(DEFAULT_IDLE_TIMEOUT));
        assert_eq!(
            p.endpoint,
            Endpoint::Stdio(StdioCommand {
                command: "/usr/bin/node".into(),
                args: vec!["--import".into(), "x.mjs".into(), "main.ts".into()],
                env: [("MODE".to_owned(), "dev".to_owned())].into(),
                cwd: Some("/home/me/p".into()),
                secret_env: vec!["API_TOKEN".into()],
                max_memory_mb: Some(512),
            })
        );
        assert!(
            matches!(&c.servers[1].endpoint, Endpoint::Stdio(s) if s.args.is_empty() && s.cwd.is_none())
        );
    }

    #[test]
    fn idle_timeout() {
        let c = parse_config(
            r#"{"servers":{
                "a":{"command":"x"},"b":{"command":"x","idleTimeout":30},"c":{"command":"x","idleTimeout":0},
                "d":{"url":"https://d/"},"e":{"url":"https://e/","idleTimeout":120}
            }}"#,
        )
        .unwrap();
        let secs = |n| Some(Duration::from_secs(n));
        assert_eq!(
            c.servers.iter().map(|s| s.idle_timeout).collect::<Vec<_>>(),
            [secs(600), secs(30), None, None, secs(120)]
        );
    }

    #[test]
    fn max_description() {
        let c = parse_config(
            r#"{"maxDescription":500,"servers":{
                "a":{"url":"https://a/"},"b":{"url":"https://b/","maxDescription":0},
                "c":{"url":"https://c/","maxDescription":9000}
            }}"#,
        )
        .unwrap();
        let max: Vec<_> = c.servers.iter().map(|s| s.max_description).collect();
        assert_eq!(max, [Some(500), None, Some(9000)]);
        let c = parse_config(r#"{"servers":{"a":{"url":"https://a/"}}}"#).unwrap();
        assert_eq!(c.servers[0].max_description, Some(DEFAULT_MAX_DESCRIPTION));
    }

    #[test]
    fn empty_config() {
        assert!(
            parse_config(r#"{"servers":{}}"#)
                .unwrap()
                .servers
                .is_empty()
        );
    }

    #[test]
    fn rejects_bad_input() {
        for (json, why) in [
            (
                r#"{"servers":{"a_b":{"url":"https://x/"}}}"#,
                "underscore in name",
            ),
            (
                r#"{"servers":{"a":{"url":"http://example.com/"}}}"#,
                "plain http off loopback",
            ),
            (
                r#"{"servers":{"a":{"url":"https://x/","oauth":{},"bearer":true}}}"#,
                "oauth and bearer",
            ),
            (
                r#"{"servers":{"a":{"url":"https://x/","bearer":false}}}"#,
                "bearer false",
            ),
            (
                r#"{"servers":{"a":{"url":"https://x/","token":"x"}}}"#,
                "unknown server key",
            ),
            (
                r#"{"servers":{},"host":"0.0.0.0"}"#,
                "unknown top-level key",
            ),
            (r#"{"servers":{"a":{}}}"#, "neither url nor command"),
            (
                r#"{"servers":{"a":{"url":"https://x/","command":"x"}}}"#,
                "url and command",
            ),
            (
                r#"{"servers":{"a":{"url":"https://x/","args":[]}}}"#,
                "args on a url server",
            ),
            (
                r#"{"servers":{"a":{"url":"https://x/","secretEnv":["T"]}}}"#,
                "secretEnv on a url server",
            ),
            (
                r#"{"servers":{"a":{"command":"x","oauth":{}}}}"#,
                "oauth on a command server",
            ),
            (
                r#"{"servers":{"a":{"command":"x","bearer":true}}}"#,
                "bearer on a command server",
            ),
            (r#"{"servers":{"a":{"command":""}}}"#, "empty command"),
            (
                r#"{"servers":{"a":{"command":"bin/x"}}}"#,
                "relative command path",
            ),
            (
                r#"{"servers":{"a":{"command":"x","cwd":"rel"}}}"#,
                "relative cwd",
            ),
            (
                r#"{"servers":{"a":{"command":"x","env":{"A=B":"c"}}}}"#,
                "= in env name",
            ),
            (
                r#"{"servers":{"a":{"command":"x","secretEnv":["1A"]}}}"#,
                "secretEnv name starts with a digit",
            ),
            (
                r#"{"servers":{"a":{"command":"x","secretEnv":["a-b"]}}}"#,
                "dash in secretEnv name",
            ),
            (
                r#"{"servers":{"a":{"command":"x","secretEnv":["T","t"]}}}"#,
                "duplicate secretEnv",
            ),
            (
                r#"{"servers":{"a":{"command":"x","env":{"T":""},"secretEnv":["T"]}}}"#,
                "env and secretEnv overlap",
            ),
            (
                r#"{"servers":{"a":{"type":"http","command":"x"}}}"#,
                "type http with command",
            ),
            (
                r#"{"servers":{"a":{"type":"sse","url":"https://x/"}}}"#,
                "type sse",
            ),
            (
                r#"{"servers":{"a":{"url":"https://x/","maxMemoryMb":100}}}"#,
                "maxMemoryMb on a url server",
            ),
            (
                r#"{"servers":{"a":{"command":"x","maxMemoryMb":0}}}"#,
                "maxMemoryMb 0",
            ),
            (
                r#"{"servers":{"a":{"command":"x","idleTimeout":"10m"}}}"#,
                "idleTimeout not a number of seconds",
            ),
            (
                r#"{"servers":{"a":{"url":"https://x/","skipWhenEnv":"A B"}}}"#,
                "skipWhenEnv not a variable name",
            ),
            (
                r#"{"servers":{"a":{"url":"https://x/","maxDescription":50}}}"#,
                "maxDescription too short to be of use",
            ),
            (
                r#"{"maxDescription":-1,"servers":{}}"#,
                "negative maxDescription",
            ),
            (
                r#"{"servers":{"hub":{"url":"https://x/"}}}"#,
                "the hub's own name",
            ),
            (r#"{"port":3050,"loginPort":3050}"#, "same ports"),
            (r#"{"port":80}"#, "privileged port"),
        ] {
            assert!(parse_config(json).is_err(), "accepted: {why}");
        }
    }
}
