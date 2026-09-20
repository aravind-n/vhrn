# Phase 4: Gate startup and make auditing health-aware

## Execution contract

This file is the authoritative implementation specification for Phase 4. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 4 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../AGENTS.md).
4. Read the frozen [consumer contract](../../proxy/consumer-contract.md).
5. Implement only this phase and stay within its editable paths and responsibility boundary.
6. Record detailed implementation, validation, and review evidence in this file.
7. After every completion requirement, independent review, and rereview are satisfied, apply
   the master plan's status-transition rules. If anything remains unresolved, leave the next phase
   `Blocked`.

## Objective

Make process configuration, startup readiness, denial auditing, and health state satisfy the
external process contract before any HTTP traffic can be served.

## Inputs and editable paths

Read:

- `AGENTS.md`, the contract's Process and image interface, Startup and readiness, diagnostics, and
  health requirements, and [`plan.md`](plan.md);
- Phase 3's strict policy APIs;
- `proxy-rs/src/config.rs`, `lib.rs`, `main.rs`, `diagnostics.rs`, `shutdown.rs`,
  `server/listener.rs`, and broker token/readiness interfaces;
- process tests and `proxy-rs/testdata/proxy-process-cases.tsv`.

Edit only:

- `proxy-rs/src/config.rs`, `lib.rs`, `main.rs`, `diagnostics.rs`, and `shutdown.rs`;
- the startup boundary in `proxy-rs/src/server/listener.rs`;
- startup/diagnostic-focused candidate fixtures and tests;
- detailed implementation, validation, and review evidence in this file;
- [`plan.md`](plan.md) only for status transitions permitted by the master plan.

Do not change HTTP target semantics, public dialing, broker wire framing, forwarding, image
selection, workflows, or `proxy/`.

## Required behavior

1. Resolve empty environment values as unset. Apply defaults to empty `VHRN_PROXY_LISTEN`,
   `VHRN_MODE_FILE`, `VHRN_ALLOWLIST`, and `VHRN_ALLOWLISTS` exactly as the contract specifies;
   preserve plural precedence only for a nonempty plural value. Treat empty optional denial-log and
   all three empty local values as absent. Reject partial nonempty local configuration, a local path
   count other than three, empty list items, invalid listener/broker values, and empty required
   paths.
2. Parse configuration without including token content, broker route, or policy/mount paths in
   process diagnostics. Startup failures may name the invalid environment variable or a stable
   error code, not its sensitive value.
3. Perform startup in this order: resolve configuration; strict-load public policy and, when
   configured, all local policy; open the configured denial log for append without truncating it;
   bind the HTTP listener without accepting from it; read and validate the token once; complete
   broker readiness; then make the bound listener serve. Any failure exits nonzero and closes all
   acquired resources.
4. Cancellation during startup must close the bound listener and any pending broker exchange. A
   failed or timed-out `READY` may not degrade to public-only service, and no client connection may
   be accepted before readiness completes.
5. Replace per-request recorder construction with one process-owned audit service. Generate a new
   RFC 3339 UTC `Z` timestamp for each record, serialize concurrent appends so complete records
   cannot interleave, append without rewriting or truncating, and use only a normalized public host
   or canonical local authority as the target.
6. Produce denial process diagnostics containing only the canonical target and effective mode.
   Operational diagnostics use bounded stable categories and do not emit token, broker address,
   file paths, resolved private addresses, request data, credentials, or raw internal errors.
7. For enforced denials, an append failure leaves the response decision as `403`. For a report-mode
   would-be denial, append failure marks the log unhealthy and returns the typed `503` outcome with
   no dial. The next required record retries; only a successful required append clears sticky log
   unhealthiness.
8. Expose a process-owned health service used later by direct endpoints. A health read validates
   the current required public files, mode, configured local files, and ability to open the denial
   log for append. It also observes sticky append failure and shutdown state. It never mutates
   policy or clears sticky log failure merely because an open check succeeds.

## Interfaces and data flow

- Add a bootstrap result that owns the bound `TcpListener`, validated `Config`, process audit/health
  state, optional broker connector, and shutdown controller. `run` passes that complete result to
  `serve`; `serve` no longer binds for itself.
- The audit service returns a typed `Recorded`, `Disabled`, or `AppendFailed` result. Routing does
  not receive an I/O error string.
- The health service calls Phase 3 strict readers for probes and holds only the sticky log and
  shutdown flags; it must not cache an allow decision.

## Edge cases and focused tests

- Cover empty-versus-absent values for every variable, plural/singular precedence, every partial
  local triple, invalid token contents, occupied listener, policy invalid at startup, log open
  failure, broker refusal/timeout, and cancellation after bind but before readiness.
- Prove connection attempts cannot receive HTTP before broker readiness and startup failures exit
  nonzero.
- Concurrently append many denials and verify complete line records, per-record timestamps, no
  target whitespace, no secrets, and no truncation of a preexisting record.
- Cover disabled logging, enforced append failure, report append failure/no dial, sticky unhealthy
  state, retry failure, successful-record recovery, and repair of invalid live policy.

## Validation

1. `cargo fmt --all -- --check`
2. `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings`
3. `cargo test -p vhrn-proxy --locked config::tests`
4. `cargo test -p vhrn-proxy --locked diagnostics::tests`
5. `cargo test -p vhrn-proxy --locked --test proxy_process`
6. `cargo test -p vhrn-proxy --locked`

## Evidence required

- Record the startup state sequence and prove the listener remains non-serving until the final
  transition.
- List the redaction assertions, concurrent append test, sticky-health tests, and process exit
  cases.
- Record validation output and independent review/rereview for security, cancellation, file
  semantics, secret handling, and scope.

## Completion criterion

Configuration matches the exact empty/default/group contract; initial invalid state cannot serve;
readiness is fail-fast and nondegraded; audit writes are canonical, append-only, and serialized;
health tracks policy, log, and shutdown state; diagnostics are redacted; tests pass; and independent
rereview is clean.

## Implementation evidence

Completed 2026-09-19.

The implementation stays within the listed Phase 4 paths except for
`proxy-rs/src/server/router.rs` and its focused unit tests. During independent review, exact denial
mode propagation and typed report-log failure handling were shown to be impossible at the listener
boundary: only the router owns the `PublicDecision` snapshot and the pre-dispatch decision point.
The user explicitly authorized that minimal path exception. The resulting change passes
`PublicDecision.effective_mode` into auditing, handles `AppendFailed` before dispatch, and connects
the bootstrap-owned health service to `/healthz`. No HTTP target classification, public dialing,
broker framing, forwarding, image selection, workflow, or Go implementation was changed.

### Configuration and bootstrap

- A shared empty-value resolver treats every empty process variable as unset. Empty listen, mode,
  plural allowlist, and singular allowlist values therefore fall through to the contract defaults;
  a nonempty plural allowlist alone takes precedence. Empty denial-log and local-group values are
  absent. Tests cover all eight local-group presence masks, plural/singular precedence, empty list
  items, local path counts, listener values, broker endpoints, and token contents.
- `Bootstrap` owns the already-bound listener, validated `Config`, shared audit and health services,
  public connector, optional ready broker connector, and shutdown controller. `serve` consumes that
  result and never binds.
- Startup advances through these states, in order:

  1. `Resolved` — environment values are parsed without emitting their contents.
  2. `PublicValidated` — every public layer and mode strict-load successfully.
  3. `LocalValidated` — all three local layers strict-load when local routing is configured.
  4. `AuditOpened` — the denial log opens as a regular append target without truncation.
  5. `BoundNotServing` — the listener is bound but no accept loop exists yet.
  6. `TokenValidated` — the token is read once with a nonblocking regular-file check and a 65-byte
     read bound, selected against shutdown.
  7. `BrokerReady` — the optional authenticated `READY` exchange completes, also selected against
     shutdown.
  8. `Serving` — and only then, the bootstrap result enters the listener accept loop.

- Cancellation branches are priority-biased. Dropping an incomplete bootstrap closes the listener,
  token handle, and pending broker exchange. The process test queues an HTTP request after bind,
  proves it receives no bytes before `READY`, and proves SIGTERM closes both that queued connection
  and the pending broker exchange with a successful process exit. Refusal and timeout instead exit
  nonzero and never serve queued HTTP.

### Audit, diagnostics, and health

- One `AuditService` is shared by the process. A mutex serializes open/write/flush operations;
  every required record obtains a fresh RFC 3339 UTC `Z` timestamp, opens with create+append, checks
  for a regular file, preserves existing bytes, and writes only a normalized public host or
  canonical local authority. Its typed result is `Recorded`, `Disabled`, or `AppendFailed`.
- Denial diagnostics contain only `target=<canonical-target> mode=<decision-mode>`. Public mode is
  the exact authorization snapshot, not a second file read; local and fail-closed denials use
  `enforce`. Other runtime reports collapse to bounded stable categories. Startup errors name only
  an environment variable or stable code.
- Enforced append failure is best-effort and leaves the response at `403`. A report-mode
  `AppendFailed` produces the exact `503`, generic body, and connection close before dispatch. An
  instrumented HTTP connector test observes zero upstream bytes. The separate CONNECT branch uses
  a counting resolver and panic-on-use dialer to prove the same response occurs before any upstream
  resolution or dial. Disabled logging proceeds normally.
- Append failure sets a sticky atomic health flag. Every later required record retries; only a
  successful append clears it. Merely reopening the repaired log during a health probe does not.
- `HealthService` strict-loads the current public layers and mode, strict-loads configured local
  layers, verifies append-open ability, observes sticky append failure, and observes shutdown. It
  stores no allow decision. Process tests prove invalid live policy makes health `503`, atomic
  repair restores policy health, log repair alone cannot clear stickiness, and a successful later
  record restores health.

### Focused evidence

- Redaction assertions inspect raw child stderr and cover the broker token, invalid token content,
  broker route, public/local policy paths, denial-log path, request credentials, and canonical-only
  denial diagnostics. A separate sanitized accessor is used only in test failure messages.
- `concurrent_appends_are_complete_and_never_truncate` launches 128 concurrent records, preserves a
  preexisting line, validates every complete timestamp/target line, and rejects whitespace or
  secret target content.
- Sticky-state coverage includes disabled logging, repeated failure, successful-record recovery,
  enforced `403`, report `503`, no dial, open-probe non-recovery, policy invalidation/repair, local
  policy invalidation, and shutdown.
- Nonzero process-exit coverage includes invalid public policy, invalid local policy, denial-log
  open failure, invalid and nonregular token files, occupied listener, partial local configuration,
  broker refusal, and broker timeout. Normal startup cancellation and normal SIGTERM exit zero.

## Validation evidence

All required commands passed on the final tree:

1. `cargo fmt --all -- --check` — passed.
2. `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings` — passed.
3. `cargo test -p vhrn-proxy --locked config::tests` — 8 passed.
4. `cargo test -p vhrn-proxy --locked diagnostics::tests` — 8 passed.
5. `cargo test -p vhrn-proxy --locked --test proxy_process` — 19 passed.
6. `cargo test -p vhrn-proxy --locked` — 92 unit tests and 19 process tests passed; doc tests
   passed.

The process suites ran outside the restricted filesystem/network sandbox because they bind
loopback listeners. No live container-engine run was required by this phase or performed.

## Independent review and rereview

The initial independent security/correctness review found four actionable issues: a synchronous
post-bind token read could block cancellation; test diagnostics masked the token before redaction
assertions; public denial mode was reread after the authorization snapshot; and listener-side
request rewriting exceeded the permitted startup boundary and lacked direct no-dial
instrumentation. The implementation was revised to use a cancellation-selected bounded async
token reader, assert against raw stderr, pass the exact router decision mode, return the typed
report failure in the router before dispatch, instrument the connector, and restore the listener
to startup/connection lifecycle work. Public/local target normalization was also tightened during
review.

Independent rereview examined security, cancellation, append semantics, sticky health, secret
handling, regression risk, test quality, clean-room integrity, and the explicitly authorized scope
exception. It reported no remaining material findings.

A final review by the specialized `rust_reviewer` agent reported no actionable findings. It noted
that the no-dial proof covered the plain-HTTP and CONNECT branches differently, so the CONNECT
branch gained its own focused exact-503, `Connection: close`, zero-resolution, zero-dial test. The
full validation suite passed again after that addition. The specialized rereview then confirmed
that the gap was closed, with no actionable findings or material test gaps. The reviewer inspected
no current or historical Go source, tests, module files, history, or diffs.
