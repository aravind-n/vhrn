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
- **`~/.claude-sandbox` is re-synced from `~/.claude` on every run** (`rsync -aL --delete`, `cp -RL` fallback), so edits made there are wiped each run — change `~/.claude` instead. Session history is written straight to the host's `~/.claude/projects/<key>`.
- **The history key must match Claude's own `projects/<key>` encoding** (`sed 's/[^A-Za-z0-9]/-/g'` on the absolute project path). If that encoding drifts, in-box history stops unifying with native history — keep it in lockstep.
- **gh auth is env-injected, never file-mounted.** The wrapper resolves a token (`$GH_TOKEN`/`$GITHUB_TOKEN`, else `gh auth token` — the only route that works with macOS Keychain storage, where no file contains the token) and passes it in as `GH_TOKEN`; the entrypoint runs `gh auth setup-git` so plain git-over-HTTPS authenticates too. Skips silently when the host has no gh login. SSH remotes stay unauthenticated by design.
- The Dockerfile deletes the base image's `node` user to free uid 1000 for `dev`, and the entrypoint clears a stale `$PWD/.git/index.lock` on boot (needs `procps`). Both are intentional — don't "clean them up".

## Known issues

- **The "do you trust this folder?" prompt reappears every run.** Trust lives in `~/.claude.json` at `.projects["<abs path>"].hasTrustDialogAccepted`. Two compounding causes:
  1. `~/.claude-sandbox.json` is re-seeded from the real `~/.claude.json` on every run (`claude-box.sh`), so accepting the prompt inside the box is wiped on the next launch.
  2. Even within a run the box can't persist the acceptance: Claude writes `.claude.json` atomically (staging file + `rename`), and a `rename` *onto* the single-file bind mount fails with `EBUSY` (its rename helper only falls back for `EXDEV`). So **no** `.claude.json` change made inside the box survives — trust, MCP approvals, dismissed dialogs, etc. In-place writes propagate; only the atomic rename fails.
  - Workaround: open the folder once in native `claude`, which writes trust to the real `~/.claude.json` so the re-seed copies it in.
  - Fix direction (not yet done): pre-seed `.projects["$PWD"].hasTrustDialogAccepted = true` in the sandbox JSON on each run — best in `entrypoint.sh`, where `jq` is present and writes are in-place (`cat >`, not `mv`), sidestepping both causes. The container is the trust boundary, so auto-trusting the workdir is aligned with the tool's purpose — but it does silently trust whatever dir claude-box is pointed at, so gate it behind a clear comment.

## Conventions

- Bash scripts use `#!/usr/bin/env bash` and `set -euo pipefail`; comments explain *why*, not *what*, and stay terse — one line where possible, never a multi-line essay. Helper functions early-`return 0` when a source path is absent.
- Commit messages: concise and imperative ("Fix claude dir mount mangling").
