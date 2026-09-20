# Phase 6: Enforce the HTTP/1 ingress and direct-endpoint contract

## Execution contract

This file is the authoritative implementation specification for Phase 6. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 6 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../AGENTS.md).
4. Read the frozen [consumer contract](../../proxy/consumer-contract.md).
5. Implement only this phase and stay within its editable paths and responsibility boundary.
6. Record detailed implementation, validation, and review evidence in this file.
7. After every completion requirement, independent review, and rereview are satisfied, apply
   the master plan's status-transition rules. If anything remains unresolved, leave the next phase
   `Blocked`.

## Objective

Replace reliance on library-default request parsing with a bounded incremental HTTP/1 ingress that
can prove the frozen wire limits and framing rules, then implement the complete direct endpoint
surface on top of it.

## Inputs and editable paths

Read:

- the contract's HTTP interface, Direct endpoints and diagnostics, and inbound resource limits;
- Phase 5's raw target and response APIs;
- `proxy-rs/src/server/listener.rs`, `server/router.rs`, `server/response.rs`, and current process
  harness.

Edit only:

- `proxy-rs/src/server.rs`, `server/listener.rs`, `server/router.rs`, and `server/response.rs`;
- new idiomatic module files such as `proxy-rs/src/http1.rs` and `proxy-rs/src/http1/*.rs` (never
  `mod.rs`);
- parsing dependencies in `proxy-rs/Cargo.toml` and `Cargo.lock` if needed;
- ingress/direct-endpoint fixtures and candidate unit/process tests;
- detailed implementation, validation, and review evidence in this file;
- [`plan.md`](plan.md) only for status transitions permitted by the master plan.

Do not change public address policy, broker protocol, origin forwarding, CONNECT relay, global
connection limits, workflows, or production selection.

## Required behavior

1. Incrementally parse cleartext HTTP/1.0 and HTTP/1.1 from a fixed-capacity connection buffer.
   Limit the request line to 8192 octets including CRLF and the raw header section to 64 KiB
   including field lines and final empty CRLF but excluding the request line. Detect each overflow
   before growing or allocating beyond its cap; return `400` for request-line overflow and `431`
   for header overflow, then close.
2. Apply a 30-second deadline to incomplete request heads. Do not apply an idle or whole-exchange
   deadline after a valid request begins streaming.
3. Reject obsolete folding, bare CR, invalid field syntax, ambiguous whitespace, conflicting or
   invalid `Content-Length`, simultaneous `Transfer-Encoding` and `Content-Length`, transfer coding
   whose final coding is not `chunked`, and body-length integer overflow with `400` and close. Do
   not repair framing.
4. Require exactly one syntactically valid `Host` field for HTTP/1.1. Keep it separate from routing:
   Phase 5's request-target remains authoritative. Accept HTTP/1.0 without Host. Return `505` for an
   unsupported HTTP version and reject HTTP/2 prior knowledge, h2c, listener TLS, and malformed
   prefaces without forwarding.
5. Reject CONNECT containing any `Transfer-Encoding`, `Expect`, or `Content-Length`, including
   `Content-Length: 0`, before policy or network work. Preserve bytes already buffered after an
   otherwise-unframed CONNECT head for Phase 10.
6. Reject a plain forwarded protocol upgrade with `501` before policy/dial. Preserve valid
   `Expect: 100-continue` on ordinary forwarding for Phase 9.
7. Represent a valid request body as a streaming framing decoder over the owned downstream
   connection, not an aggregate byte buffer. Use at most 64 KiB application buffering and preserve
   permitted trailers for forwarding. Serialize requests on a persistent downstream connection;
   no unbounded pipelining queue is allowed.
8. Implement direct requests without policy or network work. `GET` and `HEAD /healthz` use Phase 4
   health and return exact `ok\n`/`unhealthy\n` with 200/503. `GET` and `HEAD /__status` reread only
   the mode, return exact JSON with 200, or `503` plus enforce JSON on invalid mode. Both use exact
   content types. Other methods on those paths return `405` and `Allow: GET, HEAD`; every other
   origin-form path returns `404`.
9. Implement `OPTIONS *` as `204` with `Allow: GET, HEAD, OPTIONS, CONNECT` and no forwarding. Any
   other asterisk-form method returns `400`. Apply HEAD body suppression consistently.

## Interfaces and data flow

- The listener owns one `Http1Connection` with a bounded read buffer. It yields a `RequestHead`, a
  streaming `IncomingBody`, and any CONNECT prefix. The router returns a streaming response plus a
  reuse/close decision; the connection processes at most one exchange at a time.
- Phase 9 will reuse the framing primitives for origin traffic. Keep parsing/serialization pure and
  separately testable from sockets.
- Parser errors become Phase 5 typed failures; raw parser/library diagnostics never cross the
  client boundary.

## Edge cases and focused tests

- Test exact limit minus one, exact limit, and limit plus one for request lines and headers,
  fragmented byte-at-a-time input, 30-second slow heads with paused time, many small headers,
  duplicate Host, and every ambiguous framing combination.
- Test fixed, chunked, empty, and trailer-bearing request-body decoders without aggregation, plus
  early disconnect and body length overflow.
- Process-test HTTP/1.0/1.1 persistence, unsupported versions, raw malformed heads, framed CONNECT
  rejection with policy/network spies, eager CONNECT bytes, upgrade rejection, OPTIONS, endpoint
  methods, HEAD lengths, live health repair, sticky log health, and status invalid mode.

## Validation

1. `cargo fmt --all -- --check`
2. `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings`
3. `cargo test -p vhrn-proxy --locked http1`
4. `cargo test -p vhrn-proxy --locked server::listener`
5. `cargo test -p vhrn-proxy --locked --test proxy_process`
6. `cargo test -p vhrn-proxy --locked`

## Evidence required

- Record the buffer ownership model and exact raw-octet accounting tests.
- Provide a table from each ingress/direct requirement to a named unit or black-box test, including
  proof that rejected heads trigger no policy or network operation.
- Record independent parser/smuggling/resource review and clean rereview.

## Completion criterion

Ingress accepts only the frozen HTTP/1 surface, enforces exact limits incrementally, rejects
ambiguous framing, streams rather than aggregates bodies, preserves eager CONNECT bytes, implements
all direct endpoints exactly, passes validation, and has no remaining independent review finding.

## Implementation evidence

Implemented on 2026-09-20 within the Phase 6 editable paths.

- `server/http1.rs` now owns the downstream socket and one fixed-capacity 72 KiB head/read buffer:
  8192 request-line octets plus 64 KiB of header-section capacity. Reads are capped at the active
  boundary, so an over-limit line or header section is rejected before another octet is stored.
  `http1_request_line_raw_octet_limits_are_exact` and
  `http1_header_raw_octet_limits_are_exact` cover limit-minus-one, exact-limit, and limit-plus-one
  inputs and assert that the overflow cases never read beyond the active cap.
- `Http1Connection` yields the raw-target `RequestHead` and a borrowing `IncomingBody`. The body
  decoder emits fixed-length and decoded chunk data in chunks no larger than 64 KiB, reports
  permitted trailers separately, leaves pipelined bytes in the sole connection buffer, and turns
  an otherwise-unframed CONNECT into an owned `BufferedIo` which replays its eager prefix first.
  The listener processes one exchange at a time; it has no request queue.
- Response-time prefetch obeys the next request's active request-line or header boundary. A pending
  parse error carries that request's HEAD classification rather than inheriting the preceding
  request's method, so error bodies remain correctly suppressed on persistent connections.
- Request-head parsing is independent of Hyper defaults. It rejects bad line endings, obsolete
  folding, whitespace before a field colon, invalid names and values, invalid or conflicting
  lengths, ambiguous transfer coding, duplicate or invalid HTTP/1.1 Host, HTTP/1.0 transfer
  coding, TLS bytes, and malformed versions. Syntactically valid unsupported versions receive
  `505`; malformed version tokens receive `400`. Transfer-coding parameters and chunk extensions
  use quote-aware delimiter parsing and validate escaped quoted strings. Parser failures map only
  to bounded typed proxy responses.
- The 30-second timer begins after the first head octet. It does not impose an idle timeout before
  a request starts and is not active while a valid body, response, or tunnel streams.
- The listener rejects framed CONNECT and ordinary upgrades before route authorization. Successful
  CONNECT responses are serialized as the exact HTTP/1.1 establishment head without a framing
  field, then the existing bounded relay owns the prefixed downstream stream.
- Direct routing uses the Phase 5 raw request-target classifier. `/healthz`, `/__status`, `OPTIONS
  *`, method handling, status codes, content types, Allow values, representation lengths, and HEAD
  suppression are emitted by the explicit serializer. Status rereads only the bounded mode file;
  invalid mode returns 503 and the enforce JSON.

The Phase 6 production boundary is the streaming downstream decoder. The inherited Phase 5
connectors still require `Request<Full<Bytes>>`, so the router retains their existing aggregate,
size-capped compatibility bridge. Phase 6 explicitly prohibits changing origin forwarding and
does not own those connector files. The migration ledger and Phase 9 specification assign removal
of that bridge, unlimited request-body forwarding, and trailer forwarding to Phase 9, where
`IncomingBody` will feed the common origin writer incrementally.

### Requirement-to-test trace

| Requirement | Named evidence |
| --- | --- |
| 1. Incremental HTTP/1.0/1.1 parsing and exact 8192/64-KiB caps | `http1_request_line_raw_octet_limits_are_exact`, `http1_header_raw_octet_limits_are_exact`, `http1_byte_at_a_time_fragmentation_preserves_head_and_body`, `http1_many_small_headers_fit_by_raw_octets_not_field_count`, `response_prefetch_enforces_the_active_next_head_limit` |
| 2. Head-only 30-second deadline | `incomplete_head_closes_thirty_seconds_after_its_first_octet`, `idle_connection_has_no_request_head_deadline_before_its_first_octet` |
| 3. Strict syntax and unambiguous framing | `http1_rejects_ambiguous_framing_and_invalid_hosts`, `http1_transfer_parameters_observe_quoted_delimiters_and_escapes`, `http1_body_decoder_rejects_early_disconnect_and_chunk_overflow`, `http1_malformed_hosts_versions_and_prefaces_are_rejected` |
| 4. Host/version/preface rules and raw-target authority | `http1_accepts_http10_without_host_and_identical_content_lengths`, `http1_rejects_tls_and_unsupported_versions_without_waiting_for_more`, `http1_malformed_hosts_versions_and_prefaces_are_rejected`, `parsed_component_bridge_keeps_http_and_connect_disjoint` |
| 5. Framed CONNECT rejection and eager prefix | `malformed_and_https_targets_have_no_authorization_or_network_side_effects` checks all three forbidden fields with authorization, resolver, public-connector, and broker spies at zero; `http1_persistence_connect_framing_and_upgrade_are_enforced_before_broker_work` observes no broker accept; `http1_connect_prefix_is_replayed_before_socket_bytes` and `local_connect_preserves_buffered_bytes_and_survives_revocation` prove prefix order |
| 6. Upgrade rejection and valid Expect preservation | `http1_persistence_connect_framing_and_upgrade_are_enforced_before_broker_work` proves h2c receives 501 without a broker connection and exercises repeated/list-form 100-continue plus the forwarded Expect field |
| 7. Streaming decoder, 64-KiB chunks, trailers, and serialization | `http1_fixed_and_chunked_decoders_stream_and_preserve_trailers`, `http1_body_decoder_rejects_early_disconnect_and_chunk_overflow`, `persistent_client_reopens_bounded_policy_and_observes_atomic_repair`, `local_http_client_disconnect_closes_broker_origin_work` |
| 8. Complete direct endpoint surface | `http1_ingress_and_direct_endpoint_contract_is_exact`, `health_tracks_live_policy_and_sticky_audit_failure`, `status_rereads_only_mode_and_fails_closed_when_mode_is_invalid`, `healthz_does_not_poll_its_body` |
| 9. Asterisk form and consistent HEAD suppression | `http1_ingress_and_direct_endpoint_contract_is_exact`, `http1_malformed_hosts_versions_and_prefaces_are_rejected`, `failures_have_exact_status_headers_and_bytes_including_head`, `prefetched_errors_use_the_next_requests_head_state` |

Direct and malformed targets do not enter authorization: `healthz_does_not_poll_its_body` and
`malformed_and_https_targets_have_no_authorization_or_network_side_effects` observe zero body polls,
policy reads, audit writes, resolutions, public connector calls, and broker frames as applicable.
The black-box CONNECT/upgrade test independently proves that rejected local heads produce no broker
connection.

## Validation evidence

All required commands passed on 2026-09-20:

1. `cargo fmt --all -- --check`
2. `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings`
3. `cargo test -p vhrn-proxy --locked http1` — 13 unit and 3 black-box focused tests passed.
4. `cargo test -p vhrn-proxy --locked server::listener` — 7 focused tests passed.
5. `cargo test -p vhrn-proxy --locked --test proxy_process` — 22 black-box tests passed.
6. `cargo test -p vhrn-proxy --locked` — 116 unit, 22 black-box, and 0 doc tests passed.

## Independent review evidence

An independent `rust_reviewer` review and two rereviews were completed on 2026-09-20. The reviewer
inspected only the Rust candidate, Phase 6/Phase 9 ownership documents, coverage ledger, manifest,
and repository instructions; no legacy Go source, tests, module files, or history were inspected.

The initial review identified response-time prefetch exceeding the active next-head boundary,
malformed version tokens incorrectly receiving `505`, delimiter-blind transfer-parameter and chunk
extension parsing, and missing repeated/list-form `Expect: 100-continue` coverage. It also raised
the inherited aggregate forwarding bridge for boundary clarification. The implementation was
hardened and regression-tested for each parser finding. After reading the staged ownership
documents, the reviewer withdrew the bridge finding: Phase 6 owns `IncomingBody`, while Phase 9
explicitly owns replacement of aggregate origin forwarding.

The first rereview found that a prefetched parse error retained the preceding request's HEAD state.
The error now stores and restores the next request's method classification, and
`prefetched_errors_use_the_next_requests_head_state` verifies both GET-to-HEAD and HEAD-to-GET
persistence sequences at the rendered body boundary.

The final rereview reported no actionable findings, no material test gaps, no security or resource
risk, no clean-room concern, and no out-of-scope source changes. It independently reran all six
required validation commands at the final counts recorded above and authorized the Phase 6 status
transition.
