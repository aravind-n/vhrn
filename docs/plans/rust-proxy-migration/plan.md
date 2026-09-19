# Rust proxy replacement execution plan

Replace the shipping Go egress proxy with the isolated Rust candidate while preserving the
consumer-facing contract used by the CLI, packaging, and containerized clients. The Rust proxy may
be implemented independently; compatibility applies to the frozen consumer contract, not to
incidental Go implementation behavior.

This directory is the execution queue. Pre-flight Phase 1 freezes the contract, Pre-flight Phase 2
defines the implementation sequence, and each numbered phase has one authoritative specification.
Phase 1 records work that was completed before the pre-flights and does not waive either pre-flight.

## Execution rules

- Complete the pre-flights in order before assigning another implementation phase.
- Assign each numbered phase to one implementation agent and one pull request.
- Keep each PR inside its phase's editable paths and responsibility boundary.
- Start from the assigned phase specification, then follow its mandatory pointers to this plan,
  `AGENTS.md`, the consumer contract when applicable, and phase-specific inputs before editing.
- Include implementation, tests, CI, documentation, and evidence required by a behavior in the
  phase that owns it.
- Give every phase an independent review for security, correctness, regressions, test quality,
  clean-room integrity, and scope. Resolve findings and obtain rereview before completion.
- Keep numbered-phase status and implementation evidence in that phase's specification. Mark it
  complete only when implementation, validation, evidence, review, and completion criteria are all
  satisfied.
- Record work owned by another phase without expanding the active phase.
- Stop for user direction when work conflicts with an `AGENTS.md` security invariant or requires
  publishing an image, tagging a release, changing a version, or changing an external service.

## Clean-room and compatibility boundaries

- `docs/proxy/consumer-contract.md` is the sole normative description of externally observable
  proxy behavior. It describes outcomes and interfaces, not Go or Rust implementation structure.
- The Pre-flight Phase 1 contract agent may inspect the shipping Go proxy only to identify the
  stable interface that existing consumers depend on. Go quirks are not requirements unless a
  consumer depends on them or the contract explicitly adopts them.
- Rust planning and implementation agents read the frozen consumer contract, allowed host-side
  interfaces, `AGENTS.md`, and Rust source. They do not inspect current or historical Go source,
  tests, module files, diffs, or Go-derived explanations.
- The shipping Go proxy is frozen. No phase may modify it.
- Until cutover, `proxy/` remains the shipping Go implementation and `proxy-rs/` remains the
  isolated Rust candidate. Candidate work must not change production selection or existing
  immutable image tags.
- The proxy and CLI remain independent crates. Do not share policy, target, or broker protocol
  types between them.
- Keep the Rust source in the idiomatic module-file layout: parent modules use `name.rs`, children
  use `name/child.rs`, and no `mod.rs` files are added.

At cutover, the qualified Rust tree will move from `proxy-rs/` to `proxy/`; the workspace member,
CI, and packaging will follow it; Go implementation and toolchain files will be removed. The
`vhrn-proxy` image name, port 8080, scratch runtime, unprivileged identity, CLI release-clock
tagging, and multi-platform publication interface must remain stable.

---

## Pre-flight Phase 1: Build the consumer contract

- [x] Status: Complete

### Objective

Create `docs/proxy/consumer-contract.md` from scratch as the complete, implementation-neutral
contract for the replacement proxy.

The contract preserves interfaces used by the CLI, packaging, and containerized clients while
specifying correct behavior from the repository's security model and applicable network and HTTP
standards. It is not a line-by-line or edge-case recreation of the Go proxy.

### Inputs and editable paths

Read:

- `AGENTS.md`;
- the shipping proxy and its packaging only as needed to identify stable consumer dependencies;
- `cli/src/run.rs`, `cli/src/net.rs`, and `cli/src/broker.rs`;
- `README.md`, `docs/sandbox-design.md`, and release/runtime documentation;
- applicable protocol standards needed to define correct behavior.

Edit only:

- `docs/proxy/consumer-contract.md`;
- this phase's status and evidence in this plan.

Do not read `proxy-rs/**` or Rust proxy history. Do not modify the frozen Go proxy.

### Required work

1. Inventory every external dependency: process and image interface, environment variables,
   mounted paths, public and local policy inputs, broker framing, health and status endpoints,
   denial diagnostics, response classes, readiness, shutdown, and release/runtime invariants.
2. Specify the public security boundary, including hostname authorization, DNS handling, and the
   requirement that direct public dials target only globally routable unicast addresses.
3. Specify public and explicitly authorized loopback routing as separate capabilities. Local access
   must use the authenticated broker and must never be granted by public policy or open/report mode.
4. Define correct HTTP and CONNECT behavior, including target forms, TLS handling, hop-by-hop
   headers, streaming, bounded bodies and headers, cancellation, buffered tunnel bytes,
   bidirectional relay, half-close, connection reuse, and live policy replacement.
5. Define fail-closed behavior for malformed or unreadable policy, invalid configuration, DNS and
   dial failures, broker errors, readiness failures, and shutdown.
6. State observable outcomes and bounds without prescribing Go control flow, Rust module structure,
   dependency choices, or other implementation details.
7. Verify every contract statement against a consumer dependency, a repository security invariant,
   or an identified protocol requirement. Record deliberate corrections to current Go behavior as
   normative requirements, without changing the Go implementation.
8. Obtain independent review for completeness, security, internal consistency, and clean-room
   integrity. Resolve findings and freeze the reviewed document in the phase PR.

### Validation

- Check every environment variable, mount, frame, endpoint, diagnostic record, and image/runtime
  invariant against its consumer.
- Trace every `AGENTS.md` proxy security invariant to an explicit contract requirement.
- Review HTTP and network requirements against the cited standards.
- Search the contract for Go/Rust identifiers or implementation prescriptions and remove them
  unless they are part of a stable external interface.
- Record the exact review result and frozen document hash in this phase's evidence.

### Evidence

- Clean-room inputs inspected: `AGENTS.md`; this phase and the plan-wide boundaries; the
  consumer-facing launch, policy, and broker interfaces in `cli/src/run.rs`, `cli/src/net.rs`, and
  `cli/src/broker.rs`; `README.md`, `docs/sandbox-design.md`, `docs/runbooks/release.md`, the base
  entrypoint, proxy image/Makefile surface, image workflows, and the shared loopback fixture. The
  shipping proxy was inspected only for its stable process, endpoint, and diagnostic surface.
  `proxy-rs/**`, Rust proxy history/diffs, and the deleted prior contract were not read; the frozen
  Go proxy was not modified or used as the normative behavior source.
- Authoritative standards checked: RFC 1035 (DNS name limits), RFC 3339 (denial-record timestamp
  format), RFC 3986 (URI syntax), RFC 9110 (HTTP semantics, CONNECT, hop-by-hop fields), RFC 9112
  (HTTP/1.1 target forms, framing, and smuggling defenses), and the IANA IPv4/IPv6 Special-Purpose
  Address Registries (baseline last updated 2025-10-09).
- Validation performed: all proxy environment variables, mounts, five public layers, three local
  layers, exact broker frames, endpoints, denial record, image/runtime, engine, and release-clock
  interfaces were cross-checked against their consumers; every proxy security invariant in
  `AGENTS.md` was traced to an explicit requirement. Requirements were classified as stable
  consumer interfaces, repository security invariants, governing product decisions,
  protocol/registry requirements, or explicit contract-selected decisions with rationale; the
  exact-limit audit and TLS-origination knock-on search were clean.
  `git diff --check` and the equivalent whitespace check for the new untracked document passed.
- Frozen contract SHA-256: `f3499ff66aa9e15d3f7788153b3f109dc7b020956a6d306eda5e103e806075ed`.
- Review result: independent review returned six findings and rereview returned two; all were
  resolved. Final independent rereview approved Phase 1 with no remaining findings and
  independently confirmed the frozen hash.

### Completion criterion

Every consumer-visible interface and security requirement has one unambiguous normative outcome;
correct protocol behavior and failure bounds are specified without cloning Go quirks; deliberate
behavioral corrections are explicit; an independent reviewer has approved the contract; and a Rust
agent can use it without reading any Go material or requesting additional product decisions.

---

## Pre-flight Phase 2: Plan the Rust implementation phases

- [x] Status: Complete

### Objective

Inspect the frozen consumer contract and current Rust candidate, then produce the remaining
dependency-ordered Phase 2 through Phase N implementation packets as individual phase specifications.

Each phase must be a small, cohesive unit that one agent can implement in one pull request without
performing another planning pass.

### Inputs and editable paths

Read:

- the frozen `docs/proxy/consumer-contract.md`;
- `AGENTS.md`;
- this plan and the completed Phase 1 record;
- allowed host-side Rust interfaces;
- the Rust candidate, its tests, workspace configuration, image recipe, and relevant workflows.

Edit only `plan.md` and the numbered phase specifications in this directory. Do not inspect Go
source, tests, module files, history, diffs, or explanations.

### Required work

1. Build a coverage matrix mapping every consumer-contract requirement to exactly one future phase.
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

### Planning evidence

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
- The dependency-ordered phase specifications indexed below assign every contract group once.
  Requirements already satisfied by the candidate are assigned to the phase that must preserve
  and prove them; no separate cleanup phase owns behavioral work from an earlier packet.
- Approval: the user approved the complete Phase 2 through Phase 13 directory and sequence on
  2026-09-19.

### Completion criterion

Every frozen contract requirement and delivery obligation has exactly one owning phase; every phase
is independently assignable, implementation-ready, dependency-ordered, and bounded to one PR; the
coverage matrix has no gaps or duplicate ownership; and the user has approved the complete phase
sequence.

---

## Numbered phase index

Each numbered phase file is the authoritative source for that phase's status, dependency gate,
scope, implementation requirements, validation, and evidence. Assign an implementation agent the
single phase file; it starts there and follows the file's mandatory pointers. Shared rules,
pre-flight records, sequencing, and coverage remain authoritative in `plan.md`.

| Phase | Dependency gate | Specification |
| --- | --- | --- |
| 1 — Build the isolated Rust proxy candidate | Completed before the pre-flights | [`phase-01-candidate-scaffold.md`](phase-01-candidate-scaffold.md) |
| 2 — Split CLI and proxy CI ownership | Both pre-flights approved; Phase 1 complete | [`phase-02-ci-ownership.md`](phase-02-ci-ownership.md) |
| 3 — Make policy files a strict live security boundary | Phase 2 complete | [`phase-03-policy.md`](phase-03-policy.md) |
| 4 — Gate startup and make auditing health-aware | Phase 3 complete | [`phase-04-startup-audit.md`](phase-04-startup-audit.md) |
| 5 — Correct target parsing, authorization ordering, and safe outcomes | Phase 4 complete | [`phase-05-targets-outcomes.md`](phase-05-targets-outcomes.md) |
| 6 — Enforce the HTTP/1 ingress and direct-endpoint contract | Phase 5 complete | [`phase-06-http-ingress.md`](phase-06-http-ingress.md) |
| 7 — Pin public resolution to the reviewed address boundary | Phase 6 complete | [`phase-07-public-dialing.md`](phase-07-public-dialing.md) |
| 8 — Harden the authenticated loopback broker transport | Phase 7 complete | [`phase-08-broker.md`](phase-08-broker.md) |
| 9 — Stream compliant HTTP forwarding over checked transports | Phase 8 complete | [`phase-09-forwarding.md`](phase-09-forwarding.md) |
| 10 — Complete opaque CONNECT tunnel semantics | Phase 9 complete | [`phase-10-connect.md`](phase-10-connect.md) |
| 11 — Bound process resources and supervise shutdown | Phase 10 complete | [`phase-11-resources-shutdown.md`](phase-11-resources-shutdown.md) |
| 12 — Qualify the isolated candidate on its real image and engines | Phase 11 complete | [`phase-12-qualification.md`](phase-12-qualification.md) |
| 13 — Cut over atomically to the sole Rust production proxy | Phase 12 complete; explicit cutover approval | [`phase-13-cutover.md`](phase-13-cutover.md) |

---

## Contract coverage matrix

The rows below are mutually exclusive ownership groups. “Partial” means the candidate has a useful
implementation but does not yet satisfy the frozen outcome; “correction” means at least one current
candidate behavior contradicts the contract. A phase owns implementation, focused tests, fixtures,
documentation, and evidence for its row. Later phases may consume that behavior but must not
redefine it.

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
