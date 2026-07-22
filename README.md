# vhrn (Virtual Harness)

Run coding agents inside a container jailed to the current project directory, with
**default-deny network egress**. Claude Code is the harness that ships today; the CLI
is harness-agnostic (`vhrn install <harness>` / `vhrn <harness> …`), so more agents
can be added as thin images.

Only the current project is mounted into the container, so `~/.ssh`, your other projects,
and the rest of your home directory stay outside it. Outbound traffic is limited to
an allowlist. Between them, those two things let you run
`--dangerously-skip-permissions` without a prompt injection being able to reach the
rest of your machine or push your project somewhere it shouldn't go.

The project is bind-mounted at its real path. Each harness keeps a persistent,
container-owned state store (login, credentials, trust) so an in-container login sticks across
runs; a disposable copy of your `~/.claude` config (skills, commands, agents,
settings) is synced in on each run and layered on top, and session history is written
back to `~/.claude/projects/<key>` so in-container and native sessions share history.

## Requirements

- [Apple Container](https://github.com/apple/container) or Docker (auto-detected, `container` first)
- `gh` on the host if you want GitHub auth forwarded (optional)
- Rust 1.85+ / edition 2024 (only to build the CLI from source; the curl installer ships a prebuilt binary)

Claude Code does **not** need to be installed on the host — the agent binary is baked
into the container image.

## Install

Install the CLI, then install a harness (which pulls its images and adds a shell
alias):

```sh
cargo install --path .   # build and install the vhrn binary to ~/.cargo/bin
vhrn install claude      # pull the claude + proxy images, seed egress, add a `claude` alias
```

Or grab a prebuilt binary, then install the harness:

```sh
curl -fsSL https://aravind-n.github.io/vhrn/install.sh | sh
vhrn install claude
```

`vhrn install` pulls prebuilt, versioned images from ghcr — no repo checkout and no
local image build. Pin or roll back a version with `@`:

```sh
vhrn install claude           # the latest release
vhrn install claude@v0.2.0    # a specific release (rollback works the same way)
vhrn install claude@nightly   # the latest master build
```

For the CLI itself, `VHRN_VERSION` pins the installer to a tag (`VHRN_VERSION=nightly`
or `VHRN_VERSION=v0.2.0`); the default is the latest stable release.

For development, build the images locally and install from those instead of pulling:

```sh
make -C image && make -C proxy   # build base/claude + proxy locally
vhrn install claude --local      # register the make-built images
```

Restart your shell (or source your rc file) afterward to pick up the alias.

## Usage

vhrn is subcommand-first. After `vhrn install claude`, a shell alias lets you run
`claude` directly; `command claude` or `\claude` still reaches the real binary.

```sh
vhrn claude                   # guarded: egress limited to the allowlist
vhrn claude --model opus      # forwards --model opus to claude
claude --model opus           # same, via the installed alias
vhrn claude --allow docs.rs   # add domains to the allowlist for this session
vhrn claude --open-net        # drop the guard for this session (all egress)
vhrn claude -- --help         # claude's own help (-- stops wrapper flag parsing)

vhrn list                     # known + installed harnesses
vhrn uninstall claude         # drop the alias/registry entry (add --image to delete the image)
vhrn help
```

Wrapper flags (`--open-net`, `--allow`) go after the harness name and before the
agent's own flags.

## Login and state persistence

Each harness has a persistent store at `~/.cache/vhrn/state/<harness>/`, mounted as
the agent's config dir inside the container (Claude via `CLAUDE_CONFIG_DIR`). A login,
refreshed credentials, and trust state live there and survive across runs — one login
serves every project. The store is authoritative once populated: your host login is
copied in **only** to bootstrap an empty store, so an in-container login is never
overwritten.

The container stays ephemeral (`--rm`) — a fresh, tamper-proof firewall is installed
on every boot. Persistence is a property of what's mounted, not of container lifetime.
(Caveat: an in-container token refresh doesn't flow back to the host.)

## Configuration

Optional TOML config, global under per-project. Precedence: CLI flags > `./.vhrn.toml`
> `~/.config/vhrn/config.toml` > built-in defaults.

```toml
[run]
# Refuse to launch when the cwd is exactly one of these (guards against jailing
# $HOME or /). Matched exactly, so projects *under* $HOME still run.
blocked_dirs = ["~", "/"]

[toolchains]
# Provisioned into the container with mise, as a derived image cached by tool set.
tools = ["go@1.26", "node@22"]

[net]
allow = ["docs.rs"]   # extra allowlist domains
mode  = "enforce"     # enforce | report | open
```

## Network egress guard

Every run starts a small proxy sidecar. The container's firewall routes every outbound
connection through that proxy, and the proxy only allows allowlisted domains.
Everything else, including direct DNS, is refused. A blocked request fails with the
domain named, like `blocked by vhrn egress policy: example.com`.

The policy lives on the host, under `~/.cache/vhrn/net/`, and is mounted into the
proxy but never into the container. That is what stops an in-container process from widening its
own egress, even under skip-permissions. Edit it from the host while a container is running
and the proxy picks up the change on its next request, no restart needed:

```sh
vhrn net status                 # current mode + allowlist size
vhrn net allow docs.rs api.x.io # add domains (takes effect immediately)
vhrn net denied                 # domains blocked this session
vhrn net open                   # drop the guard (allow all)
vhrn net guard                  # re-enable enforcement
```

`vhrn install` seeds the allowlist with the base defaults plus the harness's own
domains. Edit `~/.cache/vhrn/net/allowlist` to change it.

## Adding a harness

A harness is a spec (`src/harness.rs`) plus a thin `FROM vhrn-base`
Dockerfile under `image/<harness>/`, and an entry in the CI publish matrix
(`.github/workflows/_build-images.yml`) so its image lands on ghcr. The spec carries
the image name, in-container command, shell alias, default egress domains, and the
persistence descriptors (state dir, synced config, bootstrap credentials). No fork of
the CLI is required.

## Make targets

`image/Makefile` builds the base and harness images (run it from the repo root as `make
-C image`); `proxy/Makefile` builds the proxy image (`make -C proxy`) — each module owns
its own image. The CLI is built and tested by cargo (`cargo build --release`, `cargo
test`) and the proxy by `cd proxy && go test ./...`.

| Target | Description |
| --- | --- |
| `make -C image` | Build the base + harness images (base + claude) |
| `make -C proxy` | Build the proxy image |
| `make -C image build-base` / `build-claude` | Build one harness image |
| `make -C image clean` / `make -C proxy clean` | Remove the images |
| `make -C image ENGINE=docker ...` | Force Docker instead of `container` |

Day to day you don't need `make` — `vhrn install <harness>` pulls prebuilt images from
ghcr. `make -C image && make -C proxy` builds them locally for development (`vhrn install <harness>
--local` then uses those). CI builds and pushes the images (`nightly` on master,
`vX.Y.Z` + `latest` on a tag); `VHRN_REGISTRY` overrides the registry the CLI pulls from.
See [docs/RELEASING.md](docs/RELEASING.md) for the CI/CD flow.

## Threat model

What it protects:

- Your host filesystem. Secrets and your other projects are never mounted, so nothing
  inside the container can read or damage them.
- Against casual exfiltration. Default-deny egress stops a prompt injection from
  POSTing your source to an outside server; it can only reach the domains you have
  allowed.

What it doesn't:

- Exfiltration to a domain you have already allowed. The proxy matches on hostname and
  doesn't terminate TLS, so it can't stop data being pushed to an allowed domain (a
  repo on `github.com`, for instance) or domain-fronted behind an allowed CDN.
- Sessions run with `--open-net` (or `net.mode = "open"`), which turn the guard off.
- A container escape under Docker, where the container shares the host's kernel. Apple
  `container` puts each container in its own lightweight VM, a stronger boundary.

## Notes

- There is no sudo inside the container; removing it is what keeps the egress firewall
  tamper-proof. Install tools in user space instead: `mise use -g <tool>` for
  runtimes, `uv tool install <pkg>` for Python CLIs — or declare them under
  `[toolchains]` in your config. A basic C toolchain (clang, libc headers) is baked into
  the base image, since native builds can't fetch one under the egress guard.
- `gh` auth is forwarded as an env token (`$GH_TOKEN` or `$GITHUB_TOKEN`, else
  `gh auth token`), which covers git-over-HTTPS inside the container. SSH remotes stay
  unauthenticated. Under an open guard, the wrapper warns that a token is present.
- The disposable config copy under `~/.cache/vhrn/sandbox/` is re-synced every run, so
  edits to it don't survive — change your real `~/.claude` on the host instead. The
  persistent store under `~/.cache/vhrn/state/` is separate and is never touched by the
  sync.
- Your host `~/.gitconfig` is copied in so in-container commits use your name and email.
  Change the host file if you want a change to stick.
