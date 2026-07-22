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

## One-time repo setup

These live in GitHub settings and can't be committed:

- **Maintainer-only releases** — Settings → Rules → Rulesets: target `refs/tags/v*`,
  restrict tag creation/update/deletion, and limit the bypass list to maintainers. Only
  they can then create a release tag.
- **Release approval** — Settings → Environments → `release`: add the maintainers as
  required reviewers. The release workflow's `approve` job waits on this.
- **Required check** — Settings → Branches (or a ruleset): require the **`ci-gate`** check
  on `master`. Require only that one — the per-component jobs skip by design, and requiring
  a job that can skip would wedge merges.
- **Pages** — Settings → Pages: set the source to **GitHub Actions**. `pages.yml` then
  serves the installer at `https://aravind-n.github.io/vhrn/`.
