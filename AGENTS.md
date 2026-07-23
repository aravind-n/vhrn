# AGENTS.md

Guidance for coding agents working in this repository, in the open
[agents.md](https://agents.md) format.

## Project overview

`vhrn` ("Virtualized Harness Runtime") runs coding agents ("harnesses") inside a container jailed to
the current project directory, with **default-deny network egress** — so an agent can run
without exposing the rest of the host or letting a prompt injection exfiltrate to arbitrary hosts.
The CLI is harness-agnostic.

A small monorepo with three independently-built parts plus packaging:

- **`src/`** — the CLI (Rust, crate `vhrn`, `#![forbid(unsafe_code)]`; `main.rs` is a thin
  shim over `lib.rs`). Subcommand-first: `vhrn install <harness>` pulls images and wires a
  shell alias, `vhrn <harness> …` runs the agent in the container,
  `vhrn uninstall`/`list`/`net`/`help`/`--version` manage the environment. It orchestrates
  and shells out to rsync/cp/gh and the container engine.
- **`proxy/`** — a hand-rolled Go CONNECT/HTTP egress proxy (a static binary in a `scratch`
  image) enforcing the domain allowlist. Its own stdlib-only module (no third-party deps,
  no `go.sum`), published as `vhrn-proxy`.
- **`image/`** — the container image recipes: `image/base/` (`Dockerfile` + `entrypoint.sh`)
  is the shared `vhrn-base`; `image/<harness>/` (e.g. `image/claude/`) is a thin
  `FROM vhrn-base` plus the agent binary.
- **`pages/`** — the `curl | sh` installer and landing page, served over GitHub Pages.
  **`.github/workflows/`** — the CI/CD pipeline. **`docs/`** — release docs.

Core behavioral invariants — keep these intact:

- **The wrapper is a thin pass-through.** `vhrn <harness> [wrapper-flags] [--] [agent args]`
  consumes only its own flags (`--open-net`/`--allow`), then forwards the rest to the agent
  verbatim. Don't bake agent flags in. Bare `vhrn` prints help.
- **Harnesses are data, not forks.** `src/harness.rs` holds the registry; a `Harness` spec
  carries the image name, in-container command, alias, default egress domains, and the
  persistence descriptors. Dispatch, install, run, and persistence all read from it. Adding
  codex/aider = a spec + a `FROM vhrn-base` Dockerfile under `image/<harness>/` + a matrix
  entry in `_build-images.yml`. No CLI fork.
- **Both Apple `container` and Docker must work, for build and run.** `image/Makefile`,
  `proxy/Makefile`, and `src/run.rs` (`detect_engine`) select the engine (explicit
  `ENGINE`/`VHRN_ENGINE`, else auto-detect `container` then `docker`) — keep them in sync.
  The CLIs differ (`container image delete` vs `docker image rm`; inspect output differs,
  and Apple escapes the CIDR slash in `ipv4Address`), so an engine switch isn't a string swap.
- **Login/state persists via a container-owned store, not the disposable copy.**
  `~/.cache/vhrn/state/<harness>/` is mounted as the harness's config dir
  (`CLAUDE_CONFIG_DIR` for claude). Host credentials are copied in **only when the store is
  empty** (bootstrap-only — an in-container login is never overwritten); `.claude.json` is
  merged in place for onboarding + this project's trust without touching `oauthAccount` or
  other projects. The disposable synced config, the container guide, and the
  `projects/<key>` history layer on top as **nested** mounts, so the config sync can never
  reach `state/`. `.claude.json` must sit in a real directory mount (Claude rewrites it via
  a backup file), never a single-file mount.
- **The disposable config copy (`~/.cache/vhrn/sandbox`) is re-synced from `~/.claude` every
  run** (`rsync -aL --delete`, `cp -RL` fallback), so edits there are wiped — change
  `~/.claude` instead. It is physically separate from `state/`.
- **The history key must match Claude's `projects/<key>` encoding** (`[^A-Za-z0-9]` → `-` on
  the absolute project path), or in-container history stops unifying with native history.
- **Terminal env crosses verbatim.** `TERM`/`COLORTERM`/`TERM_PROGRAM`/`TERM_PROGRAM_VERSION`
  are forwarded as-is, never forced. Don't reintroduce `COLORTERM=truecolor`/`FORCE_COLOR`.
- **gh auth is env-injected, never file-mounted.** The wrapper resolves a token
  (`$GH_TOKEN`/`$GITHUB_TOKEN`, else `gh auth token`) and passes it as `GH_TOKEN`; the
  entrypoint runs `gh auth setup-git`. Skips silently without a host gh login; SSH remotes
  stay unauthenticated. The host `~/.gitconfig` is copied into the cache and bind-mounted at
  `/home/dev/.gitconfig` (a disposable copy — change the host file).
- **Images are pulled from a registry, not built by users.** `vhrn install <harness>[@version]`
  pulls `vhrn-<harness>` at the *agent's* version (default `latest`) plus the `vhrn-proxy`
  matching the **CLI binary's own** version — the proxy rides the CLI's release clock, not the
  agent's, so a container and its proxy stay a matched set and upgrading the CLI upgrades its
  proxy (`proxy_tag` derives it: a nightly CLI → nightly proxy, a `vX.Y.Z` CLI → its own tag).
  Override the registry with `VHRN_REGISTRY`. `--local` uses `make`-built images (version
  `local`). The installed registry (`~/.config/vhrn/installed`, `name <tag>` per line) records
  only the agent tag the run path resolves from. `vhrn update` re-pulls a floating install; a
  daily `harness-images.yml` cron rebuilds a harness when its agent updates — both independent
  of a CLI release.
- **Config precedence: flags > `./.vhrn.toml` > `~/.config/vhrn/config.toml` > defaults**
  (`src/config.rs`, `toml` crate). `blocked_dirs` matches the resolved cwd **exactly** (not
  subtree), default `["~","/"]`. `toolchains.tools` resolves to a content-addressed derived
  image (`vhrn-<h>-tc-<hash>`, `FROM` the harness image + `mise use -g`), cached by tag.
- **Shell aliases and the installed registry are host state.** `install`/`uninstall` mutate
  `~/.config/vhrn/installed` and regenerate reversible marker-delimited alias blocks in
  bash/zsh/fish rc files (existing files + the current shell's). `command <name>`/`\<name>`
  still reach the real binary.
- The harness binary is baked into the image (native, in `~/.local`; no host install) and
  honors `HTTPS_PROXY`. The entrypoint clears a stale `$PWD/.git/index.lock` on boot (needs
  `procps`).

## Build and test commands

Three parts, each built by its own tool — there is **no root build wrapper**, so invoke
them directly:

- **CLI:** `cargo build --release` → `target/release/vhrn`; `cargo install --path .` installs
  it to `~/.cargo/bin`.
- **Images:** `make -C image` builds `vhrn-base` then the harnesses (`build-base`,
  `build-claude`; the harness is `FROM vhrn-base`, so base first). `make -C image build-base`/
  `build-claude` build one; `make -C image clean` removes them.
- **Proxy image:** `make -C proxy` builds `vhrn-proxy`; `make -C proxy clean` removes it.

The image Makefiles auto-detect the engine (`container`, then `docker`; `ENGINE=docker`
forces Docker). Baked into `vhrn-base`: a basic C toolchain (clang/lld/llvm/libc),
python3/uv, mise, gh, ripgrep/fd, zip/unzip, nftables — a non-root `dev` user, no sudo.

Day to day you build nothing — `vhrn install <harness>` pulls prebuilt images from ghcr.
For a local-image dev loop: `cargo install --path . && make -C image && make -C proxy`, then
`vhrn install claude --local`.

**CI/CD** (`.github/workflows/`): `ci.yml` is the PR gate (path-filtered per component behind
a single `ci-gate`); `nightly.yml` publishes `nightly` images + a rolling `nightly` binary
prerelease on master; `release.yml` publishes `vX.Y.Z`+`latest` images + a GitHub Release on
a `v*` tag. Three reusable workflows (`_test`, `_build-images`, `_build-binaries`) plus
`pages.yml`. See `docs/runbooks/release.md`.

## Code style guidelines

- **Rust** (`src/`, crate `vhrn`, `#![forbid(unsafe_code)]`): the code is `cargo fmt`-clean
  (enforced in CI with default settings — no `rustfmt.toml`); reach for `#[rustfmt::skip]`
  only on aligned test-case tables. Comments explain *why*, terse, one line where it fits.
  Group `use` imports std / external / crate, blank-line separated. Prefer small single-file
  helpers over new modules. Keep pure logic (arg assembly, hashing, merges, matching) in
  testable functions; split env reads into a thin edge + a pure resolver so tests never
  touch process env. Errors bubble via `anyhow`.
- **Bash/sh** (entrypoint, `pages/install.sh`): `#!/usr/bin/env bash`/`sh` + `set -euo
  pipefail`; comments terse, one line where possible; helpers early-`return 0` when a source
  path is absent. Kept shellcheck-clean.
- **Go** (`proxy/`): standard library only — no third-party modules, no `go.sum`. Keep it
  `gofmt`- and `go vet`-clean.
- **Commits:** Linux-kernel style (`cli: …`, `image: …`, `Documentation: …`), concise and
  imperative — a short subject plus at most a line or two, not verbose.

## Testing instructions

The suite runs per changed component on PRs and in full on master:

- **CLI:** `cargo fmt --all -- --check`, then `cargo clippy --all-targets -- -D warnings`,
  then `cargo test` (fmt runs before clippy).
- **Proxy:** `cd proxy && gofmt -l . && go vet ./... && go test ./...`.
- **Workflows:** `actionlint` (with shellcheck on inline `run:` scripts) validates
  `.github/workflows/**`.

Tests cover flag parsing, the history-key encoding, terminal env, allowlist add/dedup,
engine-inspect IP parsing, the harness registry, the installed registry, shell-alias blocks,
install/uninstall arg assembly, the persistence state store (creds bootstrap + `.claude.json`
merge), the mount topology, TOML config load/merge, `blocked_dirs`, net-mode resolution, and
toolchain hashing — plus the proxy's allowlist-matching and IP-classifier tests. Keep pure
logic in functions that take their inputs as arguments so new behavior stays unit-testable
without a live container.

The unit tests **don't exercise a live container**. To verify the full run path end-to-end,
`vhrn install claude` (or `make -C image && make -C proxy` then `--local`), then run
`vhrn claude` in a throwaway project directory.

## Security considerations

The whole point is that an agent can run without reaching the rest of the host or
exfiltrating freely. Guard these:

- **The egress guard is enforced from outside `dev`'s reach.** The entrypoint installs a
  default-deny nftables ruleset as root (egress only to the proxy), then drops to `dev` via
  `setpriv`; the uid transition clears capabilities and there is no sudo, so `dev` cannot
  alter the firewall. **This is why sudo was removed — do not reintroduce it.** If `nft`
  can't run, the entrypoint **aborts** rather than fall through to an unguarded session. The
  container must run with `--cap-add CAP_NET_ADMIN` or `nft` fails with a netlink error.
- **Egress policy is host-owned.** The allowlist, mode, and deny-log live in
  `~/.cache/vhrn/net/` and are mounted **only into the proxy, never the container** — that
  is what stops an in-container process from widening its own egress, even under
  skip-permissions. `vhrn net …` mutates them from the host (atomic same-dir temp + rename);
  `install` unions base + harness domains in (append-if-missing); config `net.allow`/
  `net.mode` fold in at run (`--open-net` wins). The container can never widen its own egress.
- **The container stays ephemeral (`--rm`).** A fresh, tamper-proof firewall every boot — a
  security feature. Persistence is a property of what's mounted; do **not** move to a
  persistent "container machine."
- **The proxy is the security-critical component** — a static Go binary in a `scratch` image
  (no shell, no userland), running unprivileged: minimal CVE surface. It matches on hostname
  and does **not** terminate TLS, so it can't stop exfiltration to an already-allowed domain
  or domain-fronting behind an allowed CDN.
- **Only the project is mounted.** `~/.ssh`, your other projects, and the rest of `$HOME`
  stay outside the container; `blocked_dirs` refuses to jail `$HOME` or `/`.
- **Threat model** (full version in the README): protects the host filesystem and against
  casual exfiltration. Does **not** cover exfiltration to an allowed domain, sessions run
  with `--open-net`/`net.mode = "open"`, or a container escape under Docker (Docker shares
  the host kernel; Apple `container` gives each container its own lightweight VM).
