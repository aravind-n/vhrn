#!/usr/bin/env bash
set -euo pipefail

LOCK_FILE="/workspace/.git/index.lock"

# 1. Check if a git lock file exists in the mounted workspace
if [ -f "$LOCK_FILE" ]; then
  # 2. Check if any actively running git processes exist inside THIS micro-VM
  if ! pgrep -x "git" > /dev/null 2>&1; then
    echo "[Sandbox Entrypoint] Found orphaned $LOCK_FILE from a previous crash. Cleaning up..." >&2
    rm -f "$LOCK_FILE"
  else
    echo "[Sandbox Entrypoint] Warning: Active git process detected holding the index lock." >&2
  fi
fi

# 3. Replace the entrypoint shell process with the Claude Code CLI
exec "$@"

