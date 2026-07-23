# Changelog

All notable changes to vhrn are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning is `0.MINOR.PATCH`
(see `docs/runbooks/release.md`).

## Unreleased

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
