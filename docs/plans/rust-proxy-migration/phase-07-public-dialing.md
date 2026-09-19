# Phase 7: Pin public resolution to the reviewed address boundary

## Execution contract

This file is the authoritative implementation specification for Phase 7. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 7 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../AGENTS.md).
4. Read the frozen [consumer contract](../../proxy/consumer-contract.md).
5. Implement only this phase and stay within its editable paths and responsibility boundary.
6. Record detailed implementation, validation, and review evidence in this file.
7. After every completion requirement, independent review, and rereview are satisfied, apply
   the master plan's status-transition rules. If anything remains unresolved, leave the next phase
   `Blocked`.

## Objective

Make every newly created public connection use one bounded, fully validated DNS/literal answer set
and numeric-only fallback within the frozen IANA global-unicast boundary and one cumulative
deadline.

## Inputs and editable paths

Read:

- the contract's Globally reachable unicast boundary, DNS and connection establishment, and public
  transport response classes;
- the IANA IPv4 and IPv6 Special-Purpose Address Registry snapshot fixed by the contract at
  2025-10-09; if an exact archived entry cannot be obtained, stop rather than substitute a newer
  snapshot silently;
- `proxy-rs/src/connect/public.rs`, target types, and IP fixtures.

Edit only:

- `proxy-rs/src/connect/public.rs` and new children under `proxy-rs/src/connect/public/`;
- public connector test seams and IP/registry fixtures under `proxy-rs/testdata/` or
  `shared/testdata/`;
- connector-related dependency declarations and `Cargo.lock` if required;
- detailed implementation, validation, and review evidence in this file;
- [`plan.md`](plan.md) only for status transitions permitted by the master plan.

Do not change HTTP parsing/forwarding, broker code, lifecycle limits, workflows, or production
selection.

## Required behavior

1. Encode the reviewed registry snapshot as explicit data with most-specific-prefix matching and
   the three required flags. IPv4 multicast and limited broadcast are non-unicast. A matching IPv4
   special-purpose entry is eligible only when Destination, Forwardable, and Globally Reachable are
   all true; ordinary unmatched IPv4 unicast is eligible.
2. For IPv6, reject multicast and IPv4-mapped addresses. Within `2000::/3`, allow only when no
   most-specific special entry fails the three-true test. Outside `2000::/3`, allow only an explicit
   most-specific special entry whose three flags are true; deny every other address. Do not
   reinterpret mapped or scoped input.
3. For a DNS name, resolve exactly once per newly created public connection. Require 1..=64
   returned addresses, retain the set, canonicalize only forms the contract permits, and reject the
   entire set before dialing if any answer is unsafe. CNAME aliases are not separately authorized.
4. For an IP literal, skip DNS and validate that literal through the same boundary. A target or
   answer safety denial is a typed `403` outcome and produces the Phase 4 denial diagnostic/record
   for the normalized request host.
5. Use one ten-second deadline spanning resolution and every TCP attempt. Try only retained numeric
   socket addresses and continue across eligible IPv6 and IPv4 answers when an earlier connect
   fails. One failure may not prevent fallback while deadline remains. Never perform connector or
   HTTP-client name resolution after validation.
6. Distinguish typed outcomes: unsafe/invalid answer is policy `403`; DNS failure, empty answer, or
   exhausted refusals is `502`; expiry of the cumulative deadline is `504`. Cancellation drops the
   resolver and current connect future promptly.
7. Do not read or honor ambient HTTP proxy variables. Reuse remains legal only for the identical
   normalized authority; opening a replacement connection repeats resolution and validation.

## Interfaces and data flow

- `PublicConnector::open` receives an authorized typed target and a cancellation token, resolves to
  a bounded `ValidatedAnswers`, and passes numeric `SocketAddr` values to a numeric dialer.
- Keep resolver and dialer traits available in non-test code where needed for deterministic tests;
  production implementations are thin edges. Return a `PublicConnectError` enum consumed by Phase
  4's safe response renderer.
- Store the registry date and provenance next to the table and fixtures; runtime performs no
  registry or policy fetch.

## Edge cases and focused tests

- Cover every registry prefix boundary and nested globally reachable exception, multicast,
  broadcast, documentation, benchmark, private, link-local, unspecified, loopback, mapped IPv6,
  scoped literals, outside-`2000::/3` ordinary IPv6, and mixed safe/unsafe sets.
- Cover 0, 1, 64, and 65 answers; one resolver call; literal no-resolver; first-address refusal then
  fallback; cross-family fallback; all refused; DNS delay plus dial attempts sharing one deadline;
  cancellation; and no second lookup.
- Assert address denials audit once and never reach the dialer, while refusal/timeout never creates
  a misleading denial record.

## Validation

1. `cargo fmt --all -- --check`
2. `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings`
3. `cargo test -p vhrn-proxy --locked connect::public`
4. `cargo test -p vhrn-proxy --locked --test proxy_process`
5. `cargo test -p vhrn-proxy --locked`

## Evidence required

- Record the exact registry snapshot source/date and fixture derivation, with a reviewer-confirmed
  boundary audit.
- Record resolver/dial call traces for the count, all-answer rejection, fallback, one-deadline, and
  cancellation tests.
- Record validation and independent security rereview with no remaining finding.

## Completion criterion

No public connection can target a non-global address; the reviewed most-specific IANA rules are
fully fixture-tested; DNS is single-shot and bounded to 64 answers; numeric fallback shares one
ten-second budget; outcomes map correctly; tests pass; and rereview is clean.
