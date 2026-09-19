# Phase 5: Correct target parsing, authorization ordering, and safe outcomes

## Execution contract

This file is the authoritative implementation specification for Phase 5. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 5 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../AGENTS.md).
4. Read the frozen [consumer contract](../../proxy/consumer-contract.md).
5. Implement only this phase and stay within its editable paths and responsibility boundary.
6. Record detailed implementation, validation, and review evidence in this file.
7. After every completion requirement, independent review, and rereview are satisfied, apply
   the master plan's status-transition rules. If anything remains unresolved, leave the next phase
   `Blocked`.

## Objective

Create one pure raw-target model and one safe observable failure model so routing cannot repair an
ambiguous target, authorize from `Host`, originate TLS, or leak an internal error.

## Inputs and editable paths

Read:

- the contract's Target classification and dialing, Accepted request forms, Response classes, and
  safe-error requirements;
- `proxy-rs/src/domain/target.rs`, `server/router.rs`, `server/response.rs`, `headers.rs`, connector
  call signatures, and target/HTTP fixtures;
- Phase 3 decisions and Phase 4 audit service.

Edit only:

- `proxy-rs/src/domain/target.rs` and new target children;
- `proxy-rs/src/server/router.rs`, `proxy-rs/src/server/response.rs`, and target-related tests;
- connector signatures only as needed to remove the `secure`/TLS-originating branch;
- `proxy-rs/src/connect/tls.rs`, its module declaration, TLS-only tests, `proxy-rs/Cargo.toml`, and
  `Cargo.lock` to remove proxy-originated TLS dependencies;
- target/outcome fixtures under `proxy-rs/testdata/` or `shared/testdata/`;
- detailed implementation, validation, and review evidence in this file;
- [`plan.md`](plan.md) only for status transitions permitted by the master plan.

Do not implement the raw HTTP head codec, public address tables, forwarding semantics, broker
framing changes, lifecycle limits, workflows, or cutover.

## Required behavior

1. Parse from the raw method and request-target bytes. Do not reconstruct a route from `Host` or a
   lossy/stringified parsed URI. Produce disjoint `Direct`, `Asterisk`, `PublicHttp`,
   `LocalHttp`, `PublicConnect`, `LocalConnect`, `HttpsAbsoluteRejected`, and `Malformed` values.
2. Non-CONNECT forwarding accepts only absolute-form `http`, a nonempty authority, no userinfo or
   fragment, default port 80, and an explicit canonicalizable decimal port in `1..=65535`.
   Preserve path/query and whether a port was explicit for later `Host` generation.
3. Absolute-form `https` is a terminal result before policy, audit, DNS, broker, pool, or dial. It
   maps to `400`, `Connection: close`, `Content-Type: text/plain; charset=utf-8`, and
   `HTTPS requires CONNECT\n`. Remove every TLS-originating public and local code path and the
   runtime dependencies used only by those paths.
4. CONNECT accepts authority-form only and requires an explicit port. Reject schemes, userinfo,
   paths, queries, fragments, empty/zero/overflow/nondecimal ports, unbracketed or ambiguous IPv6,
   and missing ports. There is no default 443.
5. Normalize one DNS root dot and ASCII case for a request reg-name; enforce ASCII/A-label input,
   no whitespace/control/empty labels, label length at most 63, and total length at most 253.
   Accept only canonical dotted-decimal IPv4 without leading zeroes. Reject scoped and
   IPv4-mapped IPv6 at target parsing; ordinary IPv6 remains a public literal for later policy and
   address checks.
6. Classify a syntactically canonical explicit `localhost`, `127/8`, or `[::1]` authority as local
   before public authorization. All other valid targets remain public even if DNS later resolves to
   a local/special address. Public policy and mode must never grant a local target.
7. Route policy authorization before body polling or network work. Use Phase 3 live decisions and
   Phase 4 audit results. Emit exact public and local denial bodies; an invalid-policy decision is
   an enforced denial. Report-log failure emits exact `503 proxy temporarily unavailable\n`,
   closes, and performs no network operation.
8. Introduce a bounded response/failure renderer with typed categories rather than error-string
   inspection. Syntax/policy outcomes use their exact bodies; all other generic bodies are one line
   and at most 1024 octets. A HEAD request emits the headers, including the representation length,
   but no body bytes.

## Interfaces and data flow

- The pure target parser accepts `method` plus raw target bytes and returns typed host,
  canonical authority, effective port, explicit-port marker, and raw path/query components needed
  by Phase 9. It never resolves DNS.
- Router authorization consumes the typed target, obtains one live policy decision, performs any
  required audit write, and yields either an `AuthorizedRoute` or a typed `ProxyFailure`.
- Connectors return typed transport outcomes in later phases; the response renderer is the sole
  conversion point to status, headers, safe body, and close/keep-alive intent.

## Edge cases and focused tests

- Table-test every accepted and rejected target form, default/explicit HTTP ports, root dot,
  uppercase names, maximum DNS lengths, canonical/noncanonical IPv4, bracketed IPv6, mapped/scoped
  IPv6, local spelling variants, and all missing/invalid CONNECT ports.
- Use injected policy/audit/resolver/broker spies to prove malformed and absolute-HTTPS requests do
  no policy or network work, and denied requests do not poll their bodies.
- Assert exact bytes and headers for HTTPS, public denial, local denial, report-log failure, generic
  errors, and their HEAD variants. Assert no TLS dependency or connector branch remains.

## Validation

1. `cargo fmt --all -- --check`
2. `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings`
3. `cargo test -p vhrn-proxy --locked domain::target`
4. `cargo test -p vhrn-proxy --locked server::router`
5. `cargo tree -p vhrn-proxy --locked`
6. `cargo test -p vhrn-proxy --locked`

## Evidence required

- Record the accepted-target truth table, authorization ordering tests, exact response byte tests,
  and dependency removal.
- Map both Phase 5 coverage rows to named tests and record independent review/rereview, including a
  specific search for any remaining TLS origination or default CONNECT port.

## Completion criterion

Every raw target has one unambiguous class; local/public capabilities cannot cross; CONNECT always
has an explicit port; absolute HTTPS is rejected exactly before any side effect; no proxy TLS
client remains; policy outcomes are exact and safe; validation passes; and rereview is clean.
