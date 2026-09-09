# Proxy consumer contract

This document fixes the externally observable egress-proxy interface. Rows in
`testdata/` are normative. Fields use literal bytes unless a row says otherwise;
TSV uses `\\n` for a line-feed and an empty cell for no value. Error wording,
timestamps, scheduling, allocation, and internal layout are not part of this
contract.

## Process and startup

The process reads `VHRN_ALLOWLISTS` when nonempty as a comma-separated public
policy path list. Otherwise it reads singular `VHRN_ALLOWLIST`; otherwise the
sole public path is `/etc/vhrn/allowlist`. An empty item in a plural list is a
required unreadable layer and consequently denies public traffic. `VHRN_MODE_FILE`
defaults to `/etc/vhrn/mode`, `VHRN_PROXY_LISTEN` to `:8080`, and `VHRN_DENY_LOG`
to no file output.

Local routing is disabled only when all of `VHRN_LOOPBACK_ALLOWLISTS`,
`VHRN_BROKER_ADDR`, and `VHRN_BROKER_TOKEN_FILE` are absent. Any partial set,
anything other than exactly three nonempty local policy paths, an unreadable token
file, or a token other than 64 lowercase hexadecimal bytes terminates startup.
With a complete set, a READY exchange must succeed before the HTTP listener
accepts work. Listener bind failure and readiness failure terminate startup.

The supplied sidecar interface uses port 8080, a non-root process identity, an
entry command that directly starts the proxy, and a single exposed TCP port.
The host mounts public and local policy read-only at `/etc/vhrn`, a writable
denial-log directory at `/var/log/vhrn`, and the token as
`/etc/vhrn-broker/token`; it supplies the environment names above. The service
does not require a shell or writable application filesystem.

## Public policy

Every public decision reopens every configured public policy path and the mode
path. The deployed host supplies five public layers in this order: base,
harness, global, project, and run. Their valid entries form a union. Any missing,
unreadable, empty-path, or malformed required layer, or a missing/unreadable
mode file, denies and records the request with effective mode `enforce`.
Replacement of a path is observed by the next decision even when modification
time is unchanged. A readable unknown or multi-line mode acts as `enforce` while
retaining readable entries. `proxy-modes.tsv` fixes the mode matrix.

Entries trim surrounding space, an initial `*.`, and surrounding dots; they
lowercase ASCII text. Empty labels, non-ASCII text, and characters outside ASCII
letters, digits, `-`, `_`, and `.` are invalid. A host trims outer space,
lowercases, and removes one final dot. An entry permits itself and dot-separated
subdomains only. `domain-policy.tsv` fixes these outcomes.

Public names resolve once per new connection. An empty answer set or any
forbidden answer rejects the entire set. Forbidden answers include loopback,
private, unspecified, link-local, multicast, and 100.64.0.0/10. Mapped IPv4 is
classified as its IPv4 value. The selected first permitted answer is dialed as a
numeric address; no second name lookup occurs. `ip-addresses.tsv` fixes the
address cases and mixed-answer outcome.

## HTTP and tunnels

Only absolute-form HTTP requests are forwarded. Public policy is checked before
forwarding. Forwarding preserves method,
authority, ordinary headers, and body. It removes `Proxy-Connection` and
`Proxy-Authorization`; the observed `Connection: X-Remove` header and its
`X-Remove` field remain visible to the origin. A denied
public or local request returns 403. An origin or connector failure returns 502.

For public HTTP, an origin-flushed first chunk remains buffered until origin
completion; a downstream disconnect cancels origin work. Two allowed public
requests reuse one origin connection; after live policy revocation, the next
request returns 403 without another origin request or connector call. Local HTTP
flushes each response chunk when flushing is available. A downstream
disconnect cancels the request context used for the local route. Local HTTP uses
the broker connector only and closes origin response work on downstream
cancellation. Local decisions always use the three local layers and are
independent of public `enforce`, `report`, and `open` modes.

CONNECT uses the supplied port or 443 when none is supplied. It connects before
writing 200. The public path discards HTTP-parser-buffered bytes after upgrade;
the local path relays those buffered bytes. A revoked grant blocks later
requests and tunnels but does not end an established local tunnel.
For a public CONNECT tunnel, when the upstream sends bytes then closes its write
side, the client receives those bytes followed by EOF and the upstream read side
receives EOF.
An absolute-form HTTPS request to a loopback origin with an untrusted certificate
returns 502 after verified TLS handshake failure; the origin receives zero HTTP
requests. Package certificate-root qualification is outside this contract.
`proxy-http-cases.tsv` fixes these outcomes.

## Local authority and broker exchange

Local authorities are exactly `localhost`, a decimal 127/8 IPv4 address, or
IPv6 loopback in brackets, each with a nonzero decimal port. Host spelling is
canonicalized: localhost lowercases, ports lose leading zeroes, and IPv6 becomes
`[::1]`. Numeric and localhost forms remain distinct. Every one of the three
local layers must be readable and syntactically valid; an empty record makes the
decision deny. Any matching layer grants access.

The broker receives `VHRN-BROKER/1 READY <token>\\n` during startup and
`VHRN-BROKER/1 CONNECT <token> <authority>\\n` per local connection. A token is
64 lowercase hexadecimal bytes. `OK\\n` is the only success response; a response
must complete within four bytes, and bytes already read after `OK\\n` become the
first application bytes. READY has a 3-second deadline. CONNECT has a 13-second
combined handshake deadline after its socket is connected, with a 10-second
connect timeout. A denied, partial, oversized, timed-out, or malformed response
fails the request without exposing the token in returned errors. `broker-frames.tsv`
fixes frame bytes and outcomes.

## Diagnostics and lifecycle

`GET /healthz` returns status 200 with `ok` followed by a line-feed. `GET
/__status` returns status 200, JSON containing the effective mode, and a final
line-feed. Other direct paths return 404. A public policy denial, report-mode
unmatched request, policy-file failure, or local denial records one line to the
configured denial log when writable: UTC RFC3339 timestamp, tab, destination,
line-feed. Token bytes never appear in the record, status response, or returned
broker error.
`proxy-process-cases.tsv` fixes direct endpoint, startup-validation, diagnostic,
and lifecycle observations.

SIGTERM produces a non-clean process termination within one second in the
bounded lifecycle probe. Disconnect cancellation follows the connection closure
behavior above.
