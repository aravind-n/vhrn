# Rust system end-to-end tests

Status: proposed. Treat full-system E2E coverage as absent for this work; the ad hoc
scripts added during Pi development are not the foundation for the new suite.

Build a reusable Rust integration suite, run through Cargo, that exercises the real
vhrn CLI, container images, firewall, proxy, broker, and persistence together. Cover
all supported harnesses on Apple `container` and Docker through Colima, not just Pi.
Use isolated host state and deterministic Rust test fixtures; require no personal
credentials or paid model APIs.

Cover launch and argument forwarding, filesystem isolation, public and local egress
policy, streaming and cancellation, persistent settings and project sessions, and
cleanup after failure or termination. Include regressions for simultaneous cold
starts and local HTTP revocation. Keep existing Rust and Go component tests.

Run the suite in CI on runners that can actually exercise the supported engines.
Document the same Cargo command for local use. Missing engine coverage must be
reported explicitly rather than counted as passing.

The temporary approach has been removed: `tests/fake-pi-provider.py`,
`tests/pi-local-e2e.sh`, and `tests/engine-loopback-probe.sh` are gone. The Rust
suite replaces their purpose rather than translating the scripts line by line.

Separately assess moving `proxy/` from Go to Rust: weigh a single-language codebase
and shared types against rewrite risk, dependencies, and preservation of the static,
unprivileged proxy image. Establish system tests before attempting that migration;
the E2E work does not depend on deciding to rewrite the proxy.
