# Phase 6: Enforce the HTTP/1 ingress and direct-endpoint contract

- [ ] Status: Not started
- Depends on: [Phase 5](phase-05-targets-outcomes.md) complete.

## Execution contract

This file is the authoritative implementation specification for Phase 6. Start here and:

1. Read the shared [migration plan](plan.md).
2. Read repository [`AGENTS.md`](../../../AGENTS.md).
3. Read the frozen [consumer contract](../../proxy/consumer-contract.md).
4. Confirm every dependency named above is complete before editing.
5. Implement only this phase and stay within its editable paths and responsibility boundary.
6. Record concrete evidence and update this file's status; leave later phase files unchanged.

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
- this phase's status and evidence in this file.

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

