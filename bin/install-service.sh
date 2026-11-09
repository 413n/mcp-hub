#!/usr/bin/env bash
# Install mcp-hub and run it as a per-user background service:
#   Linux: systemd user unit  ~/.config/systemd/user/mcp-hub.service
#   macOS: launchd agent      ~/Library/LaunchAgents/dev.mcp-hub.plist
#
# Works from either:
#   - an unpacked release archive (prebuilt binary next to this script):
#     installs the binary to ~/.local/bin, no Rust needed;
#   - a clone of the repo (this script in bin/): builds with `cargo install`.
# Safe to re-run to update; it restarts the service.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ -f "$HERE/mcp-hub" && -f "$HERE/systemd/mcp-hub.service.in" ]]; then
  ROOT="$HERE"
  BIN="$HOME/.local/bin/mcp-hub"
  mkdir -p "$(dirname "$BIN")"
  # Copy to a temp name and rename, so a running service keeps its old binary until restarted.
  install -m 0755 "$HERE/mcp-hub" "$BIN.new"
  if [[ "$(uname -s)" == Darwin ]]; then
    # Archives downloaded with a browser are quarantined, which blocks unsigned binaries.
    xattr -d com.apple.quarantine "$BIN.new" 2>/dev/null || true
  fi
  mv -f "$BIN.new" "$BIN"
  case ":$PATH:" in
    *":$HOME/.local/bin:"*) ;;
    *) echo "Note: add ~/.local/bin to your PATH to run 'mcp-hub' directly." ;;
  esac
else
  ROOT="$(cd "$HERE/.." && pwd)"
  command -v cargo >/dev/null || {
    echo "cargo not found: install Rust (https://rustup.rs) or use a release archive" >&2
    exit 1
  }
  cargo install --path "$ROOT" --locked --quiet
  BIN="${CARGO_HOME:-$HOME/.cargo}/bin/mcp-hub"
fi
[[ -x "$BIN" ]] || { echo "expected the binary at $BIN" >&2; exit 1; }
"$BIN" --version >/dev/null || { echo "$BIN does not run on this machine" >&2; exit 1; }

# XML-escape (for the plist), then escape for the sed replacement.
esc() { printf '%s' "$1" | sed -e 's/&/\&amp;/g; s/</\&lt;/g; s/>/\&gt;/g' -e 's/[&|\\]/\\&/g'; }

case "$(uname -s)" in
  Linux)
    UNIT="$HOME/.config/systemd/user/mcp-hub.service"
    mkdir -p "$(dirname "$UNIT")"
    rm -f "$UNIT" # may be a symlink from an older install
    sed -e "s|@BIN@|$(esc "$BIN")|g" "$ROOT/systemd/mcp-hub.service.in" > "$UNIT"
    systemctl --user daemon-reload
    systemctl --user enable mcp-hub
    systemctl --user restart mcp-hub
    echo "Installed $UNIT (binary: $BIN)"
    echo "Logs: journalctl --user -u mcp-hub -f"
    ;;
  Darwin)
    LABEL="dev.mcp-hub"
    PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
    mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
    sed -e "s|@BIN@|$(esc "$BIN")|g" -e "s|@HOME@|$(esc "$HOME")|g" \
      "$ROOT/launchd/$LABEL.plist.in" > "$PLIST"
    plutil -lint "$PLIST" >/dev/null
    launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$PLIST"
    echo "Installed $PLIST (binary: $BIN)"
    echo "Logs: tail -f ~/Library/Logs/mcp-hub.log"
    ;;
  *)
    echo "unsupported OS: $(uname -s)" >&2
    exit 1
    ;;
esac
