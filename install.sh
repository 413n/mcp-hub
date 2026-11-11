#!/bin/sh
# One-line installer for mcp-hub (Linux x86_64, macOS):
#
#   curl -fsSL https://raw.githubusercontent.com/narcisoaf/mcp-hub/main/install.sh | sh
#
# Downloads the latest release (or $MCP_HUB_VERSION, e.g. "0.1.0"), verifies its
# SHA-256 checksum, installs the binary to ~/.local/bin and starts the hub as a
# per-user service (systemd on Linux, launchd on macOS). Update later with
# `mcp-hub update` (or run this again).
# Your config and keyring entries are never touched.
set -eu

REPO="narcisoaf/mcp-hub"
RELEASES="${MCP_HUB_RELEASES:-https://github.com/$REPO/releases}"

say() { printf '%s\n' "$*"; }
fail() { printf 'mcp-hub install: %s\n' "$*" >&2; exit 1; }

# Color on a terminal only (and not with NO_COLOR).
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != dumb ]; then
  esc="$(printf '\033')"
  bold="${esc}[1m" dim="${esc}[2m" green="${esc}[32m" cyan="${esc}[36m" reset="${esc}[0m"
else
  bold="" dim="" green="" cyan="" reset=""
fi

for cmd in curl tar bash; do
  command -v "$cmd" >/dev/null 2>&1 || fail "$cmd is required"
done

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64 | Linux/amd64) platform=linux-x86_64 ;;
  Darwin/*) platform=macos-universal ;;
  *) fail "no prebuilt binary for $(uname -s)/$(uname -m); build from source: https://github.com/$REPO#from-source" ;;
esac

version="${MCP_HUB_VERSION:-}"
if [ -z "$version" ]; then
  # .../releases/latest redirects to .../releases/tag/v<version>
  latest="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$RELEASES/latest")" ||
    fail "cannot reach $RELEASES"
  version="${latest##*/v}"
  case "$version" in
    "" | */*) fail "could not determine the latest release" ;;
  esac
fi
version="${version#v}"

name="mcp-hub-$version-$platform"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "${dim}Downloading mcp-hub $version ($platform)...${reset}"
curl -fsSL -o "$tmp/$name.tar.gz" "$RELEASES/download/v$version/$name.tar.gz" ||
  fail "download failed: $RELEASES/download/v$version/$name.tar.gz"
curl -fsSL -o "$tmp/$name.tar.gz.sha256" "$RELEASES/download/v$version/$name.tar.gz.sha256" ||
  fail "checksum download failed"

expected="$(cut -d ' ' -f 1 "$tmp/$name.tar.gz.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$name.tar.gz" | cut -d ' ' -f 1)"
else
  actual="$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d ' ' -f 1)"
fi
[ -n "$expected" ] && [ "$expected" = "$actual" ] || fail "checksum mismatch, refusing to install"

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"

bin="$HOME/.local/bin/mcp-hub"
# A first install gets the next steps; an update does not need them again.
first=yes
[ -x "$bin" ] && first=""

config="$HOME/.config/mcp-hub/config.json"
if [ ! -f "$config" ]; then
  mkdir -p "$(dirname "$config")"
  cp "$tmp/$name/config.example.json" "$config"
  chmod 600 "$config"
  say "${dim}Created $config (no servers yet).${reset}"
fi

# Installs the binary, creates the hub token if missing, (re)starts the service and checks
# that the hub answers; if it does not, it says how to start it.
bash "$tmp/$name/install-service.sh"

say ""
say "${green}✓${reset} ${bold}mcp-hub $version is installed.${reset}"
[ -n "$first" ] || exit 0
say ""
say "${bold}Next steps${reset} ${dim}(https://github.com/$REPO#2-connect-your-agents)${reset}"
say "  1. Connect an agent, e.g. Claude Code:"
say "       ${cyan}claude mcp add --scope user hub -- $bin connect${reset}"
say "  2. Add upstream servers to $config, then:"
say "       ${cyan}$bin login <server>${reset}   or   ${cyan}$bin set-token <server>${reset}"
