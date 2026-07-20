#!/usr/bin/env bash
set -euo pipefail

# Run Claude Code in dangerous mode, jailed to the current project.

REAL_CLAUDE="$HOME/.claude"
# Sandbox copies live under the XDG cache dir, not $HOME, so they don't clutter it.
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/vhrn"
SANDBOX="$CACHE/sandbox"
SANDBOX_JSON="$CACHE/sandbox.json"

usage() {
  cat <<'USAGE'
vhrn runs Claude Code in a container jailed to the current project, with
default-deny network egress.

Usage:
  vhrn [flags] [claude args...]    run claude inside the box
  vhrn net <subcommand>            manage the egress policy
  vhrn help                        show this help

Flags (must come before claude's own flags):
  --open-net               drop the egress guard for this run (all egress)
  --allow <domain>...      add allowlist domains (comma-separated or repeated)
  --                       stop reading flags; forward the rest to claude

Anything not matched above is forwarded to claude untouched. Use `--` to pass a
flag the wrapper would otherwise read:
  vhrn --model opus
  vhrn -- --help     # claude's own help, not this one

net subcommands:
  net status               current mode and allowlist size
  net allow <domain>...    add domains to the allowlist (effective now)
  net denied               domains blocked this session
  net open                 drop the guard (allow everything)
  net guard                re-enable enforcement
  net report               allow everything, but log what would be denied

Environment:
  VHRN_ENGINE        container engine (default: container, then docker)
  VHRN_IMAGE         box image name (default: vhrn-sandbox)
  VHRN_PROXY_IMAGE   proxy image name (default: vhrn-proxy)
  VHRN_PROXY_PORT    proxy port (default: 8080)
USAGE
}

# Answer help only when it leads the args, so `vhrn -- --help` and a
# trailing --help still reach claude's own help.
case "${1:-}" in
  help|-h|--help) usage; exit 0 ;;
esac

# `vhrn net ...` mutates the host-side egress policy that running boxes
# read, then exits. This is the only way to change the policy — the box itself
# has no path to it — so an in-box process can at most ask the user to run this.
if [ "${1:-}" = net ]; then
  shift
  NET_STATE="$CACHE/net"; ALLOWLIST="$NET_STATE/allowlist"
  MODE_FILE="$NET_STATE/mode"; DENY_LOG="$NET_STATE/denied.log"
  mkdir -p "$NET_STATE"
  cmd="${1:-status}"; [ $# -gt 0 ] && shift
  case "$cmd" in
    status)
      mode="enforce"; [ -f "$MODE_FILE" ] && mode="$(cat "$MODE_FILE")"
      n=0; [ -f "$ALLOWLIST" ] && n="$(grep -cvE '^[[:space:]]*(#|$)' "$ALLOWLIST" || true)"
      echo "mode:    $mode"
      echo "allowed: $n domain(s) ($ALLOWLIST)" ;;
    denied)
      if [ -s "$DENY_LOG" ]; then awk '{print $2}' "$DENY_LOG" | sort -u
      else echo "no denials recorded this session"; fi ;;
    allow)
      [ $# -gt 0 ] || { echo "usage: vhrn net allow <domain>..." >&2; exit 2; }
      tmp="$(mktemp "$NET_STATE/allowlist.XXXXXX")"
      [ -f "$ALLOWLIST" ] && cat "$ALLOWLIST" > "$tmp"
      for dom in "$@"; do grep -qxF "$dom" "$tmp" 2>/dev/null || printf '%s\n' "$dom" >> "$tmp"; done
      chmod 666 "$tmp" 2>/dev/null || true
      mv -f "$tmp" "$ALLOWLIST"   # atomic on the same fs; proxy re-reads on next request
      echo "allowed: $*" ;;
    open)   echo open   > "$MODE_FILE"; echo "egress guard OFF (open) — all public hosts allowed" ;;
    guard)  echo enforce > "$MODE_FILE"; echo "egress guard ON (enforce) — allowlist enforced" ;;
    report) echo report > "$MODE_FILE"; echo "egress guard REPORT — all allowed, denials logged" ;;
    *) echo "usage: vhrn net {status|denied|allow <domain>...|open|guard|report}" >&2; exit 2 ;;
  esac
  exit 0
fi

# Consume wrapper-owned flags up front, then forward the rest to claude verbatim.
# These must precede claude's own flags (e.g. `vhrn --open-net -- --model x`).
OPEN_NET=0
EXTRA_ALLOW=()
while [ $# -gt 0 ]; do
  case "$1" in
    --open-net) OPEN_NET=1; shift ;;
    --allow) shift; [ $# -gt 0 ] || { echo "vhrn: --allow needs a domain" >&2; exit 2; }
             IFS=',' read -ra _a <<< "$1"; EXTRA_ALLOW+=("${_a[@]}"); shift ;;
    --allow=*) IFS=',' read -ra _a <<< "${1#--allow=}"; EXTRA_ALLOW+=("${_a[@]}"); shift ;;
    --) shift; break ;;
    *) break ;;
  esac
done

PROJECT="$(pwd -P)"

# Reproduce Claude's projects/<key> encoding so history unifies with native.
KEY="$(printf '%s' "$PROJECT" | sed 's/[^A-Za-z0-9]/-/g')"
HISTORY="$REAL_CLAUDE/projects/$KEY"

# Container engine: explicit override wins, else auto-detect (container first, then
# docker) to match the Makefile so build and run agree on the same engine.
ENGINE="${VHRN_ENGINE:-${ENGINE:-}}"
if [ -z "$ENGINE" ]; then
  if command -v container >/dev/null 2>&1; then ENGINE=container
  elif command -v docker >/dev/null 2>&1; then ENGINE=docker
  else echo "vhrn: no container engine found; install Apple container or Docker" >&2; exit 1
  fi
fi
command -v "$ENGINE" >/dev/null 2>&1 || { echo "vhrn: engine '$ENGINE' not found" >&2; exit 1; }
IMAGE="${VHRN_IMAGE:-vhrn-sandbox}"

mkdir -p "$SANDBOX" "$HISTORY"

# Copy globals in, dereferencing symlinks so symlinked skills come across.
copy_dir() {
  [ -d "$REAL_CLAUDE/$1" ] || return 0
  if command -v rsync >/dev/null 2>&1; then
    rsync -aL --delete "$REAL_CLAUDE/$1/" "$SANDBOX/$1/" \
      || echo "vhrn: warning: some '$1' entries were skipped (broken symlink?)" >&2
  else
    rm -rf "${SANDBOX:?}/$1"
    cp -RL "$REAL_CLAUDE/$1" "$SANDBOX/$1" \
      || echo "vhrn: warning: some '$1' entries were skipped (broken symlink?)" >&2
  fi
}
copy_file() {
  [ -f "$REAL_CLAUDE/$1" ] || return 0
  cp -L "$REAL_CLAUDE/$1" "$SANDBOX/$1" 2>/dev/null \
    || echo "vhrn: warning: could not copy '$1'" >&2
}
for d in skills commands agents; do copy_dir "$d"; done
for f in settings.json statusline.sh; do copy_file "$f"; done

# Copy the config file
if [ -f "$REAL_CLAUDE.json" ]; then
  cp -L "$REAL_CLAUDE.json" "$SANDBOX_JSON" 2>/dev/null \
    || echo "vhrn: warning: could not copy .claude.json" >&2
fi
[ -s "$SANDBOX_JSON" ] || printf '{}\n' > "$SANDBOX_JSON"

# Host git config, dereferenced into the cache so the box inherits the user's identity
# and settings. Disposable copy (re-synced each run) — edit ~/.gitconfig to persist.
GIT_MOUNT=()
if [ -f "$HOME/.gitconfig" ]; then
  cp -L "$HOME/.gitconfig" "$CACHE/gitconfig" 2>/dev/null \
    && GIT_MOUNT=(--volume "$CACHE/gitconfig:/home/dev/.gitconfig") \
    || echo "vhrn: warning: could not copy .gitconfig" >&2
else
  rm -f "$CACHE/gitconfig"
fi

# gh creds: explicit env wins, else ask host gh — with Keychain storage the token
# never exists in a mountable file. GH_ENV expands via the ${a[@]+...} guard
# because empty arrays trip `set -u` on macOS's bash 3.2.
GH_TOKEN="${GH_TOKEN:-${GITHUB_TOKEN:-}}"
if [ -z "$GH_TOKEN" ] && command -v gh >/dev/null 2>&1; then
  GH_TOKEN="$(gh auth token 2>/dev/null || true)"
fi
GH_ENV=()
[ -n "$GH_TOKEN" ] && GH_ENV=(--env "GH_TOKEN=$GH_TOKEN")

# Terminal identity must cross into the box verbatim: claude branches per-terminal
# rendering (welcome-mascot variant, color depth) on TERM_PROGRAM/COLORTERM, so
# stripping or inventing these makes in-box rendering diverge from native.
TERM_ENV=(--env TERM="${TERM:-xterm-256color}")
for v in COLORTERM TERM_PROGRAM TERM_PROGRAM_VERSION; do
  [ -n "${!v:-}" ] && TERM_ENV+=(--env "$v=${!v}")
done

# Apple `container` needs its system service up; Docker manages its own daemon.
if [ "$ENGINE" = container ]; then
  container system start >/dev/null 2>&1 || true
fi

# --- Egress guard: policy state + proxy sidecar -------------------------------
# The proxy enforces a domain allowlist; the box's in-container firewall pins all
# egress to the proxy. Policy files live host-side (mounted into the proxy only),
# so the box can never widen its own egress. See `vhrn net` for mutation.
NET_STATE="$CACHE/net"
ALLOWLIST="$NET_STATE/allowlist"
MODE_FILE="$NET_STATE/mode"
DENY_LOG="$NET_STATE/denied.log"
PROXY_IMAGE="${VHRN_PROXY_IMAGE:-vhrn-proxy}"
PROXY_PORT="${VHRN_PROXY_PORT:-8080}"
NET_MODE=enforce
[ "$OPEN_NET" = 1 ] && NET_MODE=open

mkdir -p "$NET_STATE"; chmod 777 "$NET_STATE" 2>/dev/null || true
# Seed a default allowlist on first run; never clobber later edits.
if [ ! -f "$ALLOWLIST" ]; then
  cat > "$ALLOWLIST" <<'ALLOW'
# vhrn egress allowlist — one domain per line, matching the domain and its
# subdomains. Edit freely, or run `vhrn net allow <domain>` while a box runs.
api.anthropic.com
claude.ai
platform.claude.com
statsig.anthropic.com
sentry.io
github.com
githubusercontent.com
registry.npmjs.org
pypi.org
files.pythonhosted.org
astral.sh
mise.jdx.dev
ALLOW
fi
# Session additions from --allow persist in the allowlist, like `net allow`.
if [ "${#EXTRA_ALLOW[@]}" -gt 0 ]; then
  for dom in "${EXTRA_ALLOW[@]}"; do
    grep -qxF "$dom" "$ALLOWLIST" 2>/dev/null || printf '%s\n' "$dom" >> "$ALLOWLIST"
  done
fi
printf '%s\n' "$NET_MODE" > "$MODE_FILE"
: > "$DENY_LOG" 2>/dev/null || true; chmod 666 "$DENY_LOG" 2>/dev/null || true

# Rebuild the disposable sandbox CLAUDE.md fresh each run (the host's global
# CLAUDE.md, if any, plus a guard-aware section) so it tracks the mode and never
# accumulates across runs.
{
  [ -f "$REAL_CLAUDE/CLAUDE.md" ] && cat "$REAL_CLAUDE/CLAUDE.md"
  cat <<'BOXMD'

# vhrn environment

You are running inside vhrn: a container jailed to this project with a
network egress guard. Adapt as follows:

- **No sudo, no apt.** Install tools in user space: `mise use -g <tool>` for
  runtimes (node, go, python, ...), `uv tool install <pkg>` for Python CLIs, and
  `npm i -g <pkg>` after `mise use -g node` for npm CLIs.
BOXMD
  if [ "$NET_MODE" = open ]; then
    cat <<'BOXMD'
- **Network egress is unrestricted this session** (the guard is off via `--open-net`).
BOXMD
  else
    cat <<'BOXMD'
- **Network egress is allowlisted (default-deny).** A blocked request fails with
  an error naming the domain. You cannot change the allowlist from inside the
  box; tell the user the exact host(s) and ask them to run
  `vhrn net allow <host>` on the host, then retry — no restart is needed.
BOXMD
  fi
} > "$SANDBOX/CLAUDE.md"

# Start the proxy sidecar (detached). The box and proxy share the default
# network; the box's firewall restricts its egress to the proxy alone.
PROXY_NAME="vhrn-proxy-$$"
"$ENGINE" run -d --rm --name "$PROXY_NAME" \
  --volume "$NET_STATE:/etc/vhrn" \
  --env VHRN_ALLOWLIST=/etc/vhrn/allowlist \
  --env VHRN_MODE_FILE=/etc/vhrn/mode \
  --env VHRN_DENY_LOG=/etc/vhrn/denied.log \
  --env "VHRN_PROXY_LISTEN=:$PROXY_PORT" \
  "$PROXY_IMAGE" >/dev/null

# Tear the proxy down when the box exits.
cleanup() { "$ENGINE" stop "$PROXY_NAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM

# Resolve the proxy's IP (engines differ; retry until it has one).
proxy_ip() {
  if [ "$ENGINE" = docker ]; then
    docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$PROXY_NAME" 2>/dev/null
  else
    # Apple container inspect JSON escapes the CIDR slash (192.168.64.73\/24),
    # so pull the dotted quad straight off the first ipv4Address line.
    container inspect "$PROXY_NAME" 2>/dev/null | grep -m1 ipv4Address | grep -oE '([0-9]{1,3}\.){3}[0-9]{1,3}'
  fi
}
PROXY_IP=""
for _ in $(seq 1 30); do PROXY_IP="$(proxy_ip)"; [ -n "$PROXY_IP" ] && break; sleep 0.3; done
[ -n "$PROXY_IP" ] || { echo "vhrn: proxy failed to start (is the '$PROXY_IMAGE' image built?)" >&2; exit 1; }
PROXY_URL="http://$PROXY_IP:$PROXY_PORT"

if [ "$NET_MODE" = open ]; then
  echo "vhrn: network guard OFF (open) — all public egress allowed this session." >&2
  [ -n "$GH_TOKEN" ] && echo "vhrn: a GitHub token is present in the box with the guard off." >&2
fi

# NET_ADMIN lets the entrypoint install the egress firewall (dropped before dev runs).
"$ENGINE" run -it --rm \
  --cap-add CAP_NET_ADMIN \
  --env VHRN_SANDBOX=1 \
  --env "VHRN_NET=$NET_MODE" \
  --env "VHRN_PROXY_IP=$PROXY_IP" \
  --env "VHRN_PROXY_PORT=$PROXY_PORT" \
  --env "HTTP_PROXY=$PROXY_URL" \
  --env "HTTPS_PROXY=$PROXY_URL" \
  --env "http_proxy=$PROXY_URL" \
  --env "https_proxy=$PROXY_URL" \
  --volume "$PROJECT:$PROJECT" \
  --workdir "$PROJECT" \
  --volume "$SANDBOX:/home/dev/.claude" \
  --volume "$SANDBOX_JSON:/home/dev/.claude.json" \
  --volume "$HISTORY:/home/dev/.claude/projects/$KEY" \
  ${GIT_MOUNT[@]+"${GIT_MOUNT[@]}"} \
  "${TERM_ENV[@]}" \
  ${GH_ENV[@]+"${GH_ENV[@]}"} \
  "$IMAGE" \
  claude "$@"
