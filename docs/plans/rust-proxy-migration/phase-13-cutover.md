# Phase 13: Cut over atomically to the sole Rust production proxy

## Execution contract

This file is the authoritative implementation specification for Phase 13. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 13 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../AGENTS.md).
4. Read the frozen [consumer contract](../../proxy/consumer-contract.md).
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
