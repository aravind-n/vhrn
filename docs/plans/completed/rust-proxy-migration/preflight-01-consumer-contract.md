# Pre-flight Phase 1: Build the consumer contract

This file preserves the approved Pre-flight Phase 1 specification and completion evidence. The
[master plan](plan.md) is the sole source of readiness and status; numbered phase agents do not
read this historical record.

## Objective

Create `docs/proxy/consumer-contract.md` from scratch as the complete, implementation-neutral
contract for the replacement proxy.

The contract preserves interfaces used by the CLI, packaging, and containerized clients while
specifying correct behavior from the repository's security model and applicable network and HTTP
standards. It is not a line-by-line or edge-case recreation of the Go proxy.

## Inputs and editable paths

Read:

- `AGENTS.md`;
- the shipping proxy and its packaging only as needed to identify stable consumer dependencies;
- `cli/src/run.rs`, `cli/src/net.rs`, and `cli/src/broker.rs`;
- `README.md`, `docs/sandbox-design.md`, and release/runtime documentation;
- applicable protocol standards needed to define correct behavior.

Edit only:

- `docs/proxy/consumer-contract.md`;
- this pre-flight's evidence in this file.

Do not read `proxy-rs/**` or Rust proxy history. Do not modify the frozen Go proxy.

## Required work

1. Inventory every external dependency: process and image interface, environment variables,
   mounted paths, public and local policy inputs, broker framing, health and status endpoints,
   denial diagnostics, response classes, readiness, shutdown, and release/runtime invariants.
2. Specify the public security boundary, including hostname authorization, DNS handling, and the
   requirement that direct public dials target only globally routable unicast addresses.
3. Specify public and explicitly authorized loopback routing as separate capabilities. Local access
   must use the authenticated broker and must never be granted by public policy or open/report mode.
4. Define correct HTTP and CONNECT behavior, including target forms, TLS handling, hop-by-hop
   headers, streaming, bounded bodies and headers, cancellation, buffered tunnel bytes,
   bidirectional relay, half-close, connection reuse, and live policy replacement.
5. Define fail-closed behavior for malformed or unreadable policy, invalid configuration, DNS and
   dial failures, broker errors, readiness failures, and shutdown.
6. State observable outcomes and bounds without prescribing Go control flow, Rust module structure,
   dependency choices, or other implementation details.
7. Verify every contract statement against a consumer dependency, a repository security invariant,
   or an identified protocol requirement. Record deliberate corrections to current Go behavior as
   normative requirements, without changing the Go implementation.
8. Obtain independent review for completeness, security, internal consistency, and clean-room
   integrity. Resolve findings and freeze the reviewed document in the phase PR.

## Validation

- Check every environment variable, mount, frame, endpoint, diagnostic record, and image/runtime
  invariant against its consumer.
- Trace every `AGENTS.md` proxy security invariant to an explicit contract requirement.
- Review HTTP and network requirements against the cited standards.
- Search the contract for Go/Rust identifiers or implementation prescriptions and remove them
  unless they are part of a stable external interface.
- Record the exact review result and frozen document hash in this phase's evidence.

## Evidence

- Clean-room inputs inspected: `AGENTS.md`; this phase and the plan-wide boundaries; the
  consumer-facing launch, policy, and broker interfaces in `cli/src/run.rs`, `cli/src/net.rs`, and
  `cli/src/broker.rs`; `README.md`, `docs/sandbox-design.md`, `docs/runbooks/release.md`, the base
  entrypoint, proxy image/Makefile surface, image workflows, and the shared loopback fixture. The
  shipping proxy was inspected only for its stable process, endpoint, and diagnostic surface.
  `proxy-rs/**`, Rust proxy history/diffs, and the deleted prior contract were not read; the frozen
  Go proxy was not modified or used as the normative behavior source.
- Authoritative standards checked: RFC 1035 (DNS name limits), RFC 3339 (denial-record timestamp
  format), RFC 3986 (URI syntax), RFC 9110 (HTTP semantics, CONNECT, hop-by-hop fields), RFC 9112
  (HTTP/1.1 target forms, framing, and smuggling defenses), and the IANA IPv4/IPv6 Special-Purpose
  Address Registries (baseline last updated 2025-10-09).
- Validation performed: all proxy environment variables, mounts, five public layers, three local
  layers, exact broker frames, endpoints, denial record, image/runtime, engine, and release-clock
  interfaces were cross-checked against their consumers; every proxy security invariant in
  `AGENTS.md` was traced to an explicit requirement. Requirements were classified as stable
  consumer interfaces, repository security invariants, governing product decisions,
  protocol/registry requirements, or explicit contract-selected decisions with rationale; the
  exact-limit audit and TLS-origination knock-on search were clean.
  `git diff --check` and the equivalent whitespace check for the new untracked document passed.
- Frozen contract SHA-256: `f3499ff66aa9e15d3f7788153b3f109dc7b020956a6d306eda5e103e806075ed`.
- Review result: independent review returned six findings and rereview returned two; all were
  resolved. Final independent rereview approved Phase 1 with no remaining findings and
  independently confirmed the frozen hash.

## Completion criterion

Every consumer-visible interface and security requirement has one unambiguous normative outcome;
correct protocol behavior and failure bounds are specified without cloning Go quirks; deliberate
behavioral corrections are explicit; an independent reviewer has approved the contract; and a Rust
agent can use it without reading any Go material or requesting additional product decisions.
