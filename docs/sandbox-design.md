# How the sandbox works

vhrn sandboxes two things: the **filesystem** (only the current project is bind-mounted,
at its real path) and **network egress** (default-deny, allowlist-only). This document
covers the egress mechanics, how state persists across runs, working inside the
container, and what the sandbox does and does not protect against.

## Network egress guard

Every run starts a small proxy sidecar. The container's firewall routes every outbound
connection through that proxy, and the proxy only allows allowlisted domains. Everything
else, including direct DNS, is refused. A blocked request fails with the domain named,
like `blocked by vhrn egress policy: example.com`.

The policy lives on the host at `${XDG_STATE_HOME:-~/.local/state}/vhrn/net`, with a store lock,
atomic same-directory writes, and active-run leases. It is mounted
into the proxy but **never** into the container. Five additive layers are evaluated in order:
immutable base, selected harness, persistent global, exact canonical project, and run-only wrapper
`--allow`. Mode belongs to each active run. This keeps an in-container process from widening its
own egress, even under skip-permissions. The proxy rereads every required layer and mode before a
new decision; a required-read failure enforces an empty policy. Existing tunnels stay open.

```sh
vhrn net status --domains       # persistent scopes, active runs, and provenance
vhrn net allow docs.rs api.x.io # global policy (live for active runs)
vhrn net allow --project . api.x.io # exact canonical project policy
vhrn net denied                 # denials since the last idle period
vhrn net open                   # set every active run open
vhrn net guard                  # set every active run enforce
```

`--allow` and `--open-net` are run-only. `open`, `guard`, and `report` change active runs only;
future runs default to enforce. `allow`/`deny` normalize ASCII domain input (use IDNA/punycode for
internationalized names), and parent domains match subdomains. `deny` removes no other layer and
reports remaining provenance. `[net]` configuration and install-time policy seeding are removed.
Stale `[net]` yields a targeted migration error, while `vhrn net` remains usable.

vhrn synchronously creates the named agent container before start/attach. On SIGTERM, cleanup
removes the agent, then the proxy, then the run policy; a SIGKILL cannot run cleanup, so lease
reaping removes stale active-run state later. This does not close existing proxy tunnels.

## Login and state persistence

Each harness has a persistent store at `~/.cache/vhrn/state/<harness>/`, mounted as the
harness's config dir inside the container. A login, refreshed credentials, and trust
state live there and survive across runs — one login serves every project. The store is
authoritative once populated: your host login is copied in **only** to bootstrap an
empty store, so an in-container login is never overwritten.

The container stays ephemeral (`--rm`) — a fresh, tamper-proof firewall is installed on
every boot. Persistence is a property of what's mounted, not of container lifetime.
(Caveat: an in-container token refresh doesn't flow back to the host.)

A disposable copy of your host harness config (skills, commands, agents, harness
settings) is synced into `~/.cache/vhrn/sandbox/<harness>/` on each run and layered on top
of the persistent store, so edits to that copy don't survive — change your real host config
instead (e.g. `~/.claude` for Claude, `~/.codex` for Codex). The persistent store is
separate and is never touched by the sync.

What else persists is per-harness, because the agents differ:

- **Claude** writes session history back to the host, so in-container and native sessions
  share one history.
- **Codex** keeps every project's transcripts in one flat tree, which vhrn does not mount —
  it would hand a jailed agent every transcript on the machine. Instead each project gets
  its own session store, so `codex resume` sees this project's sessions and no others. The
  databases that follow that store carry Codex's memories and goals too, so those are
  per-project as well; the login and the config stay shared across all of them.
- **Codex's `config.toml`** is written by the agent — it records which projects you trusted
  and which notices you dismissed — so vhrn never writes that file. Your host copy is
  mounted a layer *below* it, at `/etc/codex/config.toml`, together with vhrn's own
  own settings. It is read-only and is not a file Codex writes to, so an answer you give
  inside the container is the one that sticks. The host copy's project-trust entries are
  stripped on the way in: trusting a folder on your host does not trust it in the jail.
- **Codex's own sandbox is turned off** (`sandbox_mode = "danger-full-access"`), because the
  container, its firewall, and the egress proxy already are the sandbox, and nesting Codex's
  on top costs its shell commands all network access. This is a *default*, so `vhrn codex
  --sandbox workspace-write` still turns it back on for a run. bubblewrap is installed in the
  image either way: Codex aborts rather than degrades when it looks for it and finds nothing.

`~/.agents` — the vendor-neutral config dir several agent tools read, so portable
configuration like a skill library is installed once instead of once per vendor — is copied
in the same way and mounted at `/home/dev/.agents`, for every harness. The whole tree is
carried in, so a directory an agent starts reading later needs no change here. Whether an
agent reads it at all is up to that agent: Codex resolves `~/.agents/skills` as its user
skill root, while Claude Code reads `~/.claude/skills` only, so for Claude the mount is
present and unused.

## Working inside the container

- Resource limits come only from the host-owned `[resources]` config block, with exact-path
  `[project."<pwd -P>".resources]` overrides for one project. `memory` accepts `"engine"` or a
  nonzero integer ending in `m` or `g`; `cpus` is a positive integer. Project fields replace only
  the field they name, while absent fields inherit the global default. When memory is unset, vhrn
  passes `--memory 4g` only to Apple `container`; Docker retains its engine default. Explicit
  limits use long `--memory` and `--cpus` flags. They are not wrapper flags, so an agent's own
  arguments with those names remain untouched.
- The project bind mount preserves installed dependencies and build outputs such as
  `node_modules`, `.venv`, `target`, and generated files. Package-manager caches in the
  container home exist for the current invocation only and are intentionally not mounted across
  runs.
- There is no sudo inside the container; removing it is what keeps the egress firewall
  tamper-proof. The toolchain is baked at build time — a C/C++ toolchain (clang, gcc/g++,
  cmake) plus the common CLIs live in the base image, and you add anything else under
  `[tools]` in your config (`apt` packages or arbitrary `run` install commands), or under an
  exact-path project override. Arrays replace rather than append. vhrn prewarms the global and
  each distinct effective project profile at install and actual update time; a run retains a lazy
  build fallback. At runtime you can still `uv tool install
  <pkg>` for Python CLIs (PyPI is allowlisted), but you cannot apt or add system packages
  under the guard.
- `gh` auth is forwarded as an env token (`$GH_TOKEN` or `$GITHUB_TOKEN`, else
  `gh auth token`), which covers git-over-HTTPS inside the container. SSH remotes stay
  unauthenticated. Under an open guard, the wrapper warns that a token is present.
- Your host `~/.gitconfig` is copied in so in-container commits use your name and email.
  Change the host file if you want a change to stick.

## Threat model

**What it protects:**

- Your host filesystem. Only the project and your agent configuration are mounted —
  `~/.ssh`, your other projects, and the rest of `$HOME` are not, so nothing inside the
  container can read or damage them. The configuration that does come in (the harness's
  own dir, `~/.agents`, `~/.gitconfig`) is a disposable copy, so the container cannot
  write back to your real config either.
- Against casual exfiltration. Default-deny egress stops a prompt injection from POSTing
  your source to an outside server; it can only reach the domains you have allowed.

**What it doesn't:**

- Exfiltration to a domain you have already allowed. The proxy matches on hostname and
  doesn't terminate TLS, so it can't stop data being pushed to an allowed domain (a repo
  on `github.com`, for instance) or domain-fronted behind an allowed CDN.
- Repo content you have **trusted**. Once you answer an agent's trust prompt for a project,
  parts of that repo become executable configuration: a project `.codex/config.toml` can
  declare MCP servers and hooks, and a project skill under `.agents/skills/` or
  `.claude/skills/` carries its own tool grants. Those run in the container, with the
  forwarded GitHub token and the agent's credentials in reach of an allowed domain. That is
  the same bargain you make running the agent natively — the jail bounds the blast radius
  rather than preventing execution — which is why vhrn does not answer the trust prompt for
  you, on the host's behalf or otherwise. Treat trusting an unfamiliar repo as running its
  code, because it is.
- Sessions launched with `--open-net`, or active runs changed to `net open`, which turn their
  guard off.
- A container escape under Docker, where the container shares the host's kernel. Apple
  `container` puts each container in its own lightweight VM, a stronger boundary.
