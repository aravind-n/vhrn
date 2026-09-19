# Rust proxy contract coverage ledger

This is the active ownership ledger for the frozen
[consumer contract](../../proxy/consumer-contract.md). Each requirement group has exactly one
owning phase. Ownership changes require an explicit planning decision; implementation evidence
belongs in the owning phase file.

Only [Phase 12](phase-12-qualification.md) is required to read this ledger during normal numbered
phase execution. Phase 12 records named qualification evidence in the optional final column; other
phase agents rely on their own specifications.

| Frozen contract requirement group | Sole owner | Qualification evidence |
| --- | --- | --- |
| Component-specific CI ownership for the CLI, isolated Rust candidate, and shipping Go proxy during migration, with an explicit cutover rename/removal path | Phase 2 | — |
| Host-owned public and local policy, capability separation, strict public file grammar, exact mode grammar, 1 MiB and regular-file bounds, live replacement, and fail-closed decision snapshots | Phase 3 | — |
| Strict three-layer local policy grammar, canonical stored authorities, union semantics, live revocation, and failure to an empty local decision | Phase 3 | — |
| Environment resolution, empty-as-unset behavior, plural/singular precedence, exact local all-or-none group, listener validation, and secret-safe configuration errors | Phase 4 | — |
| Startup ordering and readiness: initial policy checks, append check, bound-but-not-serving listener, token load, broker `READY`, and nonzero failure | Phase 4 | — |
| Denial diagnostics and append-only records, canonical record target, RFC 3339 `Z` timestamp, non-interleaving writes, report-mode append failure, and sticky log health | Phase 4 | — |
| Raw request-target classification, DNS/IP normalization, explicit loopback classification, mandatory CONNECT port, absolute-form HTTPS rejection, and removal of proxy-originated TLS | Phase 5 | — |
| Policy authorization ordering and safe response taxonomy for syntax and policy outcomes, including exact denial and HTTPS bodies and HEAD suppression | Phase 5 | — |
| HTTP/1.0 and HTTP/1.1 ingress, Host rules, request-line/header/framing validation, slow-head timeout, upgrade rejection, and bounded incremental parsing | Phase 6 | — |
| Origin-form endpoints, `OPTIONS *`, endpoint method rules, exact health/status bodies and content types, and policy/log-sensitive health | Phase 6 | — |
| Reviewed 2025-10-09 IANA global-unicast boundary, literals, most-specific exceptions, IPv4-mapped/scoped rejection, and address-safety denial | Phase 7 | — |
| One pinned DNS answer set, 1..=64 all-address validation, one cumulative ten-second DNS/TCP deadline, numeric fallback, no ambient proxy, and typed DNS/dial outcomes | Phase 7 | — |
| Broker token secrecy, exact bounded frames, three-second readiness, fresh broker connections, retained post-`OK` bytes, broker outcome typing, and no direct loopback dial | Phase 8 | — |
| Local connection authorization and recheck flow, distinct `localhost`/IPv4/IPv6 grants, local HTTP connection reuse boundary, and supported engine route assumptions | Phase 8 | — |
| Forwarded request transformation: method and target, regenerated `Host`, hop-by-hop removal, `Via`, no forwarding identity headers, and no forbidden retry/cache behavior | Phase 9 | — |
| Unlimited-size streaming HTTP bodies, 64 KiB directional buffers, chunked bodies and trailers, informational responses, `Expect: 100-continue`, bodyless responses, response-head validation, cancellation, and safe pooling | Phase 9 | — |
| CONNECT success/failure sequencing, exact success head, eager client and broker bytes, opaque relay, 64 KiB buffers, bidirectional half-close, tunnel lifetime, and revocation semantics | Phase 10 | — |
| Process-wide client/upstream limits, at-least-128 client support, bounded task and queue ownership, overload response, and idle-pool accounting | Phase 11 | — |
| Two-stage signal shutdown, shutdown health, cancellation of pending work, five-second drain then forced close, exit status, and idempotent host cleanup | Phase 11 | — |
| Candidate image/process contract, static scratch and numeric user posture, read-only-root operation, exact mounts/env/port, multi-platform equivalence, dependency audit, and black-box contract trace | Phase 12 | — |
| Apple `container` and Docker-through-Colima end-to-end build/run/broker verification without publishing or replacing immutable tags | Phase 12 | — |
| Atomic production selection, `vhrn-proxy` image and CLI release-clock tags, workspace/CI/build context, local image name, removal of the legacy implementation/toolchain, and final operator documentation | Phase 13 | — |

