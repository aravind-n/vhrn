# Phase 12: Qualify the isolated candidate on its real image and engines

## Execution contract

This file is the authoritative implementation specification for Phase 12. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 12 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../../AGENTS.md).
4. Read the frozen [consumer contract](../../../proxy/consumer-contract.md).
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

## Implementation evidence

- The coverage ledger now names qualification evidence for every frozen requirement group. The
  audit found no missing owning-phase behavior and therefore required no new Rust test or behavior
  change.
- `_test.yml` reruns the existing 26-case black-box process suite against the optimized executable
  selected only through `VHRN_PROXY_TEST_BIN`. The harness continues to interact through environment
  variables and TCP; no implementation-specific test interface was added.
- `_build-proxy-rs.yml` installs the review-pinned `cargo-audit` 0.22.2 and rejects warnings as well
  as vulnerability findings. Its existing root-context, nonpublishing amd64+arm64 Dockerfile build
  remains intact.
- The candidate workflow also loads one disposable native `vhrn-proxy:rust-candidate` image and
  performs a compact inline smoke. It verifies the numeric user, entrypoint, exposed port, one-layer
  scratch filesystem and absence of `/bin/sh`, then obtains exact health/status responses while the
  image runs with a read-only root, `cap-drop ALL`, and only the documented policy and denial-log
  mounts. Local routing is intentionally absent in this smoke and remains covered by the release
  process suite and supported-engine qualification.
- The workflow has `contents: read`, `push: false`, no registry login, no package permission, and no
  production/nightly/release/SHA/PR tag. The shipping Go workflow, production selector, CLI runtime
  defaults, versions, and immutable tags are untouched.

## Contract and process-suite evidence

- The frozen consumer contract remained byte-for-byte unchanged at SHA-256
  `f3499ff66aa9e15d3f7788153b3f109dc7b020956a6d306eda5e103e806075ed`.
- The full workspace run passed 183 CLI tests, 177 proxy unit tests, and 26 proxy process tests. The
  optimized executable rerun separately passed all 26 process tests through
  `VHRN_PROXY_TEST_BIN`.
- Those named process cases cover exact endpoints and denial bodies; HTTP/1.0 and HTTP/1.1 ingress;
  startup/configuration/log/token failures with redaction; live public, local, mode, log, and repair
  behavior; broker readiness, short failures, timeouts, streaming, and rechecks; persistent HTTP;
  CONNECT framing, eager bytes, established-tunnel revocation semantics, and forced drain;
  cancellation; 128 established clients; first and second SIGTERM; bounded lifecycle; and removal
  of every ambient proxy variable. The updated coverage ledger traces the owning focused tests for
  the remaining normative details.
- No cross-feature gap or candidate-only assumption was found, so Phase 12 added no duplicate test
  owner and no new test harness.

## Image and engine evidence

Tool versions used for qualification were `rustc 1.98.1`, `cargo 1.98.1`, Apple `container 1.4.1`,
Colima `0.10.3`, Docker client `29.8.1` with server `29.5.2`, `cargo-audit 0.22.2`, and
`actionlint 1.7.12`. The host architecture was arm64.

- `make -C proxy-rs ENGINE=docker TAG=rust-candidate` passed against the local Colima Unix socket.
  Docker inspection reported `linux/arm64`, user `65532:65532`, entrypoint `/vhrn-proxy`, exposed
  `8080/tcp`, and one rootfs layer.
- `make -C proxy-rs ENGINE=container TAG=rust-candidate` passed. A separate repository-root
  `container build --platform linux/amd64 --tag vhrn-proxy:rust-candidate --file
  proxy-rs/Dockerfile .` passed, including the Dockerfile's `strip` and `readelf` checks for no
  interpreter and no dynamic `NEEDED` entries. Apple inspection reported a one-layer Linux amd64
  image with the same user and entrypoint. The native Linux arm64 build passed the same checks.
- The workflow's combined build uses repository context `.`, `proxy-rs/Dockerfile`, platforms
  `linux/amd64,linux/arm64`, and `push: false`. The exact inline image smoke was replayed locally and
  passed metadata, no-shell, read-only-root, no-capability, documented-mount, health/status, and
  graceful-stop checks.
- Ephemeral qualification steps exercised the same disposable tag on Apple `container` and on
  Docker through Colima. Both redacted transcripts reached these checkpoints: client proxy
  environment and firewall loaded; public allow passed; exact public denial passed; brokered
  `localhost`, IPv4, and IPv6 grants passed; direct egress firewall denial passed; live local
  revocation passed; live `report` mode plus audit passed; live `open` mode without audit passed;
  SIGTERM stop passed; cleanup confirmed. Host paths and the broker token were not retained.
- `docker_endpoint_precedence_and_frozen_command_env` passed. Inspection of
  `resolve_docker_endpoint` confirms canonical native (`unix:///var/run/docker.sock`), Docker
  Desktop, `tcp://`, and `ssh://` endpoints fail the local-Colima predicate. Residual limitation:
  because the context name is host-controlled, a context containing `colima` plus an arbitrary Unix
  endpoint is accepted; Phase 12 does not change that earlier-phase host interface.
- All containers and staging directories created by qualification were removed. Only the disposable
  local candidate tag was used. No image was pushed and no external registry state changed.

## Validation evidence

1. `cargo fmt --all -- --check` — passed.
2. `cargo clippy --workspace --all-targets --locked -- -D warnings` — passed with no warnings.
3. `cargo test --workspace --locked` — 183 CLI, 177 proxy unit, and 26 proxy process tests passed;
   both doc-test targets passed.
4. `cargo build --release --locked -p vhrn-proxy` — passed.
5. `VHRN_PROXY_TEST_BIN="$PWD/target/release/vhrn-proxy" cargo test --package vhrn-proxy
   --locked --test proxy_process` — 26 passed.
6. `cargo install cargo-audit` resolved the current release, 0.22.2, into the normal Cargo binary
   directory; `cargo audit --deny warnings` loaded 1,251 RustSec advisories, scanned 140 locked
   dependencies, and passed.
7. `make -C proxy-rs ENGINE=docker TAG=rust-candidate` — passed.
8. The workflow-equivalent inline Docker image smoke — passed.
9. The root-context Linux amd64 and Linux arm64 Dockerfile builds — passed.
10. `make -C proxy-rs ENGINE=container TAG=rust-candidate` — passed.
11. The redacted Apple `container` and Docker/Colima engine qualifications described above — passed.
12. `cargo test -p vhrn --locked run::tests::docker_endpoint_precedence_and_frozen_command_env --
    --exact` — passed.
13. `actionlint` 1.7.12 on `_build-proxy-rs.yml`, `_test.yml`, and `ci.yml` — passed, including
    ShellCheck of the inline smoke.
14. `git diff --check` — passed.

## Independent review evidence

- Per user direction, the regular reviewer was used because Phase 12 made no Rust changes. The
  initial review found one blocking medium issue: the isolated candidate workflow did not yet load
  and run an image. It also identified the low-severity host-controlled Docker-context-name
  limitation documented above and noted that a strict fix belongs to the earlier owning phase.
- The blocking issue was resolved with the compact inline smoke described above; its exact body
  passed locally and `actionlint`/ShellCheck remained clean.
- Final rereview approved the complete diff with no material correctness, security, regression, or
  test-coverage finding. It independently confirmed the smoke's failure-safe cleanup and runtime
  boundary, the separate nonpublishing two-platform build, the release-binary process seam, every
  named coverage test, the frozen contract hash, and the absence of production-selection or
  publication changes. Phase 13 remains `Blocked` because explicit cutover approval has not been
  given.
