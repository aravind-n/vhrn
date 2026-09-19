# Phase 3: Make policy files a strict live security boundary

## Execution contract

This file is the authoritative implementation specification for Phase 3. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 3 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../AGENTS.md).
4. Read the frozen [consumer contract](../../proxy/consumer-contract.md).
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
