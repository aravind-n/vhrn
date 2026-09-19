# Phase 9: Stream compliant HTTP forwarding over checked transports

## Execution contract

This file is the authoritative implementation specification for Phase 9. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 9 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../AGENTS.md).
4. Read the frozen [consumer contract](../../proxy/consumer-contract.md).
5. Implement only this phase and stay within its editable paths and responsibility boundary.
6. Record detailed implementation, validation, and review evidence in this file.
7. After every completion requirement, independent review, and rereview are satisfied, apply
   the master plan's status-transition rules. If anything remains unresolved, leave the next phase
   `Blocked`.

## Objective

Replace aggregate and size-capped origin handling with one streaming HTTP/1 forwarding pipeline
shared by checked public streams and authenticated broker streams.

## Inputs and editable paths

Read:

- the contract's Forwarding semantics, HTTP body/framing, connection reuse, and non-CONNECT error
  requirements;
- Phase 6 HTTP/1 primitives, Phase 7 public connector, Phase 8 broker stream, and Phase 5 response
  renderer;
- `proxy-rs/src/headers.rs`, `connect/origin_body.rs`, `connect/pool.rs`,
  `connect/public.rs`, `connect/broker/http.rs`, `server/router.rs`, and `server/response.rs`.

Edit only:

- the listed candidate forwarding/header/pool/router/response files;
- Phase 6's shared HTTP/1 codec files for origin response parsing and wire serialization;
- new forwarding children under `proxy-rs/src/connect/` using the required module layout;
- forwarding fixtures and focused unit/process tests;
- dependency declarations and `Cargo.lock` if required;
- detailed implementation, validation, and review evidence in this file;
- [`plan.md`](plan.md) only for status transitions permitted by the master plan.

Do not alter target grammar, address/broker authorization, CONNECT relay, process-wide limits,
workflows, or production selection.

## Required behavior

1. Forward the method unchanged over cleartext HTTP on the already checked stream. Use origin-form
   path/query; use `/` for an empty path; for absolute-form OPTIONS with empty path and no query,
   send `*`. Never originate TLS, cache, follow redirects, reinterpret methods, or automatically
   retry a non-idempotent request. Omitting all automatic retries is acceptable.
2. Regenerate `Host` solely from the normalized request-target authority, including a canonical
   explicit port when one was supplied. Ignore a conflicting inbound Host for routing and
   forwarding.
3. On requests and responses, parse every `Connection` value, remove every nominated field, and
   remove/regenerate `Connection`, `Proxy-Connection`, `Keep-Alive`, `TE`, `Trailer`,
   `Transfer-Encoding`, and `Upgrade`. Never forward `Proxy-Authorization` or origin
   `Proxy-Authenticate`. Preserve end-to-end fields, status, and permitted trailers.
4. Add or append `Via` with the received hop's HTTP version and pseudonym `vhrn` on both request and
   response. Never generate `Forwarded` or `X-Forwarded-For` and never expose an internal address.
5. Stream request and response bodies with backpressure and no product-level size cap. Bound
   application buffering to 64 KiB in each direction, independent of content length or chunk
   count. Decode and reframe valid fixed/chunked bodies incrementally, preserve permitted trailers,
   and reject integer overflow rather than allocate or truncate.
6. Forward `Expect: 100-continue` behavior and applicable informational responses. Emit no body for
   HEAD, 1xx, 204, or 304 while preserving describing headers as required. A malformed or over-64
   KiB raw origin header section becomes `502` before downstream commitment.
7. Once response headers/body bytes are committed, a later origin or framing error closes the
   affected connection and never appends a synthetic error body. Client disconnect cancels pending
   origin I/O and both body streams promptly.
8. Permit pooling only for the same normalized authority after a complete valid response body.
   Close/discard on incomplete bodies, `Connection: close`, parse errors, cancellation, or dropped
   response. Do not apply the candidate's 8 MiB caps or 30-second whole-response/body timers. A
   later downstream request must complete a new live policy decision before checking out a pooled
   stream.
9. Use the same forwarding state machine for public and local HTTP. Opening a new public stream
   repeats Phase 7; opening a new local stream repeats Phase 8. Reusing a local stream does not skip
   the proxy-side local policy reread.

## Interfaces and data flow

- Refactor connectors to return a checked raw stream plus a pool key. A common `HttpOrigin`
  serializer/parser owns one stream for one exchange and returns it to the bounded idle pool only
  after clean end-of-message.
- The downstream body from Phase 6 feeds the upstream writer incrementally while origin interim and
  final responses feed the downstream writer. Cancellation owns and drops both halves; no detached
  body task survives its exchange.
- Origin parse and transport failures return typed outcomes; only Phase 5 renders a pre-commit HTTP
  error.

## Edge cases and focused tests

- Stream bodies larger than the former 8 MiB cap and many tiny chunks while asserting bounded
  buffers and first-chunk latency. Cover fixed, chunked, trailers, zero-length, HEAD, all bodyless
  statuses, multiple 1xx responses, 100-continue, client disconnect, origin truncation, overflow,
  and errors after commitment.
- Verify exact request line, canonical Host, all hop-by-hop/nominated removals, Via append on both
  hops, absence of identity headers, and origin response header 64 KiB boundaries.
- Verify public/local reuse after complete consumption, no reuse after every failure mode, new
  policy decision before reuse, revocation blocking a pooled request without origin bytes, and no
  automatic non-idempotent retry.

## Validation

1. `cargo fmt --all -- --check`
2. `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings`
3. `cargo test -p vhrn-proxy --locked http1`
4. `cargo test -p vhrn-proxy --locked connect::public`
5. `cargo test -p vhrn-proxy --locked connect::broker::http`
6. `cargo test -p vhrn-proxy --locked --test proxy_process`
7. `cargo test -p vhrn-proxy --locked`

## Evidence required

- Record peak application buffer assertions for both directions and a successful body larger than
  the former cap.
- Map every forwarding/header/body/reuse clause to named tests and record pre-commit versus
  post-commit failure evidence.
- Record independent HTTP semantics, smuggling, cancellation, and resource review plus clean
  rereview.

## Completion criterion

Public and local HTTP use one compliant streaming pipeline; routing and Host come only from the
target; hop fields and Via are correct; bodies/trailers/informationals stream without size caps;
pooling is safe and policy-fresh; cancellation is owned; validation passes; and rereview is clean.
