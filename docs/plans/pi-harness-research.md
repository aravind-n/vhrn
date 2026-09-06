# Pi harness support

Status: **design agreed; engine feasibility gates remain.** The prerequisites have landed, and
this plan is rebased against their implemented interfaces. The completed
[`egress allowlist layering`](completed/egress-allowlist-layering.md) and
[`per-project configuration`](completed/per-project-config.md) plans define the current policy
scopes, canonical project identity, and host-owned configuration. Begin with the engine probes
below; Pi support and the broker are not yet implemented. The accepted decisions are a broker for
every harness run, dual-stack `localhost` with exact numeric grants, and filtered settings bootstrap.
Required engine coverage is Apple `container` and Docker through Colima on this Mac. Native Linux
Docker and Docker Desktop coverage are deferred at the maintainer's request and do not block this run.

## Researched behavior

Pi models inference servers as providers rather than integrating separately with LM Studio,
Ollama, llama.cpp, MLX, or vLLM. A provider supplies a base URL, API protocol, model metadata, and
optional compatibility overrides. Consequently, vhrn needs one endpoint capability rather than
runtime-specific integrations.

Pi 0.84.4 supports `openai-completions`, `openai-responses`, `anthropic-messages`, and
`google-generative-ai`. Providers can come from built-ins, custom entries in `models.json`, Pi's
first-class llama.cpp router, or extensions. `models.json` is therefore an important configuration
source, but not a complete inventory of every endpoint Pi may use. See Pi's
[model](https://pi.dev/docs/latest/models),
[provider](https://pi.dev/docs/latest/providers), and
[custom-provider](https://pi.dev/docs/latest/custom-provider) documentation.

The official installation path uses Node.js 24 and installs
`@earendil-works/pi-coding-agent` with npm. The future vhrn image must follow that supported path
instead of assuming that a standalone binary is Pi's primary distribution. See Pi's
[quickstart](https://pi.dev/docs/latest/quickstart) and
[containerization guidance](https://pi.dev/docs/latest/containerization).

Pi's agent directory defaults to `~/.pi/agent`; `PI_CODING_AGENT_DIR` overrides it.
`PI_CODING_AGENT_SESSION_DIR` separately controls session storage, with `--session-dir` taking
precedence over the environment variable and settings. The directory mixes user-authored inputs
with state Pi updates:

| Data | Pi behavior |
| --- | --- |
| `models.json` | User-authored custom providers and models; reloaded when the model picker opens |
| `settings.json` | User settings, also updated by Pi commands and UI |
| `auth.json` | API keys and OAuth credentials written by `/login` |
| `trust.json` | Project trust decisions |
| `models-store.json` | Pi-written cache/state for dynamically discovered model catalogs |
| `extensions/`, `skills/`, `prompts/`, `themes/` | User-managed capabilities and presentation |
| `keybindings.json` | User preferences, also rewritten by Pi's legacy keybinding migration |
| `SYSTEM.md`, `APPEND_SYSTEM.md` | User-authored global system-prompt inputs |
| `npm/`, `git/`, and related package state | Sources and registrations managed by Pi package commands |
| sessions | Conversations grouped by working directory unless redirected |

Pi documents these behaviors in its
[settings](https://pi.dev/docs/latest/settings),
[packages](https://pi.dev/docs/latest/packages), and
[security](https://pi.dev/docs/latest/security) references.

### Inspected local configuration

The machine used for this research had Pi 0.84.4 installed at the time of review. Its configuration
was inspected structurally without reading or printing credentials. It uses the default
`~/.pi/agent` directory, an `lmstudio` custom provider,
`http://localhost:1234/v1`, the `openai-completions` protocol, multiple model IDs, and existing
`auth.json`, `settings.json`, and `models-store.json` state. Model IDs and credential values are
intentionally omitted.

That is a valid native Pi configuration. Running it through vhrn should not require rewriting the
provider URL to an engine-specific hostname. LM Studio exposes OpenAI- and Anthropic-compatible
APIs; llama.cpp likewise offers compatible server protocols. The same approach applies to Ollama,
MLX, vLLM, or another server whenever Pi can speak the protocol it exposes. See the
[LM Studio OpenAI compatibility](https://lmstudio.ai/docs/developer/openai-compat),
[LM Studio server](https://lmstudio.ai/docs/developer/core/server), and
[llama.cpp server](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)
documentation.

### Current proxy conflict

Pi 0.84.4 uses Undici's environment-aware proxy support and honors `HTTP_PROXY`, `HTTPS_PROXY`, and
`NO_PROXY`. Vhrn supplies the proxy variables and does not exempt localhost. A local request
therefore follows this path today:

```text
Pi in the agent container
  -> vhrn egress proxy
  -> attempt to dial localhost:1234
  -> rejected as a loopback/private address
```

This rejection is intentional. The proxy's `SafeDialer` blocks loopback, private, link-local,
multicast, carrier-grade NAT, and metadata addresses. Adding `localhost` to the domain allowlist
would still fail at the dialer. Removing the dialer check would let a domain grant probe arbitrary
host and private-network services because public-domain policy does not restrict ports.
`--open-net` must continue to affect only public egress.

The plain-HTTP transport regression in
[Pi issue #8134](https://github.com/earendil-works/pi/issues/8134) is version-dependent:
[v0.84.4](https://github.com/earendil-works/pi/blob/v0.84.4/packages/coding-agent/src/core/http-dispatcher.ts)
lacks `proxyTunnel: true`, while
[upstream main](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/http-dispatcher.ts)
sets it and the issue is closed at this review. This does not identify the first fixed release.
Record the actual baked Pi version and test its transport. A successful first request is insufficient:
exercise streaming, a tool call, the following model request, and connection reuse. Support both
plain HTTP forwarding and CONNECT regardless of which transport that version selects.

## Required interfaces

Local inference is a distinct host-owned capability layered alongside, but never folded into, the
public-domain allowlist. It is not a `[net]` TOML setting and is never inferred from Pi
configuration.

```text
vhrn net allow [--project <path>] --local <host:port>...
vhrn net deny  [--project <path>] --local <host:port>...
vhrn net status [--domains] [--local]

vhrn <harness> --allow --local <host:port[,host:port...]> [--] [agent arguments...]
```

The persistent forms use the global or exact-project scope defined by the scoped-egress design.
The wrapper form grants access only to that run and writes no persistent state. Local grants work
across harnesses; there is no Pi-only network flag or broker toggle in the harness registry.

Persistent `allow`/`deny` commands accept a batch of whitespace-separated authorities after
`--local`; each invocation mutates either domains or local authorities, never a mixed batch.
The wrapper consumes exactly one following argv value for `--allow --local`, accepting commas
within that value. Repeat the complete pair for another value. Ordinary `--allow <domains>` and
`--allow=<domains>` retain their current meaning; standalone `--local` is not a wrapper flag.
An empty or flag-shaped value is an error. The first unrecognized argument or `--` ends wrapper
parsing, and the remainder crosses verbatim, including positional prompts. For example:

```text
vhrn pi --allow --local localhost:1234 --allow api.example.com --model my-model
vhrn pi --allow --local 'localhost:1234,[::1]:8080' -- "inspect this project"
```

`status` includes domain and local counts; `--domains` expands domains, `--local` expands local
authorities, and both may be combined. Local details retain global/project/run provenance. `deny`
removes only from its selected mutable layer, validates the entire batch before writing, and reports
any remaining grant in another layer, just as domain revocation does.

V1 accepts only an explicit port paired with `localhost`, an address in `127.0.0.0/8`, or IPv6
loopback (`[::1]:port`). Ports are decimal integers in 1..=65535. Normalize ASCII case for
`localhost`, standard IPv6 spellings of `::1`, and port leading zeroes. IPv4 requires four decimal
octets without ambiguous leading zeroes. Reject URLs, paths, wildcards, CIDRs, missing or zero ports,
unspecified addresses, LAN/private addresses, link-local addresses, metadata targets, arbitrary DNS
names, trailing-dot host aliases, IPv6 zone identifiers, and IPv4-mapped IPv6 addresses.

Keep `localhost:port`, `127.0.0.1:port`, `127.0.0.2:port`, and `[::1]:port` distinct policy
authorities. A numeric grant matches and dials only its exact address and port. A `localhost`
grant matches only requests addressed to `localhost`; the broker tries `127.0.0.1`, then `::1`
on connection failure, under a bounded connection deadline. It never resolves `localhost` through
DNS or retries an application request against the other address. This explicit dual-stack authority
does not grant the corresponding numeric authorities. Rust and Go share normalization fixtures.
For plain HTTP requests, an omitted port normalizes to the scheme default; CONNECT local targets
require an explicit port. Local authorization remains required in report or open mode.

Store loopback grants in separate policy files alongside the corresponding domain layers: a global
`net/loopback.allow`, `net/projects/<policy-key>/loopback.allow`, and
`net/runs/<run-id>/loopback.allow`. All three are required and created even when empty. There is no
built-in base or harness local grant. Do not overload domain records or add local endpoints to
`config.toml`. Extend `PolicyStore` in `src/net.rs`, preserving XDG state-root resolution, atomic
mutation under `policy.lock`, provenance, and `PolicyRun` publication/lease/retirement semantics.

Use the canonical project path already resolved in `prepare_container`. Egress storage continues
to use `ProjectIdentity`'s SHA-256 of exact canonical path bytes; existing history/session storage
continues to use the separate Claude-compatible `history_key`. These are two encodings of the same
project input, not interchangeable storage keys. Do not migrate existing stores as part of Pi support.

## Local endpoint architecture

Every harness run starts a host broker, including runs with zero effective local grants. With an
empty local policy it authorizes no relays. A subsequent global/project grant must work in that
already-running session without a restart or a new control service. Pi retains its native URL:

```text
Pi -> vhrn proxy -> authenticated host broker -> configured host-loopback endpoint
```

The proxy checks local authorities before its public-domain decision and routes authorized local
traffic through a dedicated broker client. `SafeDialer` continues to handle only public targets;
the broker client can connect only to its host-supplied broker destination, never a client-supplied
private address. The broker independently loads its run's global/project/run local policy and
validates the authority before dialing. Both ends deny local access on missing, unreadable, or
malformed required policy; open/report modes cannot override that decision.

Reopen local policy files for each proxy HTTP request or CONNECT decision and each broker relay
handshake. A pooled plain-HTTP connection does not skip the proxy's next request check. Atomic
global/project changes reach active runs with the same scope as domain changes. Revocation blocks
subsequent decisions; established CONNECT tunnels and in-flight requests retain the existing
egress contract and are not forcibly terminated just because a grant was removed.

Generate a random 256-bit capability token for every run and expose it only to the proxy and broker,
never to the agent container. Use OS-backed cryptographic randomness; do not derive it from the run
ID. Keep its host staging directory private and mount only the selected run's secret file into its
proxy; do not put a token in the shared world-readable policy files, command arguments, or logs.
Require a versioned, size- and time-bounded authenticated handshake before accepting relay bytes;
the handshake authenticates the token and normalized authority, and returns an explicit success
before tunneling. Tokens cannot be reused between runs and never appear in an origin request.

Implement the broker inside the host CLI process in a new `src/broker.rs`, with a per-run listener
and relay workers. `src/run.rs` owns startup, engine routing, and cleanup through a broker guard
registered with `SignalControl` immediately after binding. Publish policy and install signal
handling before broker startup; verify authenticated proxy-to-broker readiness before starting the
agent, without needing a model server to be running. Registration must synchronize with teardown
so a signal cannot leave an unowned listener. Normal/error/SIGTERM cleanup stops the agent, proxy,
broker listener and relays, then retires policy and secret staging; each cleanup is idempotent.
Because the broker lives in the wrapper process, SIGKILL also closes its listener and relays.

Bind only to the engine-specific address below, never a wildcard or LAN interface. Broker startup
or readiness failure aborts the run, including a run with zero local grants. A later loss of broker
availability fails local requests closed. A model endpoint being offline is an ordinary request
failure, not a broker startup failure.

Keep engine routing behind the run implementation:

- Apple `container`: resolve the selected network gateway from engine inspection and bind the
  broker to it.
- Docker through Colima on macOS: bind the host listener to `127.0.0.1:0` and give the proxy
  `host.docker.internal:<assigned-port>` using Colima's DNS. Verify that route from an actual Docker
  container in the initial probe; do not override it with `--add-host=host-gateway`.
  The Docker bridge gateway lives inside the Linux
  VM and is not the macOS host-loopback destination. Colima is a supported local engine backend,
  not an unsupported or remote daemon merely because Docker runs in a VM.

Colima's [default configuration](https://github.com/abiosoft/colima/blob/main/embedded/defaults/colima.yaml)
aliases `host.docker.internal` to `host.lima.internal`; its
[DNS implementation](https://github.com/abiosoft/colima/blob/main/environment/vm/lima/dns.go) maps
both to the configured host gateway. Lima documents
[access to host loopback through that gateway](https://lima-vm.io/docs/config/network/user/).
Use the DNS name, not a hardcoded gateway IP. Detect the effective Colima context/socket, including
`DOCKER_HOST` overrides, and retain the same daemon selection throughout inspection and launch.

Deferred routing recipes, retained for future verification:

- Native Linux Docker: inspect the bridge network selected for the proxy, bind on its host gateway,
  and pass that reachable destination to the proxy. Do not hardcode `172.17.0.1`.
- Docker Desktop on macOS: bind the host listener to `127.0.0.1:0`, obtain the assigned port, and
  have the proxy dial `host.docker.internal:<port>` through Desktop's DNS. The virtual destination
  and the host bind address are different; do not attempt to bind the virtual IP on the host.

Resolve the intended proxy network before binding, and launch the proxy on that same network.
Use OS-assigned listener ports on all engines. Docker documents
[host.docker.internal for host services](https://docs.docker.com/desktop/features/networking/networking-how-tos/#connect-a-container-to-a-service-on-the-host),
and VPNKit's [host-IP TCP handler](https://github.com/moby/vpnkit/blob/master/src/hostnet/slirp.ml)
connects to host loopback. This supports the Desktop recipe but is not a live verification of any
particular Desktop release. Keep host-bind and proxy-dial addresses separate in the engine-routing
result. A remote Docker daemon has a different host: reject local-broker startup unless the route
to the CLI host can be established under this same contract.

A non-privileged feasibility probe on Apple `container` 1.3.1 successfully reached an HTTP server
bound only to the inspected default-network gateway (`192.168.64.1`) from an ephemeral container.
This avoids the documented host-DNS setup that requires `sudo`, disables Private Relay, and is lost
on restart. It proves the basic route, not the complete authenticated proxy/broker flow; the latter
remains a release gate. Run small probes on Apple `container` and Docker through Colima before
building the full broker. Record engine versions/network modes, bind address,
proxy destination, successful reachability, and failure to reach the listener via the host's LAN
address. No privileged host setup is allowed. Starting Colima is authorized. The earlier Apple result
is historical evidence, not a completed current test matrix. Do not require a Desktop installation
or native Linux environment; revisit Desktop only if a concrete Colima limitation makes it necessary.

Private LAN endpoints are out of scope. Supporting them later requires a separate permission type
and dialer that still rejects metadata, link-local, multicast, and every unconfigured destination.
It must not emerge implicitly from host-loopback support.

### Rejected alternatives

- Rewriting `localhost` to `host.docker.internal` leaks engine details into Pi configuration and
  gives native and jailed Pi different files.
- Binding an unauthenticated model server to `0.0.0.0` unnecessarily exposes it to the LAN.
- Parsing `models.json` to authorize endpoints lets agent configuration widen the network jail and
  misses built-in, router, and extension-defined providers.
- Setting `NO_PROXY=localhost` would send traffic to the agent container's own loopback, not the
  host, while bypassing the intended proxy decision.
- Allowing private ranges or disabling `SafeDialer` exposes unrelated host, LAN, and metadata
  services.
- Rewriting Pi configuration in the container violates host ownership and can destroy user changes.

## Pi harness and persistence

Add Pi as a data-driven `Harness` entry with a thin `FROM vhrn-base` image. Install Node.js 24 and
`@earendil-works/pi-coding-agent` in the Pi image rather than adding Node to every harness through
the base image. Add `build-pi` and clean/default-build wiring in `image/Makefile`, plus the
`pi --version` entry in `.github/workflows/_build-images.yml`. Follow existing version tags and
image labels so install/update and daily harness publishing work without Pi-specific CLI dispatch.

Use `PI_CODING_AGENT_DIR` and nested mounts to preserve the following ownership split:

| Pi data | Vhrn behavior |
| --- | --- |
| `models.json`, `SYSTEM.md`, `APPEND_SYSTEM.md`, manually managed extensions, skills, prompts, themes | Mirror from the host's `~/.pi/agent` on every run |
| `settings.json` | Seed only when absent, stripping credential/trust controls, then leave container-owned |
| `keybindings.json` | Seed only when absent; let Pi persist its migrations and later edits |
| `auth.json` | Container-owned; never bootstrap from the host |
| `trust.json` | Container-owned; never bootstrap from the host |
| `models-store.json`, Pi-managed packages, and other Pi-written state | Container-owned and persistent |
| Global guide | First-non-empty host source composed through the guide descriptor into `AGENTS.override.md` |
| Sessions | Persistent sibling store partitioned by the existing `history_key` of the canonical project path |

Generalize `Harness.credentials` into declarative seed-file descriptors with a relative filename
and either a byte copy or a JSON-object copy with named top-level keys removed. Claude keeps its
existing credential copy behavior; Pi declares a filtered settings seed and an unfiltered
keybindings seed. The implementation belongs in `src/persist.rs`, with no harness-name branch.
Both Pi files live in the real state directory mount so Pi can replace them atomically.

For Pi settings, remove `apiKeys` and `defaultProjectTrust` before creating the container copy.
Pi 0.84.4's [startup migrations](https://github.com/earendil-works/pi/blob/v0.84.4/packages/coding-agent/src/migrations.ts)
can turn legacy settings credentials into `auth.json` when auth is absent; copying just the settings
file would otherwise defeat the no-auth-bootstrap promise. Its
[settings reference](https://pi.dev/docs/latest/settings) also permits an automatic trust default.
Retain other preference values, allowing JSON serialization to change whitespace. The host file
is never edited. An absent source creates no seed; malformed JSON or a non-object settings value
aborts preparation with a filename-only diagnostic, never a raw fallback copy or credential values.
Recheck these credential/trust fields against the Pi version selected for the image.

Existing destination files are authoritative: do not re-filter or update them on later runs. A user
can deliberately configure authentication or trust inside the container and have it persist. Publish
new seed files without replacing an existing destination, including when two first launches race;
an unexpected destination symlink or non-file is an error. Vhrn must not merge Pi-written
package/catalog state with the disposable mirror. Local package references in settings only work
when their targets already lie in mounted trees; they do not authorize extra host mounts.

Declare the guide sources in Pi's order: `AGENTS.override.md`, `AGENTS.md`, `AGENTS.MD`,
`CLAUDE.md`, `CLAUDE.MD`. Use the existing first-non-empty rule, compose into the disposable
`AGENTS.override.md`, and mount it at Pi's highest-priority global guide location. Do not mirror
competing guide files separately. Pi itself selects the first existing readable candidate, including
an empty file; vhrn's first-non-empty composition is a deliberate difference. See the
[v0.84.4 resource loader](https://github.com/earendil-works/pi/blob/v0.84.4/packages/coding-agent/src/core/resource-loader.ts).

Pi's descriptor sets `state_dir` and `host_config` to `.pi/agent`, `config_dir_env` to
`PI_CODING_AGENT_DIR`, `sessions_env` to `PI_CODING_AGENT_SESSION_DIR`, and `sessions_dir` to
empty. Auth, trust, and package/catalog state receive no seed or mirror descriptor;
`credential_env` is empty, `system_config` is false, and `share_history` is false.

Set the session environment variable directly to the project's sibling session store: Pi writes
session files there, without needing Codex's index/transcript remount. Replace the registry's
two-way session invariant with `sessions_dir` nonempty implies `sessions_env` nonempty; an env-only
store is valid. Preserve Codex's two mounts and existing session keys. Login/configuration remains
shared across projects. Pi's `--session-dir` still passes through and outranks the environment, so
partitioning is the default destination, not enforcement against an explicit agent override.

## Remote and commercial providers

Pi remains responsible for provider, protocol, model, and credential configuration. The supported
user flows are:

| Endpoint | Pi configuration | Vhrn configuration |
| --- | --- | --- |
| Built-in commercial provider | Select a model and run `/login` inside vhrn | Explicitly grant its public API domain |
| Custom OpenAI-compatible provider | Define the provider and models in `models.json`; authenticate inside vhrn | Explicitly grant the base URL's domain |
| Custom Anthropic-compatible provider | Define `anthropic-messages` provider metadata; authenticate inside vhrn | Explicitly grant the base URL's domain |
| Keyless loopback server | Configure the loopback URL; use Pi's supported dummy-key, `/login`, or `--api-key` mechanism if the provider requires a key value | Explicitly grant the exact local authority |

Pi resolves authentication from `--api-key`, `auth.json`, environment variables, and custom
provider key references. The default vhrn workflow is `/login` inside the container and persistence
of the resulting container-owned `auth.json`. Do not copy the host's aggregated `auth.json`, infer
environment-variable names from `models.json`, execute `!command` key resolvers on the host, or
automatically forward arbitrary secrets.

The Pi harness has an empty default domain layer. Relax the registry test requiring every harness
to declare domains; an empty immutable harness snapshot is valid. Pi still inherits the six domains
in `BASE_ALLOWLIST` and existing global/project/run grants. Do not automatically grant model vendors
or `pi.dev`; users grant required endpoints not already covered by those layers. Authentication can
also require public login/token domains beyond the inference API domain; surface their denials
through the ordinary host grant workflow. Never infer grants from provider configuration.

## Code ownership

| Concern | Owner |
| --- | --- |
| Pi command, paths, seed/mirror files, guide, sessions, empty harness domains | Pi data entry in `src/harness.rs` |
| Seed filtering and creation, disposable sync, session directories, guide composition | Generic helpers in `src/persist.rs` |
| Run flags and pass-through | `src/cli.rs`, extending `RunFlags` with typed local grants |
| Persistent net commands, authority normalization, scoped local files, provenance and leases | `src/net.rs`, extending `PolicyStore` and `publish_run` |
| Host listener, authentication, policy recheck, exact loopback dial and relay lifetime | New host-side `src/broker.rs`, compiled into the CLI |
| Engine bind/dial routing, per-run broker guard, proxy arguments, mount and signal lifecycle | `src/run.rs` |
| HTTP/CONNECT authority decision, live local policy and authenticated broker client | `proxy/egress/`; `proxy/main.go` wires broker destination and secret file |
| Pi installation, local builds, published image version | `image/pi/Dockerfile`, `image/Makefile`, `.github/workflows/_build-images.yml` |

The broker is generic runtime infrastructure, available to every harness. No `pi` branch, broker
boolean, endpoint list, or engine-specific setting belongs in `Harness`. `src/config.rs` continues
to own host tools/resources configuration; local endpoint permissions never enter `config.toml`.

## Implementation order and gates

1. Verify the bind/dial recipes with minimal probes on Apple `container` and Docker through Colima. Record
   evidence before implementing the broker; a missing environment leaves this gate outstanding.
2. Add typed local authority parsing, the three policy files, scoped mutations, provenance/status,
   and run flags using the existing canonical identity and `PolicyStore` lifecycle. Create empty
   local policy files on first use without disturbing existing domain policy.
3. Implement the host broker and proxy routing, then integrate guards, secret staging, readiness,
   live policy, and cleanup. Verify with a minimal HTTP client through the real proxy, including
   the first grant in a run that started with none. Keep the capability generic across harnesses.
4. Add generic seed transforms and env-only session descriptors, then the Pi registry entry,
   image, Makefile targets, and publishing matrix. Recheck the selected Pi version's migrations,
   trust controls, and HTTP transport against the researched behavior.
5. Run the automated and end-to-end gates below. Existing Claude/Codex launch and cleanup tests
   must pass with the new broker lifecycle, including runs with no local grants.
6. Complete active docs, help, container guides, changelog, and `AGENTS.md` with the implementation.
   Explain local grant commands and provenance, public-only open/report modes, settings/keybindings
   bootstrap ownership, and engine requirements. Until implementation, these remain plan-only.

Do not ship a reduced remote-only Pi harness. If exact host-loopback inference cannot work safely on
both Apple `container` and Docker through Colima without privileged host setup, defer the entire Pi harness.
Because every run will depend on broker startup, do not release that runtime change until all
required engine environments pass either; existing harnesses must not acquire an unverified startup
dependency. Native Linux and Docker Desktop results are not required for completion, and their routes
must not be described as verified. Rust CLI and Go proxy changes ship on the same CLI/proxy release clock.

## Required tests

### CLI and policy

- Parse persistent and run-scoped typed-local grants without consuming Pi arguments.
- Cover comma-separated wrapper values, repeated grant pairs, mixed public/local wrapper grants,
  `--`, missing values, and positional prompts that resemble authorities.
- Normalize, deduplicate, revoke, and report global, project, and run grants with provenance.
- Share Rust/Go normalization fixtures, including IPv6 compression, case and port normalization,
  forbidden numeric aliases, and the distinction between localhost and each numeric authority.
- Reject URLs, missing ports, wildcards, CIDRs, non-loopback targets, metadata targets, and malformed
  IPv6 authorities.
- Prove that public domain grants, report mode, and open mode cannot authorize loopback traffic.
- Preserve existing domain files on upgrade and create required empty local files before startup.

### Proxy and broker

- Route both plain HTTP and CONNECT only for an exact authorized authority.
- Reject missing/incorrect tokens, authority substitution, cross-run reuse, malformed handshakes,
  and unreadable policy.
- Exercise policy replacement and recovery in enforce/report/open modes; recheck requests on pooled
  HTTP connections after revocation. Preserve the documented lifetime of established CONNECT tunnels.
- Start two harnesses in different projects with no local grants; add the first project grant live
  and prove only its project gains access, then exercise global grant and remaining provenance.
- Preserve every existing private-address and public-domain test.
- Prove zero-grant readiness succeeds without a model server, while broker startup/readiness failure
  aborts launch. Readiness authenticates the run but opens no origin relay or policy exception.
- Prove error/SIGTERM cleanup at each registration/startup boundary is idempotent, closes listeners
  and active relays, retires policy and secret staging, and mounts no broker secret into the agent.
- Kill the host wrapper and verify its broker listener and relays cannot survive; do not depend on
  a later net command to stop an orphaned host broker.

### Pi persistence and image

- Verify user-managed inputs mirror from the host while Pi-written state persists independently.
- Verify `settings.json` seeds once, later Pi changes survive, and host changes do not overwrite it.
- Verify settings filtering removes `apiKeys` and `defaultProjectTrust`, retains unrelated values,
  fails safely on malformed/non-object JSON, and never prints secrets. Exercise concurrent seeds.
- Run Pi's startup migration with a legacy settings fixture and prove no host credentials appear
  in auth state; exercise the trust prompt with a host automatic-trust setting.
- Verify neither `auth.json` nor `trust.json` is imported, directly or through settings migration.
- Verify keybindings migration persists, including after a subsequent host sync.
- Verify package/catalog state persists and sessions select different stores for different projects.
- Verify Pi's env-only session mount, CLI override pass-through, and unchanged Codex transcript mounts.
- Verify guide source ordering/empty-source behavior and mirrored system-prompt files.
- Build the Pi image on both engine paths and verify the baked Pi version command.

### End to end

- Use a deterministic fake OpenAI-compatible streaming server bound only to host loopback.
- Exercise model response -> tool call -> model response, including a reused HTTP connection.
- Assert streamed events arrive before the response finishes, including through plain HTTP; a test
  that only checks final output can miss buffering. Check cancellation and connection cleanup.
- Prove an ungranted port is denied while the granted port succeeds in the same run.
- Put distinct servers on loopback addresses sharing a port to prove numeric grants stay exact;
  verify localhost IPv4/IPv6 fallback without DNS or application-request replay.
- Exercise a remote OpenAI-compatible endpoint and an Anthropic-compatible endpoint through
  explicit domain grants.
- Run the local-model scenario on Apple `container` and Docker through Colima. Both required engine
  paths must pass before Pi support ships; native Linux and Docker Desktop coverage is deferred.

Run the repository's required CLI checks (`cargo fmt --all -- --check`, then
`cargo clippy --all-targets -- -D warnings`, then `cargo test`), proxy checks (`gofmt`, `go vet`,
`go test`, `govulncheck`), and `actionlint` for workflow changes. Unit tests do not substitute for
the engine probes or live Pi scenarios. Record commands, versions, outcomes, and any unrun gate.

## Deferred work

- Native Linux Docker and Docker Desktop route implementation/verification as needed; neither is
  a prerequisite for the Apple `container`/Colima implementation or this ship run's completion.
- Private LAN inference endpoints.
- Automatic provider-domain discovery or authorization.
- Automatic host credential or credential-environment import.
- Provider/runtime-specific setup commands for LM Studio, Ollama, llama.cpp, MLX, or vLLM.
- Remote Docker daemon networking beyond the verified CLI-host broker routes.
