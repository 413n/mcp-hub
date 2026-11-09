//! `mcp-hub login <server>`: OAuth in the browser, redirect to a one-shot loopback listener.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{
    Request, Response, StatusCode, body::Incoming, server::conn::http1, service::service_fn,
};
use hyper_util::rt::TokioIo;
use rmcp::transport::auth::{AuthorizationManager, AuthorizationRequest, OAuthState};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use url::Url;

use crate::config::{Auth, Config, Endpoint};
use crate::oauth::KeyringCredentialStore;
use crate::secrets::SecretStore;
use crate::upstream::Upstream;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

fn page(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    let html =
        format!("<!doctype html><meta charset=\"utf-8\"><title>mcp-hub</title><p>{message}</p>");
    let mut res = Response::new(Full::new(Bytes::from(html)));
    *res.status_mut() = status;
    res.headers_mut()
        .insert("content-type", "text/html; charset=utf-8".parse().unwrap());
    res
}

/// Wait for the redirect carrying `expected_state`; returns the full callback URL.
/// Redirects with another state (an old browser tab) are refused without ending the login.
async fn wait_for_callback(
    listener: TcpListener,
    port: u16,
    expected_state: String,
) -> Result<String> {
    let (tx, mut rx) = mpsc::channel::<Result<String>>(1);
    let expected_state = Arc::new(expected_state);
    let accept = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let (tx, expected_state) = (tx.clone(), expected_state.clone());
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let (tx, expected_state) = (tx.clone(), expected_state.clone());
                    async move {
                        let host_ok = req.headers().get("host").and_then(|h| h.to_str().ok())
                            == Some(&format!("127.0.0.1:{port}"));
                        if !host_ok || req.uri().path() != "/callback" {
                            return Ok::<_, Infallible>(page(StatusCode::NOT_FOUND, "Not found."));
                        }
                        let url = format!("http://127.0.0.1:{port}{}", req.uri());
                        let parsed = Url::parse(&url).expect("valid callback URL");
                        let param = |k: &str| {
                            parsed
                                .query_pairs()
                                .find(|(name, _)| name == k)
                                .map(|(_, v)| v.into_owned())
                        };
                        if param("state").as_deref() != Some(expected_state.as_str()) {
                            println!(
                                "Ignored a redirect from an outdated login link; still waiting for the current one."
                            );
                            return Ok(page(
                                StatusCode::BAD_REQUEST,
                                "This login link is out of date. Use the most recent link printed in the terminal.",
                            ));
                        }
                        if let Some(error) = param("error") {
                            let _ = tx.send(Err(anyhow!("authorization failed: {error}"))).await;
                            return Ok(page(StatusCode::OK, "Login failed. Check the terminal."));
                        }
                        if param("code").is_none() {
                            let _ = tx
                                .send(Err(anyhow!("authorization failed: no code returned")))
                                .await;
                            return Ok(page(StatusCode::OK, "Login failed. Check the terminal."));
                        }
                        let _ = tx.send(Ok(url)).await;
                        Ok(page(
                            StatusCode::OK,
                            "Login complete. You can close this tab.",
                        ))
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    let result = tokio::time::timeout(LOGIN_TIMEOUT, rx.recv()).await;
    // Give the browser a moment to receive the final page before the listener goes away.
    tokio::time::sleep(Duration::from_millis(200)).await;
    accept.abort();
    match result {
        Ok(Some(r)) => r,
        Ok(None) => bail!("callback listener stopped"),
        Err(_) => bail!("login timed out"),
    }
}

fn open_browser(url: &str) -> Result<()> {
    // Only hand web URLs to the opener; the URL comes from the server's metadata.
    let parsed = Url::parse(url)?;
    if !matches!(parsed.scheme(), "https" | "http") {
        bail!(
            "refusing to open authorization URL with scheme {}",
            parsed.scheme()
        );
    }
    println!("\nOpen this URL to log in (trying your browser now):\n\n  {url}\n");
    let opener = if cfg!(target_os = "macos") {
        "/usr/bin/open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    Ok(())
}

pub async fn login(config: &Config, store: &SecretStore, name: &str) -> Result<()> {
    let server = config.server(name)?;
    let (Auth::OAuth { client_id, scope }, Endpoint::Http(url)) = (&server.auth, &server.endpoint)
    else {
        bail!("server \"{name}\" is not configured for OAuth");
    };
    let redirect = format!("http://127.0.0.1:{}/callback", config.login_port);

    let mut manager = AuthorizationManager::new(url.as_str()).await?;
    manager.set_credential_store(KeyringCredentialStore::new(store.clone(), name));
    let mut oauth = OAuthState::Unauthorized(manager);
    let mut request = AuthorizationRequest::new(&redirect).with_client_name("mcp-hub (local)");
    if let Some(id) = client_id {
        // Pre-registered id, e.g. Home Assistant's IndieAuth-style URL client_id.
        request = request.with_preregistered_client(id);
    }
    if let Some(scope) = scope {
        request = request.with_scopes(scope.split_whitespace());
    }

    let listener = TcpListener::bind(("127.0.0.1", config.login_port))
        .await
        .with_context(|| {
            format!(
                "cannot listen on 127.0.0.1:{} for the login redirect",
                config.login_port
            )
        })?;
    oauth
        .start_authorization(request)
        .await
        .context("starting authorization")?;
    let auth_url = oauth.get_authorization_url().await?;
    let state = Url::parse(&auth_url)?
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .context("authorization URL has no state parameter")?;
    open_browser(&auth_url)?;
    let callback = wait_for_callback(listener, config.login_port, state).await?;
    oauth
        .handle_callback_url(&callback)
        .await
        .context("completing authorization")?;

    // Verify with a fresh connection that uses only what is now in the keyring.
    let upstream = Upstream::new(server.clone(), store.clone());
    let tools = upstream.list_tools().await;
    upstream.close().await;
    let tools = tools?;
    println!("Logged in to {name}: {} tools available.", tools.len());
    Ok(())
}
