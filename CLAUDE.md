# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`vhrn` runs Claude Code inside a container jailed to the current project directory, with default-deny network egress, so it runs without exposing the rest of the host or letting a prompt injection exfiltrate to arbitrary hosts. It's a small Go CLI (the wrapper) plus a Go egress proxy and a few Docker/Make/Bash files.

- `main.go` + `internal/vhrn/` — the CLI (Go, package `vhrn`): syncs a sandbox copy of `~/.claude`, starts the egress proxy, then runs `claude "$@"` in a container with the project bind-mounted at its real absolute path. Also handles `vhrn net …` and the `--open-net`/`--allow` flags. It orchestrates but shells out to rsync/cp/gh and the engine to match the original shell behavior.
- `image/Dockerfile` + `image/entrypoint.sh` — the `vhrn-sandbox` image (`debian:trixie-slim` + native Claude Code binary, plus python3/uv, mise, gh, ripgrep/fd, zip/unzip; non-root `dev` user, no sudo). The entrypoint installs the egress firewall as root, then drops to `dev`.
- `proxy/` — a hand-rolled Go CONNECT/HTTP egress proxy (static binary in a `scratch` image) enforcing the domain allowlist. Policy files live host-side, mounted only into the proxy.
- `Makefile` builds both images (`make`), builds/installs the Go binary (`make binary` / `make install`), cross-compiles releases (`make release`), and runs tests (`make test`). `install.sh` is the curl installer that fetches a prebuilt binary from GitHub Releases.

## Commands

- `make` / `make build` — build both images (box + proxy; default goal). `make build-box`/`make build-proxy` — one image. `make rebuild` — no-cache build of both. `make clean` — remove both images and the built binary.
- `make binary` — build the static `vhrn` binary; `make release` — cross-compile darwin/linux × arm64/amd64 into `dist/`.
- `make test` — CLI + proxy unit tests. Equivalently `go test ./...` (CLI) and `cd proxy && go test ./...` (the proxy's allowlist-matching and IP-classifier tests). The CLI tests cover flag parsing, the history-key encoding, terminal env, allowlist add/dedup, and engine-inspect IP parsing.
- `make ENGINE=docker ...` — force Docker; the engine auto-detects `container` first, then `docker`.
- `make install` — build and install `vhrn` to `/usr/local/bin` (needs sudo). `make uninstall` removes it.
- No CI yet, and the unit tests don't exercise a live container. To verify the full run path, build the images and run `vhrn` in a throwaway project directory.

## Must-know invariants

- **Both Apple `container` and Docker must work, for build *and* run.** Both the Makefile and the CLI (`internal/vhrn/engine.go`) select the engine (explicit `ENGINE`/`VHRN_ENGINE`, else auto-detect `container` then `docker`) — keep them in sync. The CLIs differ (`container image delete` vs `docker image rm`; inspect output differs, and Apple escapes the CIDR slash in `ipv4Address`), so an engine switch isn't a pure string swap. The box must run with `--cap-add CAP_NET_ADMIN` or the entrypoint's `nft` fails with a netlink permission error.
- **The wrapper is a thin pass-through.** It forwards `claude "$@"` verbatim and injects no flags of its own — the user supplies whatever claude flags they want. Don't bake flags in.
- **Terminal env crosses verbatim.** `TERM`/`COLORTERM`/`TERM_PROGRAM`/`TERM_PROGRAM_VERSION` are forwarded as-is, never forced: claude branches per-terminal rendering on them (with `TERM_PROGRAM` stripped it draws the block-glyph welcome mascot instead of the native bg-painted one). Don't reintroduce `COLORTERM=truecolor`/`FORCE_COLOR` — that lies to claude about terminals that never claimed those capabilities.
- **The sandbox copy (`~/.cache/vhrn/sandbox`) is re-synced from `~/.claude` on every run** (`rsync -aL --delete`, `cp -RL` fallback), so edits made there are wiped each run — change `~/.claude` instead. Session history is written straight to the host's `~/.claude/projects/<key>`.
- **The history key must match Claude's own `projects/<key>` encoding** (`sed 's/[^A-Za-z0-9]/-/g'` on the absolute project path). If that encoding drifts, in-box history stops unifying with native history — keep it in lockstep.
- **gh auth is env-injected, never file-mounted.** The wrapper resolves a token (`$GH_TOKEN`/`$GITHUB_TOKEN`, else `gh auth token` — the only route that works with macOS Keychain storage, where no file contains the token) and passes it in as `GH_TOKEN`; the entrypoint runs `gh auth setup-git` so plain git-over-HTTPS authenticates too. Skips silently when the host has no gh login. SSH remotes stay unauthenticated by design.
- **The host `~/.gitconfig` is copied into the cache and bind-mounted at `/home/dev/.gitconfig`** (skipped if the host has none), so in-box commits use the user's identity. It's a disposable copy — in-box edits don't persist; change the host file.
- **The egress guard is enforced from outside `dev`'s reach.** The entrypoint installs a default-deny nftables ruleset as root (egress only to the proxy), then drops to `dev` via `setpriv`; because the uid transition clears capabilities and there is no sudo, `dev` (which Claude runs as) cannot alter the firewall. This is *why* sudo was removed — don't reintroduce it. If `nft` can't run, the entrypoint aborts rather than fall through to an unguarded session.
- **Egress policy is host-owned.** The allowlist, mode, and deny-log files live in `~/.cache/vhrn/net/` and are mounted only into the proxy, never the box. `vhrn net …` mutates them from the host; the proxy re-reads per request (so `net allow` needs no restart), and `net allow` writes atomically (same-dir temp + rename) to avoid torn reads. The box can never widen its own egress.
- Claude Code is the native binary in `~/.local` (not `~/.claude`, which the runtime mount shadows) and honors `HTTPS_PROXY`; the base is `debian:trixie-slim`, which has no `node` user to free. The entrypoint still clears a stale `$PWD/.git/index.lock` on boot (needs `procps`).

## Conventions

- The CLI is Go (`main.go` + `internal/vhrn/`, package `vhrn`); it orchestrates and shells out to rsync/cp/gh and the container engine to preserve the shell wrapper's exact behavior. Comments explain *why*, terse; prefer small single-file helpers over new packages.
- Bash/sh scripts (entrypoint, install.sh) use `#!/usr/bin/env bash`/`sh` and `set -euo pipefail`; comments explain *why*, not *what*, and stay terse — one line where possible, never a multi-line essay. Helper functions early-`return 0` when a source path is absent.
- Commit messages: concise and imperative ("Fix claude dir mount mangling").
