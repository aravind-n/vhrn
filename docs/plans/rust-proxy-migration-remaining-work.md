# Rust proxy migration: remaining work

Status: reassessment required. The Ship run is paused during Step 13. This document is a recovery
checklist, not an acceptance record for the existing implementation.

## Working rule

Treat every migration artifact after base commit
`9c1603a0bbe2754c411fe50529896d63f6524c08` as a candidate until the reassessment below gives it an
explicit `keep`, `rework`, or `remove` disposition. Do not resume implementation at Step 13 merely
because the paused Ship ledger labels earlier steps complete.

Preserve clean-room separation during reassessment and any recovery work:

- Characterization reviewers may inspect and run the shipping proxy only to establish
  consumer-visible outcomes.
- Rust reviewers assess the Rust implementation against the normative consumer contract, repository
  security invariants, and idiomatic Rust engineering standards.
- A discrepancy becomes a language-neutral contract outcome before a fresh Rust author receives it.
  That author receives the outcome and required observations, not an explanation derived from the
  shipping implementation.
- A conflict between observed behavior and an `AGENTS.md` security invariant stops for user
  direction.

## Current checkpoint

- Branch: `proxy/rust-migration`.
- Accepted base: `9c1603a0bbe2754c411fe50529896d63f6524c08`.
- Current committed tip: `c3dd9e08efd077a4de71df66a46fc9b3b07adc3e`.
- The branch contains the consumer contract, shared policy crate, Rust proxy, test harnesses, and
  five post-Step-12 remediation commits. Their presence is not evidence that they meet the quality
  bar.
- Step 13 is uncommitted. `proxy/Makefile` is modified and `proxy/Dockerfile.rust` is untracked.
- The native arm64 candidate built and passed its direct smoke check. The amd64 candidate built and
  passed static and image-content inspection, but its runtime smoke did not run because Rosetta is
  absent. The user explicitly declined installing Rosetta. Colima was stopped when last checked.
- `proxy/Dockerfile` and the default Make target still build the shipping implementation. Existing
  CI, release packaging, `AGENTS.md`, and `README.md` still describe that implementation.
- `docs/plans/rust-proxy-migration.md` has stale status and evidence sections. It must not be used as
  proof that a phase passed.
- Candidate CI qualification, full differential and resource evidence, both-engine qualification,
  cutover, final review, final documentation, and changelog work have not been completed.

The paused historical ledger is
`~/.agents/state/ship/vhrn-rust-proxy-migration-bbbb1a4568-20260909T181936Z.md`. Use it to locate
claims and commands that need verification; do not treat its verdicts as independent evidence.

## Gate 1: reassess the existing work

Complete this gate before accepting, extending, or committing the Step 13 worktree changes.

### 1. Audit provenance and clean-room integrity

Review every commit and uncommitted path in
`9c1603a0bbe2754c411fe50529896d63f6524c08..HEAD`, plus the Step 13 worktree diff. Assess source,
tests, comments, identifiers, module boundaries, control flow, and commit chronology for evidence
that implementation details crossed from the characterization room into Rust work. Address the
known chronology caveat that the inert Rust workspace commit preceded the final characterization
closure commit.

Completion criterion: a checked-in audit maps every migration commit and changed path to `keep`,
`rework`, or `remove`, explains the evidence for each nontrivial disposition, and gives an explicit
clean-room verdict without relying on the paused Ship ledger's conclusion.

### 2. Audit the consumer contract and characterization

Revalidate `docs/proxy-consumer-contract.md` and every shared `testdata/*.tsv` row against an
executable consumer-visible observation of the shipping proxy. Confirm that characterization tests
exercise production behavior rather than reconstructed policy logic, test-only stand-ins, fixture
text, or assertions that merely restate their expected values. Revisit startup, policy replacement,
domain and IP classification, public and local HTTP, public and local CONNECT, broker framing,
headers, streaming, cancellation, half-close, diagnostics, and shutdown.

Completion criterion: every normative statement has a direct executable observation with a hard
deadline; unsupported statements are removed or re-characterized; every fixture row is consumed by
the relevant reference test; and the complete reference suite passes repeatedly.

### 3. Audit the Rust architecture and security boundary

Review the Rust implementation from first principles rather than commit-by-commit intent. In
particular, assess:

- whether `vhrn-policy` contains only pure boundary values shared with the host;
- fail-closed policy loading and per-decision policy refresh;
- typed separation between public dialing and authenticated broker routing;
- single-resolution address validation, mixed-address rejection, and numeric dialing;
- authorization before connection-pool lookup and bounded pool/task/socket lifetimes;
- HTTP target handling, streaming bounds, cancellation, CONNECT buffering, half-close, and relay
  ownership;
- broker token validation, frame bounds, aggregate deadlines, readiness, and redaction;
- listener supervision, shutdown, diagnostic bounds, and ownership of every spawned task;
- module cohesion, duplication, error types, test seams, and whether the large service and connector
  modules should be decomposed or rewritten.

Completion criterion: every Rust module has a documented `keep`, `rework`, or `remove` disposition;
all security-significant paths have an evidence-backed review; and no unresolved CRITICAL or HIGH
finding remains hidden inside a general “tests pass” verdict.

### 4. Audit test quality and missing coverage

Build a coverage matrix from each contract outcome to the exact unit, integration, process, or image
test that observes it. Verify that tests fail when the relevant production behavior is broken and
that asynchronous and child-process checks have hard deadlines and deterministic cleanup. Recheck
the previously noted gaps instead of assuming later remediation closed them:

- executable public HTTP, CONNECT, TLS-failure, mixed-IP, and cancellation behavior;
- invalid-token startup with no listener;
- slow or partial HTTP headers and broker frames;
- public and broker pool churn, eviction, dead keep-alive reuse, and resource return;
- a process-level grant exercised from each of the five public policy layers;
- full image checks for policy and log permissions, broker traffic, allow/deny/report behavior,
  redaction, SIGTERM, and cleanup;
- distinct schemas for workload, resource, and image-size evidence.

Keep the process harness in Rust. Keep shell code limited to engine orchestration and shell-native
checks; do not wrap a different scripting language in a shell test.

Completion criterion: the checked-in matrix accounts for every contract outcome and every known
gap, mutation or targeted fault checks demonstrate that critical tests can go red, all tests have
bounded completion, and cleanup leaves no retained process, task, socket, or container owned by the
test.

### 5. Establish a reproducible baseline

Run the reference checks, Rust formatting, workspace Clippy, workspace tests, explicit package
builds, root default-member checks, dependency inspection, and available workflow/shell linters from
a clean checkout of the candidate commits. Record tool versions, commands, exit status, skipped
checks, and required elevated capabilities. Use only `cargo metadata`, `cargo tree`, and
`cargo audit` for dependency and advisory review.

Completion criterion: a checked-in evidence report reproduces every claimed pass, identifies every
unavailable or environment-blocked check as open, and distinguishes local unit/process evidence
from live-container and remote-CI evidence.

## Gate 2: choose the recovery strategy

Use the Gate 1 dispositions to propose one of three bounded paths:

1. retain the implementation and correct a finite list of defects;
2. retain only independently validated contract and boundary work, then rewrite selected Rust
   subsystems;
3. discard the Rust candidate and restart from the validated consumer contract.

Do not rewrite branch history, discard worktree changes, delete the shipping implementation, or
create a replacement branch as part of the proposal.

Completion criterion: the user approves one recovery path with exact retained commits or paths,
exact rewrite scope, acceptance gates, and a step order small enough to review without another
oversized Ship session.

## Work remaining after the recovery decision

The exact implementation scope depends on Gate 2. The following delivery gates remain regardless of
how much current Rust code survives.

### 6. Produce a qualified candidate image

Reassess the uncommitted `proxy/Dockerfile.rust` and `proxy/Makefile` changes, then retain or replace
them according to the recovery decision. The candidate build must remain isolated from the default
shipping target. Verify locked release builds, absence of ELF interpreter and dynamic dependencies,
scratch contents, unprivileged identity, port, entrypoint, and both target architectures.

Run an amd64 runtime smoke through a user-approved environment that does not require installing
Rosetta. Docker through Colima is the current local alternative, but starting or changing that
service requires explicit user direction. A CI runner is acceptable only if the user revises the
gate to accept remote evidence.

Completion criterion: arm64 and amd64 candidate images pass the agreed direct runtime checks; image
digests and exact commands are recorded; the default Make target and every release tag still select
the shipping implementation; and no smoke container remains.

### 7. Add coexistence CI and dependency review

Add path filters and workspace commands that test the Rust candidate without publishing or selecting
it for production. Use explicit `-p vhrn` for CLI binary builds. Keep the current shipping checks and
production image path until cutover. Document each direct Rust dependency's purpose, source,
license metadata, and enabled features using `cargo metadata`; inspect resolved feature and duplicate
graphs with `cargo tree`; check advisories with `cargo audit`.

Completion criterion: workflow lint passes; candidate jobs cannot publish or alter release metadata;
the shipping image path is unchanged; dependency purpose, features, sources, licenses, duplicates,
and advisories all have recorded dispositions; and no `cargo-deny`, `cargo-license`, Syft, or Trivy
configuration is introduced.

### 8. Prove behavioral parity and resource bounds

Build reference and candidate executables separately and run the same consumer corpus against both.
Route every discrepancy through the clean-room outcome protocol before changing Rust. Run at least
five interleaved repetitions for startup, idle memory, HTTP latency and throughput, concurrent
CONNECT throughput, peak memory, resource return, binary size, and compressed image size.

Completion criterion: all approved consumer outcomes match; every accepted difference has explicit
user approval; medians meet the recorded thresholds or have an approved rationale; malformed or
zero measurement evidence is rejected; and tasks, sockets, and memory return to the recorded bounds.

### 9. Qualify both supported container engines

With explicit user authorization for local services, qualify Apple `container` and Docker through a
local Colima Unix endpoint separately. For each engine, record version, architecture, image digest,
permissions, readiness, public HTTP, public CONNECT, brokered local HTTP and CONNECT, denial/report
behavior, SIGTERM, and cleanup. Run an isolated-XDG real CLI invocation with candidate image
overrides.

Completion criterion: both engine records pass against the same candidate digest or a documented
architecture pair; the real CLI and harness path works; and no owned container, token staging file,
broker socket, or active run-policy lease remains.

### 10. Cut over to Rust

Promote the exact qualified candidate recipe to `proxy/Dockerfile`, update the Makefile and workflows
to select it, and remove the temporary candidate path. Remove shipping proxy source, module files,
tests, toolchain setup, vulnerability job, and build stages that the Rust image replaces. Preserve
the image name, tag/release clock, scratch runtime metadata, environment contract, and multi-platform
publishing behavior.

Completion criterion: Rust is the only shipping proxy implementation; the production image matches
the qualified candidate; no superseded proxy toolchain remains in CI or docs; both-engine production
smoke and the full repository gate pass.

### 11. Perform the cutover review

Run a fresh full-diff security, correctness, regression, test-quality, and clean-room review from the
accepted base. Reviewers may use the removed implementation only for consumer-visible differential
and contamination analysis. Rust remediation authors remain isolated and receive outcome-only
briefs.

Completion criterion: no unresolved CRITICAL or HIGH finding remains; every MEDIUM and LOW finding
has an explicit fix or user-approved disposition; and the final review gives evidence-backed
correctness and clean-room verdicts.

### 12. Finish documentation and changelog

Update `AGENTS.md`, `README.md`, `docs/sandbox-design.md`, and `docs/runbooks/release.md` to match the
final source tree, commands, CI, dependency model, engine support, and threat boundary. Reconcile the
stale status and evidence in `docs/plans/rust-proxy-migration.md`, then move the evidence-complete
plan under `docs/plans/completed/`. Add one concise `[Unreleased]` changelog entry without a version
bump or release claim.

Completion criterion: repository instructions describe only the final implementation and verified
capabilities; every documented command has been checked; the completed plan contains reproducible
evidence and all accepted caveats; and the changelog states only shipped behavior.

### 13. Run the final local gate

Run formatting, workspace Clippy with warnings denied, locked workspace tests, explicit release
builds, dependency metadata/tree/audit, workflow lint, shell lint, multi-architecture production
image builds, both-engine smoke checks, immutable-base diff checks, and a final clean-worktree check.

Completion criterion: every required local check passes or has an explicit user-approved external
owner; the worktree contains only intended committed changes; no temporary runtime resource remains;
and the branch is ready for the user's separate publication decision.

## Hold points

Until the user approves the Gate 2 recovery strategy:

- keep the Ship run paused;
- leave Step 13 uncommitted;
- leave the shipping image and CI path unchanged;
- leave the existing branch and history intact;
- do not install Rosetta or start Colima;
- do not remove the shipping implementation;
- do not push, open a pull request, publish an image, tag a release, or update a version.
