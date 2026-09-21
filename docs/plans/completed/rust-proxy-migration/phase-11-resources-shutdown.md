# Phase 11: Bound process resources and supervise shutdown

## Execution contract

This file is the authoritative implementation specification for Phase 11. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 11 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../../AGENTS.md).
4. Read the frozen [consumer contract](../../../proxy/consumer-contract.md).
5. Implement only this phase and stay within its editable paths and responsibility boundary.
6. Record detailed implementation, validation, and review evidence in this file.
7. After every completion requirement, independent review, and rereview are satisfied, apply
   the master plan's status-transition rules. If anything remains unresolved, leave the next phase
   `Blocked`.

## Objective

Put every client, upstream socket, pool entry, and spawned task under a process-wide owner, then
implement a two-stage five-second graceful shutdown with correct health and exit behavior.

## Inputs and editable paths

Read:

- the contract's Resource bounds, cancellation, and shutdown section and all earlier typed
  connector/connection ownership APIs;
- `proxy-rs/src/main.rs`, `lib.rs`, `shutdown.rs`, `server/listener.rs`, `server/router.rs`,
  `connect/pool.rs`, public/broker connectors, and lifecycle tests.

Edit only:

- the listed candidate lifecycle, listener, pool, router, and connector ownership files;
- resource/shutdown fixtures and focused unit/process tests;
- detailed implementation, validation, and review evidence in this file;
- [`plan.md`](plan.md) only for status transitions permitted by the master plan.

Do not alter protocol semantics owned by earlier phases, candidate packaging/workflows, host CLI,
or production selection.

## Required behavior

1. Enforce at most 256 accepted client connections and 256 simultaneous upstream connections,
   counting active, tunneled, handshaking, and idle pooled upstream sockets. The implementation must
   demonstrate at least 128 simultaneous established clients. A shared upstream permit is acquired
   before opening a public or broker socket and remains held while pooled.
2. Reject excess work promptly with `503` and close when a response can still be safely written;
   otherwise close. Do not silently queue admission, DNS/dial jobs, origin work, or tunnel tasks
   beyond fixed bounds.
3. Bound all per-connection queues, body/tunnel buffers, DNS answers, diagnostics, join sets, and
   concurrent task creation. Ensure pool caps and eviction cannot exceed the global upstream cap or
   leak a driver/task after socket drop.
4. Split shutdown into `draining` and `forced`. SIGTERM/SIGINT or requested normal shutdown marks
   health unhealthy, stops accepting, rejects new/racing work, cancels pending DNS, dial, broker
   handshakes, and not-yet-committed requests, and closes all idle pooled connections.
5. Allow already active HTTP streams and established tunnels up to five seconds to finish without
   injecting cancellation into their data paths. At the deadline, close every remaining tracked
   client/upstream socket, abort and observe every remaining task, and exit.
6. Normal requested shutdown exits zero. Startup failure, listener failure, or unexpected
   supervisor/task failure initiates cleanup and exits nonzero. Task panics and join failures may
   not be logged and ignored indefinitely.
7. Make shutdown and cleanup idempotent and independent of policy/log mounts still existing. A
   second signal or host stop must not double-close unsafely or hang. Preserve the documented
   unhandleable SIGKILL caveat.
8. Propagate downstream disconnect and draining cancellation to pending resolver, connector,
   broker, and body operations; ensure no background future retains sockets after its owner ends.

## Interfaces and data flow

- A process `Supervisor` owns listener, health state, client and upstream permit pools, idle pools,
  socket registry, and task sets. Connections receive a drain token and a separate forced-close
  token; only pending work selects the drain token, while active streams select forced close.
- Each socket registration and permit is RAII-owned by the corresponding stream/pool entry. The
  supervisor can force-close registered sockets at the five-second deadline and then observe all
  joins.

## Edge cases and focused tests

- With deterministic small limits, fill client and upstream caps, test the next response/close,
  release and reacquire permits, pool eviction, failed handshakes, cancellation, and task panic.
- Demonstrate 128 established clients and never more than 256 in production settings without an
  unbounded test allocation.
- With paused time, test health failure and accept stop immediately, pending-work cancellation,
  active exchange completion at 4.999 seconds, forced closure at five seconds, idle tunnel drain,
  second signal, policy mount removal during drain, and zero/nonzero exits.

## Validation

1. `cargo fmt --all -- --check`
2. `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings`
3. `cargo test -p vhrn-proxy --locked connect::pool`
4. `cargo test -p vhrn-proxy --locked server::listener`
5. `cargo test -p vhrn-proxy --locked --test proxy_process`
6. `cargo test -p vhrn-proxy --locked`

## Evidence required

- Record maximum observed client/upstream/task counts, 128-client proof, permit lifecycle tests,
  and paused-time shutdown traces.
- Record exit-status and idempotence process results.
- Record independent resource-exhaustion, cancellation, and shutdown review plus clean rereview.

## Completion criterion

Every connection and task has a bounded owner; client/upstream caps are enforced while supporting
128 clients; overload is prompt; shutdown is health-visible, two-stage, and exactly five seconds;
all joins are observed; exit codes are correct; validation passes; and rereview is clean.

## Implementation evidence

Completed on 2026-09-20 within the Phase 11 editable paths.

- `ProcessResources` is created once during bootstrap and shared by the supervisor and the public
  and broker connectors. Non-waiting semaphores cap accepted clients and aggregate upstream work at
  256 each. A client `SocketLease` owns its permit and socket registration; every upstream stream is
  a `ManagedIo` that owns its permit and registration through handshake, active HTTP, CONNECT, and
  idle-pool states.
- Public and broker paths acquire the shared upstream permit before DNS/dial or broker dial work.
  Failed resolution, failed and cancelled broker handshakes, downstream cancellation, pool
  replacement, eviction, expiry, and shutdown all release their RAII-owned permits. Each connector
  pool is capped at 64 entries, the shared 256 permit cap remains authoritative across both pools,
  and `close` drops idle entries and rejects late returns.
- One `Supervisor` owns the listener, context and pools, process resources, and a single client
  `JoinSet`. It creates exactly one task for each admitted client and does not create a detached
  tunnel or pool-reaper task. Periodic pool pruning occurs in the supervisor loop. Every join result
  is classified; a panic or unexpected join failure starts cleanup and returns an error.
- Client overload uses a bounded closing 503 when the accepted socket is immediately writable and
  otherwise closes promptly without spawning another task. Upstream exhaustion maps to a closing
  HTTP 503 before DNS or dial work. The listener-path tests exercise both behaviors with small
  deterministic limits.
- The downstream peer-close monitor uses one process-wide poll registry rather than one poll file
  descriptor per connection. Its state is bounded by admitted connections, and registrations are
  removed before their sockets are dropped.
- `Shutdown` has independent drain and force notifications and records the first drain instant.
  The first SIGINT/SIGTERM stops acceptance, makes health unhealthy, cancels pending work, and
  closes both idle pools. A second signal forces immediately. Otherwise the supervisor derives the
  force deadline from the original request instant, closes every tracked resource at five seconds,
  aborts remaining tasks, and observes all joins.
- Normal HTTP and CONNECT response commitment races the drain notification until the first response
  byte. Once committed, only force can interrupt remaining response or tunnel bytes. A known-drain
  HTTP request gets one scheduler-bounded opportunity to commit the bounded closing 503; if the
  downstream stays backpressured, it closes without committing the origin response or a partial
  rejection.

## Resource-count and lifecycle evidence

- `production_admission_rejects_the_two_hundred_fifty_seventh_connection` observed 256 simultaneous
  client ownership records and a peak of 256, rejected the 257th without waiting, then reacquired a
  permit after release.
- `production_upstream_admission_never_exceeds_two_hundred_fifty_six` observed 256 simultaneous
  upstream permits and a peak of 256, rejected the 257th, and returned to zero after release.
- `process_supports_one_hundred_twenty_eight_established_clients` held 128 real established clients
  with partial request heads while an additional health request returned 200. Because the
  supervisor creates one task per admitted client and no child tunnel tasks, this also observed 128
  simultaneous supervised client tasks. The structural task maximum is 256, enforced before task
  creation by the client permit; the focused overload test observed a one-task limit remain at one
  while the next client received a complete 503 or prompt close.
- `pooled_upstream_permits_survive_idle_and_release_on_eviction_and_close` reached its two-permit
  test maximum, proved an idle connection retains its permit, and proved deterministic eviction and
  shutdown close release permits. `shutdown_close_drops_entries_and_refuses_late_returns` proves a
  closed pool cannot be repopulated.
- Public cancellation and broker rejection/cancellation tests prove resolution, dial, handshake,
  and failure paths release permits. `forced_registry_close_interrupts_tracked_socket_io` proves a
  registered stream observes forced closure and unregisters on drop.
- Existing fixed ingress/body/tunnel buffers, the 64-answer DNS limit, bounded diagnostic records,
  the two 64-entry idle pools, the 256-entry client join set, and the absence of detached driver or
  tunnel tasks keep all concurrent queues and allocations bounded.

## Shutdown, exit, and idempotence evidence

- Paused-time `active_task_can_finish_at_four_point_nine_nine_nine_seconds` and
  `active_http_exchange_completes_at_four_point_nine_nine_nine_seconds` prove admitted work and a
  committed HTTP stream survive drain and finish at 4.999 seconds without drain cancellation.
- `pending_task_is_forced_and_observed_at_exactly_five_seconds` advances two seconds after the first
  drain request, repeats the request, and still forces at exactly five seconds from the original
  instant. It observes the abort and returns tracked socket count to zero.
  `idle_tunnel_is_closed_at_the_five_second_force_deadline` proves both tunnel ends close at that
  exact deadline.
- `draining_before_connect_response_commit_emits_no_success` and
  `draining_before_http_response_commit_emits_no_origin_response` use backpressured output to prove
  a racing uncommitted success/origin response emits no bytes. The writable counterpart,
  `writable_http_race_emits_closing_503_during_drain`, proves a safely writable race receives 503
  plus `Connection: close`.
- The black-box SIGTERM tunnel test removes all policy and token mounts during drain, transfers
  active bytes, observes forced closure no earlier than five seconds and before six seconds, and
  observes a zero exit. `second_sigterm_forces_idempotent_shutdown_and_exits_zero` sends the second
  signal after 100 ms, observes both tunnel ends close before five seconds, and observes a zero exit.
- Startup/listener failure process tests continue to exit nonzero with bounded redacted diagnostics.
  `supervisor_task_panic_cleans_up_and_returns_failure` injects a task panic through
  `Supervisor::run`, proves shutdown and cleanup occur, and proves the supervisor returns the error
  propagated by the process entry point. Drain tests account for successful, failed, panicked, and
  aborted joins and leave their `JoinSet`s empty.

## Validation evidence

The final post-fix validation run passed every required command:

1. `cargo fmt --all -- --check` — passed.
2. `cargo clippy -p vhrn-proxy --all-targets --locked -- -D warnings` — passed with no warnings.
3. `cargo test -p vhrn-proxy --locked connect::pool` — 8 passed, 0 failed.
4. `cargo test -p vhrn-proxy --locked server::listener` — 21 passed, 0 failed.
5. `cargo test -p vhrn-proxy --locked --test proxy_process` — 26 passed, 0 failed.
6. `cargo test -p vhrn-proxy --locked` — 177 unit tests and 26 process tests passed; doc tests
   passed with no failures.

`git diff --check` also passed. Loopback-dependent tests ran outside the restricted socket sandbox.

## Independent review evidence

- The requested `rust_reviewer` agent independently reviewed resource exhaustion, cancellation,
  task ownership, shutdown timing, exit behavior, and the focused/process tests without inspecting
  current or historical Go source, tests, module files, diffs, history, or Go-derived explanations.
- The initial review found two P1 issues: response commitment did not race drain before its first
  byte, and the five-second deadline began after teardown rather than at the first shutdown request.
  The implementation added drain-aware commitment with forced-only active writes, blocked-write
  CONNECT/HTTP races, first-request deadline storage, a cleanup-delay timing trace, and a tighter
  wall-clock process bound. The review's test gaps were also closed with real serving-path overload,
  HTTP-boundary upstream exhaustion, and supervisor-panic cleanup tests.
- The first rereview found one remaining P1: the known-drain HTTP path constructed a 503 but closed
  before polling an otherwise writable downstream. The prompt rejection writer and deterministic
  writable-race test fixed that behavior while retaining the backpressured close proof.
- The final rereview reported no actionable findings, no material test gaps, and no material
  residual risks. It independently passed `git diff --check`, formatting, clippy, the 8 pool tests,
  the 21 listener tests, and the full 177-unit/26-process suite. The rereview is clean.
