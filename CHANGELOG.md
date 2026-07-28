# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- fish no longer has its alias injected into `config.fish`. `vhrn install` writes a
  vhrn-owned `$XDG_CONFIG_HOME/fish/conf.d/vhrn.fish` (the convention fish's `conf.d`
  exists for, as rustup and friends use), and `vhrn uninstall` deletes it. The path now
  honors `$XDG_CONFIG_HOME` instead of assuming `~/.config`.

  **Upgrading:** the old block is left where it is — delete the
  `# >>> vhrn managed aliases >>>` … `# <<< vhrn managed aliases <<<` block from your
  `config.fish` by hand, or the alias is defined twice.

## [0.3.0] - 2026-07-25

### Added

- The base image now bundles more of the tools an agent reaches for: gcc/g++
  (`build-essential`), `cmake`, `ninja-build`, `openssh-client`, `wget`, `rsync`, `xz-utils`,
  `gnupg`, `sqlite3`, and `patch`.

### Changed

- The `[toolchains]` config section (language runtimes provisioned with mise) is replaced by
  `[tools]`. `apt = [...]` installs Debian packages and `run = [...]` runs arbitrary
  build-time commands (vendor installers, tarballs, a private mirror), baked onto the harness
  image at build time and content-addressed so they build once. Pin language runtimes the way
  you would on any Linux box — a `run` line for rustup, a Node tarball, and so on. Old
  `[toolchains]` config is silently ignored; move your entries to `[tools]`.

- `vhrn update` now asks the registry whether a newer agent is published and re-pulls only when
  one is, instead of re-pulling every time and diffing image digests to find out. An up-to-date
  harness reports `already current` with no pull (and no `pulling …` noise). The check is a
  metadata-only query over the standard OCI bearer-challenge flow, so it works on any registry,
  not just GHCR. A registry that can't be reached is now a hard error (non-zero exit) rather
  than a blind re-pull — you can't update an offline machine.

### Removed

- mise is no longer bundled in the base image, and `mise.jdx.dev` is dropped from the default
  egress allowlist. Provision language runtimes through `[tools]` at build time instead.

### Fixed

- GitHub Release notes are taken from the release's `CHANGELOG.md` section rather than
  auto-generated from merged pull-request titles.

- Git no longer fails intermittently inside the container with `fatal: detected dubious
  ownership in repository at …`. Apple Container's virtiofs reports the project mount's
  ownership inconsistently, which tripped git's ownership check at random — hitting the agent's
  own `git status`/`commit` as readily as a statusline's git segment, which would show, drop,
  and reappear. The base image now exempts the mounted project. Re-run
  `vhrn install <harness>` to pick up the rebuilt image.

## [0.2.0] - 2026-07-24

### Security

- Remove the project-level `./.vhrn.toml` config layer, a sandbox-escape vector. It was read
  host-side before the container launched, so a `.vhrn.toml` committed to any repository was
  trusted and obeyed on the first `vhrn <harness>` run in it — able to disable the egress guard
  (`net.mode = "open"`) or permanently widen the host allowlist (`net.allow`). `git clone
  <repo> && vhrn <harness>` was the whole exploit.

### Changed

- Configuration is host-owned only. Precedence is now flags > `~/.config/vhrn/config.toml` >
  defaults; nothing is read from the project directory. Per-project settings that lived in
  `./.vhrn.toml` (`toolchains.tools`, `net.allow`, `net.mode`, `run.blocked_dirs`) must move
  into the global config — a host-owned `[project."<path>"]` form is planned (see
  `docs/plans/per-project-config.md`).

## [0.1.0] - 2026-07-23

### Added

- `vhrn update [<harness>...]` re-pulls installed harnesses (and their proxy) to the newest
  agent and reports the version move; pinned and `--local` installs are reported and skipped.
- `harness-images.yml`: a daily cron (with a `force` dispatch) that rebuilds a harness image
  when its agent updates, independent of a CLI release.
- Harness images carry the agent version as an `org.opencontainers.image.version` label and
  as `<agent-version>` / `<agent-version>-<date>` tags; `vhrn list` shows the resolved version.

### Changed

- A harness's `@version` is now the **agent's** version (e.g. `claude@2.1.30`); harness images
  no longer carry a `vX.Y.Z` tag.
- The `vhrn-proxy` image is pinned to the CLI binary's own version rather than the harness
  version, so upgrading the CLI upgrades its proxy.
- The nightly binary version derives from `Cargo.toml` (`<version>-nightly.<date>.<sha>`).

### Fixed

- The release and nightly publish jobs resolve the repository via `GH_REPO`, so `gh release
  create` no longer dies with "not a git repository"; the rolling nightly now publishes.
- A `toolchains.tools` derived image rebuilds when its harness image updates (the base image
  identity is folded into the toolchain hash), so `vhrn update` no longer keeps the old agent.

[unreleased]: https://github.com/aravind-n/vhrn/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/aravind-n/vhrn/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/aravind-n/vhrn/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/aravind-n/vhrn/releases/tag/v0.1.0
