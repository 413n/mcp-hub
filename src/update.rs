//! `mcp-hub update`: install a newer release with the release's own install.sh.
//!
//! Only for installs made by install.sh (binary in ~/.local/bin). Installs built
//! from a clone update with `git pull && bin/install-service.sh` instead.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

const REPO: &str = "narcisoaf/mcp-hub";

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

fn releases_url() -> String {
    std::env::var("MCP_HUB_RELEASES")
        .unwrap_or_else(|_| format!("https://github.com/{REPO}/releases"))
}

/// "1.2.3" or "v1.2.3" → (1, 2, 3); pre-release or build suffixes are ignored.
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

/// Follows redirects: a renamed or transferred repository answers its old
/// address with a redirect to the new one.
fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("mcp-hub/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}

/// The latest release's version.
pub async fn latest_version() -> Result<String> {
    latest_version_at(&releases_url()).await
}

/// `<releases>/latest` redirects to `.../releases/tag/v<version>`, possibly after
/// a redirect to the repository's new address; the version is read from where it ends.
async fn latest_version_at(releases: &str) -> Result<String> {
    let url = format!("{releases}/latest");
    let res = http()?
        .head(&url)
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?;
    let end = res.url();
    match end.path().rsplit_once("/tag/") {
        Some((_, tag)) if parse_version(tag).is_some() => {
            Ok(tag.trim_start_matches('v').to_owned())
        }
        _ => bail!(
            "no release found at {url} (HTTP {}, ended at {end})",
            res.status()
        ),
    }
}

/// Where install.sh puts the binary.
fn installer_bin() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local").join("bin").join("mcp-hub")
}

fn check_installed_by_installer() -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let expected = installer_bin();
    let same = expected.canonicalize().map(|e| e == exe).unwrap_or(false);
    if !same {
        bail!(
            "this mcp-hub ({}) was not installed by install.sh; update it where it came from \
             (from a clone: git pull && bin/install-service.sh)",
            exe.display()
        );
    }
    Ok(())
}

async fn run_installer(version: &str) -> Result<()> {
    // The installer from the target release's tag, so script and archives match.
    let url = std::env::var("MCP_HUB_INSTALL_SCRIPT").unwrap_or_else(|_| {
        format!("https://raw.githubusercontent.com/{REPO}/v{version}/install.sh")
    });
    let res = http()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("cannot download {url}"))?;
    if !res.status().is_success() {
        bail!("cannot download {url}: HTTP {}", res.status());
    }
    let script = res.bytes().await?;
    let mut child = Command::new("/bin/sh")
        .arg("-s")
        .env("MCP_HUB_VERSION", version)
        .stdin(Stdio::piped())
        .spawn()
        .context("cannot run /bin/sh")?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(&script)?;
    let status = child.wait()?;
    if !status.success() {
        // It may have failed after it replaced the binary and stopped the hub.
        let running = match crate::config::load_config() {
            Ok(config) => crate::http::answers(config.port).await,
            Err(_) => false,
        };
        if running {
            bail!("installer failed ({status}); the hub is still running");
        }
        bail!(
            "installer failed ({status}), and the hub is not running. Start it with:\n  {}",
            start_command()
        );
    }
    Ok(())
}

/// The command that (re)starts the service the installer set up.
pub fn start_command() -> &'static str {
    if cfg!(target_os = "macos") {
        "launchctl kickstart -k gui/$(id -u)/dev.mcp-hub || \
         launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.mcp-hub.plist"
    } else {
        "systemctl --user restart mcp-hub"
    }
}

/// `target`: a specific version to install (also older ones), or None for the latest.
pub async fn update(target: Option<&str>, check_only: bool) -> Result<()> {
    let latest = match target {
        Some(v) => {
            parse_version(v).with_context(|| format!("not a version: {v}"))?;
            v.trim_start_matches('v').to_owned()
        }
        None => latest_version().await?,
    };
    let newer = parse_version(&latest) > parse_version(CURRENT);
    let p = crate::ui::out();
    if check_only {
        if newer {
            println!(
                "{}Update available: {CURRENT} → {}. Run: {}",
                p.warn(""),
                p.green(&latest),
                p.cyan("mcp-hub update")
            );
        } else {
            println!(
                "{}mcp-hub {CURRENT} is up to date (latest release: {latest}).",
                p.ok("")
            );
        }
        return Ok(());
    }
    if target.is_none() && !newer {
        println!(
            "{}mcp-hub {CURRENT} is up to date (latest release: {latest}).",
            p.ok("")
        );
        return Ok(());
    }
    check_installed_by_installer()?;
    println!(
        "{}",
        p.bold(format!("Updating mcp-hub {CURRENT} → {latest}"))
    );
    run_installer(&latest).await
}

#[cfg(test)]
mod tests {
    use super::{latest_version_at, parse_version};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Answers like GitHub for a repository moved from `old/hub` to `new/hub`,
    /// where `new/hub` has release v1.2.3 and `empty/hub` has none.
    async fn fake_github() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let head = match path {
                    "/old/hub/releases/latest" => {
                        "301 Moved Permanently\r\nLocation: /new/hub/releases/latest".to_owned()
                    }
                    "/new/hub/releases/latest" => {
                        "302 Found\r\nLocation: /new/hub/releases/tag/v1.2.3".to_owned()
                    }
                    "/empty/hub/releases/latest" => {
                        "302 Found\r\nLocation: /empty/hub/releases".to_owned()
                    }
                    _ => "200 OK".to_owned(),
                };
                let response =
                    format!("HTTP/1.1 {head}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn latest_version_follows_a_moved_repository() {
        let base = fake_github().await;
        let at = |repo: &str| format!("{base}/{repo}/releases");
        assert_eq!(latest_version_at(&at("new/hub")).await.unwrap(), "1.2.3");
        assert_eq!(latest_version_at(&at("old/hub")).await.unwrap(), "1.2.3");
        let err = latest_version_at(&at("empty/hub")).await.unwrap_err();
        assert!(err.to_string().starts_with("no release found"), "{err}");
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_version("v1.20.3"), Some((1, 20, 3)));
        assert_eq!(parse_version("1.2.3-rc.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("latest"), None);
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
    }
}
