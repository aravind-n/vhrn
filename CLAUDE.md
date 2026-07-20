# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`claude-box` runs Claude Code inside a container jailed to the current project directory, so it runs without exposing the rest of the host. It's a handful of Bash/Docker/Make files — no application code.

- `claude-box.sh` — the wrapper: syncs a sandbox copy of `~/.claude`, then runs `claude "$@"` in a container with the project bind-mounted at its real absolute path.
- `image/Dockerfile` + `image/entrypoint.sh` — the `claude-sandbox` image (`node:lts-slim` + Claude Code, plus python3/uv, gh, ripgrep/fd, zip/unzip; non-root `dev` user).
- `Makefile` builds the image; `install.sh` installs the wrapper to `/usr/local/bin`.

## Commands

- `make` / `make build` — build the image (default goal). `make rebuild` — no-cache build. `make clean` — remove the image.
- `make ENGINE=docker ...` — force Docker; the engine auto-detects `container` first, then `docker`.
- `./install.sh` — install `claude-box` to `/usr/local/bin` (needs sudo).
- No tests or CI yet. To verify a change, rebuild the image and run `claude-box` in a throwaway project directory.

## Must-know invariants

- **Both Apple `container` and Docker must work, for build *and* run.** The Makefile already honors `ENGINE`, but `claude-box.sh` hardcodes `container run` and the image name `claude-sandbox` — treat that as a gap to parameterize, not a pattern to copy. The CLIs also differ (`container image delete` vs `docker image rm`), so an engine switch isn't a pure string swap.
- **The wrapper is a thin pass-through.** It forwards `claude "$@"` verbatim and injects no flags of its own — the user supplies whatever claude flags they want. Don't bake flags in.
- **Terminal env crosses verbatim.** `TERM`/`COLORTERM`/`TERM_PROGRAM`/`TERM_PROGRAM_VERSION` are forwarded as-is, never forced: claude branches per-terminal rendering on them (with `TERM_PROGRAM` stripped it draws the block-glyph welcome mascot instead of the native bg-painted one). Don't reintroduce `COLORTERM=truecolor`/`FORCE_COLOR` — that lies to claude about terminals that never claimed those capabilities.
- **The sandbox copy (`~/.cache/claude-box/sandbox`) is re-synced from `~/.claude` on every run** (`rsync -aL --delete`, `cp -RL` fallback), so edits made there are wiped each run — change `~/.claude` instead. Session history is written straight to the host's `~/.claude/projects/<key>`.
- **The history key must match Claude's own `projects/<key>` encoding** (`sed 's/[^A-Za-z0-9]/-/g'` on the absolute project path). If that encoding drifts, in-box history stops unifying with native history — keep it in lockstep.
- **gh auth is env-injected, never file-mounted.** The wrapper resolves a token (`$GH_TOKEN`/`$GITHUB_TOKEN`, else `gh auth token` — the only route that works with macOS Keychain storage, where no file contains the token) and passes it in as `GH_TOKEN`; the entrypoint runs `gh auth setup-git` so plain git-over-HTTPS authenticates too. Skips silently when the host has no gh login. SSH remotes stay unauthenticated by design.
- **The host `~/.gitconfig` is copied into the cache and bind-mounted at `/home/dev/.gitconfig`** (skipped if the host has none), so in-box commits use the user's identity. It's a disposable copy — in-box edits don't persist; change the host file.
- The Dockerfile deletes the base image's `node` user to free uid 1000 for `dev`, and the entrypoint clears a stale `$PWD/.git/index.lock` on boot (needs `procps`). Both are intentional — don't "clean them up".

## Conventions

- Bash scripts use `#!/usr/bin/env bash` and `set -euo pipefail`; comments explain *why*, not *what*, and stay terse — one line where possible, never a multi-line essay. Helper functions early-`return 0` when a source path is absent.
- Commit messages: concise and imperative ("Fix claude dir mount mangling").
