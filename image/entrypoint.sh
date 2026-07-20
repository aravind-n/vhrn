#!/usr/bin/env bash
set -euo pipefail

# Phase 1 (root): install the default-deny egress firewall, then drop to the
# unprivileged dev user by re-executing this script. dev has no sudo and no
# capabilities, so once we hand off it cannot reach or alter these rules — the
# firewall is enforced from outside dev's reach even though we share the box.
if [ "$(id -u)" = 0 ]; then
  if [ -n "${VHRN_PROXY_IP:-}" ]; then
    port="${VHRN_PROXY_PORT:-8080}"
    # Default-drop egress; permit only loopback, return traffic, and TCP to the
    # proxy. DNS (and everything else) is blocked, so all name resolution and
    # every connection must go through the proxy, which resolves proxy-side.
    if ! nft -f - <<EOF
table inet vhrn {
	chain output {
		type filter hook output priority 0; policy drop;
		oifname "lo" accept
		ct state established,related accept
		ip daddr ${VHRN_PROXY_IP} tcp dport ${port} accept
	}
}
EOF
    then
      echo "[vhrn] FATAL: could not install egress firewall (missing NET_ADMIN?)." >&2
      exit 1
    fi
  fi
  exec setpriv --reuid dev --regid dev --init-groups "$0" "$@"
fi

# Phase 2 (dev): unprivileged from here on.
export HOME=/home/dev

# Remove a stale git index lock in the project (the workdir) left by a crash.
LOCK_FILE="$PWD/.git/index.lock"
if [ -f "$LOCK_FILE" ]; then
  if ! pgrep -x git >/dev/null 2>&1; then
    echo "[vhrn] Removing orphaned $LOCK_FILE from a previous crash." >&2
    rm -f "$LOCK_FILE"
  else
    echo "[vhrn] Warning: a git process is holding the index lock." >&2
  fi
fi

# A gh token came in from the host: wire git's credential helper to gh so plain
# `git push`/`fetch` over HTTPS authenticate too (gh itself already honors the env).
if [ -n "${GH_TOKEN:-}" ] && command -v gh >/dev/null 2>&1; then
  gh auth setup-git >/dev/null 2>&1 || true
fi

exec "$@"
