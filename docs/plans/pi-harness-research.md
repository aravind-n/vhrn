# Pi harness support

Status: **implementation-ready, deferred.** This plan records the researched behavior and the
decisions for adding Pi to vhrn. Do not implement it until both
[`egress-allowlist-layering.md`](egress-allowlist-layering.md) and
[`per-project-config.md`](per-project-config.md) have landed. Rebase this plan against their actual
interfaces before starting; they remain the sources of truth for public-domain policy, policy
scopes, canonical project identity, and host-owned project configuration.

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
| `extensions/`, `skills/`, `prompts/`, `themes/`, keybindings | User-managed capabilities and presentation |
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

Pi also has an unresolved plain-HTTP proxy regression after its Undici change; a successful first
request is not enough to establish compatibility. The eventual implementation must exercise a
streaming model request, a tool call, the following model request, and connection reuse. Track
[Pi issue #8134](https://github.com/earendil-works/pi/issues/8134) during implementation, but do not
make correctness depend on it being fixed upstream.

## Required interfaces

Local inference is a distinct host-owned capability layered alongside, but never folded into, the
public-domain allowlist. It is not a `[net]` TOML setting and is never inferred from Pi
configuration.

```text
vhrn net allow [--project <path>] --local <host:port>...
vhrn net deny  [--project <path>] --local <host:port>...
vhrn net status [--local]

vhrn pi --allow --local <host:port>... [--] [pi arguments...]
```

The persistent forms use the global or exact-project scope defined by the scoped-egress design.
The wrapper form grants access only to that Pi run and writes no persistent state. Wrapper parsing
must still forward every unconsumed Pi argument verbatim.

V1 accepts only an explicit port paired with `localhost`, an address in `127.0.0.0/8`, or IPv6
loopback (`[::1]:port`). Normalize equivalent spellings before deduplication. Reject URLs, paths,
wildcards, CIDRs, missing or zero ports, unspecified addresses, LAN/private addresses, link-local
addresses, metadata targets, and arbitrary DNS names. A grant authorizes only that exact normalized
authority. It remains required in report or open mode.

Store loopback grants in separate policy files alongside the corresponding domain layers: a global
`loopback.allow`, an exact-project `loopback.allow`, and a run snapshot. Do not overload domain
records or add local endpoints to `config.toml`. The scoped-egress implementation remains
authoritative for state-root resolution, atomic mutation, locking, project keys, run publication,
provenance, and live-policy semantics.

## Local endpoint architecture

For each run with an effective local grant, vhrn starts a short-lived host broker. Pi retains its
native URL and the data path is:

```text
Pi -> vhrn proxy -> authenticated host broker -> configured host-loopback endpoint
```

The proxy recognizes only authorities in the resolved local policy and routes them to the broker
without weakening `SafeDialer`. The broker independently validates the requested normalized
authority, then connects only to that host-loopback address and port. Both ends fail closed on
missing, unreadable, or malformed policy.

Generate a random 256-bit capability token for every run and expose it only to the proxy and broker,
never to the agent container. Require a bounded authenticated handshake before accepting a relay.
The token identifies the run but does not replace authority validation. Bind the broker only to the
engine-reachable host gateway, not `0.0.0.0`, and tie broker cleanup to the existing proxy/run guard
lifecycle. A broker startup or routing failure aborts the run rather than silently bypassing the
guard.

Keep engine routing behind the run implementation:

- Apple `container`: resolve the selected network gateway from engine inspection and bind the
  broker to it.
- Native Linux Docker: use the bridge host gateway and provide its stable in-container route.
- Docker Desktop: use `host.docker.internal` while binding the broker only on the corresponding
  reachable host interface.

A non-privileged feasibility probe on Apple `container` 1.3.1 successfully reached an HTTP server
bound only to the inspected default-network gateway (`192.168.64.1`) from an ephemeral container.
This avoids the documented host-DNS setup that requires `sudo`, disables Private Relay, and is lost
on restart. It proves the basic route, not the complete authenticated proxy/broker flow; the latter
remains a release gate.

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
the base image. Add the normal image-build matrix entry and version probe; do not add Pi-specific
CLI dispatch.

Use `PI_CODING_AGENT_DIR` and nested mounts to preserve the following ownership split:

| Pi data | Vhrn behavior |
| --- | --- |
| `models.json`, keybindings, manually managed extensions, skills, prompts, themes | Mirror from the host's `~/.pi/agent` on every run |
| `settings.json` | Seed once only when absent, then leave container-owned |
| `auth.json` | Container-owned; never bootstrap from the host |
| `trust.json` | Container-owned; never bootstrap from the host |
| `models-store.json`, Pi-managed packages, and other Pi-written state | Container-owned and persistent |
| Global `AGENTS.md` | First-non-empty host guide source composed through the harness guide descriptor |
| Sessions | Persistent sibling store partitioned by canonical project key |

Generalize the existing bootstrap descriptor only as much as needed to seed an individual writable
state file such as `settings.json`. Keep this generic and declarative so Claude's credential
bootstrap and Pi's settings bootstrap remain harness data rather than run-path branches. Vhrn must
not update `settings.json` after the seed, import host trust, or merge Pi-written package/catalog
state with the disposable mirror.

Set `PI_CODING_AGENT_SESSION_DIR` to the per-project session store. Login/configuration remains
shared across projects, while sessions are explicitly partitioned even though Pi currently groups
them by working directory itself.

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

The Pi harness has an empty default domain layer. Do not automatically grant model vendors or
`pi.dev`; users grant every required public endpoint at global, project, or run scope. This avoids a
broad and changing commercial allowlist and keeps optional Pi operational traffic auditable.

## Implementation order and gates

1. Land scoped egress exactly far enough to provide its host-owned state model, global/project/run
   scopes, live policy loading, and run lifecycle.
2. Land per-project configuration so the run path has one canonical project identity shared by
   configuration, sessions, and egress.
3. Rebase this document against the shipped interfaces. Resolve mechanical naming differences in
   this plan; do not reopen the security and ownership decisions without new evidence.
4. Implement generic loopback policy and the authenticated host broker before adding Pi. Validate
   the broker with a minimal HTTP client through the real proxy on each supported engine.
5. Add the Pi harness descriptor, image, persistence layers, and image publishing matrix.
6. Add active user documentation, help text, changelog entries, and `AGENTS.md` invariants only when
   implementation begins, so unshipped interfaces are not documented as available.

Do not ship a reduced remote-only Pi harness. If exact host-loopback inference cannot work safely on
both Apple `container` and Docker without privileged host setup, defer the entire Pi harness.

## Required tests

### CLI and policy

- Parse persistent and run-scoped typed-local grants without consuming Pi arguments.
- Normalize, deduplicate, revoke, and report global, project, and run grants with provenance.
- Reject URLs, missing ports, wildcards, CIDRs, non-loopback targets, metadata targets, and malformed
  IPv6 authorities.
- Prove that public domain grants, report mode, and open mode cannot authorize loopback traffic.

### Proxy and broker

- Route both plain HTTP and CONNECT only for an exact authorized authority.
- Reject missing/incorrect tokens, authority substitution, cross-run reuse, malformed handshakes,
  and unreadable policy.
- Preserve every existing private-address and public-domain test.
- Prove broker startup and teardown are fail-closed and leave no reusable listener or token.

### Pi persistence and image

- Verify user-managed inputs mirror from the host while Pi-written state persists independently.
- Verify `settings.json` seeds once, later Pi changes survive, and host changes do not overwrite it.
- Verify neither `auth.json` nor `trust.json` is imported from the host.
- Verify package/catalog state persists and sessions select different stores for different projects.
- Build the Pi image on both engine paths and verify the baked Pi version command.

### End to end

- Use a deterministic fake OpenAI-compatible streaming server bound only to host loopback.
- Exercise model response -> tool call -> model response, including a reused HTTP connection.
- Prove an ungranted port is denied while the granted port succeeds in the same run.
- Exercise a remote OpenAI-compatible endpoint and an Anthropic-compatible endpoint through
  explicit domain grants.
- Run the local-model scenario on Apple `container`, native Linux Docker, and Docker Desktop. Both
  supported engine families must pass before Pi support ships.

## Deferred work

- Private LAN inference endpoints.
- Automatic provider-domain discovery or authorization.
- Automatic host credential or credential-environment import.
- Provider/runtime-specific setup commands for LM Studio, Ollama, llama.cpp, MLX, or vLLM.
- Any implementation before the two prerequisite plans have landed and this plan has been rebased.
