# Rust proxy migration

Status: proposed; implementation has not started.

Replace the Go egress proxy with an idiomatic Rust service built from the proxy's security and
protocol contract. Keep Go as the shipping implementation until the Rust image passes the cutover
gate, then remove Go and ship one proxy implementation.

The source of truth for current behavior is the combination of:

- `proxy/` for request handling, policy enforcement, safe dialing, diagnostics, and image shape;
- `src/net.rs` for policy values and live policy storage;
- `src/broker.rs` for `VHRN-BROKER/1` and local connection enforcement;
- `src/run.rs` for the process environment, mounts, image invocation, and lifecycle;
- `proxy/Makefile`, `proxy/Dockerfile`, and `.github/workflows/` for build and release behavior.

Characterize observable behavior from those boundaries. Use the Go code as evidence, not as the
Rust module design.

## Target design

### Workspace

Retain the root `vhrn` package and add two workspace members:

```text
Cargo.toml
Cargo.lock
src/                              vhrn CLI
crates/
  vhrn-policy/
    Cargo.toml
    src/
      lib.rs
      authority.rs
      broker.rs
      domain.rs
      mode.rs
proxy/
  Cargo.toml
  Dockerfile
  Dockerfile.rust                 temporary candidate image
  Makefile
  src/
    lib.rs
    main.rs
    broker.rs
    config.rs
    diagnostics.rs
    policy.rs
    public.rs
    relay.rs
    service.rs
    target.rs
testdata/
  loopback-authorities.tsv
  broker-frames.tsv
  domain-policy.tsv
  ip-addresses.tsv
  proxy-http-cases.tsv
  proxy-modes.tsv
```

Configure the workspace with resolver 3, members `.`, `crates/vhrn-policy`, and `proxy`, and
`default-members = ["."]`. The default preserves the meaning of root `cargo build`, `cargo test`,
and `cargo install --path .`; workspace CI selects all members explicitly.

Every first-party crate uses edition 2024, inherits the workspace lint policy, and declares
`#![forbid(unsafe_code)]`.

### Shared policy crate

`vhrn-policy` contains values whose bytes or meaning must agree between the host and proxy:

- `Mode`, including wire spelling and the proxy's fail-closed file interpretation;
- normalized public domain entries and label-anchored hostname matching;
- `LoopbackAuthority` parsing, canonical formatting, and exact comparison;
- bounded `VHRN-BROKER/1` request and response framing.

Keep the crate synchronous and pure. It owns parsing and validation, with inputs passed as values.
It has no environment reads, filesystem access, DNS, sockets, HTTP, Tokio, CLI dispatch, or host
policy storage. Keep host-facing diagnostics and IDNA suggestions in `vhrn`; keep proxy I/O and IP
classification in `vhrn-proxy`.

### Proxy crate

`vhrn-proxy` is a library with a thin binary edge:

- `main.rs` loads startup configuration, creates production dependencies, runs the server, and maps
  terminal errors to process exit.
- `config.rs` parses the established environment interface into a validated `Config`. It selects
  paths and addresses without loading live policy contents.
- `target.rs` parses request targets into `Target::Public` or `Target::Local`. Authority parsing
  happens once and yields typed values.
- `policy.rs` reopens the policy files needed for each decision and returns owned snapshots.
- `public.rs` owns public IP classification, one-shot DNS resolution, numeric dialing, and the
  public HTTP connection pool.
- `broker.rs` owns broker readiness, authentication, framing, deadlines, and the separate local
  HTTP connection pool.
- `service.rs` authorizes typed targets and routes HTTP and CONNECT requests to the only connector
  capable of serving that target class.
- `relay.rs` owns upgraded bytes, bidirectional copying, cancellation, and close behavior.
- `diagnostics.rs` owns status responses, denial records, redaction, and bounded log rendering.
- `lib.rs` assembles the service from narrow collaborators and exposes a run function used by the
  binary and crate tests.

Prefer static dispatch and narrow traits at test seams. Keep process-global reads in `main.rs` and
`config.rs`; pass resolved configuration, clocks, policy sources, resolvers, and connectors into the
service. The request path must have no general-purpose dial function.

### Runtime dependencies

Use Tokio for scheduling, sockets, signals, deadlines, and task ownership. Use Hyper 1 with
`hyper-util` and `http-body-util` for HTTP/1 serving and clients. Add a TLS connector only if phase
1 establishes absolute-form HTTPS forwarding as a supported behavior; use rustls with ordinary
certificate verification when required. CONNECT remains opaque TCP.

Minimize features and direct dependencies. Represent expected failures with typed library errors;
use `anyhow` only at the binary/configuration edge if it improves startup context. The dependency
review records the purpose and enabled features of every direct dependency.

## Enforcement model

### Request pipeline

Route each request through one typed path:

```text
parse target
  -> load the policy for that target class
  -> produce an authorization decision
  -> select the sealed public or broker connector
  -> forward HTTP or establish CONNECT
  -> record the result
```

Use enums for target and decision state. A local authorization value can open only a broker
connection; a public authorization value can open only a validated public connection. Pool lookup
happens after authorization, so a reused connection cannot bypass current policy.

### Public policy and dialing

For every public request or tunnel:

1. Reopen all required public allowlist files and the mode file.
2. Normalize every entry and form one owned snapshot.
3. Treat an unknown readable mode as `enforce`.
4. Deny the decision when a required file is unreadable or an allowlist entry is malformed.
5. Apply exact or label-anchored subdomain matching.
6. Record the destination when the selected mode requires a denial record.

When a new public connection is required:

1. Resolve the hostname once.
2. Require a non-empty answer set.
3. Classify every answer with the current public-address rules, including IPv4-mapped IPv6 and
   carrier-grade NAT handling.
4. Reject the entire set when any answer is forbidden.
5. Dial the first validated numeric socket address without another name lookup.
6. Preserve the original normalized authority for HTTP semantics and connection-pool identity.

The resolver and connector each have explicit deadlines. Pool keys include scheme, normalized host,
and effective port. Public and broker pools share no connector or connection.

### Local policy and broker

Classify only canonical loopback authorities as local: `localhost`, exact `127/8` addresses, and
`[::1]`, each with a nonzero port. Compare the normalized authority against the three current local
policy layers on every request or tunnel.

An allowed local target is framed as data for the fixed broker address. The broker connector:

- loads the token in the established 64-byte lowercase-hex format;
- completes the readiness exchange before the server accepts local work;
- authenticates every new broker connection;
- applies the established frame bounds and handshake deadlines;
- preserves bytes read past the broker response;
- exposes redacted errors whose debug and display forms contain no token.

The host broker remains the second enforcement point and rechecks live local policy before dialing.
An established CONNECT tunnel keeps its connection; later requests and tunnels use current policy.

### HTTP and CONNECT

Support absolute-form HTTP proxy requests and CONNECT authorities with the current defaults and
status classes. Preserve the original authority while dialing through the selected connector.
Remove `Proxy-Connection` and `Proxy-Authorization` before forwarding. Phase 1 determines the
required treatment of standard hop-by-hop and `Connection`-nominated headers.

Stream request and response bodies with bounded buffers. Flush response chunks promptly and cancel
upstream work when the downstream request ends. Authorize each request before asking its pool for a
connection.

For CONNECT, establish the authorized upstream before returning success. Preserve bytes already
buffered by the HTTP parser or broker handshake. Relay both directions under one owner; the first
EOF or error closes both transports and waits for both copy tasks to finish.

### Lifecycle and diagnostics

Own accepted connections in a Tokio `JoinSet`. Distribute shutdown through a `watch` channel. On
SIGTERM, stop accepting, signal child work, close transports, and drain tasks within a fixed grace
period. Parser buffers, broker frames, queues, concurrent connections, and diagnostic fields all
have explicit bounds. Each spawned task has an owner and a shutdown path.

Preserve `/healthz`, `/__status`, the denial-log record format consumed by the host, and the current
status-code classes. Sanitize control characters and cap attacker-controlled fields without
changing host parsing. Keep tokens out of logs, errors, panic messages, and status responses.

## Feature parity

Create language-neutral corpora under `testdata/` and make Go and Rust consume the same rows during
the migration. Each row states the required outcome rather than a function name or implementation
detail. Add a regression row before fixing any discrepancy.

| Area | Required cases |
| --- | --- |
| startup | defaults, singular/plural public paths, complete and partial broker configuration, invalid token |
| mode and policy files | enforce/report/open, unknown mode, atomic replacement, missing file, malformed entry |
| domains | normalization, exact host, subdomain boundary, confusing suffix, empty and invalid values |
| addresses | public IPv4/IPv6, loopback, private, unspecified, link-local, multicast, CGNAT, mapped forms, mixed answers |
| public HTTP | allow/deny/report, method and body forwarding, authority, proxy-header removal, streaming, pool reauthorization |
| public CONNECT | default/explicit ports, allow/deny, dial failure, buffered upgrade bytes, relay closure |
| local authorities | localhost, exact 127/8, IPv6 loopback, canonical ports, malformed hosts, exact policy match |
| broker | READY and CONNECT framing, authentication, partial/oversized responses, timeout, denial, buffered bytes |
| local HTTP | broker-only routing, body streaming, origin failure, disconnect cancellation, pool reauthorization |
| local CONNECT | broker-only routing, authorization, buffered bytes, revocation, relay closure |
| diagnostics | health, status JSON, denial log shape, escaping, status classes, token redaction |
| lifecycle | bind failure, broker readiness failure, connection cancellation, SIGTERM, bounded task drain |

Rust passes a row by producing the written outcome and the expected origin, resolver, dialer, or
broker observations. Exact error sentences, log timestamps, scheduling, allocations, and source
structure are outside parity unless a host parser consumes them.

Keep pure checks beside their modules under `#[cfg(test)]`. In the proxy library's test modules,
compose the real service with injected policy sources, resolver results, connectors, clocks, and a
controlled broker. Use loopback sockets and raw HTTP bytes where framing, upgrades, flushing, or
half-close behavior matters. Give every async test a hard deadline; use paused Tokio time for timers.

Exercise parser and framing corpora with property tests or bounded fuzzing when they add coverage.
The deterministic corpus remains part of ordinary `cargo test` so every regression runs in the
standard gate.

## Implementation sequence

### Phase 1: characterize the contract

Inventory the observable boundary from the source-of-truth files. Add shared corpora and missing Go
characterization tests. Resolve these known ambiguities before choosing Rust helpers:

- public CONNECT currently handles HTTP-parser buffering differently from local CONNECT;
- public and local HTTP currently differ in flush and cancellation behavior;
- absolute-form HTTPS may depend on certificate roots absent from the scratch image;
- standard hop-by-hop headers need an explicit forwarding rule;
- process shutdown needs an explicit drain bound.

Choose one target behavior for each ambiguity based on the security contract and client-visible
result. Record intentional corrections as contract rows rather than preserving accidental behavior.

Completion criterion: every row in the parity table has an expected outcome, both implementations
can read the applicable corpus, all Go characterization checks pass, and each ambiguity above has a
recorded decision.

### Phase 2: create the workspace and shared types

Add the workspace manifests, `vhrn-policy`, and the `vhrn-proxy` package skeleton. Move shared values
one at a time, switching host callers without changing their results. Keep the root package as the
default member and keep the Go Dockerfile as the production image.

Completion criterion: format, clippy, and tests pass across the workspace; existing root build,
test, and install commands still select `vhrn`; host policy and broker corpora pass through the
shared crate; the packaged proxy is still Go.

### Phase 3: build the service shell

Implement `Config`, live policy sources, target classification, diagnostics, server supervision,
and sealed connector interfaces. Wire health and status endpoints. Use inert test connectors so the
service cannot yet open public or broker sockets.

Completion criterion: startup/configuration, policy-file, target, diagnostics, and lifecycle rows
pass in Rust; malformed configuration and unreadable live policy take their specified failure paths;
every spawned task is owned by the supervisor.

### Phase 4: implement public egress

Implement public authorization, IP classification, single-resolution dialing, the public HTTP pool,
HTTP forwarding, CONNECT setup, streaming, and relays. Keep the original authority separate from
the numeric dial address.

Completion criterion: all domain, address, public HTTP, and public CONNECT rows pass; a mixed answer
set opens no socket; captured connector calls contain numeric addresses; pool reuse occurs only
after a fresh authorization decision; repeated cancellation and relay-close loops leave no retained
tasks or sockets.

### Phase 5: implement brokered local access

Implement readiness, token handling, bounded protocol framing, the broker connector, the local HTTP
pool, local HTTP forwarding, and local CONNECT. Preserve broker-response bytes before beginning the
relay.

Completion criterion: all local authority, broker, local HTTP, and local CONNECT rows pass; local
requests can reach only the configured broker connector; public modes cannot produce local
authorization; captured diagnostics contain no token; policy revocation has the recorded behavior
for pooled requests and established tunnels.

### Phase 6: close parity and resource bounds

Run the complete corpus against Go and Rust. Classify each difference as a Rust defect, a fixture
defect, or an intentional contract correction. Stress slow bodies, partial frames, half-closes,
disconnects, policy replacement, concurrent tunnels, and shutdown races.

Measure the Go and Rust proxy with the same local workload:

- static binary and compressed image size;
- startup time and idle RSS;
- steady HTTP latency, throughput, and RSS;
- concurrent CONNECT throughput and RSS;
- resource return after repeated cancellation and shutdown.

Set accepted bounds from the measurements and record the rationale for every accepted regression.

Completion criterion: every parity row passes or names an approved contract correction; stress runs
remain within explicit task, socket, memory, and time bounds; dependency, license, advisory, binary,
image, and resource reviews are complete.

### Phase 7: qualify the Rust image

Build the candidate from `proxy/Dockerfile.rust` under a non-release tag. Produce static
`linux/amd64` and `linux/arm64` binaries, verify their architecture and linkage, and copy only the
binary into scratch. Run as `65532:65532`, expose port 8080, and preserve the current entrypoint and
environment contract.

Run direct image checks for startup, health/status, invalid configuration, policy/log permissions,
broker readiness, request forwarding, and SIGTERM. Build and run the candidate with Apple
`container` and with Docker through Colima; record engine version, architecture, image digest,
commands, and result separately for each engine.

Completion criterion: both architectures pass static-image inspection and direct checks; both
required local engines have recorded passing results; image name/tag resolution and the proxy
Makefile interface remain unchanged; release tags still point to Go.

### Phase 8: cut over

Replace `proxy/Dockerfile` with the qualified Rust build and remove `Dockerfile.rust`. Remove the Go
sources, module, tests, toolchain setup, vulnerability job, and Go build stage. Retain all corpora and
Rust parity checks. Update `AGENTS.md`, workflow path filters, release documentation, and component
descriptions in the same change.

Build a new matched CLI/proxy release. Preserve existing immutable image tags. Rollback uses the
previous matched release or a reverted change published under a new version.

Completion criterion: Rust is the only proxy source and image artifact; CI contains no Go proxy
tooling; install and update resolve `vhrn-proxy` on the CLI release clock; the qualified Rust image
passes the full parity, resource, architecture, engine, and release-contract gates.

## CI changes

During phases 2–7, run Go checks and Rust workspace checks in parallel while packaging Go:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo build --release --locked -p vhrn
cargo build --release --locked -p vhrn-proxy
```

Make root manifests, `crates/vhrn-policy/**`, `proxy/**`, shared `testdata/**`, and relevant workflow
files trigger the affected checks. Select `-p vhrn` explicitly in binary-release workflows so the
workspace cannot change the CLI artifact by default.

At cutover, replace the Go job with Rust dependency and image checks. Keep workflow linting and the
existing multi-platform publish flow. Publish only after the candidate digest that passed
qualification is the digest selected by the release job.

## Evidence

Append evidence as phases complete:

| Date | Phase | Revision/image | Command or corpus | Platform | Result |
| --- | --- | --- | --- | --- | --- |

Record failures and open gaps alongside passes. A phase is complete only when its completion
criterion is supported by entries here and by checked-in tests or review artifacts.

No implementation evidence exists yet.
