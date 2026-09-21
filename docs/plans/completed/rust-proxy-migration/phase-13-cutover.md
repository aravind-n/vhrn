# Phase 13: Cut over atomically to the sole Rust production proxy

## Execution contract

This file is the authoritative implementation specification for Phase 13. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 13 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../../AGENTS.md).
4. Read the frozen [consumer contract](../../../proxy/consumer-contract.md).
5. Implement only this phase and stay within its editable paths and responsibility boundary.
6. Record detailed implementation, validation, and review evidence in this file.
7. After every completion requirement, independent review, and rereview are satisfied, apply the
   master plan's Phase 13 status-transition rule. If anything remains unresolved, leave Phase 13
   `Ready`.

## Objective

Move the qualified Rust tree into the stable `proxy/` production location, remove the frozen legacy
implementation and toolchain without using them as behavior inputs, update all build/test/release
paths atomically, and leave `vhrn-proxy`'s consumer and publication interface unchanged.

## Inputs and editable paths

Read:

- `AGENTS.md`, the frozen contract's Stable delivery and runtime invariants, [`plan.md`](plan.md),
  and [Phase 12 evidence](phase-12-qualification.md#evidence-required);
- the qualified `proxy-rs/` tree, root Cargo workspace/lock, allowed CLI image/tag/run interfaces,
  and current workflows;
- consumer and operator documentation that describes proxy build, security posture, CI, or release.

Do not open legacy Go source, tests, module files, diffs, history, or Go-derived explanations. The
legacy tree may be removed as an opaque frozen implementation after its exact path has been
resolved.

Edit only:

- move all qualified `proxy-rs/**` content to `proxy/**`, then remove the old `proxy-rs/` path and
  every remaining legacy implementation/toolchain file under `proxy/`;
- root `Cargo.toml`, `Cargo.lock`, and dependency-audit configuration;
- `.github/workflows/_test.yml`, `_build-proxy-go.yml`, `_build-proxy-rs.yml`, `ci.yml`,
  `nightly.yml`, `release.yml`, and the PR cleanup workflow only where test/build selection,
  ownership, publication, or cleanup requires it;
- `AGENTS.md`, `README.md`, `docs/sandbox-design.md`, `docs/runbooks/release.md`, and directly
  relevant proxy/operator docs;
- detailed implementation, validation, and review evidence in this file;
- [`plan.md`](plan.md) only for status transitions permitted by the master plan.

Do not change CLI behavior, image name, port, user, platforms, registry prefix, release-clock tag
selection, harness tags, version, or external services. Publishing, tagging a release, changing a
version, or replacing any already published tag requires separate user direction and is not part
of this phase.

## Required work

1. Perform one atomic-source cutover: the reviewed Rust source/tests/fixtures/Dockerfile/Makefile
   become `proxy/`; the root workspace member becomes `proxy`; Dockerfile copy paths use `proxy/`;
   no `proxy-rs/` path or legacy Go source/test/module/toolchain file remains.
2. Preserve the production image exactly: repository/image name `vhrn-proxy`, executable and
   entrypoint `/vhrn-proxy`, default/exposed port 8080, user `65532:65532`, scratch static image,
   `linux/amd64` and `linux/arm64`, read-only-root compatibility, and no capabilities.
3. Change the local Makefile default from the qualification tag to the unqualified/latest local
   `vhrn-proxy` reference expected by `vhrn install --local`. Preserve engine selection order and
   the distinct Apple/Docker image-delete commands.
4. Complete the build-lane cutover mechanically: remove the `build_proxy_go` filter,
   `build-proxy-go` caller jobs, and `_build-proxy-go.yml`; rename or promote the
   `build_proxy_rs` filter, `build-proxy-rs` caller jobs, and `_build-proxy-rs.yml` to
   `build_proxy`, `build-proxy`, and `_build-proxy.yml`. Update PR, nightly, and release callers.
   The promoted Rust workflow begins publishing the unchanged production `vhrn-proxy` image and
   inherits the Go workflow's image name, `linux/amd64` and `linux/arm64` platforms, tags, OCI
   metadata, cache behavior, permissions, registry paths, and CLI release-clock semantics. It uses
   repository root as context and `proxy/Dockerfile` as file. Leave `build-cli`/
   `_build-binaries.yml` and `build-harness-images`/`_build-harness-images.yml` ownership unchanged.
   The cutover PR itself performs no push, image publication, release creation, version change, or
   replacement of an existing tag.
5. Complete the Phase 2 test-lane transition mechanically: remove the `proxy_go` filter,
   `run_proxy_go` input, and `proxy-go` job; rename the `proxy_rs` filter, `run_proxy_rs` input, and
   `proxy-rs` job to `proxy`; and change its path from `proxy-rs/**` to `proxy/**`. Keep the `cli`
   filter/input/job independently named. Root Cargo files and shared proxy fixtures continue to
   select the component lanes they affect. The resulting proxy job runs Rust format, strict
   Clippy, tests, dependency audit, and release build, while `proxy/**` also selects the promoted
   production image build. Remove Go setup, formatting, vet, test, and vulnerability steps; do not
   introduce a generic `rust` lane. Keep `ci-gate` selected-job, skip, failure, and cancellation
   behavior and workflow validation.
6. Preserve host/runtime integration byte-for-byte: CLI mount paths, five public layers, three
   local layers, token mount, environment values, sidecar networking, proxy image overrides,
   local-image behavior, cleanup ordering, and supported-engine rejection remain unchanged. Use
   existing CLI tests plus Phase 12 E2E; do not import proxy types into the CLI.
7. Update repository guidance and operator docs from the Go implementation/toolchain/test commands
   to the Rust workspace, dependency audit, static scratch build, and unchanged security limits.
   Keep the consumer contract implementation-neutral and change it only if a stable interface typo
   is found; any normative change requires a new contract decision and user direction.
8. Run the complete CI-equivalent suite and build the final local production image on both
   supported engines. Repeat the Phase 12 smoke/E2E using the default `vhrn-proxy` selection, then
   remove only disposable local cutover tags/images created by the validation.
9. Obtain independent clean-room review of Rust correctness/security, deleted-path completeness,
   workflows, release tags, docs, and scope. The reviewer must not inspect deleted Go contents in a
   diff; verify absence and the resulting Rust tree instead. Resolve findings and rereview.

## Interfaces and data flow

- The source location and build context change; no consumer-side file, wire, process, engine, or
  tag interface changes.
- The promoted Rust proxy workflow inherits the production Go proxy workflow's image name,
  platforms, tag and OCI metadata surface, cache behavior, permissions, registry paths, and
  release-clock semantics. PR, nightly, and release callers select that promoted workflow;
  `build-cli` and `build-harness-images` remain separate and unchanged.
- The root workspace builds independent `vhrn` and `vhrn-proxy` packages; no compiled policy,
  target, or broker type crosses the crate boundary.

## Validation

Run and record, in order:

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets --locked -- -D warnings`
3. `cargo test --workspace --locked`
4. `cargo build --release --locked -p vhrn`
5. `cargo build --release --locked -p vhrn-proxy`
6. The repository Rust dependency audit command.
7. `make -C proxy ENGINE=docker`
8. The final Docker scratch/read-only-root and Docker/Colima E2E checks.
9. `make -C proxy ENGINE=container`
10. The final Apple `container` scratch/read-only-root and E2E checks.
11. A multi-platform, no-push build using the exact repository-root context,
    `proxy/Dockerfile`, `linux/amd64,linux/arm64` platforms, and settings from the promoted Rust
    proxy workflow.
12. `actionlint` over `.github/workflows/**`.
13. Path assertions that `proxy-rs/`, `_build-proxy-go.yml`, Go source/tests/module files, and Go
    workflow/toolchain references are absent, without opening deleted contents; caller assertions
    that only `build-proxy` remains while `build-cli` and `build-harness-images` are unchanged.

## Evidence required

- Final Rust tree/path inventory, workspace metadata, image metadata, multi-platform build result,
  and no-publish attestation.
- CLI interface regression results for mounts, env, image refs/tags, engines, readiness, and
  cleanup; final Apple and Docker/Colima E2E results.
- Documentation inventory and explicit confirmation that image/tag/version/external-service
  interfaces did not change.
- Final independent review and rereview with no findings. Record any local disposable image cleanup
  and whether it is recoverable; do not delete published artifacts.

## Completion criterion

`proxy/` contains only the qualified Rust production proxy and its Rust packaging/tests;
`proxy-rs/` and the legacy Go implementation/toolchain are absent; workspace, CI, image build,
local build, nightly, and release paths all select Rust while preserving every stable interface;
all validation and both supported-engine E2Es pass; docs are current; nothing was published or
versioned; and final independent rereview is clean.

## Implementation evidence

- The qualified tree was moved atomically into `proxy/`. Its final inventory is `Cargo.toml`,
  `Dockerfile`, `Makefile`, the idiomatic Rust module tree under `src/`, eight contract fixtures
  under `testdata/`, and `tests/proxy_process.rs`. `proxy-rs/`, every Go source/test/module file,
  and both superseded proxy workflows are absent.
- Root workspace metadata resolves exactly two independent packages: `vhrn` from
  `cli/Cargo.toml` and `vhrn-proxy` from `proxy/Cargo.toml`. Neither imports the other. The existing
  shared lock resolved both packages without a `Cargo.lock` change.
- `proxy/Dockerfile` now copies `proxy/Cargo.toml` and `proxy/src` from repository-root context.
  Its build still proves the optimized executable has no interpreter or dynamic `NEEDED` entry;
  the scratch stage still installs only `/vhrn-proxy`, sets user `65532:65532`, exposes 8080, and
  uses `/vhrn-proxy` as its entrypoint.
- `proxy/Makefile` defaults to the unqualified local reference `vhrn-proxy`; an explicit `TAG`
  remains supported. Apple `container` is still detected before Docker, and cleanup still uses
  `container image delete` versus `docker image rm`.
- The initial independent review reproduced a process-test-only released-port race between
  concurrent startup cases. A shared asynchronous handoff lock now covers both the general
  `Proxy::start` path and every startup fixture that releases an ephemeral port before spawning a
  child. The corrected suite passed against both the debug-built and release proxy executables.

## Workflow and publication evidence

- `_test.yml` now has one independently selectable `proxy` job. It runs Rust format, strict
  Clippy, all proxy tests, `cargo-audit` 0.22.2 with warnings denied, the release build, and the
  26-case process contract against that release executable. All Go setup, format, vet, test, and
  vulnerability steps are gone; the separate `cli` and `scripts` jobs remain.
- `ci.yml` now exposes only `proxy` and `build_proxy` proxy filters, `run_proxy`, and one
  `build-proxy` caller. Root Cargo files and shared proxy fixtures still select both component
  lanes, `proxy/**` selects proxy test and image-build lanes, and `ci-gate` retains its selected,
  skipped, failed, and cancelled behavior. The `build-cli` and `build-harness-images` callers and
  their reusable workflows remain separately owned and unchanged.
- `_build-proxy.yml` preserves the production workflow call surface (`flavor`, `push`, and
  `extra_tag`), `ghcr.io/${owner}/vhrn-proxy`, package permission, conditional registry login,
  PR/PR-SHA, nightly/SHA, release/latest, and extra raw tags, OCI labels, and
  `linux/amd64,linux/arm64`. It uses context `.` and `proxy/Dockerfile`. PR, nightly, and release
  now call only this workflow.
- Before its final multi-platform build, the promoted workflow performs the pinned dependency
  audit and a native disposable image smoke. That smoke checks the exact user, entrypoint, exposed
  port, one-layer scratch filesystem and absence of a shell, then checks health/status while the
  container has a read-only root, all capabilities dropped, and only the two documented mounts.
- The cutover used only local builds and pulls. No registry login or push, image publication,
  release creation, Git tag, immutable-tag replacement, version edit, or external-service change
  occurred. Both packages remain version 0.5.1, and the CLI release-clock proxy-tag behavior is
  unchanged.

## Interface, documentation, and engine evidence

- The frozen consumer contract remained byte-for-byte unchanged at SHA-256
  `f3499ff66aa9e15d3f7788153b3f109dc7b020956a6d306eda5e103e806075ed`. The production image name
  `vhrn-proxy`, executable and entrypoint `/vhrn-proxy`, port 8080, identity `65532:65532`, scratch
  runtime, platforms, registry prefix, harness tags, and version are unchanged.
- All 183 CLI tests passed. Their regression coverage includes exact public/local policy and token
  mounts, proxy environment construction, image refs and CLI-clock tags, Apple/Docker engine
  detection and argument goldens, broker readiness and authentication, lifecycle ownership,
  SIGTERM cleanup ordering, and run-policy retirement. No CLI source or behavior changed during
  cutover, and no proxy type crossed the crate boundary.
- Docker through Colima built the default `vhrn-proxy` image. Direct inspection/smoke confirmed the
  exact user, entrypoint, port, one scratch layer, no shell, read-only root, `cap-drop ALL`, two
  mounts, exact health and status bodies, exact `blocked.invalid` denial, and graceful stop. A local
  base and Pi image were installed through `vhrn install pi --local`; an actual PTY run using the
  default proxy selection completed and reported Pi 0.86.1.
- Apple `container` built the same unqualified image and passed the same metadata, scratch,
  read-only-root, no-capability, health/status, exact-denial, and graceful-stop checks. Its direct
  token mount, host-state mount, and broker readiness were also exercised. A local Pi install and
  actual PTY `vhrn pi -- --version` run used the default proxy and reported Pi 0.86.1. The earlier
  non-PTY attempt returned Apple's expected terminal-device error; rerunning through a PTY passed.
- The exact root-context no-push multi-platform build used `proxy/Dockerfile` and
  `linux/amd64,linux/arm64`. Docker's installed client lacked Buildx, so the official Buildx
  v0.37.1 asset was staged temporarily after its published SHA-256 was verified. The first attempt
  exhausted local disk only after reaching both platform builds; disposable build cache/images
  were removed and the identical retry passed all compile, strip, and static-link checks for both
  architectures. It produced no pushed or loaded output.
- `AGENTS.md`, `README.md`, `docs/sandbox-design.md`, and `docs/runbooks/release.md` now describe
  the Rust workspace, static scratch image, dependency audit, single Rust test/build lanes, and
  unchanged runtime and release security boundaries. No normative consumer-contract text changed.

## Validation evidence

Tool versions were `rustc 1.98.1`, `cargo 1.98.1`, Apple `container 1.4.1`, Colima `0.10.3`,
Docker client `29.8.1` with server `29.5.2`, `cargo-audit 0.22.2`, and `actionlint 1.7.12` on an
arm64 host.

1. `cargo fmt --all -- --check` — passed after the final test-race fix.
2. `cargo clippy --workspace --all-targets --locked -- -D warnings` — passed with no warnings after
   the final fix.
3. `cargo test --workspace --locked` — passed after the final fix: 183 CLI tests, 177 proxy unit
   tests, 26 proxy process tests, and both doc-test targets.
4. `cargo build --release --locked -p vhrn` — passed.
5. `cargo build --release --locked -p vhrn-proxy` — passed.
6. `cargo audit --deny warnings` — loaded 1,251 RustSec advisories, scanned 140 locked dependencies,
   and passed.
7. `make -C proxy ENGINE=docker` — passed and created the default local `vhrn-proxy` reference.
8. The Docker/Colima production-image smoke and local Pi E2E described above — passed.
9. `make -C proxy ENGINE=container` — passed and created the same default reference.
10. The Apple `container` production-image smoke and local Pi E2E described above — passed.
11. The exact repository-root, `proxy/Dockerfile`, no-push `linux/amd64,linux/arm64` Buildx build —
    passed on the resource-clean retry.
12. `actionlint .github/workflows/*.yml` — passed, including ShellCheck of inline workflow scripts.
13. Final path/caller assertions — passed: no `proxy-rs/`, superseded proxy workflow, Go
    source/test/module file, or active Go workflow/toolchain reference remains; only
    `_build-proxy.yml` and `build-proxy` remain, while `build-cli` and `build-harness-images` are
    unchanged.
14. `VHRN_PROXY_TEST_BIN="$PWD/target/release/vhrn-proxy" cargo test
    --package vhrn-proxy --locked --test proxy_process` — all 26 release-executable contract cases
    passed after the final race fix.
15. `git diff --check` — passed.

## Cleanup and review evidence

- The temporary local Pi installation was uninstalled; `vhrn list` again reports no installed
  harness. Docker and Apple tags/images created specifically by cutover validation were removed,
  while pre-existing candidate and registry images were preserved. Apple reported 11.77 GB
  reclaimed. Validation containers, broker/token staging, smoke directories, the temporary
  Buildx plugin and builder, and temporary target trees were removed. These deleted local
  artifacts are disposable and recoverable by rebuilding; no published artifact was deleted.
- Colima was returned to its original stopped state. Final Apple inventory showed no containers;
  only pre-existing toolchain, registry, candidate, and tools-layer images remained.
- The independent reviewer followed the clean-room boundary and inspected the resulting Rust tree
  and deleted-path absence without opening deleted Go contents. Its initial review found the
  released-port test race described above and the then-missing final evidence record; it found no
  other material issue. The race is fixed, the complete proxy and workspace suites pass, and this
  evidence record now covers the required inventory, interfaces, E2Es, no-publish attestation, and
  cleanup.
- Final clean-room rereview approved the cutover with no material findings. It independently
  confirmed that the shared handoff lock covers every released-port startup path without nested
  acquisition; reran format, strict workspace Clippy, and all 26 process cases against the release
  executable; and revalidated the contract hash, workspace, image/Makefile/workflow interfaces,
  workflow syntax, and deleted-path/toolchain absence.
