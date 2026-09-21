# AGENTS.md

Guidance for coding agents working in this repository, in the open
[agents.md](https://agents.md) format.

## Project overview

`vhrn` ("Virtualized Harness Runtime") runs coding agents ("harnesses") inside a container jailed to
the current project directory, with **default-deny network egress** — so an agent can run
without exposing the rest of the host or letting a prompt injection exfiltrate to arbitrary hosts.
The CLI is harness-agnostic.

A small monorepo with three independently-built parts plus packaging:

- **`cli/`** — the CLI (Rust, package `vhrn`, `#![forbid(unsafe_code)]`; `main.rs` is a thin
  shim over `lib.rs`). Subcommand-first: `vhrn install <harness>` pulls images and records the
  installation, `vhrn <harness> …` runs the agent in the container,
  `vhrn uninstall`/`list`/`net`/`help`/`--version` manage the environment. It orchestrates
  and shells out to rsync/cp/gh and the container engine.
- **`proxy/`** — the Rust CONNECT/HTTP egress proxy (a static binary in a `scratch` image)
  enforcing the domain allowlist, published as `vhrn-proxy`.
- **`image/`** — the container image recipes: `image/base/` (`Dockerfile` + `entrypoint.sh`)
  is the shared `vhrn-base`; `image/<harness>/` (`image/claude/`, `image/codex/`, `image/pi/`) is a thin
  `FROM vhrn-base` plus the agent binary.
- **`shared/testdata/`** — language-neutral contract fixtures consumed across components. Keep
  shared cases here instead of duplicating them under a package.
- **`pages/`** — the `curl | sh` installer and landing page, served over GitHub Pages.
  **`.github/workflows/`** — the CI/CD pipeline. **`docs/`** — release docs.

Core behavioral invariants — keep these intact:

- **The wrapper is a thin pass-through.** `vhrn <harness> [wrapper-flags] [--] [agent args]`
  consumes only its own flags (`--open-net`, `--allow`, and `--allow --local <authorities>`),
  then forwards the rest to the agent verbatim. Don't bake agent flags in. Bare `vhrn` prints
  help.
- **Harnesses are data, not forks.** `cli/src/harness.rs` holds the registry; a `Harness` spec
  carries the image name, in-container command, default egress domains, and the
  persistence descriptors. Dispatch, install, run, and persistence all read from it. Adding
  a harness = a spec + a `FROM vhrn-base` Dockerfile under `image/<harness>/` + a matrix
  entry in `_build-harness-images.yml`. No CLI fork. See `docs/adding-a-harness.md`.
- **Both Apple `container` and Docker must work, for build and run.** `image/Makefile`,
  `proxy/Makefile`, and `cli/src/run.rs` (`detect_engine`) select the engine (explicit
  `ENGINE`/`VHRN_ENGINE`, else auto-detect `container` then `docker`) — keep them in sync.
  The CLIs differ (`container image delete` vs `docker image rm`; inspect output differs,
  and Apple escapes the CIDR slash in `ipv4Address`), so an engine switch isn't a string swap.
- **Resource limits are host-owned configuration, never wrapper flags.** `[resources].memory`
  is `"engine"` or a nonzero integer ending in `m`/`g`; `[resources].cpus` is a positive
  integer. Use portable long `--memory`/`--cpus` engine flags. An unset memory value gets the
  Apple `container`-only 4 GiB default; Docker retains its engine default. Agent arguments with
  those names still pass through verbatim. Project-local dependency/install outputs persist on
  the project mount, while package-manager caches under container home are intentionally
  ephemeral — do not add cache mounts.
- **Login/state persists via a container-owned store, not the disposable copy.**
  `~/.cache/vhrn/state/<harness>/` is mounted as the harness's config dir
  (`CLAUDE_CONFIG_DIR` for claude). Host credentials are copied in **only when the corresponding
  destination file is absent** (bootstrap-only — an in-container login is never overwritten).
  Nothing else in the store is ever written by vhrn: onboarding and per-project trust belong to
  the agent, so the answer given in the container is the one that persists — **do not reintroduce seeding of
  `hasTrustDialogAccepted`**, which made untrusting a project impossible and handed a repo's
  own `.claude/skills` their `allowed-tools` grants unasked. The disposable synced config, the
  container guide, and the `projects/<key>` history layer on top as **nested** mounts, so the
  config sync can never reach `state/`. `.claude.json` must sit in a real directory mount
  (Claude rewrites it via a backup file), never a single-file mount.
- **If an agent writes its own config file, vhrn does not write it** — inject through a layer
  the agent only reads. Deriving that file each run destroys the decisions the agent records
  in it, with no way for the user to make an answer stick. `system_config` mounts the host's
  config plus vhrn's own settings at `/etc/<name>`, **read-only** (a setting the container can
  edit is not one). These are *defaults*, at the bottom of the agent's precedence chain: the
  layer above it that would have outranked even the CLI (`requirements.toml`) is **ignored by
  Codex**, verified against 0.146.0, so it is not written. `--sandbox` therefore still works
  as a per-run hatch. The host copy is filtered: its `[projects.*]`
  trust tables are stripped, because the project is mounted at its real host path, so copying
  them through would answer in the container a trust question the user answered on the host —
  `hasTrustDialogAccepted` again, by another route. Everything else crosses byte-for-byte; a
  malformed config stays the agent's error to report, not vhrn's to parse.
- **Persistence descriptors are per-harness, not universal.** `guide` (filename, host sources
  first-non-empty-wins, state-dir vs sandbox, before-or-after the host's text),
  `system_config`, `share_history`, `sessions_env`/`sessions_dir`, and `credential_env` all
  differ between claude, codex, and pi — none of them is a default that happens to suit one agent.
  The container guide is the *only* file vhrn derives into a state dir.
- **Pi's state is selectively seeded and mirrored.** Its state/config dir is `.pi/agent` through
  `PI_CODING_AGENT_DIR`. `settings.json` is seeded once after filtering `apiKeys` and
  `defaultProjectTrust`; `keybindings.json` is seeded once so Pi owns its migration. These are
  generic `SeedFile` descriptors, never credentials. Pi mirrors its user inputs
  (`models.json`, `SYSTEM.md`, `APPEND_SYSTEM.md`, extensions, skills, prompts, and themes) each
  run. It never imports host `auth.json` or `trust.json`; Pi-owned package/catalog state persists
  in its store.
  Its guide is the first non-empty `AGENTS.override.md`, `AGENTS.md`, `AGENTS.MD`, `CLAUDE.md`,
  or `CLAUDE.MD` source, composed into `AGENTS.override.md`.
- **Sessions are partitioned per project where an agent keeps one flat tree.** Codex's
  `CODEX_SQLITE_HOME` points at `state/<harness>-sessions/<key>`, a sibling of the shared
  state dir, and the transcript subdir inside it is bound back under the config dir so the
  index and the files it names cannot land in different partitions. Login and config stay
  shared; the databases that follow that variable carry memories and goals too, so those are
  per-project — a documented delta, not an accident.
- **Pi sessions are env-directed per project.** `PI_CODING_AGENT_SESSION_DIR` points at the
  sibling project session store; Pi's own `--session-dir` remains an agent argument and overrides
  that default.
- **The disposable config copy (`~/.cache/vhrn/sandbox/<harness>`) is re-synced from the
  harness config dir every run** (`rsync -aL --delete`, `cp -RL` fallback), so edits there are
  wiped — change `~/.claude` / `~/.codex` instead. Pi uses selected mirror paths under
  `~/.pi/agent`. Deleting the host source removes the copy too, so config the user deleted is
  never mounted again. It is physically separate from `state/`, and
  per-harness so one harness's `--delete` never runs on a tree another's live container has
  mounted.
- **`~/.agents` is mounted for every harness**, at `/home/dev/.agents`. It is the
  vendor-neutral config dir agents resolve from `$HOME` rather than from their own config
  dir, so it is a *top-level* mount beside the state mount, not one of `nested_mounts()`, and
  a run-path constant rather than a `Harness` field. The whole tree is synced instead of an
  enumerated set of children: unread paths are inert, so a new convention works the day an
  agent ships support for it with no vhrn change. Same disposable contract as the rest of the
  sync — edit `~/.agents` on the host.
- **The history key must match Claude's `projects/<key>` encoding** (`[^A-Za-z0-9]` → `-` on
  the absolute project path), or in-container history stops unifying with native history. It
  doubles as the per-project session key, so the same encoding is load-bearing twice.
- **Terminal env crosses verbatim.** `TERM`/`COLORTERM`/`TERM_PROGRAM`/`TERM_PROGRAM_VERSION`
  are forwarded as-is, never forced. Don't reintroduce `COLORTERM=truecolor`/`FORCE_COLOR`.
- **gh auth is env-injected, never file-mounted.** The wrapper resolves a token
  (`$GH_TOKEN`/`$GITHUB_TOKEN`, else `gh auth token`) and passes it as `GH_TOKEN`; the
  entrypoint runs `gh auth setup-git`. Skips silently without a host gh login; SSH remotes
  stay unauthenticated. The host `~/.gitconfig` is copied into the cache and bind-mounted at
  `/home/dev/.gitconfig` (a disposable copy — change the host file). The base image also sets
  `safe.directory = *` in `/etc/gitconfig`: virtiofs reports the mount root's owner
  inconsistently and would otherwise break git at random — only one project is ever mounted,
  so it weakens nothing.
- **Images are pulled from a registry, not built by users.** `vhrn install <harness>[@version]`
  pulls `vhrn-<harness>` at the *agent's* version (default `latest`) plus the `vhrn-proxy`
  matching the **CLI binary's own** version — the proxy rides the CLI's release clock, not the
  agent's, so a container and its proxy stay a matched set and upgrading the CLI upgrades its
  proxy (`proxy_tag` derives it: a `vX.Y.Z` CLI uses its own tag; a development CLI uses `latest`).
  Override the registry with `VHRN_REGISTRY`. `--local` uses `make`-built images (version
  `local`). The installed registry (`~/.config/vhrn/installed`, `name <tag>` per line) records
  only the agent tag the run path resolves from. `vhrn update` queries the registry (OCI
  tags-list over the anonymous bearer-challenge flow, `cli/src/registry.rs`) and
  re-pulls a floating install only when a newer agent is published — never pulling just to
  diff; an unreachable registry is a hard error, not a blind pull. A daily `harness-images.yml`
  cron rebuilds a harness when its agent updates — both independent of a CLI release.
- **Config precedence: flags > host XDG config (normally `~/.config/vhrn/config.toml`) > defaults**
  (`cli/src/config.rs`, `toml` crate). Config is **host-owned only** — nothing is read from the
  project directory, so repo content can never configure the jail. Global `[tools]` and
  `[resources]` are defaults; singular `[project."<absolute canonical path>"]` blocks may
  override only their tools/resources fields, selected by an exact `pwd -P` cwd match (no
  parent, glob, symlink alias, `~`, `.` or `..`). `blocked_dirs` is global-only and matches the
  resolved cwd **exactly** (not subtree), default `["~","/"]`; project blocks cannot set net.
  `[tools]` (`apt` packages +
  ordered `run` commands) resolves to a content-addressed derived image
  (`vhrn-<h>-tools-<hash>`, `FROM` the harness image: an apt layer then the run steps, as root
  with `HOME=/home/dev` and a final chown — no sudo). Install and actual updates prewarm the
  global plus every distinct normalized project profile deterministically; failures attempt all
  profiles, leave the base operation complete, and return nonzero. PATH is not managed by vhrn —
  the entrypoint sources `~/.profile` at runtime so build-time installers register themselves.
- **The installed registry is host state; shell configuration is user-owned.**
  `install`/`uninstall` mutate `<xdg>/vhrn/installed`, with the root resolved through
  `$XDG_CONFIG_HOME`. Install prints a shell-neutral hint for creating a user-owned alias.
  vhrn never reads or writes shell configuration, including alias files created by older releases.
- The harness binary is baked into the image (native, in `~/.local`; no host install) and
  honors `HTTPS_PROXY`. The entrypoint clears a stale `$PWD/.git/index.lock` on boot (needs
  `procps`).

## Build and test commands

Three parts, each built by its own tool — there is **no root build wrapper**, so invoke
them directly:

- **CLI:** `cargo build --release -p vhrn` → `target/release/vhrn`; `cargo install --path cli` installs
  it to `~/.cargo/bin`.
- **Images:** `make -C image` builds `vhrn-base` then the harnesses (`build-base`,
  `build-claude`, `build-codex`, `build-pi`; each harness is `FROM vhrn-base`, so base first).
  `make -C image build-<name>` builds one; `make -C image clean` removes them.
- **Proxy image:** `make -C proxy` builds `vhrn-proxy`; `make -C proxy clean` removes it.

The image Makefiles auto-detect the engine (`container`, then `docker`; `ENGINE=docker`
forces Docker). Baked into `vhrn-base`: a C/C++ toolchain (clang/lld/llvm/libc,
gcc/g++/cmake/ninja), python3/uv, gh, ripgrep/fd, zip/unzip, plus
openssh-client/wget/rsync/xz-utils/gnupg/sqlite3 and nftables — a non-root `dev` user, no sudo.

Day to day you build nothing — `vhrn install <harness>` pulls prebuilt images from ghcr.
For a local-image dev loop: `cargo install --path cli && make -C image && make -C proxy`, then
`vhrn install claude --local`.

**CI/CD** (`.github/workflows/`): `ci.yml` is the path-filtered PR gate behind a single
`ci-gate` and runs the full validation suite on master without publishing; `release.yml`
publishes `vX.Y.Z`+`latest` images + a GitHub Release on a `v*` tag. Reusable workflows
separately own tests, CLI binaries, the proxy, and base/harness images (`_test`,
`_build-binaries`, `_build-proxy`, `_build-harness-images`), plus `pages.yml`.
See `docs/runbooks/release.md`.

## Code style guidelines

- **Rust** (`cli/src/` and `proxy/src/`, packages `vhrn` and `vhrn-proxy`,
  `#![forbid(unsafe_code)]`): the code is `cargo fmt`-clean
  (enforced in CI with default settings — no `rustfmt.toml`); reach for `#[rustfmt::skip]`
  only on aligned test-case tables. Comments explain *why*, terse, one line where it fits.
  Group `use` imports std / external / crate, blank-line separated. Prefer small single-file
  helpers over new modules. Keep pure logic (arg assembly, hashing, merges, matching) in
  testable functions; split env reads into a thin edge + a pure resolver so tests never
  touch process env. Errors bubble via `anyhow`.
- **Bash/sh** (entrypoint, `pages/install.sh`): `#!/usr/bin/env bash`/`sh` + `set -euo
  pipefail`; comments terse, one line where possible; helpers early-`return 0` when a source
  path is absent. Kept shellcheck-clean.
- **Commits:** Use Linux kernel style (`scope: imperative command`). Write the subject as an
  instruction to edit the repository. A complete subject names both the codebase artifact and the
  edit applied to it, using a repository-edit verb such as `add`, `move`, `split`, `extract`,
  `replace`, `remove`, or `rename`. Keep the message concise: a short subject plus at most one or
  two body lines.
  - **Examples of good commit messages:**
    - `api: add pagination middleware to collection endpoints`
    - `database: split customer addresses into normalized tables`
    - `web: extract checkout form into reusable component`
  - **Reasoning:** States the repository mutation directly, so the commit history reads as a
    sequence of concrete operations that transformed the project.

## Testing instructions

The suite runs per changed component on PRs and in full on master:

- **CLI:** `cargo fmt --all -- --check`, then `cargo clippy -p vhrn --all-targets -- -D warnings`,
  then `cargo test -p vhrn` (fmt runs before clippy).
- **Proxy:** `cargo fmt --package vhrn-proxy -- --check`, then
  `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings`,
  `cargo test -p vhrn-proxy --locked`, `cargo audit --deny warnings`, and
  `cargo build --release --locked -p vhrn-proxy`. The Dockerfile builds the optimized static
  executable and verifies that it has no interpreter or dynamic dependencies before copying it
  into the `scratch` production image.
- **Workflows:** `actionlint` (with shellcheck on inline `run:` scripts) validates
  `.github/workflows/**`.

Tests cover flag parsing, the history-key encoding, terminal env, allowlist add/dedup and typed
loopback authorities, broker authentication/lifetime, engine routing, Pi seed/mirror/session
descriptors,
engine-inspect IP parsing, the harness registry, the installed registry, install/uninstall
messages and arg assembly, the guide composition and its source chain, the system
config layer (host copy, trust-table strip, sandbox-mode injection, env-policy yielding),
credential-env forwarding,
per-project session stores, the persistence state store (creds bootstrap + `.claude.json`
merge), the mount topology, TOML config load/merge, `blocked_dirs`, net-mode resolution, and
tools-layer hashing — plus the proxy's allowlist-matching and IP-classifier tests. Keep pure
logic in functions that take their inputs as arguments so new behavior stays unit-testable
without a live container.

The unit tests **don't exercise a live container**. To verify the full run path end-to-end,
`vhrn install <harness>` (or `make -C image && make -C proxy` then `--local`), then run
`vhrn <harness>` in a throwaway project directory.

## Security considerations

The whole point is that an agent can run without reaching the rest of the host or
exfiltrating freely. Guard these:

- **The egress guard is enforced from outside `dev`'s reach.** The entrypoint installs a
  default-deny nftables ruleset as root (egress only to the proxy), then drops to `dev` via
  `setpriv`; the uid transition clears capabilities and there is no sudo, so `dev` cannot
  alter the firewall. **This is why sudo was removed — do not reintroduce it.** If `nft`
  can't run, the entrypoint **aborts** rather than fall through to an unguarded session. The
  container must run with `--cap-add CAP_NET_ADMIN` or `nft` fails with a netlink error.
- **Egress policy is host-owned.** `${XDG_STATE_HOME:-~/.local/state}/vhrn/net` holds locked,
  atomically written state mounted **only into the proxy, never the container**. Each run
  composes immutable base, selected harness, global, exact-project, and run-only `--allow` layers;
  mode is per active run. `vhrn net` mutates only global/project layers, while `--allow` and
  `--open-net` are run-only. `vhrn net open` changes every active run; future runs are unaffected.
  Keep policy outside config and install: `[net]` and install seeding are removed.
- **Loopback egress is a separate, explicit capability.** `--allow --local` is run-only and
  `vhrn net allow|deny [--project <path>] --local <authorities>` mutates global or exact-project
  policy. Authorities require ports and are only `localhost`, exact `127/8`, or `[::1]`; numeric
  addresses stay distinct from `localhost`. `open` and `report` never grant loopback access.
  Every run starts the generic host broker, even with zero grants. The proxy alone mounts its
  per-run token, and the broker caps a run at 128 connections.
- **The broker is host-side and capability-gated.** `cli/src/broker.rs` authenticates a proxy-only
  per-run token and rechecks the three loopback policy layers before an exact loopback dial;
  `cli/src/run.rs` owns engine routing and cleanup, and `proxy/` routes authorized local requests.
  SIGTERM tears down agent, proxy, broker, and policy. A SIGKILL closes the broker listener and
  relays, but its containers and token staging require explicit cleanup; lease reaping retires
  only run policy.
- **Brokered routing is verified on Apple `container` and Docker through Colima only.** Native
  Linux Docker, Docker Desktop, and remote Docker endpoints are unsupported for brokered runs
  until separately implemented and verified.
- **The container stays ephemeral (`--rm`).** A fresh, tamper-proof firewall every boot — a
  security feature. Persistence is a property of what's mounted; do **not** move to a
  persistent "container machine."
- **The proxy is the security-critical component** — a static Rust binary in a `scratch` image
  (no shell, no userland), running unprivileged: minimal CVE surface. It matches on hostname
  and does **not** terminate TLS, so it can't stop exfiltration to an already-allowed domain
  or domain-fronting behind an allowed CDN.
- **Only the project and the user's agent configuration are mounted.** The config side is
  the Claude/Codex config dir, Pi's selected mirrors under `~/.pi/agent`, the vendor-neutral
  `~/.agents`, and `~/.gitconfig` — each as a disposable copy, never the host original.
  `~/.ssh`, your other projects, and the rest of `$HOME` stay outside; `blocked_dirs` refuses to
  jail `$HOME` or `/`. Config trees are synced with `rsync -aL`, so a symlink inside one is
  followed to its target — the user curates those trees, and anything they link in is a deliberate
  choice.
- **Threat model** (full version in `docs/sandbox-design.md`): protects the host filesystem
  and against casual exfiltration. Does **not** cover exfiltration to an allowed domain,
  sessions launched with `--open-net` or changed with `net open`, executable config inside a repo the
  user has **trusted** (a project `.codex/config.toml`, a project skill), or a container
  escape under Docker (Docker shares the host kernel; Apple `container` gives each container
  its own lightweight VM).
