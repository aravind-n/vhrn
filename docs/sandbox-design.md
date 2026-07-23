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

The policy lives on the host, under `~/.cache/vhrn/net/`, and is mounted into the proxy
but **never** into the container. That is what stops an in-container process from
widening its own egress, even under skip-permissions. Edit it from the host while a
container is running and the proxy picks up the change on its next request, no restart
needed:

```sh
vhrn net status                 # current mode + allowlist size
vhrn net allow docs.rs api.x.io # add domains (takes effect immediately)
vhrn net denied                 # domains blocked this session
vhrn net open                   # drop the guard (allow all)
vhrn net guard                  # re-enable enforcement
```

`vhrn install` seeds the allowlist with the base defaults plus the harness's own
domains. Edit `~/.cache/vhrn/net/allowlist` to change it. Per-session overrides
(`--allow`, `--open-net`) and the `[net]` config block are covered in the README.

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
settings) is synced into `~/.cache/vhrn/sandbox/` on each run and layered on top of the
persistent store, so edits to that copy don't survive — change your real host config
instead (e.g. `~/.claude` for Claude). The persistent store is separate and is never
touched by the sync. Session history is written back to the host so in-container and
native sessions share it.

## Working inside the container

- There is no sudo inside the container; removing it is what keeps the egress firewall
  tamper-proof. Install tools in user space instead — `mise use -g <tool>` for runtimes,
  `uv tool install <pkg>` for Python CLIs — or declare them under `[toolchains]` in your
  config. A basic C toolchain (clang, libc headers) is baked into the base image, since
  native builds can't fetch one under the egress guard.
- `gh` auth is forwarded as an env token (`$GH_TOKEN` or `$GITHUB_TOKEN`, else
  `gh auth token`), which covers git-over-HTTPS inside the container. SSH remotes stay
  unauthenticated. Under an open guard, the wrapper warns that a token is present.
- Your host `~/.gitconfig` is copied in so in-container commits use your name and email.
  Change the host file if you want a change to stick.

## Threat model

**What it protects:**

- Your host filesystem. Secrets and your other projects are never mounted, so nothing
  inside the container can read or damage them.
- Against casual exfiltration. Default-deny egress stops a prompt injection from POSTing
  your source to an outside server; it can only reach the domains you have allowed.

**What it doesn't:**

- Exfiltration to a domain you have already allowed. The proxy matches on hostname and
  doesn't terminate TLS, so it can't stop data being pushed to an allowed domain (a repo
  on `github.com`, for instance) or domain-fronted behind an allowed CDN.
- Sessions run with `--open-net` (or `net.mode = "open"`), which turn the guard off.
- A container escape under Docker, where the container shares the host's kernel. Apple
  `container` puts each container in its own lightweight VM, a stronger boundary.
