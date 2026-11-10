use std::io::{IsTerminal, Read};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use mcp_hub::{
    TOKEN_KEY,
    config::{Auth, Config, Endpoint, config_path, load_config},
    connect, http,
    hub::Hub,
    login::login,
    oauth::{credential_keys, secret_env_key},
    secrets::SecretStore,
    update,
    upstream::Upstream,
};
use tokio_util::sync::CancellationToken;

fn usage() -> String {
    format!(
        "usage: mcp-hub <command>

  serve               run the proxy on http://127.0.0.1:<port>/mcp
  login <server>      log in to an OAuth upstream (opens your browser)
  logout <server>     delete stored credentials for a server
  set-token <server>  store a static bearer token (prompted, not echoed) for a \"bearer\" server
  set-token <server> <VAR>
                      store the value of VAR, one of a command server's \"secretEnv\" (same way)
  status              connect to every upstream and count its tools
  token               print the inbound bearer token, creating it if needed
  token --rotate      replace the inbound token (then restart the hub)
  connect             for an agent's config: MCP on stdin/stdout, forwarded to the running hub
  headers             for an agent's config: print the HTTP headers to send to the hub, as JSON
  update              update to the latest release (installs made by install.sh)
  update --check      only check whether a newer release exists
  update <version>    install that version (also to go back)
  --version           print the version

config: {}",
        config_path().display()
    )
}

fn upstreams(config: &Config, store: &SecretStore) -> Vec<Arc<Upstream>> {
    config
        .servers
        .iter()
        .map(|s| Upstream::new(s.clone(), store.clone()))
        .collect()
}

/// The keyring may be locked or unreachable right after boot: wait for it instead of exiting.
async fn wait_for_token(store: &SecretStore) -> String {
    let mut warned = false;
    loop {
        let problem =
            match tokio::time::timeout(Duration::from_secs(120), store.get(TOKEN_KEY)).await {
                Ok(Ok(Some(token))) => return token,
                Ok(Ok(None)) => {
                    "no inbound token in the keyring yet (run \"mcp-hub token\")".to_owned()
                }
                Ok(Err(e)) => format!("{e:#}"),
                Err(_) => "keyring did not answer (locked?)".to_owned(),
            };
        if !warned {
            tracing::warn!("{problem}; retrying every 10s");
            warned = true;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn serve(config: Config, store: SecretStore) -> Result<()> {
    let listener = http::bind(config.port)
        .await
        .with_context(|| format!("cannot listen on 127.0.0.1:{}", config.port))?;
    let token = wait_for_token(&store).await;
    let hub = Hub::new(upstreams(&config, &store));
    let names: Vec<_> = hub
        .upstreams()
        .iter()
        .map(|u| u.name().to_owned())
        .collect();
    tracing::info!("mcp-hub listening on http://127.0.0.1:{}/mcp", config.port);
    tracing::info!(
        "upstreams: {}",
        if names.is_empty() {
            "(none)".into()
        } else {
            names.join(", ")
        }
    );

    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal as unix_signal};
            let mut term = unix_signal(SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        signal.cancel();
    });
    http::serve(listener, hub.clone(), &token, shutdown).await?;
    // All at once: a command server gets a few seconds to exit before it is killed.
    futures::future::join_all(hub.upstreams().iter().map(|u| u.close())).await;
    Ok(())
}

async fn status(config: Config, store: SecretStore) -> Result<bool> {
    let mut ok = true;
    for upstream in upstreams(&config, &store) {
        match upstream.list_tools().await {
            Ok(tools) => println!("ok    {}: {} tools", upstream.name(), tools.len()),
            Err(e) => {
                ok = false;
                println!("FAIL  {}: {e}", upstream.name());
            }
        }
        upstream.close().await;
    }
    Ok(ok)
}

async fn token(store: SecretStore, rotate: bool) -> Result<()> {
    let existing = if rotate {
        None
    } else {
        store.get(TOKEN_KEY).await?
    };
    let value = match existing {
        Some(v) => v,
        None => {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes)
                .map_err(|e| anyhow::anyhow!("no system randomness: {e}"))?;
            let v = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
            store.set(TOKEN_KEY, &v).await?;
            v
        }
    };
    println!("{value}");
    Ok(())
}

/// Read a secret: hidden prompt on a terminal, otherwise the piped input.
fn read_secret(prompt: &str) -> Result<String> {
    if std::io::stdin().is_terminal() {
        Ok(rpassword::prompt_password(prompt)?.trim().to_owned())
    } else {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        Ok(s.trim().to_owned())
    }
}

async fn set_token(
    config: Config,
    store: SecretStore,
    name: &str,
    var: Option<&str>,
) -> Result<()> {
    let server = config.server(name)?;
    let secret_env = match &server.endpoint {
        Endpoint::Stdio(stdio) => stdio.secret_env.as_slice(),
        Endpoint::Http(_) => &[],
    };
    let (key, what) = match var {
        None if server.auth == Auth::Bearer => (format!("{name}:bearer"), "token".to_owned()),
        Some(var) if secret_env.iter().any(|v| v == var) => {
            (secret_env_key(name, var), var.to_owned())
        }
        Some(_) if server.auth == Auth::Bearer => bail!("usage: mcp-hub set-token {name}"),
        _ if !secret_env.is_empty() => {
            bail!(
                "usage: mcp-hub set-token {name} <VAR>, with VAR one of: {}",
                secret_env.join(", ")
            )
        }
        _ => bail!("server \"{name}\" is not configured with \"bearer\": true or \"secretEnv\""),
    };
    let value = read_secret(&format!("{name} {what} (input hidden): "))?;
    // Bearer tokens go in an HTTP header; an environment variable may hold anything but empty.
    if value.is_empty() || (var.is_none() && value.chars().any(char::is_whitespace)) {
        bail!("empty or malformed value, nothing stored");
    }
    let previous = store.get(&key).await?;
    store.set(&key, &value).await?;

    let mut missing = Vec::new();
    for v in secret_env {
        if store.get(&secret_env_key(name, v)).await?.is_none() {
            missing.push(v.as_str());
        }
    }
    if !missing.is_empty() {
        println!(
            "Stored {what} for {name}. Still to set: {}.",
            missing.join(", ")
        );
        return Ok(());
    }
    let upstream = Upstream::new(server.clone(), store.clone());
    let result = upstream.list_tools().await;
    upstream.close().await;
    match result {
        Ok(tools) => {
            println!("Stored {what} for {name}: {} tools available.", tools.len());
            Ok(())
        }
        Err(e) => {
            match previous {
                Some(p) => store.set(&key, &p).await?,
                None => store.delete(&key).await?,
            }
            bail!("{name} failed with this value, nothing stored ({e})")
        }
    }
}

async fn logout(config: Config, store: SecretStore, name: &str) -> Result<()> {
    for key in credential_keys(config.server(name)?) {
        store.delete(&key).await?;
    }
    println!("Removed stored credentials for {name}.");
    Ok(())
}

async fn run(args: &[String]) -> Result<ExitCode> {
    let store = SecretStore::os();
    let arg = args.get(1).map(String::as_str);
    if let [cmd, name, var] = args
        && cmd == "set-token"
    {
        set_token(load_config()?, store, name, Some(var)).await?;
        return Ok(ExitCode::SUCCESS);
    }
    match (args.first().map(String::as_str), arg) {
        (Some("serve"), None) => serve(load_config()?, store).await?,
        (Some("login"), Some(name)) => login(&load_config()?, &store, name).await?,
        (Some("logout"), Some(name)) => logout(load_config()?, store, name).await?,
        (Some("set-token"), Some(name)) => set_token(load_config()?, store, name, None).await?,
        (Some("status"), None) => {
            if !status(load_config()?, store).await? {
                return Ok(ExitCode::FAILURE);
            }
        }
        (Some("token"), None) => token(store, false).await?,
        (Some("token"), Some("--rotate")) => token(store, true).await?,
        (Some("connect"), None) => {
            let port = load_config()?.port;
            connect::run(port, store, tokio::io::stdin(), tokio::io::stdout()).await?
        }
        (Some("headers"), None) => {
            let headers: serde_json::Map<String, serde_json::Value> = connect::headers(&store)
                .await?
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value.into()))
                .collect();
            println!("{}", serde_json::Value::Object(headers));
        }
        (Some("update"), None) => update::update(None, false).await?,
        (Some("update"), Some("--check")) => update::update(None, true).await?,
        (Some("update"), Some(version)) => update::update(Some(version), false).await?,
        (Some("--version" | "version"), None) => println!("mcp-hub {}", env!("CARGO_PKG_VERSION")),
        _ => {
            eprintln!("{}", usage());
            return Ok(ExitCode::from(2));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn init_logging(serving: bool) {
    use tracing_subscriber::EnvFilter;
    let default = if serving {
        "info,rmcp=warn"
    } else {
        "warn,rmcp=error"
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal());
    // journald already timestamps every line.
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        builder.without_time().init();
    } else {
        builder.init();
    }
}

/// Agents may start `mcp-hub connect` with a bare environment (Codex passes on a handful of
/// variables), and the keyring is reached over the session bus: point at the usual socket.
#[cfg(target_os = "linux")]
fn find_session_bus() {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
        return;
    }
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => std::path::PathBuf::from(dir),
        // SAFETY: getuid has no preconditions and cannot fail.
        None => format!("/run/user/{}", unsafe { libc::getuid() }).into(),
    };
    let bus = dir.join("bus");
    if bus.exists() {
        // SAFETY: called first thing in main, before any other thread exists.
        unsafe {
            std::env::set_var(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", bus.display()),
            )
        };
    }
}

fn main() -> ExitCode {
    #[cfg(target_os = "linux")]
    find_session_bus();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str);
    init_logging(command == Some("serve"));
    let mut runtime = if matches!(command, Some("connect" | "headers")) {
        // One of these runs per agent session: keep it to a single thread.
        tokio::runtime::Builder::new_current_thread()
    } else {
        // Two workers: the hub mostly waits on sockets, and with glibc every extra worker
        // thread keeps its own malloc arena, each holding on to the memory of the largest
        // burst it served.
        let mut runtime = tokio::runtime::Builder::new_multi_thread();
        runtime.worker_threads(2);
        runtime
    };
    let runtime = runtime.enable_all().build().expect("tokio runtime");
    match runtime.block_on(run(&args)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("mcp-hub: {e:#}");
            ExitCode::FAILURE
        }
    }
}
