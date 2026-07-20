#!/usr/bin/env bash
set -euo pipefail

# Run Claude Code in dangerous mode, jailed to the current project.

REAL_CLAUDE="$HOME/.claude"
# Sandbox copies live under the XDG cache dir, not $HOME, so they don't clutter it.
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/claude-box"
SANDBOX="$CACHE/sandbox"
SANDBOX_JSON="$CACHE/sandbox.json"
PROJECT="$(pwd -P)"

# Reproduce Claude's projects/<key> encoding so history unifies with native.
KEY="$(printf '%s' "$PROJECT" | sed 's/[^A-Za-z0-9]/-/g')"
HISTORY="$REAL_CLAUDE/projects/$KEY"

mkdir -p "$SANDBOX" "$HISTORY"

# Copy globals in, dereferencing symlinks so symlinked skills come across.
copy_dir() {
  [ -d "$REAL_CLAUDE/$1" ] || return 0
  if command -v rsync >/dev/null 2>&1; then
    rsync -aL --delete "$REAL_CLAUDE/$1/" "$SANDBOX/$1/" \
      || echo "claude-box: warning: some '$1' entries were skipped (broken symlink?)" >&2
  else
    rm -rf "${SANDBOX:?}/$1"
    cp -RL "$REAL_CLAUDE/$1" "$SANDBOX/$1" \
      || echo "claude-box: warning: some '$1' entries were skipped (broken symlink?)" >&2
  fi
}
copy_file() {
  [ -f "$REAL_CLAUDE/$1" ] || return 0
  cp -L "$REAL_CLAUDE/$1" "$SANDBOX/$1" 2>/dev/null \
    || echo "claude-box: warning: could not copy '$1'" >&2
}
for d in skills commands agents; do copy_dir "$d"; done
for f in settings.json CLAUDE.md statusline.sh; do copy_file "$f"; done

# Copy the config file
if [ -f "$REAL_CLAUDE.json" ]; then
  cp -L "$REAL_CLAUDE.json" "$SANDBOX_JSON" 2>/dev/null \
    || echo "claude-box: warning: could not copy .claude.json" >&2
fi
[ -s "$SANDBOX_JSON" ] || printf '{}\n' > "$SANDBOX_JSON"

# Host git config, dereferenced into the cache so the box inherits the user's identity
# and settings. Disposable copy (re-synced each run) — edit ~/.gitconfig to persist.
GIT_MOUNT=()
if [ -f "$HOME/.gitconfig" ]; then
  cp -L "$HOME/.gitconfig" "$CACHE/gitconfig" 2>/dev/null \
    && GIT_MOUNT=(--volume "$CACHE/gitconfig:/home/dev/.gitconfig") \
    || echo "claude-box: warning: could not copy .gitconfig" >&2
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

container system start >/dev/null 2>&1 || true

container run -it --rm \
  --env CLAUDE_SANDBOX=1 \
  --volume "$PROJECT:$PROJECT" \
  --workdir "$PROJECT" \
  --volume "$SANDBOX:/home/dev/.claude" \
  --volume "$SANDBOX_JSON:/home/dev/.claude.json" \
  --volume "$HISTORY:/home/dev/.claude/projects/$KEY" \
  ${GIT_MOUNT[@]+"${GIT_MOUNT[@]}"} \
  "${TERM_ENV[@]}" \
  ${GH_ENV[@]+"${GH_ENV[@]}"} \
  claude-sandbox \
  claude "$@"
