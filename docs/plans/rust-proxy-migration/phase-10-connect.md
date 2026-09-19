# Phase 10: Complete opaque CONNECT tunnel semantics

## Execution contract

This file is the authoritative implementation specification for Phase 10. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 10 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../AGENTS.md).
4. Read the frozen [consumer contract](../../proxy/consumer-contract.md).
5. Implement only this phase and stay within its editable paths and responsibility boundary.
6. Record detailed implementation, validation, and review evidence in this file.
7. After every completion requirement, independent review, and rereview are satisfied, apply
   the master plan's status-transition rules. If anything remains unresolved, leave the next phase
   `Blocked`.

## Objective

Make CONNECT a strictly validated, policy-checked connection establishment followed by an opaque,
lossless, half-close-aware tunnel with no TLS participation or tunnel timeout.

## Inputs and editable paths

Read:

- the contract's CONNECT tunnels, eager-byte, failure, and tunnel-lifetime requirements;
- Phase 5 CONNECT targets/outcomes, Phase 6 buffered prefix, Phase 7 and 8 connectors, and current
  `proxy-rs/src/server/relay.rs`, router, listener tunnel ownership, and CONNECT tests.

Edit only:

- `proxy-rs/src/server/relay.rs`, CONNECT portions of `server/router.rs`, `server/listener.rs`, and
  `server/response.rs`;
- connector handoff types only where required to transfer checked streams and prefixes;
- CONNECT/relay fixtures and focused unit/process tests;
- detailed implementation, validation, and review evidence in this file;
- [`plan.md`](plan.md) only for status transitions permitted by the master plan.

Do not change parsing rules owned by Phase 6, policy/dial/broker algorithms, ordinary HTTP
forwarding, global limits/shutdown, workflows, or production selection.

## Required behavior

1. For public and local CONNECT, finish live policy, required audit, resolution or broker
   authorization, and upstream connection before committing success. Map typed unsafe/policy
   outcomes to `403`, DNS/TCP/broker failures to `502`, deadlines to `504`, and never upgrade on
   failure.
2. Emit exactly an HTTP/1.1 `200 Connection Established` header section with no
   `Content-Length` or `Transfer-Encoding`, then transfer ownership of the downstream and upstream
   streams to the relay.
3. Deliver every downstream byte Phase 6 read beyond the CONNECT terminator to the upstream first
   and in order. Deliver every Phase 8 byte read after broker `OK\n` as the first upstream-to-client
   bytes. Test coalesced TLS ClientHello-sized prefixes, not only one-byte prefixes.
4. Relay both directions simultaneously with backpressure and at most 64 KiB application buffering
   per direction. After EOF, flush queued bytes then write-half-close that destination while the
   reverse direction continues. Fully close only after both directions end, nonrecoverable I/O,
   client cancellation, or Phase 11 forced shutdown.
5. Apply no idle, body, or absolute tunnel timeout. Do not inspect bytes or participate in TLS.
   An established tunnel is never reevaluated and survives policy replacement until a normal relay
   terminal event.
6. A client disconnect before handoff cancels resolution/dial/broker work and closes any acquired
   upstream. Tunnel tasks remain owned and observed after the HTTP request task completes; no
   detached/unobserved relay is allowed.

## Interfaces and data flow

- A successful CONNECT produces `TunnelParts { downstream, downstream_prefix, upstream,
  upstream_prefix }`. The response serializer writes the exact success head before `relay` consumes
  the parts.
- `relay` owns both streams and fixed buffers. It reports only a redacted terminal category to the
  supervisor; client-visible errors are possible only before success commitment.

## Edge cases and focused tests

- Cover all pre-success failure categories, no success bytes before upstream authorization, exact
  success headers, large/fragmented eager prefixes on both boundaries, simultaneous traffic,
  queued bytes before EOF, each half closing first, I/O error, client cancellation before and
  after success, long-lived idle tunnel, and revocation/new-tunnel denial while an old tunnel stays
  usable.
- Prove no tunnel path calls TLS code, imposes an exchange timer, loses buffered bytes, or writes an
  HTTP error after success.

## Validation

1. `cargo fmt --all -- --check`
2. `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings`
3. `cargo test -p vhrn-proxy --locked server::relay`
4. `cargo test -p vhrn-proxy --locked server::listener`
5. `cargo test -p vhrn-proxy --locked --test proxy_process`
6. `cargo test -p vhrn-proxy --locked`

## Evidence required

- Record byte-for-byte prefix and half-close traces, exact success head assertions, no-timeout
  paused-time test, and revocation split between established/new tunnels.
- Record independent tunnel lifecycle, truncation, and cancellation review plus clean rereview.

## Completion criterion

CONNECT never succeeds before authorization and connection; success bytes are exact; both buffered
boundaries are lossless; relay buffers and half-closes satisfy the contract; established tunnels
are opaque and untimed; failures are correct; tests pass; and rereview is clean.
