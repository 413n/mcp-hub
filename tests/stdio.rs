//! Command (stdio) upstreams: a real child process speaking MCP over stdin/stdout.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mcp_hub::{
    config::parse_config,
    secrets::SecretStore,
    upstream::{Timings, Upstream, UpstreamError},
};
use rmcp::model::{CallToolRequestParams, CallToolResponse};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn upstream(server: serde_json::Value, store: &SecretStore) -> Arc<Upstream> {
    upstream_with(server, store, Timings::default())
}

fn upstream_with(
    server: serde_json::Value,
    store: &SecretStore,
    timings: Timings,
) -> Arc<Upstream> {
    let config = parse_config(&json!({"servers": {"local": server}}).to_string()).unwrap();
    Upstream::with_timings(config.servers[0].clone(), store.clone(), timings)
}

/// A file a test server appends a line to every time it starts.
struct StartLog(PathBuf);

impl StartLog {
    fn new(test: &str) -> Self {
        let path = std::env::temp_dir().join(format!("mcp-hub-{test}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Self(path)
    }

    fn lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.0)
            .map(|s| s.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    fn starts(&self) -> usize {
        self.lines().len()
    }

    /// Whether the process that started last is still there.
    fn running(&self) -> bool {
        let pid = self.lines().pop().expect("a started process");
        std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    }
}

impl Drop for StartLog {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn call_text(upstream: &Upstream, tool: &str) -> Result<String, UpstreamError> {
    let response = upstream
        .call_tool(
            CallToolRequestParams::new(tool.to_owned()),
            CancellationToken::new(),
        )
        .await?;
    let CallToolResponse::Complete(result) = response else {
        panic!("expected a complete result")
    };
    Ok(result.content[0].as_text().unwrap().text.clone())
}

#[tokio::test]
async fn runs_a_command_server() {
    let store = SecretStore::memory();
    let local = upstream(
        json!({
            "command": "/bin/sh",
            "args": [fixtures().join("stdio-server.sh")],
            "cwd": fixtures(),
            "secretEnv": ["FAKE_SECRET"],
        }),
        &store,
    );

    // The secret is not in the keyring yet: the process is not even started.
    let err = local.list_tools().await.unwrap_err();
    assert!(matches!(err, UpstreamError::NeedsLogin(_)), "{err}");
    assert!(
        err.to_string()
            .contains("mcp-hub set-token local FAKE_SECRET"),
        "{err}"
    );

    store.set("local:env:fake_secret", "s3cret").await.unwrap();
    let tools = local.list_tools().await.unwrap();
    assert_eq!(
        tools.iter().map(|t| t.name.as_ref()).collect::<Vec<_>>(),
        ["env", "crash"]
    );

    let expected = format!(
        "cwd={} secret=s3cret",
        fixtures().canonicalize().unwrap().display()
    );
    assert_eq!(call_text(&local, "env").await.unwrap(), expected);

    // The process dies mid-call: that call fails, the next one gets a new process.
    assert!(call_text(&local, "crash").await.is_err());
    assert_eq!(call_text(&local, "env").await.unwrap(), expected);

    local.close().await;
}

#[tokio::test]
async fn reports_why_a_command_did_not_start() {
    let store = SecretStore::memory();
    let broken = upstream(
        json!({"command": "/bin/sh", "args": ["-c", "echo 'Error: Cannot find module tsx' >&2; exit 1"]}),
        &store,
    );
    let err = broken.list_tools().await.unwrap_err().to_string();
    assert!(
        err.contains("stderr: Error: Cannot find module tsx"),
        "{err}"
    );

    let missing = upstream(json!({"command": "/nonexistent/mcp-server"}), &store);
    let err = missing.list_tools().await.unwrap_err().to_string();
    assert!(
        err.contains("cannot start /nonexistent/mcp-server"),
        "{err}"
    );

    let no_dir = upstream(
        json!({"command": "/bin/sh", "cwd": "/nonexistent/dir"}),
        &store,
    );
    let err = no_dir.list_tools().await.unwrap_err().to_string();
    assert!(
        err.contains("cwd /nonexistent/dir is not a directory"),
        "{err}"
    );
}

#[tokio::test]
async fn leaves_a_failing_server_alone_for_a_while() {
    let log = StartLog::new("failing");
    let broken = upstream_with(
        json!({
            "command": "/bin/sh",
            "args": ["-c", "echo start >> \"$START_LOG\"; exit 1"],
            "env": {"START_LOG": log.0},
        }),
        &SecretStore::memory(),
        Timings {
            retry_delay: Duration::from_millis(400),
            ..Timings::default()
        },
    );
    assert!(broken.list_tools().await.is_err());
    assert_eq!(log.starts(), 1);

    // Asked again at once, by a list or a call: the same error, and no new process.
    let err = broken.list_tools().await.unwrap_err().to_string();
    assert!(err.contains("next attempt in 1s"), "{err}");
    assert!(broken.tools().await.is_none());
    assert!(call_text(&broken, "env").await.is_err());
    assert_eq!(log.starts(), 1);

    // After the delay it is tried again, and then left alone for twice as long.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(broken.list_tools().await.is_err());
    assert_eq!(log.starts(), 2);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(broken.list_tools().await.is_err());
    assert_eq!(log.starts(), 2);
}

#[tokio::test]
async fn stops_an_idle_server() {
    let log = StartLog::new("idle");
    let local = upstream_with(
        json!({
            "command": "/bin/sh",
            "args": [fixtures().join("stdio-server.sh")],
            "env": {"START_LOG": log.0},
            "idleTimeout": 1,
        }),
        &SecretStore::memory(),
        Timings {
            list_ttl: Duration::from_millis(100),
            ..Timings::default()
        },
    );

    // Nothing cached yet: the list starts it.
    assert_eq!(local.tools().await.unwrap().len(), 2);
    assert_eq!(log.starts(), 1);
    assert!(log.running());

    // A call longer than the idle time is not cut off.
    assert_eq!(call_text(&local, "slow").await.unwrap(), "slow done");
    assert_eq!(log.starts(), 1);
    assert!(log.running());

    // Left alone, it is stopped. Its tools are still listed, old as the list is, without
    // starting it again.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!log.running());
    assert_eq!(local.tools().await.unwrap().len(), 2);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(log.starts(), 1);

    // The next call starts it again.
    assert!(call_text(&local, "env").await.is_ok());
    assert_eq!(log.starts(), 2);
    assert!(log.running());
    local.close().await;
    assert!(!log.running());
}

/// Wait until the cached tool list has `n` tools.
async fn cached_tools(upstream: &Upstream, n: usize) -> bool {
    for _ in 0..40 {
        if upstream.tools().await.is_some_and(|tools| tools.len() == n) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn follows_a_changing_tool_list() {
    // Not a start log here, only a file that exists or not.
    let flag = StartLog::new("more");
    let local = upstream(
        json!({
            "command": "/bin/sh",
            "args": [fixtures().join("stdio-server.sh")],
            "env": {"MORE_FLAG": flag.0},
        }),
        &SecretStore::memory(),
    );
    assert_eq!(local.tools().await.unwrap().len(), 2);

    // The cached list is recent, so a change goes unnoticed...
    std::fs::write(&flag.0, "").unwrap();
    assert_eq!(local.tools().await.unwrap().len(), 2);
    // ...until the server says its tools changed.
    assert_eq!(call_text(&local, "notify").await.unwrap(), "notified");
    assert!(cached_tools(&local, 3).await);

    // A server that starts again may be another version: its tools are fetched again.
    std::fs::remove_file(&flag.0).unwrap();
    assert!(call_text(&local, "crash").await.is_err());
    assert!(call_text(&local, "env").await.is_ok());
    assert!(cached_tools(&local, 2).await);
    local.close().await;
}
