# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- GitHub Release notes are taken from the release's `CHANGELOG.md` section rather than
  auto-generated from merged pull-request titles.

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

[unreleased]: https://github.com/aravind-n/vhrn/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/aravind-n/vhrn/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/aravind-n/vhrn/releases/tag/v0.1.0
