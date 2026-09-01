# Complete Codex package installation

Implementation plan for replacing the Codex image's single-binary download with the complete,
version-locked Codex package. Reviewed against `f97c11d` and the official Codex CLI installation
documentation on 2026-08-31.

The finished behavior has five properties:

- the image contains Codex's complete native package, including `codex-code-mode-host` and packaged
  resources;
- amd64 and arm64 variants under one image tag contain the same Codex version;
- that version matches the image tag and `org.opencontainers.image.version` label;
- published builds verify the architecture-specific release-asset digests resolved before the
  multi-arch build starts;
- Codex stays a native, non-root install under `/home/dev/.local`, with no Node runtime and no
  runtime updater.

## Problem

`image/codex/Dockerfile` currently downloads
`codex-<target>-unknown-linux-musl.tar.gz`, requires the archive to contain exactly one file, and
renames that file to `/home/dev/.local/bin/codex`. That assumption is now false at the product
boundary. A current complete Codex package has a layout like:

```text
bin/
  codex
  codex-code-mode-host
codex-package.json
codex-path/
  rg
codex-resources/
  zsh/bin/zsh
```

The exact resource contents may grow. The package manifest names the entrypoint, resource directory,
and packaged PATH directory, so those files are one distribution rather than optional extras. The
current build still passes `codex --version` while omitting everything except the entrypoint; Codex
then warns at runtime that Code Mode failed closed because its sibling host executable is absent.

As of the review date, the [official Codex CLI documentation][codex-install] recommends OpenAI's
standalone installer for macOS and Linux. That is the compatibility baseline: vhrn must consume the
same complete package shape. A published container needs a stronger version contract than the
end-user `curl | sh` command provides, however, so the image will consume the complete versioned
release package directly rather than execute a mutable latest-version installer during the build.

[codex-install]: https://learn.chatgpt.com/docs/codex/cli

## Package contract

### Release assets

Use the complete package assets from the stable `rust-vX.Y.Z` release:

| BuildKit architecture | Codex target | Asset |
| --- | --- | --- |
| `amd64` / `x86_64` | `x86_64-unknown-linux-musl` | `codex-package-x86_64-unknown-linux-musl.tar.gz` |
| `arm64` / `aarch64` | `aarch64-unknown-linux-musl` | `codex-package-aarch64-unknown-linux-musl.tar.gz` |

Reject every other architecture. Keep the musl build: it is self-contained, needs no Node runtime,
and matches the existing image contract.

The installer must select an asset by its exact name from the release metadata. A missing asset,
duplicate name, absent digest, or digest other than `sha256:<hex>` is a hard build error. Do not fall
back to the legacy `codex-*.tar.gz`; a successful incomplete install is worse than a failed image
build.

### Destination layout

Extract the archive root unchanged into `/home/dev/.local` as `dev`. This produces:

```text
/home/dev/.local/bin/codex
/home/dev/.local/bin/codex-code-mode-host
/home/dev/.local/codex-package.json
/home/dev/.local/codex-path/...
/home/dev/.local/codex-resources/...
```

`vhrn-base` already puts `/home/dev/.local/bin` first on `PATH`, and the runtime state mount covers
`/home/dev/.codex`, not `.local`, so the package survives unchanged. Preserve the package's relative
layout: Codex locates the manifest, sibling executables, resources, and packaged PATH entries from
that layout. Extract every archive member rather than enumerating files to copy, so a new upstream
resource starts working without another vhrn change.

Do not remove the packaged `rg` or `zsh` merely because the base image has similar tools. They are
Codex-owned resources at versions and paths described by its manifest; the base tools remain the
ones available to ordinary agent shell commands.

### Manifest and executable checks

Before the install layer completes, require all of the following:

1. `codex-package.json` parses as JSON.
2. `layoutVersion` is supported (currently `1`).
3. `version` equals the resolved `X.Y.Z` exactly.
4. `target` equals the selected Linux musl target.
5. `entrypoint` is `bin/codex`, and that path is executable.
6. `resourcesDir` and `pathDir` name directories present under the package root.
7. `bin/codex-code-mode-host` is executable and its `--help` command exits successfully.
8. `codex --version` reports the resolved version.

These are minimum requirements, not an exact file-count assertion. Additional package members are
accepted and installed.

## Version and integrity model

### Docker build arguments

Add these build arguments to the Codex image:

```text
CODEX_VERSION=latest
CODEX_SHA256_AMD64=
CODEX_SHA256_ARM64=
```

`CODEX_VERSION` accepts `latest` or a stable `X.Y.Z` only. `latest` is for the disposable probe and
local development; canonical registry images are always built with an exact version. Reject tags,
prereleases, leading `v`, and arbitrary URL input.

Map `TARGETARCH` to the target and its matching checksum argument. When a checksum argument is
present, require the release metadata's digest to equal it and require the downloaded bytes to hash
to it. When it is absent (the unpinned probe or a local build), still require and verify the digest
from the release metadata. Download to a `mktemp -d` directory, verify before extracting, and remove
the directory with a trap on every exit path.

### Latest resolution

For `CODEX_VERSION=latest`, resolve the greatest stable `rust-vX.Y.Z` tag, retaining the existing
strict whole-tag filter that excludes alpha releases and unrelated release trains. Then fetch that
tag's release metadata and select the complete package asset. An API failure, a tag without a
published complete package, or an asset still being uploaded fails the probe; the scheduled refresh
can retry rather than publishing a partial release.

For an exact `CODEX_VERSION`, skip tag discovery and address `rust-v${CODEX_VERSION}` directly.

## Image implementation

### Installer helper

Move release resolution and package installation out of the Dockerfile's long `RUN` expression into
`image/codex/install.sh`:

- `#!/usr/bin/env bash` plus `set -euo pipefail`;
- inputs are the requested version and `TARGETARCH`, with the two optional expected checksums read
  from environment variables;
- helpers perform architecture mapping, stable-version validation, release resolution, asset
  selection, digest verification, extraction, and package validation;
- all diagnostics name the requested version, target, and failed contract;
- the script is shellcheck-clean and never prints tokens or the complete release response.

Keep the helper build-only. Use a two-stage Dockerfile:

1. A builder stage starts from `${BASE}`, copies the helper, switches to `dev`, sets
   `HOME=/home/dev`, and installs into `$HOME/.local`.
2. The final stage starts from the same `${BASE}` and copies only `/home/dev/.local` from the builder
   with `dev:dev` ownership.

The final image therefore contains the complete Codex package but not the installer script,
downloaded archive, release metadata, or temporary directory. It remains `USER root` so the shared
entrypoint can install nftables rules before dropping to `dev`, and it retains `CMD ["codex"]`.

## CI publication flow

The current workflow probes an unpinned amd64 image, parses its version, and later starts a new
multi-arch build. Because the Dockerfile resolves latest independently, a release between those
steps can place different Codex versions under one tag. Make the probe select the version and the
published build consume it:

1. Build the amd64 Codex probe with `CODEX_VERSION=latest`, `--no-cache`, and no expected checksum.
   The Dockerfile validates the complete package and its release-metadata digest.
2. Run `codex --version` as today and parse `X.Y.Z`.
3. For the Codex matrix row only, fetch the exact `rust-vX.Y.Z` release metadata on the host runner
   using the workflow's GitHub token. Select the two complete Linux package assets, require their
   `sha256:` digests, and expose the version plus both hex digests as step outputs. Do not pass the
   token into Docker BuildKit.
4. Add a matrix field identifying `CODEX_VERSION` as Codex's pin build argument; Claude leaves it
   empty and keeps its current installer flow.
5. Pass the exact version and both expected digests into the final amd64/arm64 build. Each platform
   selects and verifies its own digest.
6. Continue deriving the canonical image tags and `org.opencontainers.image.version` label from the
   probed version. The exact-version build makes that shared value a build input rather than a claim
   made after independently resolving latest.

Keep `skip_if_exists` after the probe and digest resolution. An unchanged upstream version remains a
no-op for the daily refresh. A forced dispatch still rebuilds the same version when vhrn's package
logic or base image changes.

Do not generalize Codex's two checksum fields into `Harness` or Rust CLI data. They are build-time
facts of one upstream distribution, and the workflow matrix is already the harness-specific build
registry.

## Local builds

Extend `image/Makefile` with `CODEX_VERSION ?= latest` and pass it as a build argument. When the value
is `latest`, add `--no-cache` to the Codex harness build so `make -C image build-codex` actually
rechecks upstream instead of silently reusing yesterday's install layer. An exact version remains
cacheable:

```sh
make -C image build-codex CODEX_VERSION=0.151.0
```

Keep engine selection unchanged. Both Apple `container` and Docker receive the same portable build
arguments and `--no-cache`; no engine-specific package branch is needed.

## Test and documentation changes

### Static checks

- Add `image/codex/install.sh` to the scripts path filter in `.github/workflows/ci.yml`.
- Add it to the shellcheck command in `.github/workflows/_test.yml`.
- Run `actionlint` after changing `_build-images.yml`, `ci.yml`, or `_test.yml`.

### Image checks

Build an exact-version image for the native architecture and run a command with the normal image
entrypoint bypassed so the check does not depend on a live proxy:

```sh
make -C image build-codex CODEX_VERSION=0.151.0
```

Inside the resulting image, verify:

```text
codex --version                         -> 0.151.0
codex-code-mode-host --help             -> exit 0
codex-package.json version and target   -> exact match
codex-resources and codex-path          -> present
```

CI's final multi-arch build executes the same Dockerfile assertions for amd64 and arm64. The workflow
must additionally assert that the version parsed from the probe equals the version resolved in the
exact release metadata before it creates tags.

### End-to-end check

After installing the rebuilt local or registry image, start Codex in a throwaway project through
vhrn. A normal turn must start without the `Code Mode is unavailable` warning. Exercise a turn that
offers Programmatic Tool Calling and confirm the code-mode host starts; helper presence alone is not
the final acceptance signal.

Update the image invariant in `AGENTS.md` to say that Codex is installed as its complete native
package under `~/.local`, including package-owned helpers and resources. This prevents a future
size-cleanup from returning to entrypoint-only copying. Add a short release-runbook note that a
forced harness refresh republishes packaging fixes without waiting for a new upstream agent version.

## Rollout

The daily harness workflow skips a version tag that already exists. This change repairs the package
without changing Codex's version, so merging it is not enough to replace `latest`. After CI passes:

1. Dispatch `harness-images.yml` with `force=true`.
2. Confirm the rebuilt `vhrn-codex:<version>` and `latest` manifests contain amd64 and arm64.
3. Inspect each variant's `org.opencontainers.image.version` label.
4. Pull and smoke-test one variant through Apple `container` and one through Docker.
5. Run `vhrn update codex` or reinstall the floating Codex harness and confirm the warning is gone.

The dated `<version>-<date>` tag remains the recovery point if the package layout behaves differently
on one engine. Do not delete the previous image tags during rollout.

## Non-goals

- Installing Codex through npm, Homebrew, Cargo, or a runtime package manager.
- Adding Node.js to `vhrn-base`.
- Updating Codex when a container starts; the image remains immutable and the daily host-side
  workflow owns updates.
- Mounting package resources or package-manager caches from the host.
- Changing Codex login, config, session partitioning, sandbox defaults, or egress behavior.
- Making all harness installers share one abstraction; Claude's supported installer and Codex's
  versioned complete package have different distribution contracts.

## Completion criteria

The work is complete when every item below is demonstrated:

- the legacy single-file archive and exact-one-file assertion are gone;
- both supported architectures install the complete package and pass all manifest/executable checks;
- `codex-code-mode-host` is present and starts through a real vhrn Codex turn;
- one CI-resolved Codex version is supplied to both published architecture builds;
- both release-asset digests are resolved before the multi-arch build and verified inside it;
- image tag, OCI version label, package manifest version, and `codex --version` agree;
- floating local builds bypass the stale install cache while exact-version builds remain cacheable;
- shellcheck and actionlint pass;
- a forced harness refresh republishes `latest`, and Apple `container` plus Docker smoke tests pass.
