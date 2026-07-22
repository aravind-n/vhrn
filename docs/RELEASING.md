# Releasing

Three channels, all driven from `.github/workflows/`:

| Trigger | Workflow | Publishes |
| --- | --- | --- |
| Pull request | `ci.yml` | Lints/tests the changed component; pushes `pr-<n>` images for same-repo PRs |
| Push to `master` | `nightly.yml` | `nightly` images + a rolling `nightly` prerelease of the binaries |
| Push a `vX.Y.Z` tag | `release.yml` | `vX.Y.Z` + `latest` images + a GitHub Release with binaries and `SHA256SUMS` |

## Cut a release

1. Bump `version` in `Cargo.toml` to `X.Y.Z` and commit — the release workflow fails if
   the tag and `Cargo.toml` disagree.
2. Tag and push:
   ```sh
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
3. The workflow runs the full suite, then **pauses on the `release` environment** for a
   maintainer to approve (in the run's page).
4. On approval it publishes the images (`vX.Y.Z` + `latest`) and the GitHub Release, which
   becomes "latest stable" — so `install.sh` picks it up by default.

Roll back by installing an older tag: `vhrn install claude@vX.Y.Z`.

## Nightlies

Every push to `master` republishes a single rolling `nightly` prerelease (binaries
versioned `0.0.0-nightly.<date>.<sha>`) and `nightly`-tagged images. Being a prerelease,
the installer's default (latest stable) ignores it. Grab one with:

```sh
curl -fsSL https://aravind-n.github.io/vhrn/install.sh | VHRN_VERSION=nightly sh
vhrn install claude@nightly
```
