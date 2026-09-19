# Phase 8: Harden the authenticated loopback broker transport

## Execution contract

This file is the authoritative implementation specification for Phase 8. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 8 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../AGENTS.md).
4. Read the frozen [consumer contract](../../proxy/consumer-contract.md).
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
- the raw local-stream portion of `proxy-rs/src/connect/broker/http.rs`; HTTP forwarding behavior
  remains Phase 9-owned;
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
