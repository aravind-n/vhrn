# Phase 3: Make policy files a strict live security boundary

## Execution contract

This file is the authoritative implementation specification for Phase 3. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 3 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../../AGENTS.md).
4. Read the frozen [consumer contract](../../../proxy/consumer-contract.md).
5. Implement only this phase and stay within its editable paths and responsibility boundary.
6. Record detailed implementation, validation, and review evidence in this file.
7. After every completion requirement, independent review, and rereview are satisfied, apply
   the master plan's status-transition rules. If anything remains unresolved, leave the next phase
   `Blocked`.

## Objective

Replace the candidate's permissive policy parsing with one bounded, race-aware policy reader that
implements the frozen public, mode, and local file contracts. Every request decision receives a
fresh snapshot; corruption fails that decision closed without killing the process.

## Inputs and editable paths

Read:

- `AGENTS.md`, the frozen contract's Security boundary and Policy contract sections, and [`plan.md`](plan.md);
- `proxy-rs/src/config.rs`, `proxy-rs/src/domain/policy.rs`, and
  `proxy-rs/src/domain/target.rs` only for the policy value types;
- the candidate policy tests and fixtures plus `shared/testdata/loopback-authorities.tsv`;
- the serialization-facing portions of `cli/src/net.rs` only to preserve the already documented
  file interface. Do not share compiled policy types between crates.

Edit only:

- `proxy-rs/src/config.rs` for typed policy path containers needed by the reader;
- `proxy-rs/src/domain/policy.rs` and new children under `proxy-rs/src/domain/policy/`;
- policy-focused candidate fixtures under `proxy-rs/testdata/` and language-neutral contract
  fixtures under `shared/testdata/`;
- `proxy-rs/tests/proxy_process.rs` only for Phase 3 black-box policy boundary tests, as explicitly
  authorized by the user after independent review identified the original scope conflict;
- test-only fixture references in `cli/src/net.rs` if a language-neutral fixture moves to
  `shared/testdata/`;
- detailed implementation, validation, and review evidence in this file;
- [`plan.md`](plan.md) only for status transitions permitted by the master plan.

Do not edit request routing, connectors, lifecycle code, image selection, workflows, or `proxy/`.

## Required behavior

1. Introduce one bounded file-opening primitive used by public, mode, and local policy reads. Open
   the selected path for each decision, inspect metadata from the open handle, require a regular
   file, reject a reported or observed size above 1 MiB, read at most 1 MiB plus one sentinel byte,
   and require UTF-8. Do not cache a file descriptor or successful snapshot across decisions.
2. Parse public files exactly as stored. The empty file and an optional final LF are valid. Every
   nonempty line must already be lowercase canonical ASCII with no whitespace, wildcard, leading
   or trailing dot, or empty label; only ASCII letters, digits, `_`, `-`, and `.` are allowed, and
   at least one alphanumeric byte is required. Do not trim or normalize persisted lines. Identical
   canonical duplicates are set duplicates and have no additional effect.
3. Parse the mode as exactly `enforce`, `report`, or `open`, optionally followed by one LF. Reject
   CRLF, surrounding whitespace, extra lines, missing content, and unknown values.
4. Parse local files as canonical storage, not user input. Each nonempty line must round-trip
   byte-for-byte through `LoopbackAuthority`: lowercase `localhost`, canonical dotted decimal,
   `[::1]`, and a canonical nonzero decimal port. Reject blank interior lines, whitespace,
   noncanonical duplicates, leading-zero ports, alternate IPv6 spellings, and all malformed or
   non-loopback authorities. Union the three valid layers.
5. Keep strict loaders distinct from live decisions. Startup callers receive an error for any bad
   required input. Request-time public callers convert any public-layer or mode failure into an
   empty `enforce` snapshot; local callers convert any local-layer failure into an empty grant set.
   A malformed mode must not retain allowlist matches.
6. Preserve additive public matching: exact IPv4 entries; exact DNS names and dot-boundary
   subdomains; IPv6 never matches an enforce-mode file entry. `report` records only unmatched
   syntactically valid public hosts and allows them; `open` allows them without a record. Neither
   mode changes local policy.
7. Make the decision API return an explicit effective mode and failure marker so later routing and
   diagnostics can produce the required denial without exposing an internal read error or path.
8. Keep atomic host replacement semantics: a decision uses the independently opened values it
   read, later requests see later replacements, and a failed decision does not poison the process
   or a later repaired decision.

## Interfaces and data flow

- `Config` continues to own only validated paths. A strict `PolicyReader` (or equivalently named
  value) opens them; `load_public_strict`/`load_local_strict` serve startup, while
  `decide_public_live`/`decide_local_live` apply the fail-closed request mapping.
- The public decision contains `allowed`, `record_denial`, `effective_mode`, and whether the
  result arose from invalid input. The local decision contains `allowed` and the same invalid-input
  signal. Neither exposes paths or parser text to the response layer.
- Keep request-host normalization out of the stored-file parser. Phase 5 owns request targets and
  passes a typed, already validated host or canonical local authority into this phase's decision
  functions.

## Edge cases and focused tests

- Boundary-test 1 MiB exactly and 1 MiB plus one byte, a directory, a missing file, invalid UTF-8,
  replacement after failure, and replacement between two persistent-client requests.
- Cover no-final-LF, one final LF, CRLF, blank first/interior lines, uppercase and trimmed public
  lines, `*.` and dotted forms, duplicate canonical lines, invalid labels, IPv4 leading zeroes,
  and IPv6 enforce/report/open behavior.
- Separate the CLI's user-input normalization corpus from the proxy's strict persisted-file corpus;
  keep cross-component cases under `shared/testdata/`.
- Prove an invalid public layer or mode denies a normally matched host in `report` and `open`, and
  a bad local layer empties the union even when another layer contains a grant.

## Validation

Run, in order:

1. `cargo fmt --all -- --check`
2. `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings`
3. `cargo test -p vhrn-proxy --locked domain::policy`
4. `cargo test -p vhrn-proxy --locked`
5. If `cli/src/net.rs` changed, `cargo test -p vhrn --locked net::tests`

## Evidence required

- Record the strict/live API and every fixture added or moved.
- Map both Phase 3 matrix rows to named unit and process tests, including all size and repair cases.
- Record command results and an independent security/correctness/test review. Rereview after every
  finding until no finding remains.
- Attest that the reviewer did not inspect Go material and that no behavior outside Phase 3's
  editable paths was changed.

## Completion criterion

Every public, mode, and local policy file is bounded and strictly validated; startup can demand a
valid snapshot; each request reopens policy and fails closed without terminating the process;
repair is live; all focused and workspace candidate tests pass; and independent rereview has no
remaining finding.

## Implementation evidence

### Strict and live API

- `PolicyReader::load_public_strict` opens every selected public layer and the mode, validates the
  complete snapshot, and returns an error with startup-useful context for any missing, unreadable,
  non-regular, oversized, non-UTF-8, or malformed input.
- `PolicyReader::load_local_strict` applies the same strict behavior to the three local layers and
  returns their union only when every layer is valid.
- `PolicyReader::decide_public_live` maps every strict-load failure to an empty `enforce` snapshot.
  Its `PublicDecision` reports `allowed`, `record_denial`, `effective_mode`, and `invalid_input`.
  The compatibility adapter used by the current router returns only a generic warning and never a
  parser error or policy path.
- `PolicyReader::decide_local_live` maps every strict-load failure to an empty grant set. Its
  `LocalDecision` reports `allowed` and `invalid_input`; the current router adapter preserves the
  required denial behavior without exposing strict-loader diagnostics.
- All three file kinds use `read_bounded_utf8`. It opens each path anew with nonblocking semantics,
  obtains metadata from that open handle, requires a regular file, rejects a reported size over 1
  MiB, reads through a 1 MiB-plus-one-byte sentinel limit, rejects an observed size over 1 MiB, and
  then requires UTF-8. No descriptor or successful snapshot is retained between decisions.
- Public parsing no longer trims, lowercases, removes dots, or strips wildcards. Stored entries must
  already satisfy the frozen lowercase ASCII grammar. Exact canonical duplicates collapse in the
  set. Canonical IPv4 entries match only that IPv4; dotted text with leading zeroes remains valid
  policy-domain text under the documented broad storage grammar but does not grant an IPv4 literal.
- Mode parsing accepts only `enforce`, `report`, or `open`, each with zero or one final LF. Local
  parsing accepts a line only when parsing and formatting `LoopbackAuthority` reproduces the exact
  stored bytes, so CLI user-input conveniences cannot cross the persisted-file boundary.

### Fixture record

- Moved `proxy-rs/testdata/domain-policy.tsv` to
  `shared/testdata/domain-normalization.tsv` and changed the CLI's test-only reference. This corpus
  continues to describe cross-component user-input normalization.
- Added `proxy-rs/testdata/public-policy-storage.tsv` for strict persisted public entries.
- Added `proxy-rs/testdata/local-policy-storage.tsv` for strict persisted local authorities.
- Updated `proxy-rs/testdata/proxy-modes.tsv` to distinguish invalid mode/input states and assert the
  explicit fail-closed marker.
- Retained `shared/testdata/loopback-authorities.tsv` as the cross-component user-input and
  canonicalization corpus; the proxy policy test proves only its canonical output is valid storage.

## Test and matrix evidence

### Phase 3 row: public policy files and live mode

- `strict_public_storage_corpus_and_line_framing` covers empty input, optional final LF, CRLF,
  blank first/interior lines, uppercase, whitespace, wildcard and dotted forms, invalid labels, and
  the strict public fixture.
- `mode_accepts_only_exact_storage` and `mode_corpus_maps_invalid_input_to_empty_enforce` cover exact
  mode bytes, missing files, malformed layers, matched and unmatched hosts, effective mode, and the
  invalid-input marker.
- `public_layers_are_additive_and_matching_is_exact`,
  `ipv4_leading_zeroes_remain_policy_text_but_never_grant_an_ip`, and
  `ipv6_is_unmatched_in_enforce_and_mode_controls_only_public_policy` cover duplicate/additive
  layers, exact IPv4, DNS dot-boundary matching, leading-zero dotted text, and IPv6 behavior in all
  modes.
- `invalid_public_layer_or_mode_denies_a_match_in_report_and_open` proves that a normally matched
  host is denied as empty `enforce` policy when either required input is invalid.
- `bounded_open_accepts_one_mib_and_rejects_one_byte_more`,
  `bounded_open_rejects_directory_missing_and_invalid_utf8`, and
  `bounded_open_rejects_fifo_without_waiting_for_a_writer` cover the exact 1 MiB boundary, a
  reported 1 MiB-plus-one-byte file, missing and non-regular inputs, UTF-8, and nonblocking
  rejection of a FIFO with no writer.
- `same_reader_observes_atomic_repair_between_persistent_requests` proves fresh opens across
  repeated direct live-decision calls with failed, atomically repaired, and atomically revoked
  policy files.
- Process test `policy_modes_and_live_replacement_are_observed_per_request` proves request-boundary
  mode and policy replacement in the candidate process.
- Process test `persistent_client_reopens_bounded_policy_and_observes_atomic_repair` keeps one TCP
  client connected while atomically replacing the public layer. It proves that an exact 1 MiB
  valid policy retains its matching grant, 1 MiB plus one byte fails closed, repair is live without
  restarting the process or client, and a later replacement revokes the grant on that same client.

### Phase 3 row: local policy files

- `strict_local_storage_is_distinct_from_user_input_normalization` covers no-final-LF/final-LF,
  CRLF, blank first/interior lines, exact canonical duplicates, leading-zero ports and IPv4,
  uppercase `localhost`, alternate IPv6 spellings, malformed/non-loopback authorities, and the
  shared normalization corpus's canonical outputs.
- `bad_local_layer_empties_the_union_and_repair_is_live` proves that one invalid layer empties all
  three layers even when another contains the requested grant, and that an atomic repair is live.
- `strict_loaders_return_contextual_errors` proves startup-oriented public and local loads retain
  useful internal layer/line error context.
- Process test `local_startup_exchange_and_partial_configuration_are_process_checked` covers valid
  broker readiness and rejection of partial local environment configuration;
  `local_connect_preserves_buffered_bytes_and_survives_revocation` covers request/process behavior
  around local-policy revocation. Strict-loader startup-oriented errors are covered by the unit
  test immediately above.

## Validation evidence

Final required sequence after the review fix:

1. `cargo fmt --all -- --check` — passed.
2. `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings` — passed.
3. `cargo test -p vhrn-proxy --locked domain::policy` — passed, 14 policy tests.
4. `cargo test -p vhrn-proxy --locked` — passed, 83 library tests and 13 process tests. The full
   suite requires loopback socket permission; the sandboxed attempt failed only those binds, and
   the approved rerun passed.
5. `cargo test -p vhrn --locked net::tests` — passed, 27 tests after the shared fixture move.

`git diff --check` also passed. During implementation, `cargo fmt --check` identified one formatting
change, clippy identified narrowing size casts, and the first full compile identified that the
strict stored-file parser must remain separate from the preexisting permissive `DomainPattern`
constructor. Each was corrected before the final sequence above.

## Independent review evidence

The independent review covered security, correctness, regressions, test quality, clean-room
integrity, and scope. The reviewer explicitly attested that they did not inspect current or
historical Go source, Go tests, `go.mod`, Go history/diffs, or Go-derived explanations.

Findings and dispositions:

1. A FIFO could block in `open` before handle metadata rejected it. Resolved by opening with
   platform-correct `O_NONBLOCK` and adding
   `bounded_open_rejects_fifo_without_waiting_for_a_writer`; the full required validation sequence
   passed afterward. Independent rereview confirmed the finding is resolved and found no new
   correctness, security, or regression issue in the fix.
2. The original editable-path list excluded `proxy-rs/tests/proxy_process.rs`, preventing the
   required persistent-client and process-level size/repair evidence. The user explicitly
   authorized adding that path to Phase 3. Resolved by recording the scope correction and adding
   `persistent_client_reopens_bounded_policy_and_observes_atomic_repair`; the full required
   validation sequence passed afterward. Independent rereview confirmed the same persistent TCP
   stream observes both size boundaries, atomic repair, and revocation without response-boundary
   ambiguity; no new finding was identified.
3. Detailed evidence was initially absent. This section resolved the documentation finding, and
   independent evidence rereview confirmed the implementation, mappings, validation counts, and
   review dispositions are accurate.

No Go material was inspected by the implementation session or reviewer. No production behavior
outside Phase 3's editable paths changed; the CLI and process-test changes are permitted test-only
changes. Independent rereview has no remaining finding, every completion requirement is met, and
the permitted master-plan transition was applied.

### Supplemental Rust review

At the user's request, the completed change received an additional read-only audit using the
dedicated `rust_reviewer` role. It reported no actionable finding and no material test gap. The
reviewer independently:

- verified the frozen consumer-contract SHA-256;
- traced the changed Rust policy API through the existing router, startup boundary, target types,
  fixtures, and black-box process harness;
- confirmed every changed and untracked file is within the user-authorized Phase 3 scope;
- reran formatting, clippy, the 14 focused policy tests, all 83 library and 13 process tests, the 27
  CLI net tests, and `git diff --check` successfully; and
- attested that no current or historical Go source, tests, module files, history/diffs, or
  Go-derived implementation explanations were inspected.

The review ran on `aarch64-apple-darwin`. Linux behavior was reviewed statically because no Linux
Rust target is installed on this host; the reviewer confirmed the Linux `O_NONBLOCK` value is
correct for the supported `linux/amd64` and `linux/arm64` targets and identified no defect from
that residual validation limitation.
