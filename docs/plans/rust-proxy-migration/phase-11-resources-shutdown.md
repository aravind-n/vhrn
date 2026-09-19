# Phase 11: Bound process resources and supervise shutdown

## Execution contract

This file is the authoritative implementation specification for Phase 11. Start here and:

1. Read the shared [master plan](plan.md).
2. Confirm the Phase 11 row in the master plan is exactly `Ready`. If it is not, stop; the
   table is the sole readiness source.
3. Read repository [`AGENTS.md`](../../../AGENTS.md).
4. Read the frozen [consumer contract](../../proxy/consumer-contract.md).
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
