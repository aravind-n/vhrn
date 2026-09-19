# Rust proxy replacement execution specification

Replace the shipping Go egress proxy with the Rust candidate while preserving the proxy contract,
the host-side security boundary, and the `vhrn-proxy` image interface.

This document is the single source of truth for the work. Each phase is one pull request owned by
one implementation agent. The phase gives that agent its objective, inputs, scope, required
implementation, validation, and completion criterion so it can begin work without doing a second
planning pass.

## How to execute this plan

- Assign one phase PR to one implementation agent. Keep the PR inside that phase's file and
  responsibility scope.
- Complete the two pre-flight phases before assigning the next incomplete numbered phase. Phase 1
  records the candidate that already exists; its checked status does not waive either pre-flight.
- Read this document, `AGENTS.md`, and the inputs named by the assigned phase before editing.
- Keep production code, tests, fixtures, dependencies, and CI needed by a behavior in the same
  phase PR. Do not defer tests, CI, documentation, or evidence to a later PR.
- Use Linux-kernel-style imperative commit subjects such as `proxy: Add broker timeout coverage`.
- Open each phase PR against the accepted result of the preceding phase. Its description identifies
  the contract outcomes it owns, changed paths, validation commands, evidence, and cleanup result.
- Give every phase PR an independent review for security, correctness, regressions, test quality,
  clean-room integrity, and scope. Resolve findings in that PR and obtain rereview before marking
  the phase complete.
- Mark a phase complete only when its implementation, tests, CI, documentation, evidence, review,
  and completion criterion are all satisfied in the phase PR. A partial pass leaves the checkbox
  open and blocks the next phase.
- If a phase uncovers work owned by another phase, record the finding and leave that other phase
  open. Do not grow the active phase into an omnibus change.
- Stop for user direction when an observed behavior conflicts with an `AGENTS.md` security
  invariant, a required local service is unavailable, or completion requires publishing an image,
  tagging a release, changing a version, or changing an external service.

## Clean-room boundary

The clean-room boundary applies to every phase:

- The Pre-flight Phase 1 characterization agent may inspect and execute the Go proxy under
  `proxy/`. It may write only the normative consumer contract, language-neutral fixtures,
  characterization tests, its phase evidence, and its status update. It does not read or edit Rust
  proxy implementation or tests.
- Rust implementation agents read `docs/proxy/contract.md`, the frozen fixtures,
  consumer-facing host interfaces, `AGENTS.md`, and Rust source. They do not inspect current or
  historical Go source, tests, module files, diffs, or Go-derived explanations.
- Only an orchestrator or read-only parity reviewer may execute black-box reference-versus-candidate
  comparisons. A discrepancy becomes a language-neutral contract outcome before a Rust agent
  receives it. The Rust agent receives the required outcome and observations, not the Go-derived
  explanation.
- The contract describes inputs, observable outcomes, and externally visible side effects. It does
  not encode implementation structure, control flow, identifiers, or rationale from either proxy.

## Fixed interfaces and end state

Until cutover, `proxy/` remains the shipping Go implementation and `proxy-rs/` remains the isolated
Rust candidate. Candidate work must not change release selection or existing immutable image tags.

At cutover:

- the qualified Rust tree moves from `proxy-rs/` to `proxy/` without implementation changes;
- the Cargo workspace member changes from `proxy-rs` to `proxy`;
- Go source, module files, tests, toolchain setup, and vulnerability jobs are removed;
- `vhrn-proxy`, port 8080, the scratch runtime, unprivileged identity, environment variables,
  CLI release-clock tagging, and multi-platform publication remain stable;
- Rust is the only proxy implementation and the only source of the production proxy image.

Use the idiomatic Rust 2018+ module-file layout throughout: parent modules use `name.rs` and child
modules use `name/child.rs`. The source tree must contain no `mod.rs` files.

The proxy and CLI remain independent crates. The proxy owns its executable contract; `vhrn`
independently implements the consumer side. Do not share policy, target, or broker protocol types
between the crates.

---

## Pre-flight Phase 1: Clean-room analysis

- [ ] Status: Incomplete

### Objective

Inspect the shipping Go proxy as a black-box consumer boundary and produce the normative behavioral
specification that every Rust implementation phase will use.

### Agent scope

The assigned characterization agent may read and run:

- `proxy/**`;
- the host-side code that launches the proxy and consumes its status and denial log;
- existing user-facing security and runtime documentation;
- language-neutral inputs needed to exercise the shipping binary.

It may edit only:

- `docs/proxy/contract.md`;
- `proxy-rs/testdata/*.tsv` or a replacement language-neutral fixture location selected here;
- Go characterization tests that directly exercise production behavior;
- this phase's status and evidence entry.

The agent must not read or edit `proxy-rs/src/**`, `proxy-rs/tests/**`, or Rust proxy commits.

### Required work

1. Inventory the complete consumer surface: startup environment, live policy files, public and
   local target syntax, mode behavior, DNS and address outcomes, HTTP forwarding, CONNECT,
   authenticated broker framing, direct endpoints, denial logging, errors, and shutdown.
2. Characterize every behavior with executable observations against the shipping proxy. Tests must
   exercise production behavior rather than duplicate policy logic in a test helper.
3. Define the observable result and side effects for each case, including status class, origin or
   broker contact, logged record, connection lifetime, and process exit where applicable.
4. Resolve ambiguous edge cases explicitly: parser-buffered CONNECT bytes, streaming and
   cancellation, absolute-form HTTPS, hop-by-hop headers, half-close behavior, live policy
   replacement, mixed DNS answers, malformed policy, readiness failure, and shutdown bounds.
5. Write language-neutral fixtures for tabular input spaces. Each fixture row must be consumed by a
   characterization test and must state an outcome rather than an implementation fact.
6. Run the characterization suite repeatedly with hard deadlines and deterministic cleanup.
7. Freeze the contract, fixtures, characterization tests, and evidence in the same phase PR.

### Completion criterion

Every consumer-visible proxy surface has a normative outcome; every fixture row is executed against
the shipping binary; all characterization tests pass repeatedly; no Rust proxy source or test was
used to derive the result; and a reviewer can give a Rust agent the contract and fixtures without
adding a Go-derived explanation.

---

## Pre-flight Phase 2: Implementation phase planning

- [ ] Status: Incomplete

### Objective

Turn the frozen behavioral specification into bounded, dependency-ordered assignments that fit
within one agent's context window.

### Agent scope

The planning agent reads the frozen contract and fixtures, `AGENTS.md`, this plan, host-side Rust
interfaces, and the Rust candidate. It does not inspect Go source, Go tests, Go module files, or Git
history containing them. It edits only this plan and its evidence entry.

### Required work

1. Map every contract section and fixture corpus to exactly one numbered phase below.
2. Confirm that each phase owns a small, cohesive behavior or delivery boundary and can be completed
   by one agent without also implementing another phase.
3. Confirm dependency order. A phase may rely only on completed phases and the frozen contract.
4. Give every phase exact inputs, editable paths, required behavior, validation commands, and a
   checkable completion criterion.
5. Split or reorder any phase that would require an agent to rediscover requirements, hold too many
   unrelated subsystems in context, or leave tests and CI to a later cleanup phase.
6. Map every contract outcome to a planned unit, process, image, or engine-level observation.
7. Present the revised packet list for user approval before any incomplete implementation phase is
   assigned.

### Completion criterion

Every frozen contract outcome has one owning phase; each incomplete phase is independently
assignable and dependency-ordered; validation and completion criteria are executable; and the user
has approved the resulting phase list.

---

## Phase 1: Build the isolated Rust proxy candidate

- [x] Status: Complete

### Objective

Establish a non-shipping Rust proxy candidate and the host integration needed to exercise it without
changing the production proxy selection.

### Delivered scope

- A root Cargo workspace containing the existing `vhrn` package and `vhrn-proxy` under
  `proxy-rs/`, with edition 2024 and workspace lint policy.
- A thin proxy binary over a library organized as configuration, typed domains and policy,
  diagnostics, concrete public and broker connectors, HTTP server ownership, relay lifecycle, and
  shutdown.
- Public and loopback HTTP and CONNECT routing, single-resolution public dialing, TLS verification,
  authenticated broker framing, bounded pools and bodies, live policy reads, denial records, and
  supervised shutdown.
- Language-neutral contract fixtures plus unit and process tests for the candidate.
- A scratch candidate image and candidate-only Makefile under `proxy-rs/`.
- CLI broker, policy, and lifecycle integration needed to launch the candidate contract.
- Workspace CI coverage that builds and tests the candidate while the Go image remains the shipping
  target.

### Preserved boundaries

- `proxy/` remains the production image source.
- `proxy-rs/` is not selected by install, release, or image publication workflows.
- Public dialing and brokered loopback routing use separate typed connectors.
- The module layout uses `name.rs` plus `name/child.rs`; no `mod.rs` exists.

### Completion criterion

The candidate and CLI pass formatting, strict Clippy, unit tests, proxy process tests, and locked
release builds; the candidate image recipe is isolated from production packaging; and the tracked
tree contains the complete candidate described above.

---

## Phase 2: Close public egress contract coverage

- [ ] Status: Incomplete

### Objective

Make the Rust candidate satisfy every frozen public policy, address, HTTP, and CONNECT outcome.

### Inputs and editable paths

Read `docs/proxy/contract.md`, the frozen public-domain/address/HTTP/mode fixtures, `AGENTS.md`, and
the Rust candidate. Work in public policy and target modules, the public connector, HTTP routing and
relay code, related tests, and the Phase 2 evidence artifact. Do not inspect Go materials.

### Required implementation

1. Build a coverage matrix from each public contract outcome to an exact Rust unit or process test.
2. Ensure every public decision reopens all required policy layers and mode state, fails closed on
   unreadable or malformed required state, and authorizes before pool lookup.
3. Ensure hostname dialing resolves once, requires a nonempty answer set, classifies every answer,
   rejects the whole set when any address is forbidden, and dials only a validated numeric address.
4. Cover public IPv4 and IPv6, IPv4-mapped IPv6, translated forms, loopback, private, link-local,
   multicast, unspecified, carrier-grade NAT, and mixed answer sets.
5. Preserve normalized scheme, host, effective port, and TLS identity independently from the numeric
   dial address. Verify certificate failures cannot fall back to plaintext.
6. Cover absolute-form HTTP, request and response bodies, hop-by-hop and `Connection`-nominated
   headers, origin failures, bounded bodies, cancellation, connection reuse, dead keep-alive reuse,
   eviction, and policy changes before reuse.
7. Cover CONNECT default and explicit ports, denial, dial failure, optimistic buffered bytes,
   bidirectional relay, half-close, cancellation, and tunnel lifetime.
8. Add or correct implementation and tests in the same phase PR. Every async test gets a hard
   deadline and deterministic cleanup.

### Validation

Run formatting and strict Clippy for `vhrn-proxy`, the complete proxy unit and process suites, and
targeted repeated cancellation and connection-pool tests. Record the exact commands and results.

### Completion criterion

Every frozen public outcome maps to a passing executable test; mixed or forbidden DNS answers open
no socket; captured dials are numeric and single-resolution; policy is checked before each pool
checkout; HTTP and CONNECT cancellation leave no retained task or socket; and no public route can
construct or use a broker connector.

---

## Phase 3: Close brokered loopback contract coverage

- [ ] Status: Incomplete

### Objective

Make the Rust proxy and host broker satisfy every frozen loopback authority, broker protocol, local
HTTP, and local CONNECT outcome.

### Inputs and editable paths

Read the frozen contract and local/broker fixtures, `AGENTS.md`, `src/broker.rs`, `src/net.rs`,
`src/run.rs`, and Rust candidate broker/routing modules and tests. Do not inspect Go materials.

### Required implementation

1. Build a coverage matrix from every local and broker outcome to an exact host or proxy test.
2. Accept only canonical loopback authorities with nonzero ports: `localhost`, exact `127/8`, and
   `[::1]`. Keep `localhost`, IPv4, and IPv6 grants distinct and exact.
3. Reopen the three local policy layers before each request or tunnel and authorize before local
   pool lookup. Preserve the specified behavior for established tunnels after policy changes.
4. Validate the 64-byte lowercase-hex token without exposing it in display, debug, logs, panics, or
   process output.
5. Enforce readiness before serving local work, authenticate every broker connection, bound frames
   and aggregate handshake time, preserve co-read bytes, and close pending exchanges on drop.
6. Cover local HTTP streaming, TLS identity, hop-by-hop headers, response bounds, connection reuse,
   eviction, dead connections, downstream disconnects, and origin cancellation.
7. Cover local CONNECT authorization, buffered bytes, relay lifetime, half-close, cancellation, and
   revocation behavior.
8. Verify the host broker independently rechecks live policy, caps connections, limits dials to the
   requested loopback authority, and removes sockets, relays, and token staging on cleanup.
9. Add or correct implementation and tests in the same phase PR, with hard deadlines and
   deterministic cleanup.

### Validation

Run formatting and strict Clippy for both Rust packages, both complete test suites, repeated broker
protocol and cleanup tests, and token-redaction assertions over success and failure paths.

### Completion criterion

Every frozen local outcome maps to a passing executable test; local requests reach only the fixed
authenticated broker; public policy cannot authorize loopback; current policy is checked before
reuse; protocol limits and deadlines are enforced; diagnostics are token-free; and teardown leaves
no broker listener, relay, token file, or active policy registration.

---

## Phase 4: Close startup, diagnostics, and lifecycle coverage

- [ ] Status: Incomplete

### Objective

Complete the candidate's process boundary so malformed input, load, disconnects, and termination are
bounded and observable according to the frozen contract.

### Inputs and editable paths

Read the frozen startup/process/mode fixtures, `docs/proxy/contract.md`, `AGENTS.md`, proxy
configuration, diagnostics, listener, response, shutdown, and process-test code. Work only in those
areas and their tests unless a failing contract row is owned by Phase 2 or 3.

### Required implementation

1. Cover startup defaults, singular and plural public paths, complete and partial local
   configuration, invalid tokens, occupied listeners, and failed broker readiness.
2. Keep `/healthz` and `/__status` available with their exact content types and bodies. When live
   policy cannot be trusted, report the contract's fail-closed effective status.
3. Preserve denial-log record shape consumed by the host. Bound and sanitize attacker-controlled
   fields, append atomically as required, and keep logging failures on their specified response path.
4. Bound parser headers, body collection, broker frames, diagnostic fields, concurrent connections,
   task queues, and every network deadline.
5. Own accepted connections and tunnels in supervised task sets. Every spawned task must have one
   owner, an observable result, and a shutdown path.
6. On SIGTERM, stop accepting, signal agent/proxy/broker work in the required order, close
   transports, drain within the contract bound, and reap every child.
7. Ensure the process harness strips ambient proxy variables, applies hard test deadlines, and
   deterministically removes child processes and temporary state.
8. Add or correct implementation and tests in the same phase PR.

### Validation

Run the complete proxy process suite repeatedly, strict Clippy, malformed-input cases, occupied-port
cases, disconnect loops, and SIGTERM loops. Confirm no owned child process, task, socket, or
temporary file survives.

### Completion criterion

Every frozen startup, diagnostics, and lifecycle outcome has a passing process-level observation;
all external inputs and waits are bounded; policy failures remain fail-closed; every spawned task is
owned and observed; and repeated startup failure, disconnect, and SIGTERM tests leave no resources.

---

## Phase 5: Qualify dependencies and coexistence CI

- [ ] Status: Incomplete

### Objective

Make dependency provenance and candidate CI reproducible without changing the shipping proxy or
publishing the candidate.

### Inputs and editable paths

Read the workspace manifests, lockfile, proxy manifest, and `.github/workflows/**`. Edit dependency
declarations, the lockfile, candidate CI paths, and a dependency evidence report. Preserve the
production Go job and production image selection until cutover.

### Required implementation

1. Use `cargo metadata` to record every direct proxy dependency's purpose, source, license metadata,
   and enabled features.
2. Use `cargo tree` to inspect resolved features and duplicate versions. Record a disposition for
   every direct dependency and meaningful duplicate.
3. Run `cargo audit` against the locked graph. Request permission before installing a missing tool;
   an unavailable audit leaves the phase incomplete.
4. Remove unused features or dependencies and regenerate the lockfile only when required by a
   recorded finding.
5. Ensure candidate paths trigger formatting, workspace Clippy, locked workspace tests, and explicit
   release builds for `vhrn` and `vhrn-proxy`.
6. Ensure candidate CI cannot publish images, alter release metadata, or select the Rust image for
   production. Keep Go tests and vulnerability checks active.
7. Run workflow lint, including ShellCheck over inline scripts.

### Completion criterion

Every dependency and enabled feature has a recorded purpose and license disposition; the locked
graph has no unresolved advisory; workflow lint passes; candidate changes run all Rust gates; the
shipping proxy still runs its own gate; and no candidate job can publish or change production
selection.

---

## Phase 6: Establish parity and resource bounds

- [ ] Status: Incomplete

### Objective

Produce reproducible black-box evidence that the candidate meets the frozen behavior contract and
operates within explicit resource bounds.

### Agent scope

Assign this phase to an orchestration agent allowed to execute both binaries and add
language-neutral workload tooling and evidence. Use a separate read-only reviewer to verify the
results. Neither agent may give implementation agents source-derived explanations. Any behavioral
discrepancy returns through the contract and the owning Phase 2, 3, or 4 packet.

### Required work

1. Build the reference and candidate separately and run the same frozen corpus against each.
2. Reject malformed, partial, or zero-valued measurements rather than recording them as passes.
3. Use the same machine and workload for at least five interleaved repetitions of startup time,
   idle RSS, HTTP latency and throughput, concurrent CONNECT throughput, peak RSS, and resource
   return after cancellation and shutdown.
4. Record static binary size and compressed image size separately from runtime measurements.
5. Stress slow bodies, partial frames, half-closes, downstream disconnects, policy replacement,
   pool churn, concurrent tunnels, and shutdown races.
6. Define accepted quantitative bounds from the measurements. Obtain explicit user approval for any
   accepted behavioral difference or material resource regression.
7. Check in the workload description, raw measurements, summary method, environment, commands, and
   results so another agent can reproduce them.

### Completion criterion

All approved consumer outcomes match; every measurement has valid repeated samples; medians and
peaks meet recorded bounds or have explicit user approval; stress runs stay within task, socket,
memory, and time limits; and resources return to the recorded baseline after cancellation and
shutdown.

---

## Phase 7: Qualify the multi-architecture candidate image

- [ ] Status: Incomplete

### Objective

Prove that the candidate image is a minimal, static, non-root scratch image for both release
architectures without changing production tags.

### Inputs and editable paths

Read `proxy-rs/Dockerfile`, `proxy-rs/Makefile`, workspace manifests, and the frozen startup
contract. Edit only the candidate build path, image-specific tests, and Phase 7 evidence.

### Required implementation

1. Build locked release binaries and candidate images for `linux/arm64` and `linux/amd64` under
   non-release tags.
2. Verify architecture, absence of an ELF interpreter and dynamic `NEEDED` entries, and scratch
   contents containing only the proxy executable.
3. Verify UID/GID `65532:65532`, port 8080, entrypoint, startup environment, policy and denial-log
   permissions, and a read-only root filesystem where supported.
4. Run direct image checks for valid startup, invalid configuration, health/status, public allow and
   deny, broker readiness, redaction, and SIGTERM.
5. Record image digest, architecture, builder, exact commands, and cleanup result for each image.
6. Remove every temporary container and local qualification tag after recording immutable digests.

### Completion criterion

Both architectures pass static inspection and direct runtime checks; both evidence records identify
the exact image digest and commands; no release or immutable production tag changed; and no
qualification container or temporary image remains.

---

## Phase 8: Qualify Apple container end to end

- [ ] Status: Incomplete

### Objective

Exercise the exact candidate image through Apple `container`, the real CLI run path, and host broker
integration.

### Inputs and editable paths

Use the qualified candidate digest, `VHRN_PROXY_IMAGE`, an isolated XDG root, and a throwaway project.
Edit only Apple-engine routing defects, engine-specific tests, and Phase 8 evidence. Changes to
cross-engine behavior return to their owning earlier phase.

### Required work

1. Record the Apple `container` version, host architecture, candidate digest, CLI revision, and all
   commands.
2. Exercise install/local image resolution without changing user-owned installed state.
3. Run public HTTP and CONNECT in enforce, report, and open modes, including live policy changes.
4. Run brokered local HTTP and CONNECT for `localhost`, IPv4 loopback, and IPv6 loopback grants.
5. Verify proxy IP discovery, gateway routing, policy mounts, token isolation, denial log, health,
   status, and unsupported target rejection.
6. Send SIGTERM during active public and local traffic. Confirm agent, proxy, broker, policy lease,
   token staging, socket, and container cleanup.
7. Repeat the critical path enough to expose launch and teardown races.

### Completion criterion

The real CLI path passes every required public, local, policy, and shutdown check against the
qualified candidate digest; all engine-specific tests pass; the isolated state is captured in the
evidence report; and no owned container, socket, token file, broker task, or active policy lease
remains.

---

## Phase 9: Qualify Docker through Colima end to end

- [ ] Status: Incomplete

### Objective

Exercise the exact candidate image through the supported local Docker/Colima endpoint and the real
CLI run path.

### Hold point

Starting or changing Colima, installing Rosetta, or changing Docker contexts requires explicit user
authorization. If an approved local amd64-capable environment is unavailable, leave this phase open
and record the blocker; do not substitute unapproved remote evidence.

### Inputs and editable paths

Use the qualified candidate digest, an explicit local Colima Unix endpoint, `VHRN_PROXY_IMAGE`, an
isolated XDG root, and a throwaway project. Edit only Docker-engine routing defects,
engine-specific tests, and Phase 9 evidence.

### Required work

1. Record Colima, Docker client/server, architecture, endpoint, candidate digest, CLI revision, and
   all commands.
2. Prove the CLI uses the approved local Unix endpoint and rejects unsupported native Linux,
   Docker Desktop, or remote broker-routing endpoints.
3. Run the same public HTTP, public CONNECT, policy-mode, live-update, brokered local HTTP, and local
   CONNECT checks required by Phase 8.
4. Verify `host.docker.internal` broker routing, proxy address inspection, mounts, capabilities,
   token isolation, denial logging, and unsupported target rejection.
5. Send SIGTERM during active traffic and verify complete agent, proxy, broker, policy, token,
   socket, and container cleanup.
6. Repeat the critical path enough to expose launch and teardown races.

### Completion criterion

The real CLI path passes every required check through the approved Colima endpoint against the
qualified candidate digest; unsupported endpoints fail explicitly; engine-specific tests pass; and
no owned container, socket, token file, broker task, or active policy lease remains.

---

## Phase 10: Cut over production packaging to Rust

- [ ] Status: Incomplete

### Objective

Replace the production Go proxy with the exact qualified Rust candidate while preserving all image,
CLI, release, and security contracts.

### Entry gate

Phases 2 through 9 must be complete, both engine records must identify the qualified digest or its
documented architecture pair, and the user must explicitly approve cutover before destructive source
removal or production workflow changes begin.

### Required implementation

1. Move the qualified `proxy-rs/` tree to `proxy/` without changing implementation contents during
   the move.
2. Change the Cargo workspace member to `proxy` and update paths that refer to `proxy-rs`.
3. Remove Go source, tests, `go.mod`, Go setup, `govulncheck`, and superseded Go build stages.
4. Make `make -C proxy` build the Rust production image for both supported engines with the existing
   image name and tag interface.
5. Update CI path filters and reusable workflows so proxy changes run Rust formatting, strict
   Clippy, locked tests, release builds, dependency checks, image builds, and workflow lint.
6. Update nightly and release workflows to publish the qualified Rust image through the existing
   `vhrn-proxy` release clock and multi-platform manifest flow.
7. Preserve the scratch entrypoint, non-root identity, port, environment variables, policy mounts,
   denial-log format, broker protocol, image name, and CLI tag resolution.
8. Keep existing immutable image tags untouched. Rollback is a prior matched CLI/proxy release or a
   reverted change published under a new version.

### Validation

Run the full Rust workspace gate, production image builds for both architectures, both supported
engine smoke suites, actionlint, ShellCheck, dependency audit, and an immutable-tag review.

### Completion criterion

Rust is the only proxy source and production image implementation; no Go proxy tooling remains;
production packaging selects the same qualified candidate; both engines pass through the real CLI;
and install, update, nightly, and release paths preserve the established image contract.

---

## Phase 11: Update repository documentation and changelog

- [ ] Status: Incomplete

### Objective

Make repository guidance describe only the verified Rust production proxy and its actual operational
contract.

### Inputs and editable paths

Read the completed phase evidence and final source/workflow tree. Edit `AGENTS.md`, `README.md`,
`docs/sandbox-design.md`, `docs/runbooks/release.md`, relevant contributor documentation, and the
unreleased changelog section. Do not claim behavior not supported by checked-in evidence.

### Required implementation

1. Update project layout, build commands, test commands, CI descriptions, dependency model, image
   construction, engine behavior, and release procedures for the Rust proxy.
2. Keep the security model precise: external firewall enforcement, typed public versus broker
   routing, live policy, token isolation, supported engines, and known threat-model exclusions.
3. Remove Go-specific contributor, vulnerability, build, and release instructions.
4. Verify every documented command against the final tree.
5. Add one concise `[Unreleased]` changelog entry describing the proxy replacement without a version
   bump, release claim, or unverified performance claim.
6. Confirm every preceding phase and pre-flight has a completed status and evidence entry, then move
   this plan to `docs/plans/completed/` in the same phase PR.

### Validation

Run every command changed or cited by the documentation, check internal links and paths, search for
stale Go proxy instructions, and confirm the completed plan retains all status and evidence records.

### Completion criterion

Every repository instruction and command matches the final tree; security and engine claims are
backed by completed evidence; no Go proxy instruction remains; the changelog states only the
user-visible change; the phase PR has passed its independent review; and the evidence-complete plan
is filed under `docs/plans/completed/`.

## Evidence log

Append one row only when a phase reaches its completion criterion. Link detailed reports rather than
expanding this plan with command output.

| Phase | Revision or image | Evidence | Platform | Result |
| --- | --- | --- | --- | --- |
| Phase 1 | `f8abeed` | Workspace formatting, strict Clippy, CLI tests, proxy unit/process tests, locked release builds, and tracked-tree verification | macOS arm64 | Complete |

## Publication hold

The phase PR model does not authorize image publication, tags, version bumps, or releases. Remote
branch, pull-request, and merge operations follow the user's direction for the assigned phase.
