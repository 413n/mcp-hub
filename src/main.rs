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
    stats::{self, Stats},
    update,
    upstream::{Timings, Upstream},
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
  stats [--days N] [--all] [--json]
                      what the hub did lately: calls, failures, result and definition sizes
  token               print the inbound bearer token, creating it if needed
  token --rotate      replace the inbound token; the running hub turns the old one down at once
  connect             for an agent's config: MCP on stdin/stdout, forwarded to the running hub
  headers             for an agent's config: print the HTTP headers to send to the hub, as JSON
  check [--wait N]    whether the hub answers on its port, waiting up to N seconds for it
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
    let stats = Stats::open(&stats::state_dir());
    stats.record(
        "hub",
        serde_json::json!({"version": env!("CARGO_PKG_VERSION"), "servers": config.servers.len()}),
    );
    let servers = config.servers.iter();
    let upstreams: Vec<_> = servers
        .map(|s| Upstream::with(s.clone(), store.clone(), Timings::default(), stats.clone()))
        .collect();
    for upstream in &upstreams {
        upstream.keep_tools_in(&stats::state_dir().join("tools"));
    }
    let hub = Hub::with_stats(upstreams, stats);
    mcp_hub::results::sweep(&stats::state_dir().join("results"));
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
    http::serve_with(listener, hub.clone(), &token, Some(store), shutdown).await?;
    // All at once: a command server gets a moment to exit before it is killed.
    futures::future::join_all(hub.upstreams().iter().map(|u| u.close())).await;
    tracing::info!("mcp-hub stopped");
    Ok(())
}

/// `mcp-hub status`: asked of the running hub, which has the servers' credentials at hand;
/// only if none runs does this connect to each server itself (and read each one's
/// credentials from the keyring).
async fn status(config: Config, store: SecretStore) -> Result<bool> {
    let asked = match store.get(TOKEN_KEY).await? {
        Some(token) => http::status(config.port, &token).await,
        None => Ok(None),
    };
    match asked {
        Ok(Some(servers)) => {
            let mut ok = true;
            for server in &servers {
                match (&server.tools, &server.error) {
                    (Some(tools), _) => println!("ok    {}: {tools} tools", server.name),
                    (None, error) => {
                        ok = false;
                        let error = error.as_deref().unwrap_or("no answer");
                        println!("FAIL  {}: {error}", server.name);
                    }
                }
            }
            let running: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
            let configured: Vec<_> = config.servers.iter().map(|s| s.name.as_str()).collect();
            if running != configured {
                println!(
                    "The config changed since the hub started: restart it to apply ({}).",
                    update::start_command()
                );
            }
            return Ok(ok);
        }
        Ok(None) => println!("(No hub running that can say: asking each server from here.)"),
        Err(e) => println!("({e:#}: asking each server from here.)"),
    }
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

/// `mcp-hub stats`: the record of the last days, and the tools the running hub lists.
async fn show_stats(config: Config, store: SecretStore, options: &[String]) -> Result<()> {
    let (mut days, mut all, mut as_json) = (7, false, false);
    let mut options = options.iter().map(String::as_str);
    while let Some(option) = options.next() {
        match (option, options.clone().next().and_then(|n| n.parse().ok())) {
            ("--days", Some(n)) => {
                days = n;
                options.next();
            }
            ("--all", _) => all = true,
            ("--json", _) => as_json = true,
            _ => bail!("usage: mcp-hub stats [--days N] [--all] [--json]"),
        }
    }
    let tools = match store.get(TOKEN_KEY).await {
        Ok(Some(token)) => stats::listed_tools(config.port, &token).await.ok(),
        _ => None,
    };
    let summary = stats::summary(&stats::state_dir(), days, tools.as_deref());
    if as_json {
        println!("{summary}");
    } else {
        print!("{}", stats::render(&summary, all));
    }
    Ok(())
}

async fn token(store: SecretStore, rotate: bool) -> Result<()> {
    let existing = store.get(TOKEN_KEY).await?;
    let old = if rotate { existing.clone() } else { None };
    let existing = existing.filter(|_| !rotate);
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
    // The running hub still takes the old token: have it read the new one.
    if let Some(old) = old {
        let told = match load_config() {
            Ok(config) => http::reload_token(config.port, &old).await,
            Err(e) => Err(e),
        };
        match told {
            Ok(http::Reloaded::Yes) => {
                eprintln!("The running hub takes only the new token from now on.")
            }
            Ok(http::Reloaded::NotRunning) => {}
            Ok(http::Reloaded::TooOld) => eprintln!(
                "The running hub is an older version and still takes the old token: \
                 restart it with {}",
                update::start_command()
            ),
            Err(e) => eprintln!(
                "mcp-hub: could not hand the new token to the running hub ({e:#}). \
                 Restart it with {}",
                update::start_command()
            ),
        }
    }
    println!("{value}");
    Ok(())
}

/// `mcp-hub check`: whether the hub answers, for scripts.
async fn check(options: &[String]) -> Result<bool> {
    let wait = match options {
        [] => 0,
        [flag, n] if flag == "--wait" => n.parse().context("usage: mcp-hub check [--wait N]")?,
        _ => bail!("usage: mcp-hub check [--wait N]"),
    };
    let port = load_config()?.port;
    let deadline = std::time::Instant::now() + Duration::from_secs(wait);
    loop {
        let found = http::probe(port).await;
        if found == http::Probe::Hub {
            println!("mcp-hub is running on http://127.0.0.1:{port}/mcp");
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            match found {
                http::Probe::Other(status) => println!(
                    "Another program answers on 127.0.0.1:{port} (HTTP {status}), not mcp-hub: \
                     stop it, or give the hub another \"port\" in the config."
                ),
                http::Probe::Silent => println!(
                    "Something holds 127.0.0.1:{port} but does not answer: a hub that is still \
                     starting or waits for the keyring (on macOS, look for a Keychain dialog)."
                ),
                _ => println!("Nothing listens on 127.0.0.1:{port}: the hub is not running."),
            }
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
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
    if args.first().map(String::as_str) == Some("check") {
        let running = check(&args[1..]).await?;
        return Ok(if running {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    if args.first().map(String::as_str) == Some("stats") {
        show_stats(load_config()?, store, &args[1..]).await?;
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
            let config = load_config()?;
            let skip = connect::skipped(&config, connect::env_is_set);
            let (input, output) = (tokio::io::stdin(), tokio::io::stdout());
            connect::run(config.port, store, skip, input, output).await?
        }
        (Some("headers"), None) => {
            let skip = connect::skipped(&load_config()?, connect::env_is_set);
            let headers: serde_json::Map<String, serde_json::Value> =
                connect::headers(&store, &skip)
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
    let result = runtime.block_on(run(&args));
    // Not to wait on a task that is stuck, such as a keychain read waiting for a dialog.
    runtime.shutdown_timeout(Duration::from_secs(1));
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("mcp-hub: {e:#}");
            ExitCode::FAILURE
        }
    }
}
