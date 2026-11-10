# mcp-hub

Personal MCP proxy: one URL (`http://127.0.0.1:3050/mcp`) for Claude Code, Codex
and Cursor, fanning out to several upstream MCP servers: remote ones over HTTP
and local ones the hub starts as stdio processes. You log in to each
upstream once, in the hub, and every agent gets all of their tools through a
single connection. Tools appear as `<server>__<tool>`, e.g. `sentry__search_issues`.

A single Rust binary built on the official MCP Rust SDK
([`rmcp`](https://github.com/modelcontextprotocol/rust-sdk)). Deliberately small:
no dashboard, no admin API, no runtime reconfiguration.
Stateless towards the agents, and it passes upstream results through without
re-validating them, so memory stays flat: about 15 MB at start and about 30 MB
with 170 tools (half a megabyte of JSON) cached, however many agents ask for
them. Stdio servers are processes of their own on top, stopped while unused.

Runs on Linux (GNOME keyring, systemd) and macOS (Keychain, launchd). Each
machine is independent: only the code is shared through this repo. The config,
upstream logins and the hub's own token live on that machine alone.

## 1. Install

On Linux a GNOME keyring (or another Secret Service) must be running, as on a
normal desktop session.

### One-line install (Linux x86_64, macOS)

    curl -fsSL https://raw.githubusercontent.com/413n/mcp-hub/main/install.sh | sh

[`install.sh`](install.sh) downloads the latest release for your platform,
verifies its SHA-256 checksum, installs the binary to `~/.local/bin/mcp-hub`,
creates an empty config and the hub token if missing, and starts the service.
No Rust needed; the macOS build is a universal binary (Apple Silicon and Intel).
Update later with `mcp-hub update` (or run the same command again); pin a
version with `curl -fsSL .../install.sh | MCP_HUB_VERSION=0.1.0 sh`. Your config and keyring
entries are never touched. Add `~/.local/bin` to your `PATH` to run `mcp-hub`
directly (the rest of this README assumes it).

Prefer to look first? Download the archive from the
[Releases](https://github.com/413n/mcp-hub/releases) page, unpack it and run its
`./install-service.sh` yourself.

### From source

Needs Rust (stable, 1.88+): `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`.

    git clone https://github.com/413n/mcp-hub ~/Code/mcp-hub
    cd ~/Code/mcp-hub
    mkdir -p ~/.config/mcp-hub
    [ -f ~/.config/mcp-hub/config.json ] || cp config.example.json ~/.config/mcp-hub/config.json
    bin/install-service.sh             # builds, installs ~/.cargo/bin/mcp-hub, starts the service
    mcp-hub token

Re-run `bin/install-service.sh` after `git pull` to rebuild and restart.

### Either way

`install-service.sh` creates the hub token if missing, sets up a systemd user
unit (Linux) or a launchd agent (macOS) that starts the hub now and at every
login, and checks that the hub answers. If it does not, it says so and prints
the command that starts it. Check at any time with:

    mcp-hub check              # or: curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:3050/mcp (401 = running)

**macOS:** the first time the hub reads the Keychain, macOS asks whether
`mcp-hub` may access it: choose *Always Allow*. Each rebuild is a new binary to
macOS, so it asks again once after every `install-service.sh`.

## 2. Connect your agents

The hub listens at `http://127.0.0.1:3050/mcp` and wants its token on every
request (`Authorization: Bearer <token>`). So that the token does not have to
sit in each agent's config file, `mcp-hub` hands it over itself, read from the
keyring:

- `mcp-hub headers` prints the headers as JSON, for agents that can run a
  command to get them (Claude Code).
- `mcp-hub connect` is a small stdio MCP server that forwards everything to the
  running hub, for the others (Codex, Cursor, Claude Desktop). It also tells the
  agent when the hub's tools change (`notifications/tools/list_changed`), as when
  a slow server comes up after the agent first asked.

Register the hub once per agent; servers you add to the hub later show up
without touching the agents again. The commands below write the full path of
`mcp-hub` into the agent's config, since agents are not always started with your
shell's `PATH`.

### Claude Code

    claude mcp add-json --scope user hub \
      "{\"type\":\"http\",\"url\":\"http://127.0.0.1:3050/mcp\",\"headersHelper\":\"$(command -v mcp-hub) headers\"}"

`--scope user` makes it available in every project (stored in `~/.claude.json`).
Check with `claude mcp list` or `/mcp` inside a session. Tools are named
`mcp__hub__<server>__<tool>`.

### Codex

    codex mcp add hub -- "$(command -v mcp-hub)" connect

which writes this to `~/.codex/config.toml`:

    [mcp_servers.hub]
    command = "/home/you/.local/bin/mcp-hub"
    args = ["connect"]

Check with `codex mcp list`.

### Cursor

Add to `~/.cursor/mcp.json` (global, all projects); create the file if missing:

    {
      "mcpServers": {
        "hub": {
          "command": "/home/you/.local/bin/mcp-hub",
          "args": ["connect"]
        }
      }
    }

If the file already has other servers, add only the `"hub"` entry inside
`mcpServers`. Then open Cursor Settings → MCP (Tools & Integrations) and check
that `hub` is enabled and lists its tools.

### Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS;
`Settings → Developer → Edit Config` opens it), then restart Claude Desktop:

    {
      "mcpServers": {
        "hub": {
          "command": "/Users/you/.local/bin/mcp-hub",
          "args": ["connect"]
        }
      }
    }

It replaces a `mcp-remote` entry with the token in it (`npx mcp-remote
http://127.0.0.1:3050/mcp --header ...`), if you had one.

### Any other agent

Give it `mcp-hub connect` as a stdio server. If it only speaks HTTP, give it the
URL and the header `Authorization: Bearer <token>`, with the output of
`mcp-hub token` for `<token>`; that copy of the token is then yours to update.

### Rotating the token

    mcp-hub token --rotate

The running hub takes the new token at once and turns the old one down from the
moment the command returns; no restart. Agents set up as above need no change:
`connect` reads the new token by itself, and Claude Code asks `headers` again
when a session starts. Only a config that holds the token itself has to be
edited. (A hub older than 0.5 cannot take a new token while running: the command
then says to restart it.)

### Upgrading from 0.3

Up to 0.3 the agents' configs held the token itself. To move to `connect` and
`headers`:

1. `mcp-hub update`.
2. Set up each agent as above, replacing its old `hub` entry: Claude Code,
   Codex, Cursor, Claude Desktop, and any other that has the hub's URL with an
   `Authorization` header.
3. `mcp-hub token --rotate`, so that the copies of the token left in old configs
   and their backups stop working.

## 3. Add upstream servers

Edit `~/.config/mcp-hub/config.json` and add an entry under `servers`
(name: `a-z`, `0-9`, `-`):

    {
      "port": 3050,
      "loginPort": 3051,
      "servers": {
        "sentry": {
          "url": "https://mcp.sentry.dev/mcp",
          "description": "Sentry error monitoring: issues, events, stack traces, releases",
          "oauth": {}
        },
        "cloudflare": {
          "url": "https://mcp.cloudflare.com/mcp",
          "description": "Cloudflare API: Workers, R2, DNS, zones and docs",
          "bearer": true
        }
      }
    }

Each server is either a `url` (an HTTP server, below) or a `command` (a local
stdio server, see [Local (stdio) servers](#local-stdio-servers)).

- `url`: `https`, or `http` on loopback (`127.0.0.1`, `localhost`) only.
- `description`: agents see it in the hub's MCP instructions and use it to
  decide when to look for that server's tools. Keep it short and concrete.
- Authentication, one of:
  - `"oauth": {}` for servers with OAuth (discovery plus dynamic client
    registration, as most hosted MCP servers do). Then log in:
    `mcp-hub login <name>`, which opens your browser.
  - `"oauth": { "clientId": "http://127.0.0.1:3051/" }` for Home Assistant, which
    has no registration endpoint and uses a fixed IndieAuth client id.
  - `"oauth": { "scope": "..." }` to request specific scopes instead of the
    ones the server advertises.
  - `"bearer": true` for servers that take a static token (GitHub, Cloudflare API
    tokens). Store it with `mcp-hub set-token <name>` (input hidden).
    For GitHub use a fine-grained personal access token limited to the repos and
    permissions you want agents to have.
  - neither, for servers that need no authentication.
- `idleTimeout`: optional, in seconds; close the session with the server after
  that long without a tool call (the next call opens a new one). Off by default:
  an open session costs the hub nothing. See [Local (stdio) servers](#local-stdio-servers),
  where it matters.
- `maxDescription`: optional, in characters, default 2000; `0` for no limit.
  See [Long descriptions](#long-descriptions). Also accepted at the top level of
  the config, next to `port`, as the default for every server.
- `maxResult`: optional, in characters, default 40000; `0` for no limit.
  `compactResults`: optional, `true` to drop empty fields from results. See
  [Large results](#large-results). Both are also accepted at the top level.
- `skipWhenEnv`: optional, the name of an environment variable. An agent started
  with that variable set does not get this server: it is not in its tool list
  or in the instructions, and a call to it is refused with a note. For a server
  whose application also gives its own agents the same tools directly, where
  the hub's copies would be duplicates the agent can pick by mistake. T3 Code
  is one: it sets `T3CODE_THREAD_ID` for the agents it runs, so
  `"skipWhenEnv": "T3CODE_THREAD_ID"` on the `t3code` server keeps its tools
  for agents started anywhere else. It is `mcp-hub connect` and
  `mcp-hub headers` that look at the variable, so this needs the agent set up
  as in step 2; Codex passes a stdio server only the variables listed in its
  entry, so add `env_vars = ["T3CODE_THREAD_ID"]` to `[mcp_servers.hub]`.

Then restart the hub and check it:

    mcp-hub status                     # ok/FAIL and tool count per server

Agents pick up the new tools on their next session (Claude Code: `/mcp` →
reconnect; Cursor: toggle the server in settings).

The hub keeps each server's tool list in memory and answers agents from that
copy, so an agent connecting does not wait for the upstreams. A copy older than
5 minutes is still served, and refreshed in the background for the next agent:
a tool an upstream just added can take one more reconnect to show up. The copy
is also refreshed when a server announces that its tools changed
(`notifications/tools/list_changed`) and whenever a local server starts. Only
right after the hub starts are the upstreams asked directly, each for at most
5 seconds; a slower one is left out of that first list and appears in the next.

### Long descriptions

An agent that loads a tool takes in its whole description and those of its
parameters, each time. Most are a few hundred characters; some servers put a
manual in one (a single tool of 40,000 characters costs about 10,000 tokens per
load). The hub cuts any description longer than `maxDescription` characters
where a paragraph or line ends, and appends a note saying how much is missing
and how to get it:

    [mcp-hub cut the rest, 37512 more characters. To read all of it, call
    hub__describe with {"tool": "posthog__exec"}.]

`hub__describe` is a tool of the hub's own, listed whenever something was cut.
It returns the cut descriptions of that tool in full, so nothing is lost: an
agent that needs the detail asks for it once, and the others never carry it.
Every tool stays listed under its name, and what a tool does comes first in its
description, so finding the right tool works as before. `mcp-hub stats` says how
many tools were cut and what that saves; set `"maxDescription": 0` on a server
whose tools work worse for it.

### Large results

A tool result goes into the agent's context whole. The hub can trim that:

- **`maxResult`** (default 40000 characters, about 10,000 tokens): a larger
  result is written to a file, and the agent gets, instead of it, the file's
  path, the result's size and its first 2000 characters. It reads what it needs
  from the file (JSON is written indented, so `jq`, `grep` or a range of lines
  work). The files are in `~/.local/state/mcp-hub/results/`, readable by you
  alone, and deleted after an hour. Agents have such limits of their own, but
  higher ones: Claude Code does the same past about 25,000 tokens.
- **`compactResults`** (off by default): drops fields that say nothing (`null`,
  `""`, `[]`, `{}`) from JSON results. Some servers return every field of every
  record, set or not; for those it saves up to a third. A server whose agents
  need to tell "empty" from "missing" should leave it off.

Both apply to the text of a result and to its structured copy
(`structuredContent`), which some servers send as well: agents differ in which
of the two they show their model (Claude Code the structured one, Codex both).
`mcp-hub stats` counts the results kept in files and what compacting saved.

### Local (stdio) servers

A server with a `command` instead of a `url` is a program the hub starts itself
and talks to over stdin/stdout, as an agent would. The same entry you would give
an agent works, with a `description` added:

    "profilery": {
      "command": "/Users/me/.vite-plus/js_runtime/node/24.14.0/bin/node",
      "args": ["--import", "file:///Users/me/Code/profilery/node_modules/tsx/dist/loader.mjs", "src/mcp/main.ts"],
      "cwd": "/Users/me/Code/profilery",
      "env": { "LOG_LEVEL": "warn" },
      "description": "My profile: projects, skills, certifications, trainings, languages"
    }

- `command`: an absolute path, or a program name looked up in `PATH`.
  (`"type": "stdio"`, as agents' configs often have, is accepted and ignored.)
- `args`, `env`, `cwd` (absolute): optional. `env` is added to the hub's own
  environment.
- `secretEnv`: optional list of environment variables whose values are kept in
  the keyring instead of the config, e.g. `"secretEnv": ["GITHUB_TOKEN"]`.
  Store each with `mcp-hub set-token <name> <VAR>` (input hidden).
- `idleTimeout`: optional, in seconds; how long the server may go without a tool
  call before the hub stops it. 600 (10 minutes) by default, `0` to never stop it.
- `maxMemoryMb`: optional; stop the server when it uses more memory than this,
  e.g. `"maxMemoryMb": 1024`. No limit by default.

**`PATH` is minimal.** The service does not load your shell profile, so it
typically sees only system directories such as `/usr/bin` and `/bin`: Homebrew,
nvm, `npx`, `uvx` and the like are not found. Give `command` as an absolute path
and, for launchers that start other programs (`npx` needs `node`), set
`"env": { "PATH": "/opt/homebrew/bin:/usr/bin:/bin" }`, which is used for the
lookup too.

How it runs:

- Started on first use, shared by every agent, and restarted on the next request
  if it exits. It runs in its own process group, so stopping it also stops
  anything it started.
- Stopped when no agent has called one of its tools for `idleTimeout`, and
  started again by the next call, which then also waits for the server to start.
  Agents still see its tools meanwhile, from the hub's copy of the list, and
  listing them does not start it. Only the first list after the hub itself
  started does, to learn the tools.
- A tool call may take 5 minutes. When one runs out of time the hub pings the
  server, and if it does not answer within 5 seconds stops it, so that the next
  call gets a new process instead of one that is stuck.
- On Linux the systemd unit caps the memory of the hub and all its local
  servers together (`MemoryHigh=2G`, `MemoryMax=3G`): past the first they are
  slowed down, at the second the kernel kills the largest process, normally the
  server that leaks, and the hub starts it again on its next call. To change the
  numbers: `systemctl --user edit mcp-hub`, then for example
  `[Service]` / `MemoryMax=6G`.
- With `maxMemoryMb`, on Linux and macOS, the hub measures every 15 seconds the
  memory of the server and of everything it started (resident memory, as `ps`
  reports it) and stops it when that is above the limit twice in a row. A call
  in progress then fails; the next one starts the server again. This is for a
  server that grows slowly: one that takes gigabytes within seconds is only
  stopped in time by the unit's cap.
- It runs in `cwd` (else the hub's directory), not in the agent's project, and
  agents' MCP roots are not forwarded. Servers meant to work on "the current
  repo" (filesystem, git) are better configured in each agent directly.
- The hub's log says when it starts (with its pid) and when and why it stops.
  Its stderr goes there too, one line per message prefixed with the
  server name. If it fails to start, `mcp-hub status` shows the error line it
  printed last.
- `oauth` and `bearer` do not apply; servers that need a login of their own
  usually keep it in their own files.

## Day to day

    mcp-hub status             # can every upstream be reached?
    mcp-hub stats              # what the hub did lately (see "Measurements")
    mcp-hub login <server>     # OAuth login in the browser (also to re-login)
    mcp-hub logout <server>    # delete stored credentials for a server
    mcp-hub set-token <server> # static token for a "bearer" server
    mcp-hub set-token <server> <VAR>  # a "secretEnv" value for a local server
    mcp-hub token [--rotate]   # print (or replace) the hub token
    mcp-hub check [--wait N]   # does the hub answer? (exit status 0 if so)
    mcp-hub connect            # what agents run: MCP on stdin/stdout, forwarded to the hub
    mcp-hub headers            # what agents run: the HTTP headers for the hub, as JSON
    mcp-hub update [--check]   # update to the latest release (one-line installs)
    mcp-hub update <version>   # install a specific version, e.g. to go back

Restart after editing the config:

    systemctl --user restart mcp-hub                     # Linux
    launchctl kickstart -k gui/$(id -u)/dev.mcp-hub      # macOS

Stopping takes a few seconds at most: calls still running get 3 seconds to
finish, and local servers that do not exit within 2 more are killed. After a
restart the hub lists every server's tools at once, from the lists it saved
before (`~/.local/state/mcp-hub/tools/`), and fetches them again as each server
starts.

Update: `mcp-hub update` for one-line installs (it runs that release's
`install.sh`: checksum check, binary swap, service restart; config, logins and
token stay). For a clone: `git pull && bin/install-service.sh`. On macOS the
Keychain may ask once more after an update: choose *Always Allow*.

Logs (`RUST_LOG=debug` in the service environment for more detail; local
servers' stderr is logged at `info` under the target `mcp_hub::stderr`, so
`RUST_LOG=info,mcp_hub::stderr=warn` silences it):

    journalctl --user -u mcp-hub -f                     # Linux
    tail -f ~/Library/Logs/mcp-hub.log                   # macOS

Upstream logins (`login`, `set-token`) and `token --rotate` are picked up
without a restart (a local server that is already running keeps its environment
until it restarts). Config changes need one.

### Measurements

The hub keeps a record of what it does, to see which tools are used, what
fails and what costs agents context:

    mcp-hub stats              # the last 7 days
    mcp-hub stats --days 30    # another period
    mcp-hub stats --all        # also name the tools nobody called
    mcp-hub stats --json       # the same numbers, for a script or an agent

It shows, per tool, the calls, failures, durations and result sizes; the same
per agent (an agent that was not given a server shows as "without <server>");
how often each server connected, stopped and why, and failed; and, asked of the
running hub, how large the tool definitions are and which tools were never
called.

The record is a file on this machine,
`~/.local/state/mcp-hub/events.jsonl` (`$XDG_STATE_HOME` is honoured), readable
by you alone: one line of JSON per tool call, tool list, server start, stop and
failure. It holds names, sizes, durations and outcomes, and the hub's own error
messages; never the arguments of a call or what it returned. Nothing is sent
anywhere. It is kept to about 8 MB (some 50,000 events), the oldest half
dropped when full; delete the directory to start over.

### Troubleshooting

- **Agent shows no hub tools**: `mcp-hub check`. No answer means the service is
  not running: check the logs. `401` from the agent means its token is stale: if
  the agent's config holds the token itself, copy the current one from
  `mcp-hub token`, or better, set it up with `connect` or `headers`.
- **Some servers' tools missing right after a restart**: agents set up with
  `connect` are told when they arrive. Claude Code set up with `headers` keeps
  the list it got when its session started: `/mcp` → hub → reconnect.
- **`mcp-hub update` or `install-service.sh` says the hub is not running**: the
  new binary is in place; run the command it prints, then `mcp-hub check`. On
  macOS, look first for a Keychain dialog: the hub waits for it.
- **One server missing**: `mcp-hub status`. A `FAIL` saying to log in means its
  login is missing or expired: run `login` (or `set-token`) again.
- **Local server FAIL "cannot start"**: the `command` path is wrong, or a bare
  name is not in the service's `PATH` (see [Local (stdio) servers](#local-stdio-servers)).
  "connection closed: initialize response" means it started but exited: the
  `stderr:` part of the message usually says why.
- **Local server works in a terminal but not in the hub (Linux)**: the systemd
  unit restricts sockets to IPv4, IPv6 and Unix ones (`RestrictAddressFamilies`),
  and the server inherits that.
- **Error ends with "next attempt in 40s"**: that server failed (unreachable,
  rate limited, did not start) and the hub leaves it alone for a while instead
  of trying on every request: 5 seconds after the first failure, doubling with
  each further one up to 5 minutes. Restart the hub to try at once. A missing or
  expired login is never delayed this way.
- **Log says it is waiting for the keyring**: unlock the keyring (log in to the
  desktop session); the hub retries every 10 seconds.
- **Port in use**: change `port` (and the URL in every agent) or `loginPort` in
  the config.

## Files

- Config: `~/.config/mcp-hub/config.json` (example: `config.example.json`)
- Binary: `~/.local/bin/mcp-hub` (installer) or `~/.cargo/bin/mcp-hub` (from source)
- Record for `mcp-hub stats`: `~/.local/state/mcp-hub/events.jsonl`
- Results too large to pass on whole: `~/.local/state/mcp-hub/results/` (kept an hour)
- Each server's last tool list, for a restarted hub: `~/.local/state/mcp-hub/tools/`
- Service, Linux: `~/.config/systemd/user/mcp-hub.service`, generated from `systemd/`
- Service, macOS: `~/Library/LaunchAgents/dev.mcp-hub.plist`, generated from `launchd/`
- Code: `src/http.rs` (inbound checks), `src/hub.rs` (tool merging and routing),
  `src/connect.rs` (what agents run to reach the hub),
  `src/upstream.rs` (HTTP and stdio upstreams), `src/oauth.rs`, `src/login.rs`,
  `src/secrets.rs` (keyring), `src/stats.rs` (the record and `mcp-hub stats`),
  `src/results.rs` (`maxResult`, `compactResults`)

## Security model

- Listens on `127.0.0.1` only; the bind address is hardcoded.
- Every request must come from loopback, have the exact `Host`
  (`127.0.0.1:3050` or `localhost:3050`), no `Origin` header (so browser pages
  are refused), and the bearer token (compared in constant time).
- Besides `/mcp`, the hub answers one path, `/reload-token`, used by
  `mcp-hub token --rotate`: with the current token, it makes the hub read the
  token from the keyring again. Nothing else.
- Upstream OAuth credentials, static tokens and the hub token live in the OS
  keyring: on Linux the Secret Service (schema `dev.mcp-hub.Secret`, attribute
  `key`), on macOS the login Keychain (generic passwords, service `mcp-hub`).
  Nothing secret is written to disk by mcp-hub, and nothing is synced between
  machines.
- The record for `mcp-hub stats` says which tools were called, when and by which
  agent, not with what or what came back; it stays on this machine, in a
  directory only you can read.
- A result larger than `maxResult` is written to a file in that same directory
  for the agent to read, and deleted after an hour.
- The hub token only grants access to this localhost endpoint. Agents set up
  with `mcp-hub connect` or `mcp-hub headers` get it from the keyring each time,
  so it is in none of their config files, where a search or a pasted config
  would show it.
- Local servers run as you, with the hub's environment plus their `env`: anyone
  who can edit the config can run commands as you (as with any agent's MCP
  config). Put tokens in `secretEnv` rather than `env`, which is plain text in
  the config file.
- The keyring is readable by any process running as you while you are logged in
  (on macOS, other apps get a prompt). It protects against disk theft and stray
  files, not against your own user.

## Development

    cargo test                 # unit tests + end-to-end tests with fake HTTP and stdio upstreams
    cargo clippy --all-targets

To try a change to the service setup next to the hub you use, give the test
instance its own name, binary, config (with another `port`) and state:

    MCP_HUB_SERVICE=mcp-hub-test MCP_HUB_BIN=/tmp/hub-test/mcp-hub \
      MCP_HUB_CONFIG=/tmp/hub-test/config.json MCP_HUB_STATE=/tmp/hub-test/state \
      bin/install-service.sh

(on macOS name it `dev.mcp-hub-test`). It shares the keyring, so do not rotate
the token from it. Remove it afterwards: `systemctl --user disable --now
mcp-hub-test` and its unit file, or `launchctl bootout gui/$(id -u)/dev.mcp-hub-test`
and its plist.

### Releasing

1. Bump `version` in `Cargo.toml` (and run `cargo build` so `Cargo.lock` follows), commit, push.
2. GitHub → Actions → **Release** → *Run workflow* (or `gh workflow run release.yml -R 413n/mcp-hub`).

The workflow (`.github/workflows/release.yml`) creates a draft release, then on
Linux and macOS in parallel runs the tests, builds the `linux-x86_64` and
`macos-universal` archives (with SHA-256 checksums) and uploads them, and
finally publishes `v<version>`. It refuses to overwrite a published release; a
draft left by a failed run is replaced on the next run.
