# Phase 2: Split CLI and proxy CI ownership

## Execution contract

This file is the authoritative implementation specification for Phase 2. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 2 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../AGENTS.md).
4. Implement only this phase and stay within its editable paths and responsibility boundary.
5. Record detailed implementation, validation, and review evidence in this file.
6. After every completion requirement, independent review, and rereview are satisfied, apply
   the master plan's status-transition rules. If anything remains unresolved, leave the next phase
   `Blocked`.

## Objective

Replace the language-based `rust` CI lane with component-specific lanes before behavioral proxy
work begins. During migration the CLI, isolated Rust candidate, and shipping Go proxy must be
independently selectable; cutover must have an explicit, mechanical lane rename/removal rather than
another CI redesign.

## Inputs and editable paths

Read:

- `AGENTS.md`, [`plan.md`](plan.md), the root Cargo workspace configuration, and package manifests;
- `.github/workflows/_test.yml` and `.github/workflows/ci.yml`;
- the path/build interfaces in `.github/workflows/_build-images.yml` only to preserve when image
  validation should run.

Edit only:

- `.github/workflows/_test.yml`;
- `.github/workflows/ci.yml`;
- detailed implementation, validation, and review evidence in this file;
- [`plan.md`](plan.md) only for status transitions permitted by the master plan.

Do not edit Rust or Go source/tests, image recipes, other workflows, production selection, tags, or
external services.

## Required behavior

1. Replace the reusable workflow's `run_rust` input and `rust` job with independent `run_cli` and
   `run_proxy_rs` inputs and `cli` and `proxy-rs` jobs. Keep the shipping implementation independent
   as an explicitly named `run_proxy_go` input and `proxy-go` job during migration; do not leave a
   generic language-named job or an ambiguous `proxy` job.
2. The `cli` job runs package-scoped format, strict Clippy, tests, and release build for `vhrn`.
   The `proxy-rs` job runs the same package-scoped checks for `vhrn-proxy`. A failure or skip in one
   component must not cause the other component's job to run or be reported under the same stage.
3. Split `ci.yml` path-filter outputs into `cli`, `proxy_rs`, and `proxy_go`. `cli/**` selects only
   CLI; `proxy-rs/**` selects only the Rust candidate; `proxy/**` selects only the shipping Go proxy.
   Root `Cargo.toml` and `Cargo.lock` select both Rust packages because they are shared workspace
   inputs. `Cross.toml` remains CLI-owned. Conservatively select all three code-component lanes for
   `shared/testdata/**`; do not inspect Go tests to narrow that trigger. Encode these paths
   explicitly rather than relying on a language bucket.
4. Preserve production image validation during migration: shipping `proxy/**` changes continue to
   select the production proxy image build. Candidate `proxy-rs/**` changes do not silently build
   or publish the shipping Go image; candidate-image CI remains owned by the later qualification
   phase.
5. Keep scripts and workflow lanes and the single `ci-gate` behavior unchanged. Update all reusable
   workflow call inputs, `needs`, and green-or-skipped checks so every new component job is accounted
   for and a skipped lane cannot mask a failed lane.
6. Record the cutover transition now: Phase 13 removes `proxy-go`, renames the `proxy-rs` filter,
   input, and job to `proxy`, and changes its path from `proxy-rs/**` to `proxy/**`. The `cli` lane
   remains `cli`; no `rust` lane is reintroduced.

## Interfaces and data flow

- `ci.yml` owns path classification and passes three independent booleans to `_test.yml`:
  `run_cli`, `run_proxy_rs`, and `run_proxy_go`.
- `_test.yml` owns three independently named jobs. Root workspace files fan out to both Rust jobs;
  component files do not.
- The image job continues to consume shipping image/path ownership, not programming-language
  ownership.

## Edge cases and focused checks

- Verify the path matrix for: CLI-only; candidate-only; shipping-proxy-only; root Cargo-only;
  shared-fixture-only; workflow-only; scripts-only; and combined changes.
- Verify a candidate-only PR does not run Go checks or the shipping image build, a shipping-proxy
  PR does not run either Rust package, and root Cargo changes run both Rust jobs.
- Verify job display names and required `needs` contain no generic `rust` stage and distinguish
  `proxy-rs` from `proxy-go`.

## Validation

1. Run `actionlint` over `.github/workflows/**`.
2. Run the repository's shellcheck-enabled workflow validation for inline `run:` scripts.
3. Exercise or statically validate the complete path-filter truth table above.
4. Invoke `_test.yml` with each boolean independently and confirm only the selected job is eligible.

## Evidence required

- Record the before/after input, output, and job-name mapping without inspecting a Go source file.
- Record the path-filter truth table and green-or-skipped gate evaluation.
- Record validation output and independent workflow/scope review. Rereview after findings until no
  finding remains.

## Completion criterion

CI exposes independent `cli`, `proxy-rs`, and `proxy-go` lanes; path filters and package-scoped
commands do not cross-trigger component work except for explicit shared workspace/fixture inputs;
shipping image validation remains attached only to the shipping implementation; the Phase 13
rename/removal is fully specified; workflow validation passes; and rereview is clean.
