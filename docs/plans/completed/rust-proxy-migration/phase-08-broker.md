# Phase 8: Harden the authenticated loopback broker transport

## Execution contract

This file is the authoritative implementation specification for Phase 8. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 8 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../../AGENTS.md).
4. Read the frozen [consumer contract](../../../proxy/consumer-contract.md).
5. Implement only this phase and stay within its editable paths and responsibility boundary.
6. Record detailed implementation, validation, and review evidence in this file.
7. After every completion requirement, independent review, and rereview are satisfied, apply
   the master plan's status-transition rules. If anything remains unresolved, leave the next phase
   `Blocked`.

## Objective

Complete the proxy half of the broker contract and make it the only possible route from a local
grant to a host-loopback stream.

## Inputs and editable paths

Read:

- the contract's Authenticated loopback broker and local connection flow sections;
- allowed host interfaces in `cli/src/broker.rs` and the broker route/mount construction in
  `cli/src/run.rs`;
- `proxy-rs/src/connect/broker.rs`, `connect/broker/protocol.rs`,
  `connect/broker/http.rs`, config/token types, Phase 3 local decisions, and broker fixtures.

Edit only:

- `proxy-rs/src/connect/broker.rs`, `proxy-rs/src/connect/broker/protocol.rs`, and broker test
  seams;
- the raw local-stream and typed broker-error propagation portions of
  `proxy-rs/src/connect/broker/http.rs`; HTTP forwarding semantics remain Phase 9-owned;
- the broker-readiness cancellation call site in `proxy-rs/src/lib.rs`;
- the broker-only cancellation and safe response-classification call sites in
  `proxy-rs/src/server/router.rs`;
- broker fixtures and broker-focused candidate tests;
- detailed implementation, validation, and review evidence in this file;
- [`plan.md`](plan.md) only for status transitions permitted by the master plan.

Do not change host broker code unless a test-only seam is strictly required; do not share compiled
protocol types, change the host wire contract, alter public dialing, or edit packaging/workflows.

## Required behavior

1. Read the token exactly once during Phase 4 startup. Accept exactly 64 lowercase hexadecimal
   bytes and no newline. Redact its `Debug`/`Display`, never clone it into errors, and ensure panic,
   process, endpoint, and origin output cannot reveal it.
2. Use a fresh TCP connection for startup readiness and every new local upstream. Send exactly the
   bounded ASCII `READY` or `CONNECT` frame with canonical authority. The proxy never accepts a
   token from HTTP and sends it only to the configured broker route.
3. Bound response parsing to four octets and accept only `OK\n`; treat EOF, `ERR\n`, malformed or
   overlong response text, extra response-line content, and timeout as failure. For a successful
   CONNECT exchange, retain every byte already read after the three-byte response as the first
   upstream bytes.
4. Complete readiness within the broker's three-second handshake bound and cancel earlier on
   startup cancellation. Readiness grants no authority and opens no origin. Failure remains fatal
   through Phase 4.
5. For a local request, canonicalize through Phase 5, obtain a fresh Phase 3 three-file decision,
   and send CONNECT only for an exact grant. The proxy never directly dials `localhost`, `127/8`,
   or `::1`, never resolves `localhost`, and never converts a public DNS result into a broker call.
6. Bound the full local CONNECT exchange so the broker's three-second frame work plus cumulative
   ten-second loopback connect cannot hold the proxy indefinitely. Return typed broker rejection or
   I/O as `502` and expiry as `504`, without exposing which broker check failed.
7. After `OK\n`, remove handshake deadlines and return an owned raw stream. Cancellation before
   handoff closes it. A new local TCP connection always repeats broker CONNECT and the broker's
   independent recheck. Later Phase 9 pooling may reuse an established local HTTP connection only
   after a fresh proxy-side policy decision for the later request.
8. Preserve the supported routes exactly: Apple default-network gateway or
   `host.docker.internal` on Docker through local Colima. Treat the configured broker endpoint as
   the host-supplied route, never a public target.

## Interfaces and data flow

- `BrokerConnector::ready` and `BrokerConnector::connect` return typed redacted errors and accept
  cancellation. `BrokerStream` owns a bounded prefix buffer plus the TCP stream.
- Router obtains policy permission before invoking `connect`; the connector itself has no public
  policy fallback. Phase 9 consumes `BrokerStream` exactly like a checked public stream.

## Edge cases and focused tests

- Table-test every token case and exact frame length/content for all canonical local spellings.
- Fragment `OK\n`, coalesce it with payload, send malformed/long/EOF/ERR replies, stall dial/write/
  read, cancel each step, and assert retained payload and socket closure.
- Prove a missing/invalid/revoked local grant produces no broker connection; `report`/`open` and a
  matching public entry still cannot invoke the broker; a newly granted or repaired file takes
  effect on the next request.
- Assert errors, panic formatting, endpoint bodies, denial log, and process diagnostics contain no
  token or broker address.

## Validation

1. `cargo fmt --all -- --check`
2. `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings`
3. `cargo test -p vhrn-proxy --locked connect::broker`
4. `cargo test -p vhrn-proxy --locked --test proxy_process`
5. `cargo test -p vhrn-proxy --locked`

## Evidence required

- Record exact wire fixtures, deadline/cancellation tests, prefix preservation, policy-before-broker
  spies, and redaction searches.
- Record compatibility results against the allowed CLI Rust interfaces without modifying or
  importing their types.
- Record independent broker/security review and clean rereview.

## Completion criterion

The token and exact frames remain secret and bounded; readiness and CONNECT use fresh authenticated
connections; only exact live local grants reach the broker; post-response bytes survive; failures
are typed and redacted; no direct loopback route exists; tests pass; and rereview is clean.

## Implementation evidence

- `BrokerToken` accepts only 64 lowercase hexadecimal octets, owns one boxed startup value shared
  through the connector, and redacts both `Debug` and `Display`. The token table covers valid lower
  hex, empty, 63/65-octet, uppercase, non-hex, LF, CR, and non-ASCII inputs. A process test replaces
  the token file after readiness and proves the next CONNECT still uses the startup token.
- `BrokerProtocol` creates a fresh transport for READY and every CONNECT. Exact frame tests cover
  `localhost:80`, `127.255.255.255:81`, and `[::1]:82`; every emitted frame is checked byte-for-byte
  and against the 256-octet bound. The configured numeric or hostname endpoint is the dialer's only
  input; the canonical local authority appears only in the authenticated frame.
- Response parsing uses one four-octet buffer and accepts only `OK\n`. The broker fixture covers
  READY and CONNECT success, `ERR\n`, EOF, timeout, `OK extra\n`, `OK\r\n`, and overlong text.
  Fragmented `O`, `K`, `\n` succeeds, while a coalesced payload is preserved through the bounded
  prefix and raw stream.
- READY has one cumulative three-second dial/write/read budget. CONNECT has one cumulative
  thirteen-second budget matching the host's three-second frame work plus ten-second loopback dial.
  Typed redacted errors distinguish rejection, transport I/O, deadline, cancellation, and later
  origin failure. Unit tests cancel stalled dial, write, and CONNECT-response reads and observe
  pending-work or socket closure; the process timeout returns `504` and closes the broker socket.
- The live-policy process spy proves revoked, report-mode, open-mode, invalid, and missing local
  policy never connect to the broker even with a matching public entry. An atomic repair is observed
  on the next request. Existing persistent-local-HTTP coverage proves a later request is denied
  before reuse after revocation, while established tunnels survive policy replacement.
- Token/error/stream panic formatting, HTTP error bodies, denial logs, and process diagnostics are
  checked for absence of the token and broker address. Source searches show direct TCP dialing in
  the broker module is confined to the configured `BrokerEndpoint`; local target authorities are
  never passed to `TcpStream::connect` or `lookup_host`.

## Compatibility evidence

- The frozen consumer contract hash was rechecked as
  `f3499ff66aa9e15d3f7788153b3f109dc7b020956a6d306eda5e103e806075ed`.
- Read-only comparison with `cli/src/broker.rs` confirms the independent implementations agree on
  64-byte lowercase-hex tokens, exact READY/CONNECT frames, canonical authorities, the 256-octet
  request cap, exact `OK\n`/`ERR\n` responses, the three-second handshake deadline, and the
  cumulative ten-second loopback dial. No CLI protocol types are imported or shared.
- Read-only comparison with `cli/src/run.rs` confirms the endpoint remains the Apple default-network
  gateway or Docker/Colima `host.docker.internal`, with the token mounted read-only at
  `/etc/vhrn-broker/token`. No CLI or host-broker source was modified. Live engine execution was not
  performed in this unit/process-test phase.

## Validation evidence

- `cargo fmt --all -- --check` — passed.
- `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings` — passed.
- `cargo test -p vhrn-proxy --locked connect::broker` — passed 17 focused tests.
- `cargo test -p vhrn-proxy --locked --test proxy_process` — passed 23 process tests.
- `cargo test -p vhrn-proxy --locked` — passed 134 library tests and 23 process tests; doc tests
  contained no cases and passed.
- `git diff --check` — passed. Redaction and dial-route searches were recorded during the audit.

## Independent review evidence

- The required `rust_reviewer` review on 2026-09-20 found no correctness, security, compatibility,
  or test-quality defect, but initially raised one P2 scope finding. The required cancellation
  plumbing and `502`/`504` classification changed production call sites in `proxy-rs/src/lib.rs`
  and `proxy-rs/src/server/router.rs`, while the editable-path list at that point named only broker
  files and test seams. The typed error propagation in `connect/broker/http.rs` also reached the
  boundary of the HTTP path whose forwarding behavior remains Phase 9-owned.
- The user explicitly authorized the narrow Phase 8 integration scope on 2026-09-20. The editable
  paths now name only the readiness cancellation call site, broker-only router cancellation and
  response classification, and typed broker-error propagation without taking ownership of Phase 9
  forwarding semantics.
- The same `rust_reviewer` then inspected the complete authorized diff and reran or confirmed all
  required validation. Rereview was clean with no actionable findings or material test gaps. The
  reviewer confirmed the integration edits stay within the authorized limits and preserve Phase 9
  forwarding semantics.
