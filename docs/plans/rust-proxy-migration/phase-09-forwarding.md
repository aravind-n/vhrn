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

## Implementation evidence

- `connect/forward.rs` is the single owned HTTP/1 exchange state machine used by both
  `PublicConnector::forward` and `BrokerConnector::forward`. The connectors now check out a raw
  `HttpOrigin` keyed by normalized authority; a new public stream repeats Phase 7 and a new local
  stream repeats Phase 8. The frozen consumer-contract hash was rechecked as
  `f3499ff66aa9e15d3f7788153b3f109dc7b020956a6d306eda5e103e806075ed`.
- Request serialization preserves the method, emits origin-form path/query, uses `/` for an empty
  path and `*` for the applicable OPTIONS target, and regenerates `Host` only from the normalized
  target authority. `request_line_uses_origin_form_options_star_and_canonical_host` and
  `post_origin_failure_is_not_automatically_retried` cover clauses 1-2, including absence of an
  automatic POST retry.
- `connection_nominations` parses every `Connection` value before the shared hop sanitizer removes
  fixed and nominated fields. The request and response paths regenerate framing, append `Via`,
  preserve permitted trailers (including repeated response trailer fields), and remove proxy
  credentials and identity headers.
  `forwards_expect_informationals_hop_filtering_via_and_trailers`,
  `forwards_request_trailers_and_removes_connection_nominations`,
  `removes_connection_nominated_and_fixed_hop_headers`, and
  `appends_received_protocol_to_existing_via_chain` cover clauses 3-4.
- Origin response transfer coding has a response-specific model for absent, final chunked, and
  final non-chunked coding. A final non-chunked coding is close-delimited upstream and rechunked for
  HTTP/1.1; HTTP/1.0 never receives a chunked declaration over dechunked bytes.
  `reframes_origin_transfer_codings_for_http11_and_http10_clients` covers both required wire cases.
- Requests and responses stream incrementally with no product body limit or whole-body timer.
  Request frames are at most 32 KiB; a pending chunk prefix accounts for the complete data frame in
  its next state, and the state machine never reads another downstream frame while an origin write
  is pending. Trailer fields are parsed incrementally and their names, separators, values, and line
  endings are forwarded as separate retained components instead of retaining raw, parsed, and
  serialized copies. Response body frames reserve space for downstream chunk prefixes, which are
  encoded on the stack. Test-only aggregate accounting includes unread
  connection bytes, decoder state, pending bytes, and pending next states. Origin syntax read-ahead
  is bounded, retained prefixes are accounted with copied frames, and direct body reads transfer
  one allocation into `Bytes` without a second copy. The recorded peaks are asserted at or below
  64 KiB by `http1_fixed_and_chunked_decoders_stream_and_preserve_trailers`,
  `stalled_chunk_prefix_accounts_for_its_full_retained_data_frame`,
  `request_trailer_limit_stays_within_the_aggregate_buffer_budget`,
  `forwards_a_limit_sized_request_trailer_with_bounded_state`,
  `response_frames_reserve_space_for_downstream_chunk_framing`,
  `chunk_prefix_and_maximum_response_frame_fit_the_aggregate_budget`,
  `origin_many_tiny_chunks_preserve_trailers_and_bound_each_data_frame`, and
  `origin_trailer_limit_stays_within_the_aggregate_buffer_budget`.
  `streams_request_larger_than_former_cap_with_aggregate_buffer_bound` sends 8 MiB + 17 bytes, and
  `streams_response_larger_than_former_cap_before_origin_finishes` proves first-byte latency while
  receiving 8 MiB + 19 bytes. These tests, the tiny-chunk case, and the trailer tests cover clause 5.
- Applicable 1xx responses are forwarded while request upload continues, while HTTP/1.0 clients do
  not receive unsupported informational responses or forward an `Expect: 100-continue` header to
  an origin that cannot answer it compatibly. HEAD, 204, and 304 emit no body while preserving
  describing fields. `forwards_expect_informationals_hop_filtering_via_and_trailers`,
  `suppresses_origin_informationals_for_http10_clients`,
  `request_line_uses_origin_form_options_star_and_canonical_host`,
  `head_204_and_304_preserve_describing_headers_without_body_bytes`,
  `origin_header_section_raw_limit_is_exact_without_overallocation`, and
  `origin_header_limit_is_enforced_before_downstream_commit` cover clause 6, including exact 64 KiB
  acceptance and 64 KiB + 1 pre-commit `502` with no origin bytes leaked.
- The exchange owns both origin halves and has no detached body task. Production TCP connections
  use a safe `mio` close monitor so EOF remains observable even when unread request bytes hide it
  from an ordered async read; no extra request frame is staged merely to detect closure. The
  exchange observes client closure while origin response I/O, request-head writes, or
  backpressured request-body writes are pending, and the response body observes process shutdown.
  `tcp_peer_close_monitor_ignores_unread_data_and_observes_eof` verifies the transport property with
  a full 64 KiB unread, while `precommit_origin_error_is_502_but_postcommit_error_only_closes`
  distinguishes the error boundary. `client_disconnect_cancels_pending_origin_io_without_detached_work`,
  `client_disconnect_cancels_a_backpressured_origin_upload` (two full request frames and both origin
  halves), `client_disconnect_cancels_public_resolution_and_dial`,
  `client_disconnect_cancels_pending_broker_authentication`, and
  `shutdown_after_response_commit_cancels_pending_origin_body_io` cover clause 7.
- A stream returns to the bounded idle pool only after clean end-of-message and only under the same
  normalized authority. `pool_returns_only_clean_complete_reusable_responses` separately covers a
  clean body, incomplete fixed body, malformed and incomplete chunked bodies, a non-poolable
  `Connection: close` response, a dropped response, cancellation, and coalesced unsolicited bytes
  after both fixed and bodyless responses. An origin is reusable only when the parser has no
  retained surplus bytes.
  `origin_head_errors_and_connection_close_never_reuse_streams` covers origin-head parse failure and
  close semantics through the complete forwarding path. `public_and_local_pool_reuse_remain_policy_fresh`
  proves both transports reuse only after a new proxy policy decision and that revocation sends no
  origin bytes; the process tests `persistent_client_reopens_bounded_policy_and_observes_atomic_repair`
  and `local_policy_is_rechecked_before_every_broker_connection` supply black-box policy evidence.
  Together with `post_origin_failure_is_not_automatically_retried`, these map clause 8.
- `broker_http_uses_the_shared_streaming_forwarder`,
  `public_and_local_pool_reuse_remain_policy_fresh`, and the process test
  `local_http_forwards_and_streams_through_authenticated_broker` prove clause 9: public and brokered
  local HTTP share the state machine, and the broker path still authenticates before handing off its
  raw stream.

## Validation evidence

- `cargo fmt --all -- --check` — passed.
- `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings` — passed.
- `cargo test -p vhrn-proxy --locked http1` — passed 18 library tests and 3 process tests selected
  by the filter.
- `cargo test -p vhrn-proxy --locked connect::public` — passed 16 focused tests.
- `cargo test -p vhrn-proxy --locked connect::broker::http` — passed its focused shared-forwarder
  test.
- `cargo test -p vhrn-proxy --locked --test proxy_process` — passed all 23 process tests.
- `cargo test -p vhrn-proxy --locked` — passed 153 library tests and 23 process tests; doc tests
  contained no cases and passed.
- `git diff --check` — passed. Source searches confirmed the former production 8 MiB forwarding
  caps, 30-second whole-response/body timers, Hyper client driver, and aggregate `send_request` path
  are absent from the production forwarding path; the remaining 8 MiB/30-second constants are
  confined to the test-only parsed-request compatibility bridge in `server/router.rs`.

## Independent review evidence

- The required `rust_reviewer` review on 2026-09-20 found three issues: request-oriented transfer
  coding rules were incorrectly reused for origin responses; downstream disconnect was not polled
  while an upstream body write was blocked; and individual frame-size tests did not prove the
  aggregate 64 KiB application-buffer bound. It also requested explicit request-trailer,
  no-reuse-failure, bodyless-status, origin-header-boundary, and mid-response-shutdown evidence.
- The first fixes made the transfer-coding model and downstream reframing protocol-specific, added
  the requested semantic and lifecycle cases, and introduced aggregate instrumentation. Rereview
  then found two remaining boundary errors: a FIN hidden behind two unread request frames could
  outlive a full upload staging buffer, and pending chunk/trailer next states were omitted from the
  aggregate byte proof.
- The next fixes use TCP EOF/error readiness independently of ordered body reads, remove production
  response-time request prefetch, accounts the complete pending state graph, and parses and emits
  trailers incrementally. Exact-limit trailer tests cover both directions, the stalled-prefix test
  counts its retained full frame, and the real-TCP backpressure test proves the exchange and both
  origin halves terminate with two frames sent before client close.
- Further rereview found and resolved response chunk-prefix headroom, duplicate response trailer
  emission, HTTP/1.0 informational/Expect handling, client disconnects during public setup and
  broker authentication, and pooling with parser-buffered unsolicited response bytes. Focused
  tests now exercise each boundary.
- The required `rust_reviewer` performed a final stable-tree rereview on 2026-09-20 and reported no
  actionable correctness or security findings, no material test gaps, and no material residual
  implementation risks. It independently rechecked HTTP/1 framing and smuggling defenses,
  informationals/Expect/bodyless behavior, pre/post-commit failures, cancellation ownership,
  aggregate 64 KiB accounting, pool reuse and policy freshness, resource behavior, editable-path
  scope, and the complete validation matrix. The clean rereview concluded that Phase 9 satisfies
  its completion criteria and justified the Phase 9 `Complete` / Phase 10 `Ready` transition.
