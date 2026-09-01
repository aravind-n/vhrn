# Release runbook

This is the complete on-call procedure for publishing a `vhrn` release. Follow
the numbered steps in order. Every step has a completion criterion: stop at the
first failed criterion and use [Recovery](#recovery).

GitHub CLI (`gh`) is the primary forge interface. Every required `gh` operation
has a GitHub web UI alternative beside it; use either route, then meet the
shared completion criterion.

## Prerequisites

You need a local clone of `aravind-n/vhrn`, `git`, Cargo/Rust, `curl`, a
browser, and either authenticated `gh` or a GitHub web session. You also need
permission to push release branches, merge the protected-branch PR, create
protected `v*` tags, and approve the `release` environment.

## Release model and version selection

- A CLI release is a protected `vX.Y.Z` tag on `master`. Prepare it on
  `release/vX.Y.Z`, land it through a PR, and squash merge it.
- CLI version controls the binaries plus `vhrn-base` and `vhrn-proxy`. Harness
  images use their bundled agent version on a separate release clock.
- A release is complete only after its workflow, public release, binaries,
  checksums, images, and installer smoke test succeed.
- A published release tag is immutable. Correct it with a new patch release.

Versions are `0.MINOR.PATCH` until 1.0. Choose a **minor** version when an
existing documented command, config key, persisted state, or image reference
requires migration or becomes incompatible: renamed/removed flags,
subcommands/config keys, changed state format, or changed tag meaning. Choose
a **patch** version for backward-compatible work: fixes, optional config keys
or flags, allowlist additions, tooling, docs, and compatible defaults. Adding
`[resources]` is a patch; renaming `[resources].memory` is a minor.

## Cut a release

### 1. Set release values

At the repository root, replace only `X.Y.Z`:

```sh
export RELEASE_VERSION=X.Y.Z
export RELEASE_TAG="v$RELEASE_VERSION"
export RELEASE_BRANCH="release/$RELEASE_TAG"
export RELEASE_DATE="$(date -u +%F)"
printf '%s\n' "$RELEASE_VERSION $RELEASE_TAG $RELEASE_BRANCH $RELEASE_DATE"
```

Completion criterion: printed values are intended version, tag,
`release/vX.Y.Z`, and today's UTC date. Keep this shell open.

### 2. Preflight

```sh
git fetch origin master --tags
git remote get-url origin
git status --short
git tag --list "$RELEASE_TAG"
git ls-remote --heads origin "$RELEASE_BRANCH"
git ls-remote --tags origin "refs/tags/$RELEASE_TAG"
git describe --tags --match 'v[0-9]*.[0-9]*.[0-9]*' --abbrev=0 origin/master
sed -n '/^## \[Unreleased\]/,/^## /p' CHANGELOG.md
```

Completion criterion:

- `git status --short`, the tag query, and both `git ls-remote` commands print
  no output. Resolve local changes; do not delete them blindly.
- `git describe` prints the previous stable tag.
- `git remote get-url origin` identifies the canonical `aravind-n/vhrn`
  repository.
- `Unreleased` is complete and accurate for `origin/master`. Stop and correct
  it through normal review if empty, inaccurate, or describing unmerged work.

If available, verify GitHub access:

```sh
gh auth status
gh repo view --json nameWithOwner --jq .nameWithOwner
```

Completion criterion: `gh` identifies an authenticated account and
`aravind-n/vhrn`. If unavailable, use the UI alternatives below.

### 3. Create the release commit

```sh
git switch --create "$RELEASE_BRANCH" origin/master
```

In `CHANGELOG.md`, retain an empty `Unreleased` heading and move its items to a
dated heading. Change:

```markdown
## [Unreleased]

- Release notes.
```

to this, replacing placeholders with `$RELEASE_VERSION` and `$RELEASE_DATE`:

```markdown
## [Unreleased]

## [X.Y.Z] - YYYY-MM-DD

- Release notes.
```

At the file bottom, set the links exactly as below, replacing `vPREVIOUS` with
the tag printed in step 2:

```markdown
[unreleased]: https://github.com/aravind-n/vhrn/compare/vX.Y.Z...HEAD
[X.Y.Z]: https://github.com/aravind-n/vhrn/compare/vPREVIOUS...vX.Y.Z
```

Set `Cargo.toml` package `version` to `$RELEASE_VERSION`, then:

```sh
cargo build --release
```

Completion criterion: exactly `Cargo.toml`, `Cargo.lock`, and `CHANGELOG.md`
changed. The version bump updates the root package entry in `Cargo.lock`.

The release workflow derives its GitHub Release body from this tag's own
`CHANGELOG.md` section. An empty or missing `$RELEASE_VERSION` section fails
publication; it never falls back to generated PR notes.

### 4. Validate, commit, and push

```sh
git diff --check
git diff -- CHANGELOG.md Cargo.toml Cargo.lock
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
git status --short
```

Completion criterion: every command succeeds, diff is accurate, and status
lists exactly `CHANGELOG.md`, `Cargo.toml`, and `Cargo.lock`.

```sh
git add CHANGELOG.md Cargo.toml Cargo.lock
git commit -m "release: $RELEASE_VERSION"
git push --set-upstream origin "$RELEASE_BRANCH"
export RELEASE_BRANCH_SHA="$(git rev-parse HEAD)"
printf '%s\n' "$RELEASE_BRANCH_SHA"
```

Completion criterion: push succeeds and `RELEASE_BRANCH_SHA` is the
40-character `release: X.Y.Z` commit ID.

### 5. Open and squash-merge the release PR

**GitHub CLI:**

```sh
gh pr create --base master --head "$RELEASE_BRANCH" \
  --title "release: $RELEASE_VERSION" --body "Release $RELEASE_TAG."
export PR_NUMBER="$(gh pr view "$RELEASE_BRANCH" --json number --jq .number)"
gh pr checks "$PR_NUMBER" --required --watch --fail-fast
gh pr merge "$PR_NUMBER" --squash --subject "release: $RELEASE_VERSION" \
  --match-head-commit "$RELEASE_BRANCH_SHA"
```

**GitHub web UI:** Open **Pull requests** → **New pull request**. Set base to
`master` and compare to `$RELEASE_BRANCH`. Create it with title
`release: X.Y.Z` and body `Release vX.Y.Z.`; record its number. Wait for
required `ci-gate`, resolve conversations, choose **Squash and merge**, retain
`release: X.Y.Z` as subject, and confirm. Do not use an administrator bypass.

Completion criterion: PR is **Merged** into `master`, `ci-gate` is green, and
the PR's squash commit is titled `release: X.Y.Z`.

### 6. Verify exact merged candidate and nightly

**GitHub CLI:**

```sh
export RELEASE_SHA="$(gh pr view "$PR_NUMBER" --json mergeCommit --jq .mergeCommit.oid)"
printf '%s\n' "$RELEASE_SHA"
```

**GitHub web UI:** Open the squash commit on the merged PR, copy its full
40-character SHA, then run:

```sh
export RELEASE_SHA='paste-the-full-SHA-here'
```

Both paths:

```sh
git fetch origin master --tags
git merge-base --is-ancestor "$RELEASE_SHA" origin/master
test "$(git show -s --format=%s "$RELEASE_BRANCH_SHA")" = "release: $RELEASE_VERSION"
test "$(git diff-tree --no-commit-id --name-only -r "$RELEASE_BRANCH_SHA" | sort)" = "$(printf '%s\n' CHANGELOG.md Cargo.lock Cargo.toml | sort)"
test "$(git show "$RELEASE_SHA:Cargo.toml" | sed -n 's/^version = "\([^"]*\)"/\1/p' | head -1)" = "$RELEASE_VERSION"
git show "$RELEASE_SHA:CHANGELOG.md" | grep -F "## [$RELEASE_VERSION] - $RELEASE_DATE"
```

Completion criterion: each check succeeds. The release branch commit has the
required subject and changes exactly the three release files; the merged
candidate's committed version and changelog heading match the release. A
concurrent merge is safe: `RELEASE_SHA`, not the branch tip, is the candidate.

Wait for full `nightly` for this SHA.

**GitHub CLI:**

```sh
NIGHTLY_RUN_ID=""
attempt=0
while [ "$attempt" -lt 60 ]; do
  NIGHTLY_RUN_ID="$(gh run list --workflow nightly.yml --commit "$RELEASE_SHA" --json databaseId --jq '.[0].databaseId')"
  [ -n "$NIGHTLY_RUN_ID" ] && break
  attempt=$((attempt + 1))
  sleep 10
done
test -n "$NIGHTLY_RUN_ID"
gh run watch "$NIGHTLY_RUN_ID" --exit-status
```

**GitHub web UI:** Open **Actions** → **nightly**, select the run whose commit
SHA exactly matches `RELEASE_SHA`, and wait for every job.

Completion criterion: that nightly run is green. Do not tag before it is green.

### 7. Tag, approve, and publish

```sh
git tag "$RELEASE_TAG" "$RELEASE_SHA"
git push origin "refs/tags/$RELEASE_TAG"
```

Completion criterion: push succeeds and starts `release`.

**GitHub CLI:**

```sh
RELEASE_RUN_ID=""
attempt=0
while [ "$attempt" -lt 60 ]; do
  RELEASE_RUN_ID="$(gh run list --workflow release.yml --branch "$RELEASE_TAG" --commit "$RELEASE_SHA" --json databaseId --jq '.[0].databaseId')"
  [ -n "$RELEASE_RUN_ID" ] && break
  attempt=$((attempt + 1))
  sleep 10
done
test -n "$RELEASE_RUN_ID"
gh run view "$RELEASE_RUN_ID" --web
```

**GitHub web UI:** Open **Actions** → **release**, select run for
`$RELEASE_TAG` whose commit SHA is `RELEASE_SHA`.

For either path, select **Review deployments**, select environment `release`,
and approve. Approval starts full suite, image/binary builds, checksums, and
GitHub Release publication.

**GitHub CLI:** after approval:

```sh
gh run watch "$RELEASE_RUN_ID" --exit-status
```

**GitHub web UI:** keep the run open until every job is green.

Completion criterion: every release-workflow job is green.

### 8. Verify publication

**GitHub CLI:**

```sh
test "$(git rev-parse "$RELEASE_TAG^{commit}")" = "$RELEASE_SHA"
test "$(git ls-remote origin "refs/tags/$RELEASE_TAG" | cut -f1)" = "$RELEASE_SHA"
test "$(gh release view --json tagName --jq .tagName)" = "$RELEASE_TAG"
test "$(gh release view "$RELEASE_TAG" --json assets --jq '.assets[].name' | sort)" = "$(printf '%s\n' SHA256SUMS vhrn-darwin-amd64 vhrn-darwin-arm64 vhrn-linux-amd64 vhrn-linux-arm64 | sort)"
```

**GitHub web UI:** Open **Releases** → `$RELEASE_TAG`. Confirm **Latest**,
its tag target is `RELEASE_SHA`, and assets `vhrn-darwin-amd64`, `vhrn-darwin-arm64`,
`vhrn-linux-amd64`, `vhrn-linux-arm64`, and `SHA256SUMS`. Open **Packages** and
verify the image tags below.

For either path, inspect **Packages** (or the release workflow's **images**
job), record each harness's probed agent version and the exact dated tag emitted
by that release run, and verify all of these tags:

- `vhrn-base` and `vhrn-proxy`: `$RELEASE_TAG` and `latest`.
- Each `vhrn-<harness>`: its recorded `<agent-version>`,
  recorded `<agent-version>-<YYYYMMDD>` tag, and `latest`. The dated tag uses
  the release workflow's UTC build date; do not derive it from `RELEASE_DATE`.

For either path:

```sh
export RELEASE_SMOKE_DIR="$(mktemp -d)"
curl -fsSL https://aravind-n.github.io/vhrn/install.sh | \
  VHRN_VERSION="$RELEASE_TAG" VHRN_BINDIR="$RELEASE_SMOKE_DIR" sh
"$RELEASE_SMOKE_DIR/vhrn" --version
```

Completion criterion: local and remote tags resolve to `RELEASE_SHA`; latest
stable release is `$RELEASE_TAG`; it has exactly the five assets; the explicitly
listed base, proxy, and recorded harness tags exist; installer checksum
verification succeeds; and last command prints `vhrn vX.Y.Z`.

## Recovery

Use the first failed step.

| Stopped at | Recovery |
| --- | --- |
| 2–4, before PR merge | Correct metadata with new commits on `$RELEASE_BRANCH`, rerun step 4, push normally, then continue at step 5. |
| 5, PR checks fail | Fix through commits on `$RELEASE_BRANCH`, rerun step 4, push, and wait for `ci-gate`. |
| 6, candidate verification fails | Do not tag. Merge a corrective PR to `master`, set `RELEASE_SHA` to its merge commit, then rerun the shared invariant block and nightly check in step 6. |
| 6, nightly transiently fails | Run `gh run rerun "$NIGHTLY_RUN_ID" --failed`, or use **Re-run failed jobs**. Continue only when that SHA is green. |
| 6, nightly deterministically fails | Do not tag. Fix through a PR to `master`; update the pending release section if needed; capture that PR's merge SHA as `RELEASE_SHA`; then rerun step 6's ancestry, committed-version, changelog-heading, and nightly checks. The initial release commit remains the required three-file metadata commit. |
| 7, tag push rejected | Leave local tag and escalate to a maintainer permitted to create protected `v*` tags. |
| 7, tag exists but no workflow started | Prove in **Actions** and **Releases** no run/publication began, then prove local `$RELEASE_TAG` resolves to unchanged `RELEASE_SHA`. Delete only remote tag with `git push origin ":refs/tags/$RELEASE_TAG"`, re-push the verified local tag, and repeat step 7. |
| 7, workflow job failed | Keep tag fixed. Run `gh run rerun "$RELEASE_RUN_ID" --failed`, or use **Re-run failed jobs**. Approve again if asked. |
| 8, published source, notes, or artifacts are wrong | Leave tag/artifacts intact. Cut a corrective patch release. |

## Roll back

Choose affected release clock.

- **CLI and proxy:** install older CLI; matched proxy follows.

  ```sh
  curl -fsSL https://aravind-n.github.io/vhrn/install.sh | VHRN_VERSION=vX.Y.Z sh
  ```

- **Harness image:** pin older bundled-agent version; pin prevents `vhrn update`.

  ```sh
  vhrn install claude@AGENT_VERSION
  ```

  Replace `claude` with affected harness. Return to newest with
  `vhrn install claude`.

## Refresh a harness image

Daily `harness-images` republishes only when agent version changed.
`force=true` republishes unchanged version, for example after base change.

**GitHub CLI:**

```sh
gh workflow run harness-images.yml --ref master -f force=true
```

**GitHub web UI:** Open **Actions** → **harness-images** → **Run workflow**,
select `master`, set **force** to `true`, then select **Run workflow**.

Completion criterion: dispatched run is green. With `force=false`, an already
published agent version is intentionally skipped.

## What each trigger publishes

| Trigger | Workflow | Publishes |
| --- | --- | --- |
| Pull request | `ci.yml` | Lints/tests changed components; same-repository PRs changing image/proxy paths also publish `pr-<n>` base, proxy, and harness images. |
| Push to `master` | `nightly.yml` | Full suite; `nightly` base/proxy/harness images and rolling `nightly` prerelease binaries plus `SHA256SUMS`. |
| Push a `vX.Y.Z` tag | `release.yml` | After approval/full suite: `vX.Y.Z` + `latest` base/proxy; harness agent-version, dated, + `latest`; GitHub Release binaries + `SHA256SUMS`. |
| Daily cron / dispatch | `harness-images.yml` | Rebuilds harnesses from published base; republishes changed agent versions unless forced. |

## Image tags

| Image | Tags |
| --- | --- |
| `vhrn-base`, `vhrn-proxy` | `vX.Y.Z`, `latest` (release) · `nightly`, `sha-<sha>`, `nightly-<date>-<sha>` (master) · `pr-<n>`, `pr-<n>-<sha>` (same-repository image/proxy PR) |
| `vhrn-<harness>` | `<agent-version>`, `<agent-version>-<date>`, `latest` (release / refresh) · `nightly`, `nightly-<date>-<sha>` (master) · `pr-<n>`, `pr-<n>-<sha>` (same-repository image/proxy PR) |
