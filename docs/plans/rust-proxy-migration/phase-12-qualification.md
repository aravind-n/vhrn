# Phase 12: Qualify the isolated candidate on its real image and engines

## Execution contract

This file is the authoritative implementation specification for Phase 12. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 12 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../AGENTS.md).
4. Read the frozen [consumer contract](../../proxy/consumer-contract.md).
5. Read the active [contract coverage ledger](coverage.md).
6. Implement only this phase and stay within its editable paths and responsibility boundary.
7. Record detailed implementation, validation, and review evidence in this file.
8. After every completion requirement, independent review, and rereview are satisfied, apply
   the master plan's status-transition rules. If anything remains unresolved, leave the next phase
   `Blocked`.

## Objective

Prove the complete candidate contract through black-box process, scratch-image, multi-platform, and
supported-engine tests while the shipping image remains untouched.

## Inputs and editable paths

Read:

- the active [contract coverage ledger](coverage.md);
- the entire frozen contract, its delivery/runtime invariants, `AGENTS.md`, and [`plan.md`](plan.md);
- all completed candidate code and tests, `proxy-rs/Cargo.toml`, Dockerfile, and Makefile;
- root Cargo workspace/lock, allowed CLI run/net/broker interfaces, and relevant test/image/CI
  workflows.

Edit only:

- candidate tests and fixtures under `proxy-rs/` and `shared/testdata/` for cross-feature black-box
  coverage (behavioral fixes must go back to their owning phase instead of landing here);
- `proxy-rs/Dockerfile` and `proxy-rs/Makefile` for qualification-only correctness;
- candidate-only test scripts under `proxy-rs/tests/`;
- `.github/workflows/_build-proxy-rs.yml`, `_test.yml`, and `ci.yml` only to qualify and extend
  the already-existing isolated candidate build with image smoke, dependency-audit, and
  qualification integration without selecting it for production or publishing it;
- dependency-audit configuration at the workspace root if required;
- [`coverage.md`](coverage.md) only to record qualification evidence;
- detailed implementation, validation, and review evidence in this file;
- [`plan.md`](plan.md) only for status transitions permitted by the master plan.

Do not edit `proxy/`, production image context/tag selection, nightly/release publication, CLI
runtime defaults, versions, or immutable tags.

## Required work

1. Audit the active [contract coverage ledger](coverage.md) against named tests. Add only
   cross-feature black-box cases here; if a normative behavior lacks its owning phase's focused
   evidence, reopen that phase and do not mark Phase 12 complete.
2. Run the process suite against the built executable through only its environment and TCP
   interface. Cover all exact endpoints/error bodies, live public/local/mode/log replacement,
   policy repair, public and broker outcomes, persistent HTTP, streaming, CONNECT prefixes and
   half-close, concurrency, cancellation, and shutdown. Assert secrets/internal paths never appear.
3. Build the candidate image from the repository-root context for `linux/amd64` and `linux/arm64`.
   Prove `/vhrn-proxy`, entrypoint, port 8080, `65532:65532`, scratch/no shell, static linkage, and
   behavioral equivalence. Run with a read-only root filesystem and all capabilities dropped using
   only the three documented mounts; it must not require any other writable path.
4. Qualify and extend the already-existing nonpublishing `build-proxy-rs` branch in
   `_build-proxy-rs.yml` with the required candidate-image smoke checks and a Rust dependency
   vulnerability audit. Pin tool invocation sufficiently for reproducible review and fail on
   reachable/advisory findings according to the repository's security posture. Keep production
   image selection and Go proxy publication untouched. Do not push the candidate or assign it any
   production, nightly, release, SHA, dated-nightly, or PR publication tag.
5. Exercise the candidate under Apple `container` and Docker through a local Colima Unix socket,
   using only a disposable `vhrn-proxy:rust-candidate` tag or `VHRN_PROXY_IMAGE` override. Verify
   build, sidecar IP/readiness, exact mounts/env, agent proxy variables, firewall pinning, public
   allow/deny, brokered `localhost`/127/IPv6 grants, live revocation, cleanup, and SIGTERM.
6. Confirm native Linux Docker, Docker Desktop, and remote Docker remain rejected for brokered vhrn
   runs; qualification does not broaden supported engines.
7. Run an independent end-to-end security/correctness review over Rust candidate and allowed host
   interfaces only. Resolve findings in the phase that owns the behavior, rerun qualification, and
   obtain final rereview.

## Interfaces and data flow

- The process harness remains executable-agnostic through `VHRN_PROXY_TEST_BIN`.
- Image smoke uses the contract's exact container paths and environment. Engine E2E selects the
  candidate only through the documented local tag/override and leaves production selection intact.
- The existing `_build-proxy-rs.yml` branch may cache/build candidate artifacts and gain smoke and
  audit steps, but it must never call a registry push or assign production, nightly, release, SHA,
  dated-nightly, or PR publication tags to the candidate. `_build-proxy-go.yml` remains the sole
  production `vhrn-proxy` publisher.

## Validation

Run and record:

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets --locked -- -D warnings`
3. `cargo test --workspace --locked`
4. `cargo build --release --locked -p vhrn-proxy`
5. The reviewed Rust dependency audit command added by this phase.
6. `make -C proxy-rs ENGINE=docker TAG=rust-candidate`
7. The candidate scratch/read-only-root Docker smoke and multi-platform build command matching
   `_build-proxy-rs.yml`'s repository-root context, `proxy-rs/Dockerfile`, platforms, and settings.
8. `make -C proxy-rs ENGINE=container TAG=rust-candidate`
9. The documented Apple `container` E2E script/steps.
10. The documented Docker/Colima E2E script/steps.
11. `actionlint` for any changed workflows.

## Evidence required

- A complete contract-to-test trace with no `untested`, duplicate-owner, or candidate-assumption
  rows.
- Image metadata, static-link, read-only-root, mount, no-capability, amd64, and arm64 results.
- Exact engine/tool versions and successful Apple/Colima run transcripts with secrets and host
  paths redacted; explicit confirmation that nothing was published.
- `_build-proxy-rs.yml` candidate build/smoke result, dependency-audit result, and final independent
  review/rereview approval.

## Completion criterion

The isolated candidate passes the entire workspace and black-box contract suite, dependency audit,
static scratch/read-only-root and both-architecture checks, and verified Apple `container` plus
Docker/Colima runs; no production selector or immutable tag changed; the coverage trace is exact;
and final rereview has no finding.
