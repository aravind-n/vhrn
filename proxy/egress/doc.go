// Package egress implements vhrn's default-deny egress guard: an HTTP
// CONNECT and plain-HTTP forward proxy that permits outbound connections only to
// allowlisted domains.
//
// It is split by responsibility: mode and matcher hold pure logic (the
// enforcement mode and the label-anchored domain match), policy holds the
// hot-reloaded state read from host-controlled files, dialer performs SSRF-safe
// dialing, denylog records denials, and proxy is the HTTP handler. The handler
// depends only on the Checker, Dialer, and DenyRecorder interfaces, so each
// collaborator can vary — and be faked in tests — independently.
package egress
