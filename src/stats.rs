//! A local record of what the hub did, and `mcp-hub stats` to read it.
//!
//! One JSON object per line in `~/.local/state/mcp-hub/events.jsonl`: which tool was
//! called by which agent, how long it took, how it ended and how large the result was; when
//! a server started, stopped and failed. Never what was asked or answered. Nothing leaves
//! the machine.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{Map, Value, json};

const FILE: &str = "events.jsonl";
/// The file before this one: `events.jsonl` is moved here when it is full.
const OLD_FILE: &str = "events.1.jsonl";
/// With the old file, at most twice this is kept: some 50,000 events.
const MAX_BYTES: u64 = 4 << 20;

/// Where the hub keeps what it writes for itself: `$XDG_STATE_HOME/mcp-hub`, by default
/// `~/.local/state/mcp-hub`.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MCP_HUB_STATE") {
        return PathBuf::from(dir);
    }
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"),
    };
    base.join("mcp-hub")
}

/// Create `dir` for the user alone.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn now() -> u64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    since.map_or(0, |d| d.as_secs())
}

struct Log {
    dir: PathBuf,
    file: File,
    bytes: u64,
}

impl Log {
    fn open(dir: &Path) -> std::io::Result<Self> {
        private_dir(dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(FILE))?;
        let bytes = file.metadata()?.len();
        Ok(Self {
            dir: dir.to_owned(),
            file,
            bytes,
        })
    }

    fn write(&mut self, line: &[u8]) -> std::io::Result<()> {
        if self.bytes + line.len() as u64 > MAX_BYTES {
            std::fs::rename(self.dir.join(FILE), self.dir.join(OLD_FILE))?;
            *self = Self::open(&self.dir)?;
        }
        self.file.write_all(line)?;
        self.bytes += line.len() as u64;
        Ok(())
    }
}

/// The record being written. Cheap to clone; one that is off records nothing.
#[derive(Clone, Default)]
pub struct Stats(Option<Arc<Mutex<Log>>>);

impl Stats {
    pub fn off() -> Self {
        Self(None)
    }

    /// Record into `dir`. If that cannot be written, the hub runs without a record.
    pub fn open(dir: &Path) -> Self {
        match Log::open(dir) {
            Ok(log) => Self(Some(Arc::new(Mutex::new(log)))),
            Err(e) => {
                tracing::warn!("no record for \"mcp-hub stats\": {}: {e}", dir.display());
                Self(None)
            }
        }
    }

    /// Add an event: `fields` is a JSON object, to which the time and `event` are added.
    pub fn record(&self, event: &str, fields: Value) {
        let Some(log) = &self.0 else { return };
        let mut line = Map::new();
        line.insert("t".into(), now().into());
        line.insert("ev".into(), event.into());
        if let Value::Object(fields) = fields {
            line.extend(fields.into_iter().filter(|(_, value)| !value.is_null()));
        }
        let mut line = Value::Object(line).to_string().into_bytes();
        line.push(b'\n');
        // A full disk must not take the hub down with it.
        if let Err(e) = log.lock().unwrap().write(&line) {
            tracing::debug!("cannot record an event: {e}");
        }
    }
}

/// The events of the last `days` days, oldest first.
fn read(dir: &Path, days: u64) -> Vec<Value> {
    let since = now().saturating_sub(days * 86_400);
    let mut events = Vec::new();
    for name in [OLD_FILE, FILE] {
        let Ok(text) = std::fs::read_to_string(dir.join(name)) else {
            continue;
        };
        let lines = text.lines().filter_map(|l| serde_json::from_str(l).ok());
        events.extend(lines.filter(|e: &Value| e["t"].as_u64().is_some_and(|t| t >= since)));
    }
    events
}

/// Calls of one tool, or by one agent.
#[derive(Default)]
struct Calls {
    ms: Vec<u64>,
    /// Characters of text in each result.
    chars: Vec<u64>,
    /// Calls that ended in an error, of the tool or on the way to it.
    failed: u64,
    /// Results the hub kept in a file instead of passing them on whole.
    cut: u64,
    /// Characters saved by dropping empty fields.
    compacted: u64,
}

impl Calls {
    fn add(&mut self, event: &Value) {
        self.ms.push(event["ms"].as_u64().unwrap_or(0));
        self.chars.push(event["text"].as_u64().unwrap_or(0));
        self.failed += u64::from(event["outcome"] != "ok");
        self.cut += u64::from(event["cut"].as_u64().is_some());
        self.compacted += event["compacted"].as_u64().unwrap_or(0);
    }

    fn summary(&mut self) -> Value {
        self.ms.sort_unstable();
        self.chars.sort_unstable();
        let median = |v: &[u64]| v.get(v.len() / 2).copied().unwrap_or(0);
        json!({
            "calls": self.ms.len(),
            "failed": self.failed,
            "medianMs": median(&self.ms),
            "slowestMs": self.ms.last(),
            "medianChars": median(&self.chars),
            "largestChars": self.chars.last(),
            "totalChars": self.chars.iter().sum::<u64>(),
            "cut": self.cut,
            "compacted": self.compacted,
        })
    }
}

/// The agent an event came from: its name without the version, and what it was not given.
fn agent(event: &Value) -> String {
    let client = event["client"].as_str().unwrap_or("unknown");
    let name = client.split(['/', ' ']).next().unwrap_or(client);
    match event["skip"].as_array() {
        Some(skip) if !skip.is_empty() => {
            let skipped: Vec<_> = skip.iter().filter_map(Value::as_str).collect();
            format!("{name} (without {})", skipped.join(", "))
        }
        _ => name.to_owned(),
    }
}

/// Everything `mcp-hub stats` shows, from the events in `dir` and, if the hub answered,
/// the tools it lists right now.
pub fn summary(dir: &Path, days: u64, tools: Option<&[Value]>) -> Value {
    let events = read(dir, days);
    let mut by_tool: BTreeMap<String, Calls> = BTreeMap::new();
    let mut by_agent: BTreeMap<String, (Calls, u64)> = BTreeMap::new();
    let mut servers: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    let mut errors: BTreeMap<(String, String), u64> = BTreeMap::new();
    // Per server, as of its last tool list: tools with a description cut, and bytes saved.
    let mut cuts: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut count = |server: &Value, what: &str| {
        let server = servers.entry(server.as_str().unwrap_or("?").to_owned());
        let n = server.or_default().entry(what).or_insert(json!(0));
        *n = json!(n.as_u64().unwrap_or(0) + 1);
    };
    for e in &events {
        match e["ev"].as_str().unwrap_or("") {
            "call" => {
                let server = e["server"].as_str().unwrap_or("?");
                let tool = format!("{server}__{}", e["tool"].as_str().unwrap_or("?"));
                if let Some(error) = e["error"].as_str() {
                    *errors.entry((tool.clone(), error.to_owned())).or_default() += 1;
                }
                by_tool.entry(tool).or_default().add(e);
                by_agent.entry(agent(e)).or_default().0.add(e);
            }
            "list" => by_agent.entry(agent(e)).or_default().1 += 1,
            "tools" => {
                let saved = e["bytesBeforeCuts"].as_u64().unwrap_or(0);
                let saved = saved.saturating_sub(e["bytes"].as_u64().unwrap_or(0));
                let cut = (e["toolsCut"].as_u64().unwrap_or(0), saved);
                cuts.insert(e["server"].as_str().unwrap_or("?").to_owned(), cut);
            }
            "connect" => count(&e["server"], "connected"),
            "fail" => count(&e["server"], "failed"),
            "stop" => count(&e["server"], e["why"].as_str().unwrap_or("stopped")),
            _ => {}
        }
    }

    let mut total = Calls::default();
    for calls in by_tool.values() {
        total.ms.extend(&calls.ms);
        total.chars.extend(&calls.chars);
        total.failed += calls.failed;
        total.cut += calls.cut;
        total.compacted += calls.compacted;
    }
    let mut called: Vec<_> = by_tool.iter_mut().map(|(k, v)| (k, v.summary())).collect();
    called.sort_by_key(|(_, s)| std::cmp::Reverse(s["totalChars"].as_u64()));
    let mut errors: Vec<_> = errors.into_iter().collect();
    errors.sort_by_key(|(_, n)| std::cmp::Reverse(*n));

    let mut summary = json!({
        "days": days,
        "events": events.len(),
        "total": total.summary(),
        "tools": called.iter().map(|(name, s)| {
            let mut s = s.clone();
            s["tool"] = json!(name);
            s
        }).collect::<Vec<_>>(),
        "agents": by_agent.iter_mut().map(|(name, (calls, lists))| {
            json!({"agent": name, "calls": calls.ms.len(), "failed": calls.failed, "toolLists": lists})
        }).collect::<Vec<_>>(),
        "servers": servers,
        "descriptionsCut": {
            "tools": cuts.values().map(|(tools, _)| tools).sum::<u64>(),
            "bytesSaved": cuts.values().map(|(_, saved)| saved).sum::<u64>(),
        },
        "errors": errors.iter().take(15).map(|((tool, error), n)| {
            json!({"tool": tool, "error": error, "times": n})
        }).collect::<Vec<_>>(),
    });

    if let Some(tools) = tools {
        let size = |tool: &Value| tool.to_string().len() as u64;
        let name = |tool: &Value| tool["name"].as_str().unwrap_or("?").to_owned();
        let mut per_server: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for tool in tools {
            let name = name(tool);
            let server = per_server.entry(name.split("__").next().unwrap_or("?").to_owned());
            let (n, bytes) = server.or_default();
            *n += 1;
            *bytes += size(tool);
        }
        let mut largest: Vec<_> = tools.iter().map(|t| (size(t), name(t))).collect();
        largest.sort_unstable_by(|a, b| b.cmp(a));
        let unused: Vec<_> = tools
            .iter()
            .map(name)
            .filter(|name| !by_tool.contains_key(name))
            .collect();
        summary["definitions"] = json!({
            "tools": tools.len(),
            "bytes": tools.iter().map(size).sum::<u64>(),
            "servers": per_server.iter().map(|(server, (n, bytes))| {
                json!({"server": server, "tools": n, "bytes": bytes})
            }).collect::<Vec<_>>(),
            "largest": largest.iter().take(8).map(|(bytes, name)| {
                json!({"tool": name, "bytes": bytes})
            }).collect::<Vec<_>>(),
            "neverCalled": unused,
        });
    }
    summary
}

/// 12345 as "12.3k", for sizes in characters or bytes.
fn short(n: &Value) -> String {
    match n.as_u64().unwrap_or(0) {
        n if n >= 10_000_000 => format!("{:.0}M", n as f64 / 1e6),
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 10_000 => format!("{:.0}k", n as f64 / 1e3),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

/// A count out of the summary. As a number, so that it lines up in a column.
fn n(value: &Value) -> u64 {
    value.as_u64().unwrap_or(0)
}

/// The summary as text for a terminal.
pub fn render(s: &Value, all: bool) -> String {
    use std::fmt::Write;
    let list = |v: &Value| v.as_array().cloned().unwrap_or_default();
    let mut out = String::new();
    let total = &s["total"];
    let _ = writeln!(
        out,
        "Last {} days: {} tool calls ({} failed), {} characters of results (about {} tokens)",
        n(&s["days"]),
        n(&total["calls"]),
        n(&total["failed"]),
        short(&total["totalChars"]),
        short(&json!(total["totalChars"].as_u64().unwrap_or(0) / 4)),
    );

    if n(&total["cut"]) > 0 || n(&total["compacted"]) > 0 {
        let _ = writeln!(
            out,
            "{} results kept in a file for being too large; {} characters saved by dropping empty fields",
            n(&total["cut"]),
            short(&total["compacted"]),
        );
    }

    let tools = list(&s["tools"]);
    if !tools.is_empty() {
        let _ = writeln!(
            out,
            "\nBy tool, largest total first (time in ms, results in characters)\n\
             {:<44}{:>6}{:>7}{:>8}{:>9}{:>8}{:>9}{:>8}",
            "", "calls", "failed", "median", "slowest", "median", "largest", "total"
        );
        for t in &tools {
            let _ = writeln!(
                out,
                "{:<44}{:>6}{:>7}{:>8}{:>9}{:>8}{:>9}{:>8}",
                t["tool"].as_str().unwrap_or("?"),
                n(&t["calls"]),
                n(&t["failed"]),
                n(&t["medianMs"]),
                n(&t["slowestMs"]),
                short(&t["medianChars"]),
                short(&t["largestChars"]),
                short(&t["totalChars"]),
            );
        }
    }
    let errors = list(&s["errors"]);
    if !errors.is_empty() {
        let _ = writeln!(out, "\nFailures");
        for e in &errors {
            let (tool, error) = (e["tool"].as_str(), e["error"].as_str());
            let (tool, error) = (tool.unwrap_or("?"), error.unwrap_or("?"));
            let _ = writeln!(out, "{:>5}x {tool}: {error}", n(&e["times"]));
        }
    }
    let agents = list(&s["agents"]);
    if !agents.is_empty() {
        let _ = writeln!(
            out,
            "\nBy agent{:>36}{:>7}{:>11}",
            "calls", "failed", "tool lists"
        );
        for a in &agents {
            let name = a["agent"].as_str().unwrap_or("?");
            let _ = writeln!(
                out,
                "{name:<38}{:>6}{:>7}{:>11}",
                n(&a["calls"]),
                n(&a["failed"]),
                n(&a["toolLists"])
            );
        }
    }
    if let Some(servers) = s["servers"].as_object().filter(|s| !s.is_empty()) {
        let _ = writeln!(out, "\nServers");
        for (server, counts) in servers {
            let counts = counts.as_object().into_iter().flatten();
            let counts: Vec<_> = counts.map(|(what, n)| format!("{what} {n}")).collect();
            let _ = writeln!(out, "{server:<16}{}", counts.join(", "));
        }
    }
    let d = &s["definitions"];
    if d.is_object() {
        let _ = writeln!(
            out,
            "\nTool definitions now: {} tools, {} bytes (about {} tokens if all were loaded)",
            n(&d["tools"]),
            short(&d["bytes"]),
            short(&json!(d["bytes"].as_u64().unwrap_or(0) / 4)),
        );
        for server in list(&d["servers"]) {
            let name = server["server"].as_str().unwrap_or("?");
            let _ = writeln!(
                out,
                "{name:<16}{:>4} tools{:>8}",
                n(&server["tools"]),
                short(&server["bytes"])
            );
        }
        let cut = &s["descriptionsCut"];
        if n(&cut["tools"]) > 0 {
            let _ = writeln!(
                out,
                "Descriptions cut short in {} tools, which saves {} bytes (hub__describe has them in full)",
                n(&cut["tools"]),
                short(&cut["bytesSaved"]),
            );
        }
        let _ = writeln!(out, "Largest:");
        for tool in list(&d["largest"]) {
            let name = tool["tool"].as_str().unwrap_or("?");
            let _ = writeln!(out, "{:>8}  {name}", short(&tool["bytes"]));
        }
        let unused = list(&d["neverCalled"]);
        let _ = write!(
            out,
            "Not called in this time: {} of {} tools",
            unused.len(),
            n(&d["tools"])
        );
        if all {
            let names: Vec<_> = unused.iter().filter_map(Value::as_str).collect();
            let _ = writeln!(out, "\n{}", names.join(" "));
        } else {
            let _ = writeln!(out, " (--all lists them)");
        }
    } else {
        let _ = writeln!(
            out,
            "\nThe hub is not running: no tool definitions to measure."
        );
    }
    out
}

/// Ask the hub on `port` for the tools it lists.
pub async fn listed_tools(port: u16, token: &str) -> Result<Vec<Value>> {
    let http = reqwest::Client::builder().no_proxy().build()?;
    let response = http
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .timeout(Duration::from_secs(30))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header(
            "User-Agent",
            concat!("mcp-hub-stats/", env!("CARGO_PKG_VERSION")),
        )
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .send()
        .await?
        .error_for_status()?;
    let mut answer: Value = serde_json::from_slice(&response.bytes().await?)?;
    match answer["result"]["tools"].take() {
        Value::Array(tools) => Ok(tools),
        _ => anyhow::bail!("unexpected answer to tools/list"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_summarises() {
        let dir = std::env::temp_dir().join(format!("mcp-hub-stats-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let stats = Stats::open(&dir);
        let call = |tool: &str, ms: u64, outcome: &str, text: u64| {
            json!({"server": "s", "tool": tool, "client": "claude-code/2.1 (cli)", "ms": ms,
                   "outcome": outcome, "text": text, "skip": null})
        };
        stats.record("call", call("a", 10, "ok", 100));
        stats.record("call", call("a", 30, "ok", 300));
        stats.record("call", call("b", 5, "failed", 40));
        stats.record("list", json!({"client": "codex/1", "skip": ["t3code"]}));
        stats.record("stop", json!({"server": "s", "why": "idle"}));
        let listed = json!({"server": "s", "bytes": 900, "bytesBeforeCuts": 5000, "toolsCut": 2});
        stats.record("tools", listed);
        Stats::off().record("call", call("c", 1, "ok", 1));

        let tools = [json!({"name": "s__a"}), json!({"name": "s__z"})];
        let s = summary(&dir, 7, Some(&tools));
        assert_eq!(s["total"]["calls"], 3);
        assert_eq!(s["total"]["failed"], 1);
        assert_eq!(s["tools"][0]["tool"], "s__a");
        assert_eq!(s["tools"][0]["totalChars"], 400);
        assert_eq!(s["tools"][0]["slowestMs"], 30);
        assert_eq!(s["agents"][0]["agent"], "claude-code");
        assert_eq!(s["agents"][1]["agent"], "codex (without t3code)");
        assert_eq!(s["agents"][1]["toolLists"], 1);
        assert_eq!(s["servers"]["s"]["idle"], 1);
        assert_eq!(
            s["descriptionsCut"],
            json!({"tools": 2, "bytesSaved": 4100})
        );
        assert_eq!(s["definitions"]["neverCalled"], json!(["s__z"]));
        let text = render(&s, false);
        assert!(text.contains("3 tool calls (1 failed)"), "{text}");
        assert!(
            text.contains("Not called in this time: 1 of 2 tools"),
            "{text}"
        );

        // A full file is moved aside, and both are read.
        let line = vec![b'x'; MAX_BYTES as usize];
        std::fs::write(dir.join(FILE), &line).unwrap();
        let stats = Stats::open(&dir);
        stats.record("call", call("a", 1, "ok", 1));
        assert_eq!(
            std::fs::metadata(dir.join(OLD_FILE)).unwrap().len(),
            MAX_BYTES
        );
        assert_eq!(summary(&dir, 7, None)["total"]["calls"], 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
