# Phase 2: Split CLI and proxy CI ownership

## Execution contract

This file is the authoritative implementation specification for Phase 2. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 2 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../../AGENTS.md).
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

## Implementation evidence

Phase 2 was `Ready` before implementation began. Changes were limited to the two owned workflow
files and this evidence record; no Go source, tests, or module file was opened. The reusable
workflow now has independent direct input guards and no dependencies between its `cli`, `proxy-rs`,
and `proxy-go` jobs. Nightly and release callers continue to select all jobs through the inputs'
`true` defaults.

### Interface mapping

| Interface | Before | After |
| --- | --- | --- |
| Rust path-filter output | `rust` | `cli`, `proxy_rs` |
| Shipping proxy path-filter output | `proxy` | `proxy_go` |
| Rust reusable input | `run_rust` | `run_cli`, `run_proxy_rs` |
| Shipping proxy reusable input | `run_proxy` | `run_proxy_go` |
| Rust job ID | `rust` | `cli`, `proxy-rs` |
| Shipping proxy job ID | `proxy` | `proxy-go` |
| Script filter, input, and job | `scripts`, `run_scripts`, `scripts` | unchanged |

The `cli` job runs `cargo fmt`, strict Clippy, tests, and a release build with package `vhrn`
selected on every command. The `proxy-rs` job runs the same four checks with package
`vhrn-proxy` selected. The shipping Go job body is unchanged; only its input and job were given the
explicit `proxy-go` identity. No generic `rust` or ambiguous `proxy` job remains.

### Path-filter and eligibility truth tables

The following table was evaluated by parsing the actual embedded `dorny/paths-filter` YAML. The
image-job column evaluates its real condition, `images || proxy_go`.

| Changed-path case | `cli` | `proxy_rs` | `proxy_go` | `images` | `scripts` | `workflows` | Image job |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `cli/**` only | true | false | false | false | false | false | skipped |
| `proxy-rs/**` only | false | true | false | false | false | false | skipped |
| `proxy/**` only | false | false | true | false | false | false | eligible |
| `Cargo.toml` or `Cargo.lock` only | true | true | false | false | false | false | skipped |
| `Cross.toml` only | true | false | false | false | false | false | skipped |
| `shared/testdata/**` only | true | true | true | false | false | false | eligible |
| `.github/workflows/**` only | false | false | false | false | false | true | skipped |
| `pages/**` only | false | false | false | false | true | false | skipped |
| CLI + candidate + shipping proxy + image + script + workflow | true | true | true | true | true | true | eligible |

For each reusable input, `act` 0.2.89 invoked the reusable workflow's `workflow_call` event in
dry-run mode with that input `true` and every other input `false`. Its GitHub-expression evaluator
reported the selected job eligible and each other job skipped:

| Selected input | Eligible job |
| --- | --- |
| `run_cli` | `cli` |
| `run_proxy_rs` | `proxy-rs` |
| `run_proxy_go` | `proxy-go` |
| `run_scripts` | `scripts` |

The called `test` workflow remains one of the four `ci-gate` needs, alongside `changes`, `images`,
and `actionlint`; its result aggregates all independently guarded reusable jobs. Exhaustive
evaluation of `success`, `skipped`, `failure`, and `cancelled` across those four needs confirmed the
gate passes only when every result is `success` or `skipped`. Thus a skipped component lane cannot
mask another component's failure.

### Phase 13 cutover transition

Phase 13 will remove the `proxy_go` filter, `run_proxy_go` input, and `proxy-go` job; rename the
`proxy_rs` filter, `run_proxy_rs` input, and `proxy-rs` job to `proxy`; and change that lane's path
from `proxy-rs/**` to `proxy/**`. The `cli` filter, input, job, and path stay `cli`; no `rust` lane is
reintroduced.

## Validation evidence

- `actionlint` 1.7.12 linted all nine workflow files with ShellCheck 0.11.0 passed explicitly via
  `-shellcheck`: zero parse errors and zero total errors. Debug output confirmed inline scripts were
  sent to ShellCheck and the ShellCheck rule found zero errors.
- A static parser exercised CLI-only, candidate-only, shipping-proxy-only, both root Cargo files,
  `Cross.toml`, shared-fixture-only, workflow-only, script-only, and combined changes against the
  checked-in filter definition. Every row matched the table above. It also verified shipping-image
  selection, reusable-call input completeness, and the exhaustive green-or-skipped gate model.
- Four independent `act` 0.2.89 `workflow_call` dry runs exercised `run_cli`, `run_proxy_rs`,
  `run_proxy_go`, and `run_scripts`. In each run the selected input evaluated to `true`; all three
  other inputs evaluated to `false` and their jobs were reported skipped.
- `cargo fmt --package vhrn -- --check` and
  `cargo clippy --package vhrn --all-targets --locked -- -D warnings` passed. The package-scoped CLI
  tests passed all 183 tests and `cargo build --release --locked -p vhrn` passed. The first test run
  was denied local socket binds by the execution sandbox; the unrestricted rerun passed.
- `cargo fmt --package vhrn-proxy -- --check` and
  `cargo clippy --package vhrn-proxy --all-targets --locked -- -D warnings` passed. The
  package-scoped candidate tests passed 75 unit tests and 12 process tests, and
  `cargo build --release --locked -p vhrn-proxy` passed. The first test run had the same sandbox
  socket-bind denial; the unrestricted rerun passed.
- `git diff --check` passed. Searches over the two workflow files found no old `run_rust`, `rust`
  output/job, or ambiguous `proxy` job reference. The shipping image build still uses `proxy/` and
  is selected by `proxy_go`, never by `proxy_rs`.

## Review evidence

Independent read-only review covered workflow correctness, regressions, gate semantics, path
selection, shipping-image ownership, clean-room integrity, and editable-path scope without opening
Go source, tests, or module files. The workflow implementation was clean. The review's sole
completion-blocking finding was that this implementation and validation record had not yet been
written; this section and the evidence above resolve that finding. Independent rereview found the
resolution complete and reported no remaining correctness, regression, security, gate,
path-filter, validation-coverage, clean-room, or scope finding.
