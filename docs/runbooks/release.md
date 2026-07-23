# Release runbook

Procedures for cutting releases and refreshing images. All driven from
`.github/workflows/`.

## What each trigger publishes

| Trigger | Workflow | Publishes |
| --- | --- | --- |
| Pull request | `ci.yml` | Lints/tests the changed component; pushes `pr-<n>` base/proxy images for same-repo PRs |
| Push to `master` | `nightly.yml` | `nightly` base/proxy/harness images + a rolling `nightly` prerelease of the binaries |
| Push a `vX.Y.Z` tag | `release.yml` | `vX.Y.Z` + `latest` base/proxy images, the harness images at the agent version + `latest`, and a GitHub Release with the binaries and `SHA256SUMS` |
| Daily cron / dispatch | `harness-images.yml` | Rebuilds each harness FROM the current base and republishes when the agent version changed |

## Cut a release

1. Promote the `Unreleased` section of `CHANGELOG.md` to `## X.Y.Z` with today's date.
2. Bump `version` in `Cargo.toml` to `X.Y.Z` — the release workflow fails if the tag and
   `Cargo.toml` disagree.
3. `cargo build --release` to refresh `Cargo.lock`.
4. Commit the three together and land them on `master`.
5. Tag the landed commit and push it:
   ```sh
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
6. Approve the `release` environment on the run's page in Actions. On approval it publishes
   the images and the GitHub Release, which becomes "latest stable" — `install.sh` picks it
   up by default.

## Choosing the number

`0.MINOR.PATCH`; the leading zero stays until 1.0.

- **Minor** if a user must change something they wrote or typed: a renamed or removed flag
  or subcommand, a `.vhrn.toml` key, a state-file format, or what an image tag means.
- **Patch** otherwise: fixes, additive flags, new allowlist domains, base tooling, docs.

## Refresh a harness image

The daily cron rebuilds each harness and republishes only when the agent version changed.
Force one now (e.g. to pick up a base change) by dispatching it:

```sh
gh workflow run harness-images.yml -f force=true
```

Without `force`, an unchanged agent version is a no-op.

## Roll back

Pin an older agent version — a pin, so `vhrn update` then leaves it alone:

```sh
vhrn install claude@<agent-version>
```

Return to tracking the newest with `vhrn install claude`. Roll the CLI itself back by pinning
the installer: `curl -fsSL <install.sh> | VHRN_VERSION=vX.Y.Z sh`.

## If a release fails partway

Publish steps are idempotent — re-run the failed job from the run's page in Actions.

- **A build or push failed:** re-run; image pushes and `gh release upload --clobber`
  overwrite cleanly.
- **The tag pushed but no run started:** re-push the tag (`git push origin :vX.Y.Z`, then
  re-tag and push).
- Re-running after the approval gate needs another approval.

## Image tags

| Image | Tags |
| --- | --- |
| `vhrn-base`, `vhrn-proxy` | `vX.Y.Z`, `latest` (release) · `nightly`, `sha-<sha>` (master) · `pr-<n>`, `pr-<n>-<sha>` (PR) |
| `vhrn-claude` and future harnesses | `<agent-version>`, `<agent-version>-<date>`, `latest` (release / cron) · `nightly` (master) · `pr-<n>`, `pr-<n>-<sha>` (PR) |
