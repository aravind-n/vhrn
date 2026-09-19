# Pre-flight Phase 2: Plan the Rust implementation phases

This file preserves the approved Pre-flight Phase 2 specification, candidate analysis, planning
rationale, and completion evidence. The [master plan](plan.md) is the sole source of readiness and
status; numbered phase agents do not read this historical record.

## Objective

Inspect the frozen consumer contract and current Rust candidate, then produce the remaining
dependency-ordered Phase 2 through Phase N implementation packets as individual phase specifications.

Each phase must be a small, cohesive unit that one agent can implement in one pull request without
performing another planning pass.

## Inputs and editable paths

Read:

- the frozen `docs/proxy/consumer-contract.md`;
- `AGENTS.md`;
- the master [`plan.md`](plan.md) and completed [Phase 1 record](phase-01-candidate-scaffold.md);
- allowed host-side Rust interfaces;
- the Rust candidate, its tests, workspace configuration, image recipe, and relevant workflows.

Edit only this pre-flight record, [`coverage.md`](coverage.md), [`plan.md`](plan.md), and the
numbered phase specifications in this directory. Do not inspect Go source, tests, module files,
history, diffs, or explanations.

## Required work

1. Build an active [coverage ledger](coverage.md) mapping every consumer-contract requirement to
   exactly one future phase.
2. Inspect the candidate and classify each requirement as already satisfied, incomplete, absent, or
   requiring correction. Use that result to define Phase 2 through Phase N.
3. Keep each phase small and cohesive, with dependencies only on completed earlier phases.
4. Give every phase its own implementation-ready specification containing an execution contract,
   objective, inputs, editable paths, required behavior, interfaces and data flow, edge cases,
   validation commands, evidence, and a checkable completion criterion.
5. Keep production code, tests, fixtures, CI, documentation, and evidence with the phase that owns
   the behavior. Do not create later cleanup phases for work that belongs with an earlier change.
6. Include all remaining qualification, engine verification, cutover, cleanup, and documentation
   work needed to make Rust the sole production proxy without changing established image or release
   interfaces.
7. Check that the full phase sequence covers the contract exactly once, respects clean-room
   boundaries, and fits within one agent's context per phase.
8. Present the complete Phase 2 through Phase N directory and sequence for user approval before
   assigning Phase 2.

## Planning evidence

- Clean-room inputs inspected: `AGENTS.md`; the frozen consumer contract; this plan and its Phase 1
  record; the host-side Rust interfaces in `cli/src/run.rs`, `cli/src/net.rs`, and
  `cli/src/broker.rs`; the root Rust workspace configuration and resolved candidate dependency
  tree; all Rust candidate source, tests, fixtures, Dockerfile, and Makefile under `proxy-rs/`;
  `shared/testdata/loopback-authorities.tsv`; and the CI, image-build, nightly, release, PR-image,
  and harness-image workflows relevant to candidate qualification and cutover.
- Prohibited inputs were not inspected: current or historical Go source, Go tests, Go module
  files, repository history, diffs, or Go-derived explanations. The shipping Go tree was not
  modified.
- Frozen contract SHA-256 independently confirmed as
  `f3499ff66aa9e15d3f7788153b3f109dc7b020956a6d306eda5e103e806075ed`.
- Candidate baseline: `cargo test -p vhrn-proxy --locked` passed all 75 library tests and all 12
  black-box process tests when run with loopback networking available. The first sandboxed run's
  loopback binds were denied by the execution sandbox; the same tests passed unchanged outside
  that restriction.
- Candidate classification: the isolated image shape, typed public/local split, broker token and
  frame basics, live reads, pooling skeleton, buffered CONNECT handoff, half-close relay, and
  candidate-only build are useful foundations. Contract corrections remain for strict file and
  mode validation, startup gating, canonical targets, CONNECT-only HTTPS, the reviewed IANA
  boundary, pinned multi-address fallback, exact HTTP/1 framing and limits, fully streaming
  forwarding, exact diagnostics and endpoint health, resource ownership, five-second shutdown,
  engine qualification, and production cutover.
- The dependency-ordered phase specifications in [`plan.md`](plan.md) and the active
  [coverage ledger](coverage.md) assign every contract group once.
  Requirements already satisfied by the candidate are assigned to the phase that must preserve
  and prove them; no separate cleanup phase owns behavioral work from an earlier packet.
- Approval: the user approved the complete Phase 2 through Phase 13 directory and sequence on
  2026-09-19.

## Completion criterion

Every frozen contract requirement and delivery obligation has exactly one owning phase; every phase
is independently assignable, implementation-ready, dependency-ordered, and bounded to one PR; the
active coverage ledger has no gaps or duplicate ownership; and the user has approved the complete
phase sequence.

## Historical candidate classification and ownership rationale

This approved planning-time snapshot is historical. The active ownership source is
[`coverage.md`](coverage.md); numbered implementation agents do not use this classification table
for readiness or execution. The rows below are mutually exclusive ownership groups. “Partial”
means the candidate has a useful implementation but does not yet satisfy the frozen outcome;
“correction” means at least one current candidate behavior contradicts the contract. A phase owns
implementation, focused tests, fixtures, documentation, and evidence for its row. Later phases may
consume that behavior but must not redefine it.

| Frozen contract requirement group | Candidate classification | Sole owner |
| --- | --- | --- |
| Component-specific CI ownership for the CLI, isolated Rust candidate, and shipping Go proxy during migration, with an explicit cutover rename/removal path | Correction | Phase 2 |
| Host-owned public and local policy, capability separation, strict public file grammar, exact mode grammar, 1 MiB and regular-file bounds, live replacement, and fail-closed decision snapshots | Correction | Phase 3 |
| Strict three-layer local policy grammar, canonical stored authorities, union semantics, live revocation, and failure to an empty local decision | Partial / correction | Phase 3 |
| Environment resolution, empty-as-unset behavior, plural/singular precedence, exact local all-or-none group, listener validation, and secret-safe configuration errors | Partial / correction | Phase 4 |
| Startup ordering and readiness: initial policy checks, append check, bound-but-not-serving listener, token load, broker `READY`, and nonzero failure | Incomplete | Phase 4 |
| Denial diagnostics and append-only records, canonical record target, RFC 3339 `Z` timestamp, non-interleaving writes, report-mode append failure, and sticky log health | Partial / correction | Phase 4 |
| Raw request-target classification, DNS/IP normalization, explicit loopback classification, mandatory CONNECT port, absolute-form HTTPS rejection, and removal of proxy-originated TLS | Correction | Phase 5 |
| Policy authorization ordering and safe response taxonomy for syntax and policy outcomes, including exact denial and HTTPS bodies and HEAD suppression | Partial / correction | Phase 5 |
| HTTP/1.0 and HTTP/1.1 ingress, Host rules, request-line/header/framing validation, slow-head timeout, upgrade rejection, and bounded incremental parsing | Mostly absent | Phase 6 |
| Origin-form endpoints, `OPTIONS *`, endpoint method rules, exact health/status bodies and content types, and policy/log-sensitive health | Partial / correction | Phase 6 |
| Reviewed 2025-10-09 IANA global-unicast boundary, literals, most-specific exceptions, IPv4-mapped/scoped rejection, and address-safety denial | Correction | Phase 7 |
| One pinned DNS answer set, 1..=64 all-address validation, one cumulative ten-second DNS/TCP deadline, numeric fallback, no ambient proxy, and typed DNS/dial outcomes | Correction | Phase 7 |
| Broker token secrecy, exact bounded frames, three-second readiness, fresh broker connections, retained post-`OK` bytes, broker outcome typing, and no direct loopback dial | Largely present / incomplete | Phase 8 |
| Local connection authorization and recheck flow, distinct `localhost`/IPv4/IPv6 grants, local HTTP connection reuse boundary, and supported engine route assumptions | Partial | Phase 8 |
| Forwarded request transformation: method and target, regenerated `Host`, hop-by-hop removal, `Via`, no forwarding identity headers, and no forbidden retry/cache behavior | Partial / correction | Phase 9 |
| Unlimited-size streaming HTTP bodies, 64 KiB directional buffers, chunked bodies and trailers, informational responses, `Expect: 100-continue`, bodyless responses, response-head validation, cancellation, and safe pooling | Correction / mostly absent | Phase 9 |
| CONNECT success/failure sequencing, exact success head, eager client and broker bytes, opaque relay, 64 KiB buffers, bidirectional half-close, tunnel lifetime, and revocation semantics | Partial | Phase 10 |
| Process-wide client/upstream limits, at-least-128 client support, bounded task and queue ownership, overload response, and idle-pool accounting | Correction / absent | Phase 11 |
| Two-stage signal shutdown, shutdown health, cancellation of pending work, five-second drain then forced close, exit status, and idempotent host cleanup | Correction | Phase 11 |
| Candidate image/process contract, static scratch and numeric user posture, read-only-root operation, exact mounts/env/port, multi-platform equivalence, dependency audit, and black-box contract trace | Partial / unqualified | Phase 12 |
| Apple `container` and Docker-through-Colima end-to-end build/run/broker verification without publishing or replacing immutable tags | Absent | Phase 12 |
| Atomic production selection, `vhrn-proxy` image and CLI release-clock tags, workspace/CI/build context, local image name, removal of the legacy implementation/toolchain, and final operator documentation | Absent | Phase 13 |
