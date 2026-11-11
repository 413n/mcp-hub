#!/usr/bin/env bash
# Install mcp-hub and run it as a per-user background service:
#   Linux: systemd user unit  ~/.config/systemd/user/mcp-hub.service
#   macOS: launchd agent      ~/Library/LaunchAgents/dev.mcp-hub.plist
#
# Works from either:
#   - an unpacked release archive (prebuilt binary next to this script):
#     installs the binary to ~/.local/bin, no Rust needed;
#   - a clone of the repo (this script in bin/): builds with `cargo install`.
# Safe to re-run to update; it restarts the service and checks that the hub answers.
#
# To try it out next to the hub you use, without touching that one:
#   MCP_HUB_SERVICE  name of the service (default: mcp-hub on Linux, dev.mcp-hub on macOS)
#   MCP_HUB_BIN      where to install the binary (from a clone: built, then copied there)
#   MCP_HUB_CONFIG, MCP_HUB_STATE
#                    passed on to the service (use a config with another port)
# On macOS, MCP_HUB_SIGN names a certificate of your own to sign the binary with (default:
# mcp-hub; see "Signing on macOS" in the README). Without one the binary keeps its signature:
# release binaries are signed, builds from a clone are not.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OS="$(uname -s)"
SIGN_AS="${MCP_HUB_SIGN:-mcp-hub}"

# macOS: sign binary $1 with your own certificate, if there is one, so that the Keychain
# knows each new version as the same program and does not ask again.
sign() {
  [[ "$OS" == Darwin ]] || return 0
  # Without -v: a self-signed certificate is not "valid" (trusted), and need not be.
  security find-identity -p codesigning 2>/dev/null | grep -qF "\"$SIGN_AS\"" || return 0
  codesign --force --sign "$SIGN_AS" --identifier dev.mcp-hub "$1" ||
    echo "Warning: could not sign $1 with the certificate \"$SIGN_AS\"." >&2
}

# Whether $BIN is signed with a certificate (not only ad hoc, as builds are by default).
signed() { codesign --display --verbose=2 "$BIN" 2>&1 | grep -q '^Authority='; }

# Put binary $1 in place as $BIN: copy to a temp name, sign, and rename, so a running
# service keeps its old binary until restarted.
place() {
  mkdir -p "$(dirname "$BIN")"
  install -m 0755 "$1" "$BIN.new"
  if [[ "$OS" == Darwin ]]; then
    # Archives downloaded with a browser are quarantined, which blocks unsigned binaries.
    xattr -d com.apple.quarantine "$BIN.new" 2>/dev/null || true
  fi
  sign "$BIN.new"
  mv -f "$BIN.new" "$BIN"
}

if [[ -f "$HERE/mcp-hub" && -f "$HERE/systemd/mcp-hub.service.in" ]]; then
  ROOT="$HERE"
  BIN="${MCP_HUB_BIN:-$HOME/.local/bin/mcp-hub}"
  place "$HERE/mcp-hub"
  if [[ -z "${MCP_HUB_BIN:-}" ]]; then
    case ":$PATH:" in
      *":$(dirname "$BIN"):"*) ;;
      *) echo "Note: add $(dirname "$BIN") to your PATH to run 'mcp-hub' directly." ;;
    esac
  fi
else
  ROOT="$(cd "$HERE/.." && pwd)"
  command -v cargo >/dev/null || {
    echo "cargo not found: install Rust (https://rustup.rs) or use a release archive" >&2
    exit 1
  }
  if [[ -n "${MCP_HUB_BIN:-}" ]]; then
    cargo build --manifest-path "$ROOT/Cargo.toml" --release --locked --quiet
    BIN="$MCP_HUB_BIN"
    place "$ROOT/target/release/mcp-hub"
  else
    cargo install --path "$ROOT" --locked --quiet
    BIN="${CARGO_HOME:-$HOME/.cargo}/bin/mcp-hub"
    # Sign a copy rather than the file a running hub may have been started from.
    if [[ "$OS" == Darwin ]]; then place "$BIN"; fi
  fi
fi
[[ -x "$BIN" ]] || { echo "expected the binary at $BIN" >&2; exit 1; }
"$BIN" --version >/dev/null || { echo "$BIN does not run on this machine" >&2; exit 1; }
# The service waits for the hub token: create it if missing. On macOS this is also where the
# Keychain asks whether the new binary may read it.
if [[ "$OS" == Darwin ]]; then
  echo "If macOS asks for your password to let mcp-hub use the Keychain: choose Always Allow."
  if ! signed; then
    echo "(It asks after every update. To stop that, see \"Signing on macOS\" in the README.)"
  fi
fi
"$BIN" token >/dev/null

xml() { printf '%s' "$1" | sed -e 's/&/\&amp;/g; s/</\&lt;/g; s/>/\&gt;/g'; }
# XML-escape (for the plist), then escape for the sed replacement.
esc() { xml "$1" | sed -e 's/[&|\\]/\\&/g'; }

# The variables to pass on to the service, as NAME=value lines.
service_env() {
  local var
  for var in MCP_HUB_CONFIG MCP_HUB_STATE; do
    if [[ -n "${!var:-}" ]]; then printf '%s=%s\n' "$var" "${!var}"; fi
  done
}

# Render template $1: fill in @BIN@, @LABEL@ and @LOG@, and put file $2 in place of the
# line @ENV@ (awk: BSD sed cannot insert lines portably).
render() {
  sed -e "s|@BIN@|$(esc "$BIN")|g" -e "s|@LABEL@|$(esc "${LABEL:-}")|g" \
    -e "s|@LOG@|$(esc "${LOG:-}")|g" "$1" |
    awk -v env="$2" '$0 == "@ENV@" { while ((getline line < env) > 0) print line; next } { print }'
}
ENV_FILE="$(mktemp "${TMPDIR:-/tmp}/mcp-hub-env.XXXXXX")"
NEW="$(mktemp "${TMPDIR:-/tmp}/mcp-hub-service.XXXXXX")"
trap 'rm -f "$ENV_FILE" "$NEW"' EXIT

# Say how to start the hub by hand, and fail.
not_running() {
  echo "" >&2
  echo "mcp-hub: $BIN is installed, but the hub is not running." >&2
  if [[ -n "${1:-}" ]]; then echo "$1" >&2; fi
  echo "Start it with:" >&2
  echo "  $START" >&2
  echo "Logs: $LOGS" >&2
  exit 1
}

case "$OS" in
  Linux)
    NAME="${MCP_HUB_SERVICE:-mcp-hub}"
    UNIT="$HOME/.config/systemd/user/$NAME.service"
    START="systemctl --user restart $NAME"
    LOGS="journalctl --user -u $NAME -f"
    mkdir -p "$(dirname "$UNIT")"
    rm -f "$UNIT" # may be a symlink from an older install
    service_env | while IFS= read -r line; do printf 'Environment="%s"\n' "$line"; done > "$ENV_FILE"
    render "$ROOT/systemd/mcp-hub.service.in" "$ENV_FILE" > "$UNIT"
    systemctl --user daemon-reload
    systemctl --user enable "$NAME"
    # Waits for the old process to exit (TimeoutStopSec) before it starts the new one.
    systemctl --user restart "$NAME" || not_running
    echo "Installed $UNIT (binary: $BIN)"
    ;;
  Darwin)
    LABEL="${MCP_HUB_SERVICE:-dev.mcp-hub}"
    PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
    LOG="$HOME/Library/Logs/${LABEL#dev.}.log"
    DOMAIN="gui/$(id -u)"
    START="launchctl bootstrap $DOMAIN $PLIST || launchctl kickstart -k $DOMAIN/$LABEL"
    LOGS="tail -f $LOG"
    mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
    if [[ -n "$(service_env)" ]]; then
      {
        echo "  <key>EnvironmentVariables</key>"
        echo "  <dict>"
        service_env | while IFS='=' read -r name value; do
          echo "    <key>$name</key>"
          echo "    <string>$(xml "$value")</string>"
        done
        echo "  </dict>"
      } > "$ENV_FILE"
    fi
    render "$ROOT/launchd/dev.mcp-hub.plist.in" "$ENV_FILE" > "$NEW"
    plutil -lint "$NEW" >/dev/null

    loaded() { launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; }
    pid_of() { launchctl print "$DOMAIN/$LABEL" 2>/dev/null | awk '$1 == "pid" && $2 == "=" { print $3; exit }'; }

    if loaded && [[ -f "$PLIST" ]] && cmp -s "$NEW" "$PLIST" &&
      launchctl kickstart -k "$DOMAIN/$LABEL"; then
      # Same plist: stop the running hub and start the (new) binary in one step.
      :
    else
      # `bootout` returns before the hub has exited, and `bootstrap` fails with
      # "Input/output error" (5) until it has: wait for it to be gone.
      if loaded; then
        pid="$(pid_of)"
        launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
        for _ in $(seq 60); do
          if ! loaded && { [[ -z "$pid" ]] || ! kill -0 "$pid" 2>/dev/null; }; then break; fi
          sleep 1
        done
        loaded && not_running "launchd still has the old $LABEL after a minute."
      fi
      mv -f "$NEW" "$PLIST"
      tries=0
      until launchctl bootstrap "$DOMAIN" "$PLIST"; do
        tries=$((tries + 1))
        # Loaded after all, as when launchd took the plist itself: start it.
        if loaded && launchctl kickstart -k "$DOMAIN/$LABEL"; then break; fi
        ((tries < 5)) || not_running "launchctl bootstrap kept failing."
        sleep "$tries"
      done
    fi
    echo "Installed $PLIST (binary: $BIN)"
    ;;
  *)
    echo "unsupported OS: $OS" >&2
    exit 1
    ;;
esac

# On macOS the hub may be waiting for a Keychain dialog (a new binary has to be allowed again).
if ! why="$("$BIN" check --wait 30)"; then
  not_running "It did not answer within 30s. $why"
fi
"$BIN" check
echo "Logs: $LOGS"
