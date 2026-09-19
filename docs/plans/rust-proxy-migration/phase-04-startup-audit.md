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
