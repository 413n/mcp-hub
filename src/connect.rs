//! What an agent needs to reach the running hub without its token in a config file.
//!
//! `mcp-hub connect` speaks MCP on stdin/stdout and forwards every message to the hub over
//! HTTP; `mcp-hub headers` prints the HTTP headers for agents that can ask a command for
//! them. Both read the token from the keyring.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::task::AbortHandle;

use crate::TOKEN_KEY;
use crate::config::Config;
use crate::hub::{CLIENT_HEADER, SKIP_HEADER};
use crate::secrets::SecretStore;

/// Sent on requests after `initialize`, as an HTTP client would.
const PROTOCOL_VERSION: &str = "MCP-Protocol-Version";
/// How long a request waits for a hub that is not listening, as while it restarts.
const HUB_DOWN_TRIES: u32 = 6;

async fn token(store: &SecretStore) -> Result<String> {
    store
        .get(TOKEN_KEY)
        .await?
        .context("no hub token in the keyring yet (run \"mcp-hub token\")")
}

/// The servers to leave out for an agent in this environment: those whose `skipWhenEnv`
/// variable `is_set`.
pub fn skipped(config: &Config, is_set: impl Fn(&str) -> bool) -> Vec<String> {
    let skip = |s: &&crate::config::ServerConfig| s.skip_when_env.as_deref().is_some_and(&is_set);
    let servers = config.servers.iter().filter(skip);
    servers.map(|s| s.name.clone()).collect()
}

/// Whether environment variable `name` is set to something.
pub fn env_is_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

/// The HTTP headers an agent has to send to the hub: the token, and the servers to `skip`.
pub async fn headers(store: &SecretStore, skip: &[String]) -> Result<Vec<(&'static str, String)>> {
    let mut headers = vec![("Authorization", format!("Bearer {}", token(store).await?))];
    if !skip.is_empty() {
        headers.push((SKIP_HEADER, skip.join(",")));
    }
    Ok(headers)
}

struct Bridge<W> {
    url: String,
    http: reqwest::Client,
    store: SecretStore,
    skip: Vec<String>,
    headers: Mutex<Vec<(&'static str, String)>>,
    /// The version the hub and the agent agreed on, once they have.
    protocol: Mutex<Option<String>>,
    /// What the agent called itself when it opened the session, for the hub's record.
    agent: Mutex<Option<String>>,
    output: tokio::sync::Mutex<W>,
    /// The requests being forwarded, by id, so that a cancelled one can be dropped.
    running: Mutex<HashMap<String, AbortHandle>>,
}

/// What to tell the agent when its request could not be forwarded.
fn failure(id: &Value, message: String) -> Vec<u8> {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": message}})
        .to_string()
        .into_bytes()
}

impl<W: AsyncWrite + Unpin + Send + 'static> Bridge<W> {
    async fn write(&self, mut line: Vec<u8>) {
        // One message per line: a body that spans several is written again as one.
        if line.contains(&b'\n') {
            match serde_json::from_slice::<Value>(&line) {
                Ok(value) => line = value.to_string().into_bytes(),
                Err(_) => line.retain(|b| *b != b'\n' && *b != b'\r'),
            }
        }
        line.push(b'\n');
        let mut output = self.output.lock().await;
        let _ = output.write_all(&line).await;
        let _ = output.flush().await;
    }

    /// POST one message to the hub. `Ok(None)` when the hub has nothing to answer.
    async fn post(&self, body: &str) -> Result<Option<Vec<u8>>, String> {
        let mut reloaded = false;
        let mut tries = 0;
        loop {
            let mut request = self
                .http
                .post(&self.url)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream")
                .body(body.to_owned());
            for (name, value) in self.headers.lock().unwrap().iter() {
                request = request.header(*name, value);
            }
            if let Some(version) = self.protocol.lock().unwrap().as_ref() {
                request = request.header(PROTOCOL_VERSION, version);
            }
            if let Some(agent) = self.agent.lock().unwrap().as_ref() {
                request = request.header(CLIENT_HEADER, agent);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(e) if e.is_connect() && tries < HUB_DOWN_TRIES => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                Err(e) if e.is_connect() => {
                    return Err(format!("mcp-hub is not running at {}", self.url));
                }
                Err(e) => return Err(format!("mcp-hub did not answer: {e}")),
            };
            let status = response.status().as_u16();
            // The token was rotated since this started: read the new one, once.
            if status == 401 && !reloaded {
                reloaded = true;
                if let Ok(fresh) = headers(&self.store, &self.skip).await {
                    *self.headers.lock().unwrap() = fresh;
                    continue;
                }
            }
            let event_stream = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("text/event-stream"));
            let body = response
                .bytes()
                .await
                .map_err(|e| format!("mcp-hub did not answer: {e}"))?;
            return match status {
                200 if event_stream => Ok(last_event(&body)),
                200 => Ok(Some(body.to_vec())),
                202 | 204 => Ok(None),
                401 => Err(
                    "mcp-hub refused the token: restart it after \"mcp-hub token --rotate\"".into(),
                ),
                _ => Err(format!(
                    "mcp-hub answered {status}: {}",
                    String::from_utf8_lossy(&body[..body.len().min(300)]).trim()
                )),
            };
        }
    }

    async fn request(&self, id: Value, initialize: bool, body: String) {
        let answer = match self.post(&body).await {
            Ok(Some(answer)) => answer,
            Ok(None) => return,
            Err(message) => failure(&id, message),
        };
        if initialize
            && let Ok(value) = serde_json::from_slice::<Value>(&answer)
            && let Some(version) = value["result"]["protocolVersion"].as_str()
        {
            *self.protocol.lock().unwrap() = Some(version.to_owned());
        }
        self.write(answer).await;
    }
}

/// The data of the last event in a server-sent event stream: the answer to the request.
fn last_event(body: &[u8]) -> Option<Vec<u8>> {
    String::from_utf8_lossy(body)
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("data:"))
        .map(|data| data.trim().as_bytes().to_vec())
        .filter(|data| !data.is_empty())
}

/// Forward MCP messages read from `input`, one JSON object per line, to the hub on `port`
/// and write its answers to `output`, asking the hub to leave out the servers in `skip`.
/// Returns when `input` ends.
pub async fn run<R, W>(
    port: u16,
    store: SecretStore,
    skip: Vec<String>,
    input: R,
    output: W,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let bridge = Arc::new(Bridge {
        url: format!("http://127.0.0.1:{port}/mcp"),
        // No proxy for loopback, whatever the environment says, and no kept connections:
        // a new one costs nothing here, and a kept one can stall on a delayed ACK.
        http: reqwest::Client::builder()
            .no_proxy()
            .pool_max_idle_per_host(0)
            .build()?,
        headers: Mutex::new(headers(&store, &skip).await?),
        store,
        skip,
        protocol: Mutex::new(None),
        agent: Mutex::new(None),
        output: tokio::sync::Mutex::new(output),
        running: Mutex::new(HashMap::new()),
    });
    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            bail!("not a JSON-RPC message on stdin");
        };
        let Some(method) = message["method"].as_str() else {
            continue; // an answer: the hub never asks the agent anything
        };
        let id = message.get("id").filter(|id| !id.is_null()).cloned();
        if method == "notifications/cancelled" {
            // The hub keeps no session, so it cannot be told. Dropping the request closes
            // its connection, which the hub takes as the cancellation.
            let key = message["params"]["requestId"].to_string();
            if let Some(task) = bridge.running.lock().unwrap().remove(&key) {
                task.abort();
            }
            continue;
        }
        let initialize = method == "initialize";
        if initialize {
            let info = &message["params"]["clientInfo"];
            let name = info["name"].as_str().unwrap_or("unknown");
            let agent = format!("{name}/{}", info["version"].as_str().unwrap_or("?"));
            // Header values are printable ASCII.
            let agent = agent.chars().filter(|c| c.is_ascii_graphic()).take(80);
            *bridge.agent.lock().unwrap() = Some(agent.collect());
        }
        let key = id.as_ref().map(Value::to_string);
        // Locked across the spawn, so the task cannot finish before it is listed.
        let mut running = bridge.running.lock().unwrap();
        let task = tokio::spawn({
            let (bridge, key) = (bridge.clone(), key.clone());
            async move {
                match id {
                    Some(id) => bridge.request(id, initialize, line).await,
                    None => drop(bridge.post(&line).await),
                }
                if let Some(key) = key {
                    bridge.running.lock().unwrap().remove(&key);
                }
            }
        });
        if let Some(key) = key {
            running.insert(key, task.abort_handle());
        }
    }
    for (_, task) in bridge.running.lock().unwrap().drain() {
        task.abort();
    }
    Ok(())
}
