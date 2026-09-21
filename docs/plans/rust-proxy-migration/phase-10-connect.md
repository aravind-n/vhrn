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

## Implementation evidence

- `connect_http1` completes the live public/local policy decision and the typed public or broker
  connection before returning `ConnectedUpstream`. Public streams carry an empty prefix; broker
  streams transfer bytes co-read after `OK\n` into the explicit upstream prefix. The listener
  races the complete precommit future against downstream peer closure, so disconnect drops pending
  policy, audit, resolution, dial, and broker work and any stream already owned by that future.
- Only `write_connect_established` commits success, after the checked upstream exists. Its focused
  assertion records the exact wire bytes as
  `HTTP/1.1 200 Connection Established\r\n\r\n`, with no content-length or transfer-encoding
  field. After this write the listener separates Phase 6's raw downstream stream and unread prefix,
  constructs `TunnelParts { downstream, downstream_prefix, upstream, upstream_prefix }`, and has no
  response serializer on the relay error path.
- Successful handoffs cross a bounded supervisor channel into a dedicated, observed tunnel
  `JoinSet`. The connection admission permit moves with the handoff and remains owned until the
  tunnel task ends. Completed and failed relays are joined during service, and shutdown drains then
  aborts and observes every remaining task; no relay is detached.
- Relay uses two simultaneous directional futures and one fixed 32 KiB heap buffer per direction.
  Each transferred prefix is capped at the remaining 32 KiB, enforcing at most 64 KiB of
  application buffering per direction. Prefix bytes are written first and in order; later reads
  use `write_all` for backpressure. EOF flushes and write-half-closes only the destination, leaving
  the reverse future active. A nonrecoverable error drops both streams and exposes only the
  `CONNECT tunnel I/O failure` terminal category.
- The established relay has no idle, body, or absolute timer and imports no TLS facility. Its only
  cancellation input is process shutdown; normal client and upstream closure are stream EOF/error.
  Policy is consulted only before the handoff, so replacement does not reach established tunnels.

## Focused test evidence

- `connect_success_head_is_exact_and_has_no_framing_fields` asserts the complete success head byte
  for byte and independently rejects both forbidden framing field names.
- `eager_prefixes_are_first_and_lossless_in_both_directions` records 12 KiB traces in each
  direction: downstream prefix then `client-suffix`, and upstream prefix then `origin-suffix`.
  The black-box `local_connect_preserves_buffered_bytes_and_survives_revocation` sends patterned
  16 KiB ClientHello-sized prefixes at both boundaries, fragments broker `OK\n`, asserts exact
  byte equality at both peers, keeps the established tunnel usable after policy replacement, and
  verifies a new CONNECT gets `403` without reaching the broker.
- `simultaneous_traffic_and_queued_bytes_survive_client_eof` transfers 128 KiB concurrently in
  both directions through 1 KiB transport capacities, then observes exact bytes after both queued
  writes and half-closes. `upstream_eof_keeps_client_write_half_open` records the opposite
  half-close order and proves client bytes still arrive after upstream EOF.
- `idle_tunnel_has_no_timeout` advances paused time by 8,760 hours while the relay stays pending.
  `nonrecoverable_io_error_is_redacted_and_closes_both_streams` checks the redacted terminal
  category and full stream closure. Listener drain tests prove completed, failed, panicked, and
  forced-abort tunnel tasks are all observed.
- `public_connect_emits_no_success_while_resolution_is_pending` proves public CONNECT emits no
  success during pending resolution. The black-box
  `local_connect_client_disconnect_before_approval_cancels_broker_work` proves pre-handoff client
  disconnect closes the broker stream without a retry, while the broker failure/timeout tests
  retain the `502`/`504` mappings and never emit success.
- `client_disconnect_after_success_closes_upstream_and_observes_relay_result` crosses the
  successful listener handoff, asserts the exact `200` head, drops the client, observes the
  upstream write-half-close, forces a reverse-direction terminal write, then verifies the upstream
  is fully closed and the owned relay reports only its redacted category. Together with the tunnel
  supervisor drain test, this covers cancellation after commitment without detached work or a
  post-success HTTP error.

## Validation evidence

Initial validation on 2026-09-20 passed exactly as required:

1. `cargo fmt --all -- --check`
2. `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings`
3. `cargo test -p vhrn-proxy --locked server::relay` — 7 passed
4. `cargo test -p vhrn-proxy --locked server::listener` — 8 passed initially
5. `cargo test -p vhrn-proxy --locked --test proxy_process` — 24 passed
6. `cargo test -p vhrn-proxy --locked` — 157 unit tests and 24 process tests passed; doc tests
   passed

After resolving the independent review finding, the same six commands passed again: 7 relay tests,
9 listener tests, 24 process tests, 158 total unit tests, and doc tests all passed.

## Independent review evidence

- The independent `rust_reviewer` review inspected the complete Phase 10 diff and contract on
  2026-09-20. It reported no implementation, security, regression, scope, clean-room, or existing
  test-quality defects. It identified one phase-blocking coverage gap: client cancellation after a
  successful listener handoff was inferred from lower-level tests rather than exercised directly.
- The finding was resolved with
  `client_disconnect_after_success_closes_upstream_and_observes_relay_result`, which covers the
  exact success commit, downstream cancellation, upstream half-close then full close, and the
  observed redacted relay result.
- The same `rust_reviewer` rereview inspected the resolution, production handoff/supervisor flow,
  relay terminal behavior, updated evidence, and complete diff. It confirmed the prior gap was
  adequately closed, reported no actionable findings or material residual risks, and declared the
  Phase 10 rereview clean.
