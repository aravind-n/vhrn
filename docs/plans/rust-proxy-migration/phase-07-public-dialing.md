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
- the minimal production integration seams in `proxy-rs/src/domain/target.rs`,
  `proxy-rs/src/server/router.rs`, and `proxy-rs/src/server/response.rs` (explicitly authorized by
  the user on 2026-09-20 after independent review identified the original scope conflict);
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

## Implementation evidence

- `connect/public/registry.rs` encodes the frozen registry as explicit prefix and
  Destination/Forwardable/Globally-Reachable triples. Lookup selects the longest matching prefix;
  IPv4 multicast and limited broadcast are denied outside the table, while IPv6 applies the
  contract's `2000::/3` default, explicit outside-prefix exceptions, multicast denial, and mapped
  address denial.
- `ValidatedAnswers` accepts exactly 1 through 64 results and rejects the complete set before any
  dial when a result is unsafe, scoped, mapped, or over the bound. `SystemResolver` collects at
  most 65 results so an oversized answer set is observable without an unbounded allocation.
- Each new connection owns one `Instant` deadline across its single resolver future and every
  numeric `TcpStream::connect` attempt. Failed attempts continue in retained answer order and
  across address families while budget remains. Cancellation races and drops the resolver or
  current dial future.
- IP literals skip resolution and enter the same address validator. Valid zone-qualified and
  mapped IPv6 request syntax is retained through target classification and denied at the address
  boundary, producing the same audited `403` as an unsafe DNS answer. DNS/empty/exhausted failures
  produce `502`, deadline expiry produces `504`, and cancellation produces `503`; transport
  failures do not create denial records.
- Public HTTP reuse is keyed by normalized host, retained IPv6 scope, and port. Consequently a
  scoped authority cannot reuse an unscoped connection. Opening any replacement connection
  repeats resolution and validation. Production dialing uses only numeric `SocketAddr` values and
  does not consult ambient proxy variables.

## Registry provenance and boundary audit

- Snapshot date: **2025-10-09**, the date fixed by the frozen contract and still identified as the
  last update by the official IANA IPv4 and IPv6 Special-Purpose Address Registry pages when this
  phase was executed on 2026-09-20:
  <https://www.iana.org/assignments/iana-ipv4-special-registry/> and
  <https://www.iana.org/assignments/iana-ipv6-special-registry/>.
- `proxy-rs/testdata/iana-special-purpose-addresses.tsv` is a direct transcription of every IPv4
  and IPv6 prefix and the three contract-relevant flags. `N` preserves an IANA blank or
  indeterminate value; the one IPv4 registry row naming two `/32` blocks is split into two runtime
  entries without changing its flags.
- `runtime_registry_is_the_complete_reviewed_fixture` requires exact ordered equality between the
  runtime tables and the snapshot fixture. `every_registry_prefix_boundary_matches_the_reviewed_fixture`
  independently evaluates the address immediately before, first address, last address, and
  immediately after every prefix, including nested exceptions. The focused address fixture also
  covers multicast, broadcast, documentation, benchmark, private, link-local, unspecified,
  loopback, mapped/scoped inputs, ordinary IPv6 outside `2000::/3`, and mixed sets.
- The independent Rust reviewer checked every snapshot row and relevant flag against the frozen
  IANA sources, confirmed most-specific-prefix behavior and both outer-boundary rules, and found no
  remaining registry discrepancy.

## Resolver and dial traces

- Count and lookup trace: `dns_answer_count_accepts_one_through_sixty_four_only` records outcomes
  `0 -> 502`, `1 -> accepted`, `64 -> accepted`, and `65 -> 403`. In
  `resolves_once_and_retains_the_validated_answer_set`, normalized `api.example.com:80` is resolved
  once and its retained `8.8.8.8:80`, `1.1.1.1:80` answers are dialed in that order; no second
  lookup occurs.
- Whole-set rejection trace: empty, mixed `8.8.8.8 + 127.0.0.1`, mixed global + mapped IPv6, and
  loopback-only answer sets each make one resolver call and zero dial calls. Unsafe literal,
  scoped literal, and mapped literal cases make zero resolver and zero dial calls. Router tests
  confirm a single normalized-host denial record.
- Fallback trace: an IPv6 refusal at `[2606:4700:4700::1111]:8080` is followed by a successful
  IPv4 attempt at `8.8.8.8:8080`, after one resolution. Exhausting two retained IPv4 addresses
  produces `502` after exactly two dials and no denial record.
- Deadline trace under paused time: resolution consumes seconds 0-6, the first dial starts at
  second 6 and refuses at second 9, the second dial starts at second 9, and the shared deadline
  expires at second 10 with `504`. A pending resolver independently expires at the same single
  ten-second boundary.
- Cancellation trace: cancellation while resolution is pending returns the typed cancellation
  result and observes the resolver future's drop guard; cancellation during a pending dial does
  the same for the dial future. Neither waits for the thirty-second test budget.
- Reuse trace: a live unscoped IPv6 HTTP exchange is returned to the pool; a subsequent scoped
  request for the same address and port receives one audited `403`, with zero resolver calls, no
  second dial, and no second request bytes on the pooled stream.

## Validation evidence

Final post-fix validation on 2026-09-20:

- `cargo fmt --all -- --check` — passed.
- `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings` — passed.
- `cargo test -p vhrn-proxy --locked connect::public` — passed all 21 focused tests.
- `cargo test -p vhrn-proxy --locked --test proxy_process` — passed all 22 process tests.
- `cargo test -p vhrn-proxy --locked` — passed all 132 unit tests, all 22 process tests, and doc
  tests.
- `git diff --check` — passed in the independent review.

Earlier validation/review invocations each exposed one transient timeout in an unchanged broker
lifecycle process test; each isolated rerun passed, and both final required process invocations
above passed all 22 tests. These were treated as existing timing-test flakiness rather than a
confirmed Phase 7 defect.

## Independent security review

The required `rust_reviewer` performed the initial review and two rereviews without consulting Go
source, tests, module files, history, diffs, or Go-derived explanations.

- Initial findings were that mapped/scoped IPv6 literals stopped at malformed-target `400` instead
  of the audited address-boundary `403`, report-mode address denial could write the same audit
  twice, and the necessary target/router/response seams were outside the original edit list. The
  implementation now preserves those literals to the boundary, tracks a prior report-mode audit,
  and this document records the user's explicit 2026-09-20 scope authorization.
- The first rereview found that a scoped authority could reuse a pooled connection keyed only by
  its unscoped IP and port. The pool key now retains scope, and the live pooled-stream regression
  proves the bypass closed without network or audit ambiguity.
- The final rereview reported no actionable findings or material test gaps, reconfirmed the full
  registry audit and typed transport behavior, and explicitly judged Phase 7 completion and Phase
  8 readiness justified after recording this evidence and applying the ledger transition.
