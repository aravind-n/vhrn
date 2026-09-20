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

## Implementation evidence

Completed implementation on 2026-09-20. The frozen consumer contract SHA-256 was verified as
`f3499ff66aa9e15d3f7788153b3f109dc7b020956a6d306eda5e103e806075ed` before work began.

### Raw targets and authorization

- `domain::target::classify` now accepts a `Method` plus raw request-target bytes and returns the
  disjoint `Direct`, `Asterisk`, `PublicHttp`, `LocalHttp`, `PublicConnect`, `LocalConnect`,
  `HttpsAbsoluteRejected`, or `Malformed` class. Forward targets retain raw path/query bytes,
  canonical authority, effective port, and whether the port was explicit. The current Hyper
  listener is bridged from its preserved URI components without formatting the URI or consulting
  `Host`; Phase 6 owns replacement of that bridge with its raw request-line codec.
- Absolute HTTP parsing requires a nonempty authority, rejects userinfo and fragments, defaults
  only HTTP to port 80, and canonicalizes explicit decimal ports in `1..=65535`. CONNECT accepts
  only authority-form with an explicit valid port; there is no 443 default. Framed CONNECT is
  rejected before policy.
- Request reg-names remove one root dot, lowercase ASCII, and enforce the 63-octet label and
  253-octet total limits. Numeric parsing accepts only canonical dotted-decimal IPv4. Bracketed
  ordinary IPv6 remains public, while scoped and IPv4-mapped literals are malformed.
- Canonical `localhost`, `127/8`, and IPv6 loopback targets become local before any public-policy
  decision. The router takes one live Phase 3 decision, performs the required Phase 4 audit write,
  and only then permits body polling or connector work. Invalid policy is an exact enforced denial;
  report-mode append failure is a typed terminal `503` before network work.

### Safe outcomes and TLS removal

- `ProxyFailure` is the sole typed syntax/policy/transport-to-response renderer. It supplies exact
  public denial, local denial, HTTPS rejection, and report-log failure bodies; generic responses
  remain one line and at most 1024 octets. It sets representation `Content-Length`, uses
  `text/plain; charset=utf-8`, applies required connection-close intent, and suppresses body bytes
  for HEAD while retaining representation headers. Forwarded HEAD responses are likewise stripped
  of body bytes.
- Absolute-form HTTPS becomes `HttpsAbsoluteRejected` before policy, audit, body polling,
  resolution, broker, pool, or dial work and renders exactly `HTTPS requires CONNECT\n` with 400
  and connection close.
- Removed `connect/tls.rs`, public and broker `secure` branches, TLS startup configuration, and the
  `rustls`, `tokio-rustls`, `webpki-roots`, and TLS-test-only `rcgen` dependencies. The connector
  constructor call sites in `lib.rs` were updated only for those signature removals, and target
  uses in `diagnostics.rs` were test-only API adaptations. `cargo tree -p vhrn-proxy --locked`
  contains no TLS client dependency.

### Accepted-target truth table and focused tests

- `accepted_target_truth_table_is_canonical_and_disjoint` covers all eight target classes,
  default and explicit HTTP ports, raw path/query retention, case/root-dot normalization, public
  and local IPv4/IPv6, and HTTPS rejection.
- `connect_requires_one_explicit_canonicalizable_port` covers the accepted explicit-port case and
  missing, empty, zero, overflow, nondecimal, scheme, userinfo, path, query, fragment, unbracketed
  IPv6, missing bracketed-IPv6 port, and multiple-port rejections.
- `dns_lengths_root_dot_and_ascii_alabels_are_enforced`,
  `numeric_hosts_reject_ambiguous_ipv4_mapped_and_scoped_forms`, and
  `local_spellings_normalize_before_public_classification` cover exact DNS boundaries, ASCII
  A-labels, empty/oversized/invalid labels, canonical and noncanonical IPv4, ordinary/mapped/scoped
  IPv6, and canonical local spelling variants.
- `shared_http_outcome_fixture_uses_the_raw_target_classifier` binds the updated HTTP fixture to the
  new classes, including terminal HTTPS and missing-port CONNECT outcomes.

### Authorization ordering and exact response tests

- `malformed_and_https_targets_have_no_authorization_or_network_side_effects` injects policy/audit
  counters plus resolver, dial, and broker probes. It proves malformed, HTTPS, and framed CONNECT
  targets do not read policy, write audit records, poll bodies, touch the public pool, resolve,
  dial, or send a broker frame.
- `denied_public_request_does_not_poll_its_body`,
  `invalid_public_policy_is_an_exact_enforced_denial_before_body_polling`, and
  `local_denial_is_exact_and_does_not_poll_the_body` prove authorization precedes body polling,
  `Host` cannot repair the route, invalid input is fail-closed, public mode cannot grant a local
  capability, and denial bytes name only the canonical target.
- `report_mode_audit_write_failure_is_503_and_never_dials` and
  `report_mode_connect_audit_write_failure_is_503_and_never_resolves` prove the exact terminal
  audit-failure outcome and absence of public network work.
- `failures_have_exact_status_headers_and_bytes_including_head` asserts status, content type,
  representation length, close intent, and exact bytes for HTTPS, public/local denial,
  report-log failure, and generic errors, then repeats every case as HEAD with zero body bytes.
  `dynamic_failure_content_cannot_escape_the_one_line_bound` verifies the renderer's hard bound,
  and `head_origin_preserves_representation_headers_without_body_bytes` covers successful HEAD.

### Phase 5 coverage mapping

- Coverage row “Raw request-target classification, DNS/IP normalization, explicit loopback
  classification, mandatory CONNECT port, absolute-form HTTPS rejection, and removal of
  proxy-originated TLS” maps to the target truth-table tests above,
  `connect_uses_only_an_explicit_authority_port`, the HTTPS side-effect test, the source search, and
  the dependency-tree result.
- Coverage row “Policy authorization ordering and safe response taxonomy for syntax and policy
  outcomes, including exact denial and HTTPS bodies and HEAD suppression” maps to the router
  ordering tests and response byte/header tests above.

## Validation evidence

The final required sequence passed on 2026-09-20:

1. `cargo fmt --all -- --check` — passed.
2. `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings` — passed.
3. `cargo test -p vhrn-proxy --locked domain::target` — passed, 10 focused tests.
4. `cargo test -p vhrn-proxy --locked server::router` — passed, 12 focused tests.
5. `cargo tree -p vhrn-proxy --locked` — passed; the candidate tree contains no Rustls,
   Tokio-Rustls, WebPKI roots, or certificate-generation dependency.
6. `cargo test -p vhrn-proxy --locked` — passed with 101 library tests and 19 process tests.
   Loopback tests required the approved unsandboxed run. An earlier full run had one readiness
   timing test expire; that test passed immediately in isolation, and the complete final rerun
   passed.

`git diff --check` also passed. Searches found no TLS module, client, handshake, `secure` connector
branch, or default CONNECT port in `proxy-rs`; Rustls remains elsewhere in the workspace lock only
through the CLI's unrelated `ureq` dependency. Neither the implementation session nor its searches
inspected current or historical Go source, Go tests, Go module files, Go history/diffs, or
Go-derived explanations.

## Independent review evidence

A dedicated Rust reviewer completed an independent read-only review and a post-fix rereview. The
initial review found two target-parser defects:

- numeric IPv4 spellings with a trailing dot were accepted because root-dot removal preceded
  numeric parsing; `parse_authority` now attempts canonical IPv4 parsing before reg-name root-dot
  normalization and rejects numeric-looking noncanonical forms. HTTP and CONNECT regression cases
  for public and loopback trailing-dot IPv4 spellings are in
  `numeric_hosts_reject_ambiguous_ipv4_mapped_and_scoped_forms`;
- raw `[` and `]` were accepted in path/query components even though brackets are reserved for an
  IPv6 authority; `valid_path_and_query` now rejects them, with direct-form and absolute-form
  regressions in `malformed_absolute_forms_do_not_fall_back_to_direct_or_connect`.

After both fixes, the complete required validation sequence was rerun and passed. The same reviewer
then reinspected the implementation and tests, independently reran formatting, lint, focused and
full tests, the dependency tree, `git diff --check`, and targeted source searches, and reported no
actionable findings or material test gaps. The rereview specifically confirmed:

- no TLS configuration, TLS stream, client handshake, `secure` connector branch, or
  HTTPS-capable proxy route remains, and the proxy dependency tree has no TLS client dependency;
- CONNECT requires an explicit valid port and has no default port. The only remaining `443` in the
  target parser validates an absolute-form HTTPS target immediately before returning the terminal
  `HttpsAbsoluteRejected` class and cannot reach a connector;
- the named tests cover the required target truth table, malformed forms, normalization,
  local/public separation, authorization-before-body ordering, exact outcomes, bounded errors,
  and HEAD suppression;
- changes remain within the Phase 5 editable paths, and no current or historical Go source, tests,
  module files, history, diffs, or Go-derived explanations were inspected.

The reviewer identified only the planned residual boundary: the running router bridges preserved
Hyper URI components into the pure target classifier until Phase 6 installs the raw HTTP/1 ingress
codec. The directly tested raw classifier satisfies Phase 5, so the rereview concluded that Phase 5
meets its completion criterion and Phase 6 may become ready.
