# Egress proxy consumer contract

This document is the normative contract between the `vhrn` CLI, the proxy image, and software in
the jailed container. It specifies observable behavior and security outcomes. An implementation
may choose any internal design that satisfies every requirement here.

The key words **MUST**, **MUST NOT**, **SHOULD**, and **MAY** are used as normative terms. The
contract follows [RFC 9110](https://www.rfc-editor.org/rfc/rfc9110.html) for HTTP semantics,
[RFC 9112](https://www.rfc-editor.org/rfc/rfc9112.html) for HTTP/1.1 syntax and framing,
[RFC 3986](https://www.rfc-editor.org/rfc/rfc3986.html) for URI syntax, and
[RFC 1035](https://www.rfc-editor.org/rfc/rfc1035.html) for DNS name limits. Denial-record
timestamps follow [RFC 3339](https://www.rfc-editor.org/rfc/rfc3339.html). The public address
boundary uses the IANA
[IPv4](https://www.iana.org/assignments/iana-ipv4-special-registry) and
[IPv6](https://www.iana.org/assignments/iana-ipv6-special-registry) Special-Purpose Address
Registries. Where this contract is stricter than a protocol permits, this contract wins.

## Security boundary

The proxy is an HTTP/1.0 and HTTP/1.1 forward proxy. It is the jailed container's only permitted
network peer: the container firewall permits TCP to the proxy address and port and blocks direct
DNS and every other non-loopback destination. The proxy therefore owns name resolution and all
outbound connection decisions.

The two egress capabilities are deliberately separate:

- **Public egress** authorizes a textual public host through additive domain policy, then permits a
  direct TCP dial only to a globally reachable unicast address. `enforce`, `report`, and `open`
  affect only this capability.
- **Host-loopback egress** authorizes one canonical loopback `host:port`, asks the authenticated
  host broker to dial it, and is always enforced. Public allowlists, `report`, and `open` MUST NOT
  grant or route a local destination.

The proxy matches hosts and does not inspect tunnel contents or terminate CONNECT TLS. It cannot
prevent exfiltration to an authorized domain, distinguish tenants behind that domain, or prevent
domain fronting at an authorized service. Those limitations are part of the product threat model,
not permission to weaken hostname or address checks.

## Process and image interface

The production OCI image has these stable properties:

- Its name is `vhrn-proxy`; its executable and entrypoint are `/vhrn-proxy`.
- It exposes TCP port `8080` and listens on `:8080` by default. The CLI normally overrides only the
  port value, not the protocol.
- It is a Linux `scratch` image containing a statically linked executable, with no shell or general
  userland.
- It runs as the unprivileged numeric identity `65532:65532`, does not require capabilities, and
  MUST NOT require a writable root filesystem.
- It is published for `linux/amd64` and `linux/arm64` with one behaviorally equivalent interface.

The CLI starts one detached, ephemeral (`--rm`) sidecar per agent run, on Apple `container`'s
`default` network or Docker/Colima's `bridge` network. It does not publish the proxy port to the
host. The CLI obtains the sidecar IP and injects all four of `HTTP_PROXY`, `HTTPS_PROXY`,
`http_proxy`, and `https_proxy` into the agent container as `http://<sidecar-ip>:<port>`. It also
injects `VHRN_PROXY_IP` and `VHRN_PROXY_PORT` so the agent entrypoint can pin its firewall to that
exact peer.

### Mounts

The proxy receives only these host-owned mounts:

| Container path | Access | Meaning |
| --- | --- | --- |
| `/etc/vhrn` | read-only directory | Public policy, local policy, and the selected run's live mode. |
| `/var/log/vhrn` | writable directory | Append-only denial records consumed by `vhrn net denied`. |
| `/etc/vhrn-broker/token` | read-only file | The selected run's broker token; present only with complete local configuration. |

The agent container MUST NOT receive `/etc/vhrn`, the denial-log mount, or the broker token. The
proxy MUST NOT require the project, agent configuration, credentials, container-engine socket, or
any other host path.

### Environment

Empty values count as unset for defaults and optional groups. Nonempty values have these meanings:

| Variable | Contract |
| --- | --- |
| `VHRN_PROXY_LISTEN` | TCP listen address; default `:8080`. An invalid or unbindable value is fatal. |
| `VHRN_ALLOWLISTS` | Comma-separated, nonempty public-policy paths. It takes precedence over the singular compatibility variable. The CLI supplies five paths in base, harness, global, project, run order. |
| `VHRN_ALLOWLIST` | One-path standalone compatibility input, used only when `VHRN_ALLOWLISTS` is unset. |
| `VHRN_MODE_FILE` | Live public-mode path; default `/etc/vhrn/mode`. |
| `VHRN_DENY_LOG` | Append-only denial-log path. Empty means process diagnostics only; the CLI supplies `/var/log/vhrn/denied.log`. |
| `VHRN_LOOPBACK_ALLOWLISTS` | Exactly three comma-separated, nonempty local-policy paths in global, project, run order. |
| `VHRN_BROKER_ADDR` | TCP authority for the host broker. It is never treated as a public target. |
| `VHRN_BROKER_TOKEN_FILE` | Path to the proxy-only broker token. |

If neither public allowlist variable is set, the standalone default is `/etc/vhrn/allowlist`.
Commas delimit paths and cannot be escaped; an explicitly selected list containing an empty path is
invalid. The three local variables form an all-or-none group. With all three absent, local routing
is disabled and local requests are denied. A partial group, a local path count other than three, or
any invalid address or path is a fatal configuration error.

`VHRN_PROXY_IMAGE` and `VHRN_PROXY_PORT` are host-side CLI overrides, not variables consumed from
inside the proxy process. A nonempty port override is decimal `1..=65535`; any other value aborts
the run before the agent starts. `VHRN_REGISTRY` selects the host-side registry prefix.

For a normal CLI run, the path-bearing values are exactly:

```text
VHRN_ALLOWLISTS=/etc/vhrn/runs/<run-id>/base.allow,/etc/vhrn/runs/<run-id>/harness.allow,/etc/vhrn/allow.local,/etc/vhrn/projects/<project-key>/allow.local,/etc/vhrn/runs/<run-id>/run.allow
VHRN_MODE_FILE=/etc/vhrn/runs/<run-id>/mode
VHRN_DENY_LOG=/var/log/vhrn/denied.log
VHRN_LOOPBACK_ALLOWLISTS=/etc/vhrn/loopback.allow,/etc/vhrn/projects/<project-key>/loopback.allow,/etc/vhrn/runs/<run-id>/loopback.allow
VHRN_BROKER_TOKEN_FILE=/etc/vhrn-broker/token
```

The CLI supplies `VHRN_BROKER_ADDR` as the Apple default-network gateway and ephemeral broker port,
or as `host.docker.internal:<port>` on Docker/Colima. The proxy treats either value only as an
opaque, prevalidated broker route.

### Startup and readiness

Before it can serve a request, the process MUST:

1. parse and validate all configuration without printing secret values;
2. read and validate one complete public-policy decision, and the local policy when configured;
3. verify that a configured denial log can be opened for append without truncation;
4. bind the HTTP listener; and
5. when local routing is configured, read and validate the token and complete the broker `READY`
   exchange defined below.

Any failure exits nonzero. The HTTP listener MUST NOT serve traffic before these steps complete.
Completing the broker `READY` exchange is the signal consumed by the CLI's host-side readiness
wait; failure or timeout is fatal rather than a degraded public-only start.

## Policy contract

Policy is host-owned. The proxy reads it from the mounted files and never writes, creates,
repairs, or normalizes those files.

### Public policy files

Each selected public-policy file is UTF-8 text no larger than 1 MiB containing zero or more
canonical ASCII entries, one per line. A final LF is optional. The empty file is valid. A
canonical entry is lowercase,
has no whitespace, wildcard prefix, leading dot, trailing dot, or empty label, and contains only
ASCII letters, digits, `_`, `-`, and `.` with at least one alphanumeric character. Duplicate
entries across or within layers have no additional effect. Any other nonempty line invalidates the
entire decision.

The CLI writes five additive layers:

1. the immutable vhrn base snapshot;
2. the selected harness snapshot;
3. live persistent global policy;
4. live policy for the exact canonical project; and
5. the immutable run-only `--allow` snapshot.

An entry `example.com` matches the normalized host `example.com` and any host ending in
`.example.com`. It does not match `evilexample.com` or `example.com.attacker.invalid`. A leading
`*.` in user input has already been normalized by the CLI and does not change these semantics.

Before comparison, a request reg-name is lowercased and one DNS root dot is removed. It MUST be an
ASCII DNS name or IDNA A-label form; the proxy does not convert Unicode to IDNA. Empty names,
embedded whitespace or control characters, empty labels, labels longer than 63 octets, and a name
longer than 253 octets after root-dot removal are invalid targets. An IPv4 literal uses canonical
dotted decimal without leading zeroes and matches public policy only exactly. An IPv6 literal can
be admitted by `report` or `open`, but cannot match the domain-file grammar in `enforce` mode.

The mode file contains exactly one of `enforce`, `report`, or `open`, with an optional final LF and
no other content:

| Mode | Public hostname result |
| --- | --- |
| `enforce` | An unmatched host is logged and denied. |
| `report` | An unmatched host is recorded as a would-be denial, then allowed. If a configured denial-log append fails, the request receives the `503` outcome defined below and is not dialed. |
| `open` | Every syntactically valid public host passes hostname policy without a denial record. |

All three modes still enforce target syntax, the globally reachable address boundary, and HTTP
protocol validation. `report` and `open` have no effect on local policy.

### Local policy files

Each configured local-policy file is no larger than 1 MiB and contains zero or more canonical
authorities, one per line, with an optional final LF. Blank interior lines, duplicates in
noncanonical form, malformed authorities, or a read error invalidate the entire local decision.
The union of the three valid files grants an authority.

The grammar and canonicalization are fixed by `shared/testdata/loopback-authorities.tsv`:

- `localhost:<port>`, with `localhost` case-insensitive on input and lowercase in storage;
- an exact dotted-decimal address in `127.0.0.0/8`; or
- bracketed IPv6 loopback, canonicalized to `[::1]:<port>`.

The decimal port is required, canonicalized without leading zeroes, and in `1..=65535`.
`localhost`, numeric IPv4, and IPv6 are distinct grants. IPv4-mapped IPv6, zone identifiers,
hostnames other than `localhost`, URLs, paths, whitespace, and every non-loopback address are
invalid. `localhost` resolution belongs exclusively to the broker, which tries `127.0.0.1` and
then `::1` without DNS.

### Live replacement and failure

The proxy MUST reopen and validate every required public layer and the mode for every new public
request or CONNECT decision. It MUST likewise reopen all three local layers for every new local
decision. A new request on a reused client or upstream HTTP connection still gets a new policy
decision. Implementations MUST NOT retain an allow decision across requests.

Host writes replace individual files atomically. A decision uses the values it read for that one
decision; it need not lock the host store. A missing, unreadable, oversized, non-regular, or invalid
required file makes that decision fail closed: public policy becomes empty `enforce`, or local
policy becomes empty. Other requests may be evaluated again after a correct replacement appears.
The process remains alive so a host repair can take effect.

Policy replacement is live after the container engine exposes the replaced mount entry. There is
no tighter cross-engine propagation guarantee. Revocation affects later decisions; it does not
terminate an in-flight HTTP exchange or an established CONNECT tunnel. A reusable upstream TCP
connection can remain pooled, but no later HTTP request may use it without another policy check.

## Target classification and dialing

The parsed request target, never a conflicting `Host` field, is the authority for routing and
policy. Classification occurs before any network operation:

1. A canonical explicit loopback authority takes the local path.
2. Every other syntactically valid authority takes the public path.
3. A name that is not literally `localhost` but resolves to loopback or another non-public address
   is denied by the public address boundary; it is never converted into a broker request.

Thus, a public policy entry such as `127.0.0.1` cannot grant loopback, and a private, link-local,
multicast, documentation, benchmark, or otherwise special address cannot be reached even in
`open` mode.

### Globally reachable unicast boundary

A direct public dial is eligible only when every candidate address is globally reachable unicast
under this test:

- IPv4 multicast `224.0.0.0/4` and the limited broadcast address are not unicast. For an IPv4
  unicast address that matches an IANA IPv4 Special-Purpose entry, use the most-specific entry;
  `Destination`, `Forwardable`, and `Globally Reachable` must all be `True`. A false or `N/A` value
  denies the address. Ordinary IPv4 unicast with no special-purpose match is eligible.
- IPv6 multicast is not unicast. An IPv6 address in `2000::/3` is eligible unless its most-specific
  IANA IPv6 Special-Purpose entry fails the same three-`True` test. A special-purpose IPv6 address
  outside `2000::/3` is eligible only when its most-specific entry passes that test. Every other
  IPv6 address is denied. IPv4-mapped IPv6 and scoped/zone-qualified literals are denied rather
  than reinterpreted.

This rule intentionally honors globally reachable exceptions inside broader special ranges and
denies registry entries whose reachability is indeterminate. Classification is against a reviewed
registry snapshot fixed at source/build review time; the proxy MUST NOT fetch policy or registries
at runtime. Updating the snapshot is a reviewed security change. The baseline for this contract is
the IANA registries last updated 2025-10-09.

### DNS and connection establishment

For each newly created public upstream connection, the proxy MUST:

1. resolve the already-authorized ASCII host exactly once, or use a parsed literal without DNS;
2. require between one and 64 addresses and reject the entire result if any returned address fails
   the global-unicast test;
3. retain the validated answer set and attempt only those numeric addresses, without a second name
   resolution in the connector or HTTP client; and
4. try eligible IPv6 and IPv4 answers within one cumulative 10-second DNS-and-TCP deadline, so one
   unreachable first answer does not prevent fallback.

A CNAME is not separately authorized because it is not used as the HTTP authority, but all of its
resulting addresses are subject to the same test. A DNS failure, empty answer, or timeout
does not fall back to an unchecked dial. Connection reuse is allowed only for the same scheme and
authority; a new TCP connection requires a fresh resolution and validation. The proxy MUST dial
directly and MUST NOT honor ambient proxy environment variables for its own upstream traffic.

HTTPS egress uses CONNECT exclusively. An absolute-form `https` request is rejected before policy,
DNS, or dial with `400 Bad Request`, `Connection: close`, `Content-Type: text/plain; charset=utf-8`,
and the exact body `HTTPS requires CONNECT\n`. The proxy never originates TLS. After a successful
CONNECT, it remains an opaque byte relay and does not participate in tunneled TLS. For HEAD, this
and every other response omits the body while retaining the headers that describe it.

## HTTP interface

### Accepted request forms

Ingress is cleartext HTTP/1.0 or HTTP/1.1. HTTP/2 prior knowledge, h2c upgrade, HTTP/3, and TLS on
the proxy listener are outside this interface.

- A non-CONNECT forward request MUST use absolute-form with an `http` URI, a nonempty authority,
  and no userinfo or fragment. The default origin port is 80; an explicit port must be decimal
  `1..=65535`. Absolute-form `https` receives the exact error above. Other schemes and ambiguous
  authorities receive `400 Bad Request` without a policy read or dial.
- CONNECT MUST use authority-form `host:port`. The port is mandatory and has no default. A scheme,
  userinfo, path, query, fragment, empty port, zero, overflow, or ambiguous IPv6 spelling receives
  `400 Bad Request` without a dial.
- A CONNECT request containing `Transfer-Encoding`, `Expect`, or `Content-Length` is rejected with
  `400 Bad Request` and connection close before policy, DNS, broker, or dial. This includes
  `Content-Length: 0`. Only an otherwise-unframed CONNECT can have eager tunnel bytes.
- Origin-form requests are requests to the proxy itself and are handled only by the endpoints
  below. They are never reconstructed into an outbound request from `Host`.
- `OPTIONS *` returns `204 No Content` with `Allow: GET, HEAD, OPTIONS, CONNECT` and is never
  forwarded. Any other asterisk-form request receives `400`.

HTTP/1.1 requests require one syntactically valid `Host` field as RFC 9112 specifies. For
absolute-form forwarding, the proxy ignores its value and generates `Host` from the request-target.
For CONNECT, the authority-form request-target alone selects the destination.

The total request line is limited to 8192 octets including its terminating CRLF; exceeding that
single limit returns `400 Bad Request` and closes the connection. Each request or response header
section is limited to 64 KiB of raw wire octets. The count includes every field line in full,
including its field-name, colon delimiter, field-value, and terminating CRLF, plus the terminating
empty CRLF; it excludes the request line or status line. The proxy MUST enforce this limit
incrementally and reject the section before buffering or allocating beyond the cap. Exceeding the
inbound header limit returns `431 Request Header Fields Too Large`; an oversized origin response
becomes `502 Bad Gateway`. Obsolete line folding, bare-CR tolerance, conflicting or invalid
`Content-Length`, simultaneous `Transfer-Encoding` and `Content-Length`, and a request transfer
coding whose final coding is not `chunked` are rejected with `400` and connection close. The proxy
does not repair ambiguous framing.

### Forwarding semantics

After a successful policy and connection decision, the proxy forwards the method unchanged and
uses origin-form path and query upstream. An empty path becomes `/`. For an absolute-form OPTIONS
request whose URI has an empty path and no query, the last proxy forwards `*` as RFC 9112 requires.
The proxy does not cache, follow redirects, reinterpret methods, or automatically retry a
non-idempotent request. It may retry an idempotent request only when no request body bytes reached
the origin and RFC 9110 permits the retry.

On both request and response, the proxy MUST parse `Connection`, remove every field it names, and
remove or regenerate connection-specific fields including `Connection`, `Proxy-Connection`,
`Keep-Alive`, `TE`, `Trailer`, `Transfer-Encoding`, and `Upgrade`. `Proxy-Authorization` MUST never
reach an origin, and an origin's `Proxy-Authenticate` MUST not be forwarded as if it came from this
proxy. Message framing fields may be regenerated for the outbound hop. End-to-end fields, status,
and trailers are otherwise preserved. The proxy adds or appends a `Via` member containing the
received HTTP version and the pseudonym `vhrn` (for example, `Via: 1.1 vhrn`) without exposing an
internal hostname or address. It does not synthesize `Forwarded` or `X-Forwarded-For`.

Protocol upgrade through a plain forwarded request is unsupported and receives `501 Not
Implemented`; callers use CONNECT for an opaque upgraded protocol. The proxy supports
`Expect: 100-continue`, forwards applicable informational responses, never emits content for HEAD,
and preserves the bodyless semantics of 1xx, 204, and 304 responses.

Request and response bodies have no product-level size cap. They MUST be streamed with
backpressure; application buffering is bounded to 64 KiB per direction per exchange and MUST NOT
grow with `Content-Length` or chunk count. Valid chunked bodies and permitted trailer fields remain
streaming. A body-length integer overflow is a framing error, not a reason to allocate or truncate.
Client disconnect cancels outstanding resolution, dialing, upstream I/O, and body streaming.
An error after response headers or body bytes have begun closes the affected connection; it does
not append a synthetic HTTP error to the body.

Each forward request, including one on a persistent downstream connection, gets a new live policy
decision. Upstream HTTP/1.1 keep-alive pooling is permitted only after the prior response body is
fully consumed. There is no contract-level whole-exchange deadline for an active HTTP stream; the
connection caps and cancellation rules bound proxy resource exposure.

### CONNECT tunnels

The proxy performs policy, resolution or broker authorization, and the upstream connection before
sending a successful response. Failure before tunnel establishment receives an HTTP error. Success
receives an HTTP/1.1 `200 Connection Established` header section with no `Content-Length` or
`Transfer-Encoding`, then immediately becomes an opaque byte tunnel.

For an otherwise-unframed CONNECT, bytes already read beyond the header terminator MUST be
delivered to the upstream first and in order. For a brokered connection, bytes read beyond the
broker's `OK\n` response MUST likewise be retained as the first upstream bytes. Neither boundary
may lose a TLS ClientHello or other eagerly sent data.

Relay is simultaneous in both directions with backpressure and at most 64 KiB application
buffering per direction. EOF in one direction causes a TCP write-half-close after queued bytes are
flushed while the opposite direction continues. The relay fully closes after both directions end,
on a nonrecoverable I/O error, client cancellation, or forced process shutdown. Established
tunnels are not reevaluated and survive policy replacement until one of those events.

### Response classes and safe errors

Before response bytes are committed, failures map as follows:

| Outcome | Response |
| --- | --- |
| Malformed method, framing, URI, authority, or port | `400 Bad Request` and connection close. |
| Public or local policy denial, policy-read failure, or non-global target or resolved address | `403 Forbidden`. |
| Report-mode would-be denial when the configured denial log cannot be appended | `503 Service Unavailable`, generic bounded body, and connection close; no dial. |
| Unsupported HTTP version | `505 HTTP Version Not Supported`. |
| Unsupported plain-HTTP protocol upgrade or method not implemented by a direct endpoint | `501 Not Implemented` or `405 Method Not Allowed`, respectively. |
| DNS failure, TCP refusal, invalid origin response, or broker `ERR` | `502 Bad Gateway`. |
| DNS, dial, or broker deadline | `504 Gateway Timeout`. |
| Proxy concurrency exhaustion or shutdown in progress | `503 Service Unavailable` and connection close. |

A public policy denial body is exactly `blocked by vhrn egress policy: <normalized-host>\n`. A
local policy denial body is exactly `blocked by vhrn local policy: <canonical-authority>\n`.
An absolute-form `https` request uses the exact error specified above. The report-log failure body
is exactly `proxy temporarily unavailable\n`. Other error bodies are one generic line, no more
than 1024 octets. Errors and process diagnostics MUST NOT reveal the broker address or token,
policy or mount paths, resolved private addresses, request headers or bodies, credentials, or
internal implementation errors.

## Authenticated loopback broker

The broker is the only component allowed to translate a local grant into a host-loopback TCP
connection. The proxy never dials a loopback address directly.

### Token and framing

At startup the proxy reads the token file once. The content is exactly 64 lowercase hexadecimal
ASCII characters with no newline: 32 bytes of per-run entropy encoded as hex. Invalid content is
fatal. The host creates the per-run staging directory with mode `0700` and the token file with mode
`0444`, never overwrites a colliding staging directory, and mounts only that file read-only into
the proxy. The token is sent only to `VHRN_BROKER_ADDR`, is never accepted from an HTTP client,
and is never emitted to logs, diagnostics, status, panic output, or an origin. The mount remains
proxy-only and is removed by the host broker's cleanup.

The broker protocol is newline-delimited ASCII over a fresh TCP connection. A request frame is at
most 256 octets including LF and is exactly one of:

```text
VHRN-BROKER/1 READY <token>\n
VHRN-BROKER/1 CONNECT <token> <canonical-authority>\n
```

The only responses are exactly `OK\n` and `ERR\n`. Any EOF, extra response line content, malformed
response, `ERR`, or timeout is failure. The proxy MUST bound response parsing to four octets and
must not wait for an unbounded line. It uses a fresh broker connection for readiness and for every
new local upstream TCP connection.

### Readiness and local connection flow

When local configuration is present, the proxy sends one `READY` frame during startup and waits
for `OK\n` before serving HTTP. The broker's three-second handshake deadline is the protocol bound;
the proxy MUST cancel earlier if startup is cancelled. This exchange authenticates the proxy to
the correct per-run broker and unblocks the CLI's readiness wait. It grants no authority and opens
no origin connection.

For local HTTP or CONNECT, the proxy first canonicalizes the target and rereads all three local
policy files. Only an exact grant permits a `CONNECT` frame. The host broker authenticates the
token in constant time, independently rereads global, exact-project, and run-only local policy,
and dials that exact authority only if still granted. The broker caps a run at 128 accepted
connections, applies a three-second frame deadline and a cumulative ten-second loopback-connect
deadline, and returns `ERR\n` for every authentication, syntax, policy, capacity, or dial failure
without revealing which check failed.

After `OK\n`, the broker connection is the origin byte stream. The proxy removes its handshake
deadline, retains bytes already buffered after the response line, and hands the stream to normal
HTTP forwarding or CONNECT relay. Cancellation before handoff closes it. Connection reuse for
plain local HTTP is allowed, but each later HTTP request still requires a fresh proxy-side local
policy decision; a new local TCP connection always uses a new broker `CONNECT` exchange and broker
policy recheck.

The broker is started for every supported vhrn run even when no local grants exist. Brokered runs
are supported on Apple `container` and Docker through a local Colima Unix socket. Native Linux
Docker, Docker Desktop, and remote Docker endpoints remain unsupported until separately
implemented and verified.

## Direct endpoints and diagnostics

Origin-form direct requests never leave the proxy. Only GET and HEAD are implemented:

- `/healthz` returns `200 OK`, `Content-Type: text/plain; charset=utf-8`, and `ok\n` after startup
  readiness while current required policy files remain valid and a configured denial log can be
  opened for append. An observed append failure also keeps health at `503` until a later append
  succeeds. During shutdown or while any of those checks fail, health returns `503 Service
  Unavailable` and `unhealthy\n`.
- `/__status` rereads the mode and returns `200 OK`, `Content-Type: application/json`, and exactly
  `{"mode":"enforce"}\n`, `{"mode":"report"}\n`, or `{"mode":"open"}\n`. If the mode cannot be
  read or validated, it returns `503` with the fail-closed body `{"mode":"enforce"}\n`.

HEAD returns the corresponding headers without content. Other methods on those paths return `405`
with `Allow: GET, HEAD`; any other origin-form path returns `404`. Endpoint responses contain no
allowlist entries, file paths, broker details, addresses, build identifiers, or secrets.

Every enforced public denial, enforced local denial, address-safety denial, and report-mode
would-be public denial produces a process diagnostic containing only the normalized host or
canonical authority and effective mode. When `VHRN_DENY_LOG` is configured, the proxy also appends
one complete record:

```text
<RFC3339-UTC-timestamp>\t<normalized-host-or-canonical-authority>\n
```

The timestamp uses `Z`; the target cannot contain whitespace. Records are never rewritten or
truncated by the proxy. Concurrent records MUST NOT interleave. An append failure cannot turn an
enforced public or local denial into an allow or change its `403` response. A report-mode request
that requires a record and cannot append it returns `503 Service Unavailable`, the generic body
defined above, and connection close before dialing; no record is fabricated. Operational failures
that are not policy denials go only to process diagnostics and never add misleading entries to
`vhrn net denied`. Any failed append marks the log unhealthy; the next required record retries the
append, and only a successful append clears that state.

## Resource bounds, cancellation, and shutdown

The proxy enforces these process-wide bounds in addition to the parsing and timeout limits above:

- at most 256 accepted client connections and at most 256 simultaneous upstream connections;
- no unbounded per-connection queues, request-body aggregation, response-body aggregation, DNS
  answer accumulation, diagnostic buffering, or concurrent work-item creation; and
- prompt rejection of excess work with `503` when an HTTP response can still be written, otherwise
  connection close.

The implementation may use fewer resources under its container limits but MUST support at least
128 concurrent established client connections so the proxy is not a lower ceiling than the host
broker. Slow or incomplete request headers are closed after 30 seconds. There is no idle or
absolute-duration timeout on an established CONNECT tunnel: the connection caps, cancellation,
and run lifetime are its bounds.

As PID 1, the process handles SIGTERM and SIGINT. It stops accepting new connections, makes health
fail, cancels pending DNS, dial, and broker handshakes, closes idle pooled connections, and
allows active HTTP streams and tunnels at most five seconds to drain. It then closes every
remaining socket and exits. Normal requested shutdown exits zero; startup failure, listener
failure, or an unexpected supervisor failure exits nonzero.

The host lifecycle stops the agent, proxy, broker, and run policy on normal completion and
SIGTERM. Proxy shutdown must therefore be idempotent and must not depend on policy still being
mounted. A host SIGKILL cannot be handled; broker sockets close, while the CLI's documented stale
container and token cleanup caveat remains.

## Requirement provenance and selected decisions

Every normative group has one of these bases:

| Basis | Requirements it owns |
| --- | --- |
| Stable consumer interface | Image name, entrypoint, `:8080`, numeric user, platforms, environment variables, mounts and exact CLI paths, public/local layer counts, endpoint names and bodies, denial-record shape, broker token/frame/deadline/capacity values, engine routes, and release tags. These values are consumed by the CLI, broker, packaging, status clients, or `vhrn net`. |
| Repository security invariant | Host-only live policy, fail-closed reads, public/local capability separation, proxy-only token, authenticated broker routing, globally reachable direct dials, CONNECT-only opaque HTTPS with no proxy TLS termination, ephemeral scratch/unprivileged runtime, bounded cleanup, and verified engine scope. These come from `AGENTS.md` and the sandbox threat model. |
| Governing product decision | HTTPS egress uses CONNECT exclusively: the proxy never originates TLS, and it rejects absolute-form `https` before policy, DNS, or dial. This explicit product decision governs every replacement implementation; it is not an implementation-selected behavior. |
| Protocol or registry requirement | URI and request-target forms, Host replacement, `/` and `OPTIONS *` generation, hop-by-hop removal, message framing, CONNECT response/tunnel semantics, label and port ranges, RFC 3339 timestamps, and IANA global-reachability classification. These come from the RFC and registry sources cited above. |
| Contract-selected security or operational decision | Framed-CONNECT rejection, unsupported upgrades, report-log failure behavior, and the finite limits below. They are frozen here because no existing consumer or standard selects one value, but replacement implementations require one deterministic outcome. |

The contract-selected limits are intentionally few:

| Decision | Frozen value | Rationale |
| --- | --- | --- |
| Policy input size | 1 MiB per public or local file | Bounds attacker-influenced parsing memory while far exceeding CLI-generated policy. |
| DNS answer set | 64 addresses | Bounds validation and connection-attempt work without selecting one address prematurely. |
| Public DNS plus TCP establishment | 10 seconds cumulative | Bounds a pre-response operation while allowing validated-address fallback. |
| Request line | 8192 octets, then `400` and close | Meets RFC 9112's recommended minimum support with one unambiguous over-limit outcome. |
| Request or response headers | 64 KiB of raw wire octets per section | Bounds parser memory with incremental enforcement; `431` applies inbound and `502` applies to an origin response. |
| Body and tunnel buffering | 64 KiB per direction | Requires streaming and backpressure without imposing a payload-size cap. |
| Generic error body | 1024 octets maximum | Prevents internal diagnostics or reflected input from producing unbounded errors. |
| Proxy concurrency | At most 256 client and 256 upstream connections; support at least 128 clients | Bounds process resources while not undercutting the broker's stable 128-connection capacity. |
| Incomplete request headers | 30 seconds | Bounds slow-header occupancy of a connection slot. |
| Graceful shutdown | 5 seconds before forced socket close | Gives buffered data a bounded drain window while preserving prompt host cleanup. |
| Established tunnel lifetime | No idle or absolute timeout | Preserves long-lived agent protocols; concurrency, cancellation, shutdown, and run lifetime provide the bounds. |
| Report-log append failure | `503`, fixed generic body, close, no record, no dial | Avoids both unaudited report-mode egress and a misleading denial record. |
| Plain-HTTP upgrade | `501` before forwarding | Keeps opaque upgraded protocols on the policy-checked CONNECT path. |

## Stable delivery and runtime invariants

- The CLI and proxy are a matched runtime pair. Registry installs pull `vhrn-proxy` at the CLI
  binary's release tag: a `vX.Y.Z` CLI uses `vX.Y.Z`, a nightly CLI uses `nightly`, and an otherwise
  untagged development CLI uses `latest`. The harness agent version does not select the proxy tag.
- `VHRN_REGISTRY` changes the registry prefix. `VHRN_PROXY_IMAGE` may replace the full runtime image
  reference. A `--local` harness install uses the local unqualified `vhrn-proxy` image.
- Release publishes `vX.Y.Z` and `latest`; master publishes `nightly`, `sha-<sha>`, and the dated
  nightly tag; same-repository image pull requests publish `pr-<n>` and `pr-<n>-<sha>`.
  Published immutable tags are not replaced to perform migration testing.
- Apple `container` and Docker remain supported build and run engines. The image name, executable,
  port, env and mount interface, scratch/unprivileged posture, multi-platform publication, and
  CLI release clock remain unchanged across an implementation replacement.
- The proxy remains an independently built image and does not share compiled policy, target, or
  broker types with the CLI. Compatibility is this wire/file/process contract.

## Deliberate normative corrections

These requirements are intentional corrections, not compatibility promises for incidental legacy
behavior:

- CONNECT requires RFC authority-form with an explicit valid port; malformed targets are rejected
  rather than repaired or given a default port.
- Absolute-form routing comes only from the request-target, and forwarding performs complete
  RFC hop-by-hop removal, safe framing validation, `Host` regeneration, and `Via` handling.
- Every new public connection uses one pinned DNS answer set, rejects the whole set if any address
  crosses the IANA global-unicast boundary, and can fall back among only the validated addresses.
- Absolute-form `https` is rejected before policy or network work; CONNECT is the only HTTPS route
  and remains opaque.
- HTTP bodies and tunnels stream with fixed memory bounds. Parser-buffered client and broker bytes
  are preserved, and tunnel EOF uses half-close so queued bytes and the reverse stream are not
  truncated.
- Invalid startup configuration and readiness fail before service. Live policy read or validation
  errors fail the affected decision closed, and report mode never silently allows a request whose
  required denial record could not be written.
- Denial responses and append-only records name only the canonical target and use stable bounded
  formats; internal error strings and sensitive routing details never cross the proxy boundary.
- Cancellation reaches pending network work, and signal shutdown is supervised and bounded rather
  than relying on abrupt process or socket destruction.
