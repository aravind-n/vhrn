# vhrn (Virtualized Harness Runtime)

Run coding agents inside a container jailed to the current project directory, with **default-deny network egress**. Only the current project and your agent configuration are mounted — SSH keys, other projects, and the rest of your home directory stay outside the container — and outbound traffic is limited to an allowlist. The harness binary runs in the container; it is not installed on the host.

## Requirements

- [Apple Container](https://github.com/apple/container) or Docker (auto-detected, `container` first)
- `gh` on the host for forwarded GitHub auth (optional)
- [Rust](https://rust-lang.org/tools/install/) if building from code

## Getting Started

Install the CLI, then install a harness (pulls its images and adds a shell alias):

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

Each run composes immutable base and selected-harness domains with optional persistent global and
exact-project domains, then run-only wrapper domains. Installing a harness never changes egress
policy.

## Usage

A shell alias runs the harness directly (e.g. `claude` → `vhrn claude`); `command
<harness>` or `\<harness>` reaches the real binary.

```sh
vhrn <harness>                   # guarded: egress limited to the allowlist
vhrn <harness> --allow docs.rs   # add domains to the allowlist for this session
vhrn <harness> --open-net        # drop the guard for this session (all egress)
vhrn <harness> -- --help         # harness's own help (-- stops wrapper flag parsing)

vhrn net status [--domains]      # persistent scopes and active runs (with provenance)
vhrn net allow docs.rs            # persist a global domain
vhrn net allow --project . api.x.io # persist a domain for this canonical project
vhrn net deny api.x.io             # remove from the global mutable scope
vhrn net deny --project . api.x.io # remove from this project's mutable scope
vhrn net denied                  # denials since the last idle period
vhrn net open|guard|report       # change mode for active runs only

vhrn list                        # known + installed harnesses
vhrn update [<harness>]          # re-pull installed harnesses when a newer agent is published
vhrn uninstall <harness>         # drop the alias/registry entry (--image also deletes the image)
```

Wrapper flags (`--open-net`, `--allow`) go after the harness name, before the agent's own flags.
They never persist. `open`, `guard`, and `report` affect active runs only; when idle they report
`no active runs; future runs default to enforce`.

`allow` and `deny` accept ASCII domain names only. vhrn trims and lowercases input, accepts a
leading `*.` and leading/trailing dots as the same domain, and matches a parent domain's subdomains.
Use an IDNA/punycode spelling for an internationalized domain rather than storing Unicode. A denied
batch is atomic: every requested domain must be present in the selected scope. Status and deny
output identify remaining sources rather than treating deny as a negative override.

## Configuration

Optional TOML in `~/.config/vhrn/config.toml`. Precedence: CLI flags >
`~/.config/vhrn/config.toml` > built-in defaults. Config is host-owned only — vhrn reads
nothing from the project directory, so a cloned repo can never reconfigure the jail.

```toml
[run]
blocked_dirs = ["~", "/"]        # refuse to launch when cwd is exactly one of these

[resources]
memory = "4g"                     # nonzero integer + m/g, or "engine"
cpus = 4                           # positive integral CPU count

[tools]                          # extra tooling baked onto the harness image at build time
apt = ["postgresql-client"]      # Debian packages
run = ["curl -fsSL https://sh.rustup.rs | sh -s -- -y"]   # arbitrary build-time install commands

# Exact project overrides; use `pwd -P` for the key.
[project."/Users/me/work/payments".resources]
memory = "8g"

[project."/Users/me/work/payments".tools]
apt = ["postgresql-client", "jq"]

```

Global `[tools]` and `[resources]` are defaults for every project. A singular
`[project."<absolute-canonical-path>".tools]` or `.resources` block may independently replace
`tools.apt`, `tools.run`, `resources.memory`, or `resources.cpus`; an absent field inherits its
global value. Arrays replace rather than append, so `apt = []` explicitly clears the global apt
list for that project.

vhrn canonicalizes the cwd once and matches the project key byte-for-byte. Entries do not apply to
children, glob patterns, or symlink spellings; keys cannot use `~`, `.` or `..`. Use `pwd -P` to
obtain the spelling to configure. Declared project paths need not exist when vhrn parses config or
prewarms images. Project blocks cannot set `run.blocked_dirs`; it is global-only.

`[net]` has been removed from configuration. A retained top-level `[net]` produces a targeted
error: first move persistent domains with `vhrn net allow`, then remove `[net]` and retry. `vhrn
net` itself remains usable while that configuration error blocks other commands. For a clean reset,
stop active runs, remove `${XDG_STATE_HOME:-~/.local/state}/vhrn/net`, and recreate only the global
or project domains you want with `vhrn net allow`; this is a reset, not an automatic migration.

Resource limits are host-owned configuration, not wrapper flags. An unset `memory` makes
vhrn pass `--memory 4g` only to Apple's `container`; Docker keeps its engine default.
`memory = "engine"` leaves either engine unchanged. Explicit values use the portable long
`--memory` and `--cpus` forms. Agent arguments named `--memory` or `--cpus` still pass through
unchanged after the harness command.

Install and actual image updates prewarm the global effective tools profile and every distinct
project profile, independent of the directory from which the command is run. Equivalent normalized
apt/run profiles build once; vhrn attempts all profiles and reports the affected paths. A failed
profile does not undo the base install or update, but the command exits nonzero. Running a harness
still lazily builds its selected profile if it was not already cached.

The host project mount preserves project-local installed dependencies and outputs such as
`node_modules`, `.venv`, `target`, and generated files. Package-manager caches under the
container home last only for that invocation and are intentionally not mounted between runs.

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
