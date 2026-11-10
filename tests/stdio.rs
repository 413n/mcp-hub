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

    fn starts(&self) -> usize {
        std::fs::read_to_string(&self.0)
            .map(|s| s.lines().count())
            .unwrap_or(0)
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
