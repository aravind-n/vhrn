# vhrn (Virtualized Harness Runtime)

Run coding agents inside a container jailed to the current project directory, with **default-deny network egress**. Only the current project and your agent configuration are mounted — SSH keys, other projects, and the rest of your home directory stay outside the container — and outbound traffic is limited to an allowlist. The harness binary runs in the container; it is not installed on the host.

## Requirements

- [Apple Container](https://github.com/apple/container) or Docker (auto-detected, `container` first)
- `gh` on the host for forwarded GitHub auth (optional)
- [Rust](https://rust-lang.org/tools/install/) if building from code

## Getting Started

Install the CLI, then install a harness (pulls its images, seeds egress, adds a shell alias):

```sh
curl -fsSL https://aravind-n.github.io/vhrn/install.sh | sh
vhrn install <harness>
```

Restart your shell to pick up the alias. Pin or roll back a harness to a specific agent
version with `@` (`vhrn install claude@2.1.30`, or `@nightly` for the latest master build),
and `vhrn update` re-pulls installed harnesses only when the registry has a newer agent.
`VHRN_VERSION` pins the CLI installer.

| Harness | Agent | Logging in |
| --- | --- | --- |
| `claude` | Claude Code | Your host login bootstraps an empty store, once |
| `codex` | OpenAI Codex | `codex login --device-auth` inside the container, once |

Codex uses device-auth because the browser callback port isn't reachable from inside the
container, and because it mints the container its own token instead of sharing your host
one. Either login persists across runs and serves every project. For non-interactive use,
`CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`, and `OPENAI_API_KEY` are forwarded from the host
when set. vhrn defaults to hiding them from commands the agent spawns; set
`[shell_environment_policy]` in your host config and yours wins.

Harnesses share one egress allowlist, so installing a second one widens egress for the
first. `vhrn net status` shows the current list.

## Usage

A shell alias runs the harness directly (e.g. `claude` → `vhrn claude`); `command
<harness>` or `\<harness>` reaches the real binary.

```sh
vhrn <harness>                   # guarded: egress limited to the allowlist
vhrn <harness> --allow docs.rs   # add domains to the allowlist for this session
vhrn <harness> --open-net        # drop the guard for this session (all egress)
vhrn <harness> -- --help         # harness's own help (-- stops wrapper flag parsing)

vhrn list                        # known + installed harnesses
vhrn update [<harness>]          # re-pull installed harnesses when a newer agent is published
vhrn uninstall <harness>         # drop the alias/registry entry (--image also deletes the image)
```

Wrapper flags (`--open-net`, `--allow`) go after the harness name, before the agent's own flags.

## Configuration

Optional TOML in `~/.config/vhrn/config.toml`. Precedence: CLI flags >
`~/.config/vhrn/config.toml` > built-in defaults. Config is host-owned only — vhrn reads
nothing from the project directory, so a cloned repo can never reconfigure the jail.

```toml
[run]
blocked_dirs = ["~", "/"]        # refuse to launch when cwd is exactly one of these

[tools]                          # extra tooling baked onto the harness image at build time
apt = ["postgresql-client"]      # Debian packages
run = ["curl -fsSL https://sh.rustup.rs | sh -s -- -y"]   # arbitrary build-time install commands

[net]
allow = ["docs.rs"]              # extra allowlist domains
mode  = "enforce"                # enforce | report | open
```

Your harness config dir (`~/.claude`, `~/.codex`) and the vendor-neutral `~/.agents` are
copied into the container on each run, the latter at `/home/dev/.agents` for every harness.
Both copies are disposable — edit the host directories, not the copies. Whether an agent
reads `~/.agents` is up to that agent: Codex resolves `~/.agents/skills` as its user skill
root, while Claude Code reads `~/.claude/skills` only, so for Claude the mount is inert.

Codex's own `~/.codex/config.toml` is applied as *defaults* rather than as your config
layer, so that the copy the agent writes inside the container — trust answers, dismissed
notices — is the one that persists. Edit the host file to change a setting; its
`[projects.*]` trust entries are deliberately left behind, so trusting a folder on the host
does not trust it in the jail.

## Building from source

| Part | Source | Build | Test |
| --- | --- | --- | --- |
| CLI (`vhrn`) | `src/` (Rust) | `cargo build --release` | `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` |
| Container images | `image/` (base + harnesses) | `make -C image` | — |
| Egress proxy | `proxy/` (Go) | `make -C proxy` | `cd proxy && go test ./...` |

`cargo install --path .` installs the CLI to `~/.cargo/bin`. To iterate on images
locally, build them and register with `--local` instead of pulling from ghcr:

```sh
make -C image && make -C proxy
vhrn install <harness> --local
```

## Documentation

Project documentation is stored in `docs/`. This includes design discussions, contribution guidelines, and runbooks

## License

[MIT](LICENSE)
