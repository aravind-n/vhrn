# Phase 1: Build the isolated Rust proxy candidate

## Record contract

This file is the authoritative record of completed Phase 1 and the boundary later phases must
preserve. It is not an active implementation assignment. Read [`plan.md`](plan.md) and repository
[`AGENTS.md`](../../../../AGENTS.md) before changing any delivered Phase 1 boundary in a later phase.

## Objective

Establish a non-shipping Rust proxy candidate and the host integration needed to exercise it without
changing the production proxy selection.

## Delivered scope

- A root Cargo workspace containing the existing `vhrn` package and `vhrn-proxy` under
  `proxy-rs/`, with edition 2024 and workspace lint policy.
- A thin proxy binary over a library organized as configuration, typed domains and policy,
  diagnostics, concrete public and broker connectors, HTTP server ownership, relay lifecycle, and
  shutdown.
- Public and loopback HTTP and CONNECT routing, single-resolution public dialing, TLS verification,
  authenticated broker framing, bounded pools and bodies, live policy reads, denial records, and
  supervised shutdown.
- Language-neutral contract fixtures plus unit and process tests for the candidate.
- A scratch candidate image and candidate-only Makefile under `proxy-rs/`.
- CLI broker, policy, and lifecycle integration needed to launch the candidate contract.
- Workspace CI coverage that builds and tests the candidate while the Go image remains the shipping
  target.

## Preserved boundaries

- `proxy/` remains the production image source.
- `proxy-rs/` is not selected by install, release, or image publication workflows.
- Public dialing and brokered loopback routing use separate typed connectors.
- The module layout uses `name.rs` plus `name/child.rs`; no `mod.rs` exists.

## Completion criterion

The candidate and CLI pass formatting, strict Clippy, unit tests, proxy process tests, and locked
release builds; the candidate image recipe is isolated from production packaging; and the tracked
tree contains the complete candidate described above.
