# Rust proxy replacement master plan

Replace the shipping Go egress proxy with the isolated Rust candidate while preserving the
consumer-facing contract used by the CLI, packaging, and containerized clients. Compatibility
applies to the frozen consumer contract, not to incidental implementation behavior.

This file is the small shared entrypoint for every numbered phase. It contains only common rules,
security boundaries, document pointers, and the authoritative readiness ledger.

## Shared execution rules

- Start from the assigned phase specification and follow only the pointers it requires.
- Assign each numbered phase to one fresh implementation session and one pull request.
- Execute a numbered phase only when its row in the status table is `Ready`.
- Keep the implementation inside that phase's editable paths and responsibility boundary.
- Include implementation, tests, CI, documentation, and detailed evidence required by a behavior in
  the phase that owns it.
- Give every phase an independent review for security, correctness, regressions, test quality,
  clean-room integrity, and scope. Resolve every finding and obtain rereview before completion.
- Record work owned by another phase without expanding the active phase.
- Stop for user direction when work conflicts with an `AGENTS.md` security invariant or requires
  publishing an image, tagging a release, changing a version, or changing an external service.
- Pre-flight records, completed-phase records, and the coverage ledger are disclosed references.
  Read one only when the active phase specification explicitly requires it.

## Clean-room and compatibility boundaries

- [`docs/proxy/consumer-contract.md`](../../proxy/consumer-contract.md) is the sole normative
  description of externally observable proxy behavior. Its frozen SHA-256 is
  `f3499ff66aa9e15d3f7788153b3f109dc7b020956a6d306eda5e103e806075ed`.
- Rust implementation agents may read the frozen consumer contract, allowed host-side Rust
  interfaces, `AGENTS.md`, and Rust source. They must not inspect current or historical Go source,
  tests, module files, history, diffs, or Go-derived explanations.
- The shipping Go proxy is frozen. No pre-cutover phase modifies it.
- Until cutover, `proxy/` remains the shipping implementation and `proxy-rs/` remains the
  isolated Rust candidate. Candidate work must not change production selection or existing
  immutable image tags.
- The proxy and CLI remain independent crates. Do not share policy, target, or broker protocol
  types between them.
- Keep the Rust source in the idiomatic module-file layout: parent modules use `name.rs`, children
  use `name/child.rs`, and no `mod.rs` files are added.
- At cutover, the qualified Rust tree moves from `proxy-rs/` to `proxy/`; workspace, CI, and
  packaging follow it; and the legacy implementation and toolchain files are removed. The
  `vhrn-proxy` image name, port 8080, scratch runtime, unprivileged identity, CLI release-clock
  tagging, and multi-platform publication interface remain stable.

## Authoritative status and dependency table

This table is the sole source of phase readiness. A fresh session verifies only its own row; it
does not open predecessor or pre-flight documents to reconstruct readiness.

| Work item | Status | Dependency gate | Specification |
| --- | --- | --- | --- |
| Pre-flight 1 | Complete | — | [Consumer contract record](preflight-01-consumer-contract.md) |
| Pre-flight 2 | Complete | Pre-flight 1 | [Phase-planning record](preflight-02-phase-planning.md) |
| Phase 1 | Complete | — | [Candidate scaffold](phase-01-candidate-scaffold.md) |
| Phase 2 | Complete | Both pre-flights and Phase 1 complete | [CLI and proxy CI ownership](phase-02-ci-ownership.md) |
| Phase 3 | Complete | Phase 2 | [Strict live policy](phase-03-policy.md) |
| Phase 4 | Complete | Phase 3 | [Startup and audit health](phase-04-startup-audit.md) |
| Phase 5 | Complete | Phase 4 | [Targets and safe outcomes](phase-05-targets-outcomes.md) |
| Phase 6 | Complete | Phase 5 | [HTTP/1 ingress](phase-06-http-ingress.md) |
| Phase 7 | Complete | Phase 6 | [Public dialing](phase-07-public-dialing.md) |
| Phase 8 | Complete | Phase 7 | [Loopback broker](phase-08-broker.md) |
| Phase 9 | Ready | Phase 8 | [HTTP forwarding](phase-09-forwarding.md) |
| Phase 10 | Blocked | Phase 9 | [CONNECT tunnels](phase-10-connect.md) |
| Phase 11 | Blocked | Phase 10 | [Resources and shutdown](phase-11-resources-shutdown.md) |
| Phase 12 | Blocked | Phase 11 | [Candidate qualification](phase-12-qualification.md) |
| Phase 13 | Blocked | Phase 12; explicit user cutover approval | [Atomic cutover](phase-13-cutover.md) |

## Status transitions

- `Complete` means every implementation requirement, validation, evidence obligation, independent
  review, rereview, and completion criterion is satisfied.
- `Ready` identifies the only unfinished phase authorized to execute. `Blocked` means its
  immediate predecessor is not complete or an additional explicit gate remains unsatisfied.
- After satisfying the full completion bar, the executing session may change only its own row from
  `Ready` to `Complete` and the immediately following phase from `Blocked` to `Ready`.
- If any implementation, validation, evidence, review, rereview, or completion item remains
  unresolved, leave the active row `Ready` and the following row `Blocked`.
- Phase 12 may mark Phase 13 `Ready` only when explicit user approval to perform cutover is already
  recorded. Otherwise Phase 13 remains `Blocked` after Phase 12 completes.
- Phase 13 has no successor. Pre-flight and completed historical rows change only through an
  explicit planning decision.
- A status transition authorizes only the next specification. It does not authorize publishing,
  version changes, release tagging, external-service changes, or work from another phase.

## Disclosed references

- [Pre-flight Phase 1](preflight-01-consumer-contract.md) preserves the consumer-contract work and
  evidence.
- [Pre-flight Phase 2](preflight-02-phase-planning.md) preserves candidate analysis, planning
  rationale, approval, and the historical classification snapshot.
- [Contract coverage](coverage.md) is the active requirement-to-owner ledger. Only Phase 12 is
  required to read and update its qualification-evidence field.
