//! `mcp-hub update`: install a newer release with the release's own install.sh.
//!
//! Only for installs made by install.sh (binary in ~/.local/bin). Installs built
//! from a clone update with `git pull && bin/install-service.sh` instead.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

const REPO: &str = "413n/mcp-hub";

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

fn releases_url() -> String {
    std::env::var("MCP_HUB_RELEASES").unwrap_or_else(|_| format!("https://github.com/{REPO}/releases"))
}

/// "1.2.3" or "v1.2.3" → (1, 2, 3); pre-release or build suffixes are ignored.
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("mcp-hub/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}

/// The latest release's version, from the redirect of `<releases>/latest` to `.../tag/v<version>`.
pub async fn latest_version() -> Result<String> {
    let url = format!("{}/latest", releases_url());
    let res = http()?.get(&url).send().await.with_context(|| format!("cannot reach {url}"))?;
    let location = res
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|l| l.to_str().ok())
        .with_context(|| format!("no release found at {url} (HTTP {})", res.status()))?;
    let version = location.rsplit('/').next().unwrap_or_default().trim_start_matches('v');
    if parse_version(version).is_none() {
        bail!("unexpected latest-release redirect: {location}");
    }
    Ok(version.to_owned())
}

/// Where install.sh puts the binary.
fn installer_bin() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
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
    let url = std::env::var("MCP_HUB_INSTALL_SCRIPT")
        .unwrap_or_else(|_| format!("https://raw.githubusercontent.com/{REPO}/v{version}/install.sh"));
    let res = http()?.get(&url).send().await.with_context(|| format!("cannot download {url}"))?;
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
    child.stdin.take().expect("piped stdin").write_all(&script)?;
    let status = child.wait()?;
    if !status.success() {
        bail!("installer failed ({status})");
    }
    Ok(())
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
    if check_only {
        if newer {
            println!("Update available: {CURRENT} -> {latest}. Run: mcp-hub update");
        } else {
            println!("mcp-hub {CURRENT} is up to date (latest release: {latest}).");
        }
        return Ok(());
    }
    if target.is_none() && !newer {
        println!("mcp-hub {CURRENT} is up to date (latest release: {latest}).");
        return Ok(());
    }
    check_installed_by_installer()?;
    println!("Updating mcp-hub {CURRENT} -> {latest}...");
    run_installer(&latest).await
}

#[cfg(test)]
mod tests {
    use super::parse_version;

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
