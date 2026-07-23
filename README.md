# vhrn (Virtualized Harness Runtime)

Run coding agents inside a container jailed to the current project directory, with **default-deny network egress**. Only the current project is mounted — credentials, other projects, and the rest of your home directory stay outside the container — and outbound traffic is limited to an allowlist. The harness binary runs in the container; it is not installed on the host.

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

Restart your shell to pick up the alias. Pin or roll back versions with `@`
(`vhrn install <harness>@v0.2.0`, or `@nightly` for the latest master build);
`VHRN_VERSION` pins the installer.

## Usage

A shell alias runs the harness directly (e.g. `claude` → `vhrn claude`); `command
<harness>` or `\<harness>` reaches the real binary.

```sh
vhrn <harness>                   # guarded: egress limited to the allowlist
vhrn <harness> --allow docs.rs   # add domains to the allowlist for this session
vhrn <harness> --open-net        # drop the guard for this session (all egress)
vhrn <harness> -- --help         # harness's own help (-- stops wrapper flag parsing)

vhrn list                        # known + installed harnesses
vhrn uninstall <harness>         # drop the alias/registry entry (--image also deletes the image)
```

Wrapper flags (`--open-net`, `--allow`) go after the harness name, before the agent's own flags.

## Configuration

Optional TOML, global then per-project. Precedence: CLI flags > `./.vhrn.toml` >
`~/.config/vhrn/config.toml` > built-in defaults.

```toml
[run]
blocked_dirs = ["~", "/"]        # refuse to launch when cwd is exactly one of these

[toolchains]
tools = ["go@1.26", "node@22"]   # provisioned into the container with mise

[net]
allow = ["docs.rs"]              # extra allowlist domains
mode  = "enforce"                # enforce | report | open
```

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
