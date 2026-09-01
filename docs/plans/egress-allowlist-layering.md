# Layered egress policy

Implementation plan for making egress policy derived, per-run, and reversible. Reviewed against
`f97c11d`.

The finished behavior has four user-visible properties:

- removing a domain from `config.toml` removes it from the next run;
- `--allow` and `--open-net` affect only the run that received the flag;
- `vhrn net allow` and `vhrn net deny` update active runs before the command returns;
- one harness's built-in domains never appear in another harness's run.

The host remains the policy owner. Policy files are mounted read-only into the proxy and are never
mounted into the agent container. A missing required layer or mode file fails closed.

## Policy model

### Ordered layers

The effective allowlist is the normalized union of five layers. Ordering controls stable output and
provenance; it does not grant an overriding deny because every layer is additive.

| Layer | Source | Lifetime |
| --- | --- | --- |
| 0 — base | `BASE_ALLOWLIST` in `src/net.rs` | compiled into the CLI; snapshotted at launch |
| 1 — harness | `Harness::allow_domains` | only the harness being run; snapshotted at launch |
| 2 — config | `[net].allow` in host `config.toml` | snapshotted at launch |
| 3 — machine | `allow.local` plus the migration-only `allow.legacy` | shared across harnesses; live |
| 4 — session | wrapper `--allow` values | only that run; snapshotted at launch |

Layers 0, 1, 2, and 4 never change underneath a running session. Editing `config.toml` therefore
changes the next run, not an already-running one. Layer 3 is intentionally live: changing it through
`vhrn net allow` or `vhrn net deny` affects every active proxy on its next policy check.

Layer 3 is deliberately machine-wide, not per-harness. `vhrn net allow openai.com` during a Codex run
also widens Claude's egress, and keeps it widened. Per-harness isolation is a property of the
*built-in* layers 0 and 1; layer 3 is the single host-owned escape hatch, and `net status --domains`
is what makes its contents discoverable. Do not add a per-harness variant of `net allow` here.

Keep the current six base domains:

```text
github.com
githubusercontent.com
registry.npmjs.org
pypi.org
files.pythonhosted.org
astral.sh
```

Do not add `ghcr.io` to the base layer. vhrn pulls its own images from the host, outside the guarded
container; a user who needs GHCR inside a run can add it through config or `vhrn net allow`. A plain
Claude run resolves to 6 base + 5 Claude domains = 11 domains. A plain Codex run resolves to 6 base +
2 Codex domains = 8 domains.

### Normalization contract

Rust owns input validation, and Go keeps the same comparison normalization as defense in depth.
Normalize each entry by trimming whitespace, lowercasing ASCII, removing one leading `*.`, and
trimming leading and trailing dots. Reject an entry when the result:

- is empty or non-ASCII;
- contains an empty label;
- contains a byte other than ASCII alphanumeric, `-`, `_`, or `.`;
- contains no ASCII alphanumeric byte.

This admits DNS names, single-label names, and IPv4 literals while rejecting ports, URLs, comments,
wildcards in the middle, and embedded newlines. Reject a non-ASCII entry with an error naming the
punycode form to type instead (`xn--…`), not a bare "non-ASCII" complaint: an internationalized
domain used to land in the hand-edited file unchallenged, so the rejection has to say what to do
about it. Migration parses the old file the way the proxy does
today: strip a trailing `#` comment first, then normalize. Invalid old lines are preserved in the old
backup but are not imported.

`resolve_layers` returns every source for a normalized domain rather than only the first:

```rust
struct ResolvedDomain {
    domain: String,
    sources: Vec<LayerSource>,
}
```

Use a `HashSet` only for membership and a `Vec` for output so layer order and source order remain
byte-stable.

## Host state and mounts

### Paths

Move egress state out of the cache and into `${XDG_STATE_HOME:-~/.local/state}/vhrn/net`:

```text
net/
  policy.lock
  migration-v1
  allow.local
  allow.legacy
  log/
    denied.log
  runs/
    <pid>-<start-nanos>/
      lease
      harness
      base.allow
      harness.allow
      config.allow
      session.allow
      mode
```

`allow.local` is the only destination written by `net allow`. `allow.legacy` holds ambiguous entries
imported from the old append-only file and is otherwise never appended to. Both files are semantic
layer 3, and `net deny` can remove an entry from either one.

Create `net/`, `runs/`, and published run directories as `0755`; create proxy-readable policy and
metadata files as `0644`. Create `policy.lock` and `lease` as `0600`. Keep only `log/` world-writable
(`0777`) and `denied.log` `0666`, because the proxy runs as uid 65532 and must append there.

This narrowing is a security fix, not housekeeping. Today `NetPolicy::ensure` chmods the entire policy
dir `0777`, so the proxy — the one component whose compromise this design actually budgets for — can
unlink and replace the allowlist it exists to enforce, whatever the individual file modes say. Under
the split above it can write the deny log and nothing else. Say so in the changelog and in the
AGENTS.md egress invariant rather than landing it silently.

Mount the policy root into the proxy as `/etc/vhrn:ro`. Mount `net/log` separately at
`/var/log/vhrn` read-write and keep `VHRN_DENY_LOG=/var/log/vhrn/denied.log`. The agent container gets
neither mount.

### Direct layer loading

The proxy reads the layer files directly; the host does not fan a regenerated allowlist out to every
active run. `start_proxy` sets one ordered comma-separated variable:

```text
VHRN_ALLOWLISTS=/etc/vhrn/runs/<id>/base.allow,
                /etc/vhrn/runs/<id>/harness.allow,
                /etc/vhrn/runs/<id>/config.allow,
                /etc/vhrn/allow.local,
                /etc/vhrn/allow.legacy,
                /etc/vhrn/runs/<id>/session.allow
```

The actual environment value is one line with no spaces. Every named file is required and is created
before the proxy starts, including empty config, local, legacy, and session files. If any named file
cannot be read, the effective allowlist is empty. `VHRN_MODE_FILE` points at the run's own `mode`
file.

The separator is a comma, and the state root comes from `$XDG_STATE_HOME`, so a root containing a
comma would truncate a path — and because an unreadable path empties the allowlist, the run would
silently become deny-all. Reject a comma in the resolved state root at startup with an explicit
error instead of letting it degrade into a mystery.

This arrangement makes a layer-3 edit a single atomic file replacement. Concurrent runs keep their
own harness, config, session, and mode snapshots while seeing the same machine-local update.

## Synchronization and lifecycle

### Policy transaction lock

Use `std::fs::File::lock` on `policy.lock`; no dependency is needed. Hold the exclusive lock around:

- first-use migration;
- reads and read-modify-writes of `allow.local` and `allow.legacy`;
- active-run discovery and reaping *other* processes' abandoned runs;
- publishing a run directory;
- truncating the deny log;
- `net open`, `net guard`, and `net report` updates to active mode files.

Release it before starting an engine, waiting on a container, or doing any network operation. Atomic
rename still protects proxy readers; the lock serializes host writers and prevents lost updates.

**The lock is not reentrant.** `flock` is per-open-file-description, so a second `File::lock` on
`policy.lock` from the *same* process blocks forever rather than succeeding — verified on stable
rustc. Nothing reachable from a `Drop` implementation or from the signal thread may take it, or a `?`
unwinding mid-transaction, or a SIGTERM arriving mid-transaction, hangs the wrapper with the user's
own interrupt already spent. Removing a run you own is therefore lock-free (below), and the lock is
only ever acquired on the main thread at a point where it is provably not already held.

`write_atomic(path, contents, mode)` must create a unique same-directory temporary file with
`create_new`, write the complete contents, apply the requested mode, and rename it over the target.
Remove the temporary file on error. Use it for both mutable allow files and live mode changes.

### Run publication and lease

Generate the run id as `<pid>-<nanos>`, where `nanos` is a `SystemTime::now()` reading captured at
launch — not the kernel's process start time, which would need `/proc` (absent on the macOS host) or a
libproc/sysctl dependency this crate cannot take under `#![forbid(unsafe_code)]`. The id only has to
be unique; the lease, not the id, is what proves liveness. Under the policy lock:

1. Reap abandoned published runs using their leases.
2. Create `runs/.<id>.tmp`.
3. Create and exclusively lock its `lease` file.
4. Write the harness name, four normalized layer snapshots, and resolved mode.
5. Rename the complete directory to `runs/<id>`.

Return a `PolicyRun` value holding the open, locked lease. Bind it immediately after `prepare_policy`
returns, before tool-image building or proxy startup, so every later `?` path cleans up.

Removing your *own* run is lock-free: rename `runs/<id>` to `runs/.<id>.dead`, then `remove_dir_all`
it. No other process can be mid-transaction on a directory whose lease you hold, so the only thing
needing exclusion is an enumerator seeing a half-deleted tree — and enumerators already skip
dot-prefixed names. Both `PolicyRun::Drop` and the signal path use this, and both must be idempotent
so the two can race harmlessly.

Create `ProxyGuard` after `PolicyRun`. Reverse drop order then stops the proxy before its policy files
are removed. The SIGTERM path currently calls `process::exit`, so it must explicitly stop the proxy
and remove its run directory before exiting; destructors are only the normal/error-path fallback.
Share that removal with `Drop` through a handle the signal thread can hold — never by taking the
policy lock on that thread. SIGKILL and host crashes are handled on the next policy operation.

To reap, open each published run's `lease` and call `try_lock`:

- `WouldBlock` means its wrapper process still holds the lease; keep it regardless of age;
- a successful lock means the owning process is gone; remove the directory;
- a missing or malformed lease means an invalid published run; remove it.

Ignore dot-prefixed construction and teardown directories (`.<id>.tmp`, `.<id>.dead`) while
enumerating active runs, then remove abandoned ones while holding the policy lock. There is no
time-based sweep, so a week-long active session is never mistaken for stale.

A SIGKILLed wrapper leaves its proxy and agent containers running — that is true today and this change
does not fix it. Reaping its run directory drops that orphaned proxy to deny-all, which is the correct
direction to fail; cleaning up orphaned containers stays out of scope.

### Per-run mode

Resolve mode at launch exactly as today: `--open-net` wins, then host config, then `enforce`. Store the
result in the run directory and point only that run's proxy at it. This makes `--open-net` genuinely
session-scoped even when another harness is already running.

`vhrn net open`, `guard`, and `report` retain their live-control role by atomically rewriting every
active run's mode file under the policy lock. They do not change `config.toml` or a future run. With
no active runs they print `no active runs; future runs use config.toml` and exit successfully.

### Deny log

`denied.log` stays a single shared file (see Non-goals), which makes the per-run truncation that
`prepare_policy` performs today actively wrong: with concurrent runs now a first-class case, launching
a second harness would wipe a still-running session's denial record. **Do not truncate at run start.**

Truncate at run publication instead, inside the same locked section: after reaping, if no run remains
published, the log provably belongs to nobody and can be emptied before this run's directory lands.
`vhrn net denied` then reports denials since the last moment the machine was idle rather than "this
session", and its `USAGE` line in `src/cli.rs` has to be reworded to match.

## Proxy changes

Change `proxy/main.go` to use `VHRN_ALLOWLISTS` when it is non-empty; otherwise retain
`VHRN_ALLOWLIST` as the single-file fallback for standalone use and compatibility diagnostics.
Construct `Policy` with an ordered path slice and one mode path.

In `proxy/egress/policy.go`, remove mtime caching. A policy check reads every allowlist path and the
mode file afresh. CONNECT and plain-HTTP policy checks happen once per outbound connection/request,
not once per tunneled packet, and the files are tiny. Deterministic correctness is worth these local
reads.

The read rules are:

- any unreadable required allowlist path produces an empty effective allowlist;
- a missing or unreadable mode produces `enforce`;
- blank lines and comments remain ignored;
- entries from all files are normalized and unioned before matching;
- an established tunnel is unaffected by later policy edits, as it is today.

Atomic host replacement means a check sees either the old complete layer or the new complete layer.
Because each check opens paths again, a replacement with the same mtime is still observed.

## CLI behavior

### Run and install

Remove `seed_allowlist` from `vhrn install`; installation pulls images and writes host installation
state only. At run time, pass the selected `Harness`, config allowlist, wrapper session allowlist,
resolved mode, cache root, and state root into `prepare_policy`.

Add `vhrn_state_from(home, xdg_state)` and `vhrn_state(home)` beside the cache helpers. Carry the
resolved vhrn state root on `ContainerConfig`; keep harness login state, session databases, sandbox
copies, build layers, and gitconfig under the existing cache root.

Update `start_proxy` to accept a `PolicyRun`, emit the ordered layer paths and per-run mode path, mount
the policy root read-only, and mount only the log directory read-write. Include the run id in the
proxy container name.

### `vhrn net`

Every `net` command resolves home, cache, and state paths, opens the policy store, runs migration if
needed, and reaps abandoned runs before inspecting or mutating state.

Only the commands that actually read layer 2 — `status` and `deny` — load and validate the host
config, and for those a malformed config fails before any policy mutation. `denied`, `allow`, `open`,
`guard`, and `report` never touch it: `net denied` is the command you reach for when the setup is
already broken, so a malformed `config.toml` must not be fatal there.

Commands behave as follows:

- `net allow <domain>...` validates the whole batch, then adds missing normalized entries to
  `allow.local` in argument order. It never writes `allow.legacy`. Print success only after the atomic
  replacement completes.
- `net deny <domain>...` validates the whole batch and requires every entry to exist in
  `allow.local`, `allow.legacy`, or both. If any entry is absent from layer 3, print its other known
  provenance and exit 1 without changing either file — exit 2 is reserved for usage errors throughout
  `src/cli.rs`, and `vhrn net deny github.com` is a well-formed command against a layer the user
  cannot edit, not a typo. Otherwise remove the batch from both files,
  write each changed file atomically, and report any domains that remain allowed by a base, harness,
  config, or active session layer.
- `net status` prints the configured mode for future runs, layer-3 counts, and one line per active
  run with id, harness, current mode, and effective domain count.
- `net status --domains` additionally prints normalized domains with all their provenance. For an
  active run, use its snapshots plus current layer 3. With no active runs, show shared base/config/
  machine layers followed by each registered harness's layer-1 domains.
- `net open`, `guard`, and `report` update every active run as described above.
- `net denied` keeps the existing shared-log behavior in this change.

All batch operations are validation-atomic: a bad or non-removable member prevents changes to every
other member. Exit 2 is reserved for argument-shape errors (no domains given, unknown subcommand);
policy-state and filesystem failures return exit 1 and leave the last successfully renamed complete
file visible. No command prints a success message before all of its intended writes finish.

## Migration

Migration runs under the new policy lock and is complete only when `migration-v1` exists. It is
idempotent across crashes:

1. Ensure empty `allow.local` and `allow.legacy` files exist without replacing existing new-format
   files.
2. If `~/.cache/vhrn/net/allowlist` (or its XDG cache equivalent) exists, parse it using the legacy
   comment rules and current normalization. Record the line number of each rejected non-comment
   entry for the migration notice.
3. Subtract the current base layer, every currently registered harness layer, the current config
   layer, and entries already present in `allow.local`.
4. Write the remainder to `allow.legacy` with a header explaining its ambiguous provenance.
5. Atomically write `migration-v1` last.
6. Print one notice naming the old file, `allow.legacy`, and `vhrn net status --domains`; if any old
   entries were rejected, include their line numbers and state that the untouched old file is the
   backup. The notice must also announce that built-in domains are now per-harness. Subtracting every
   registered harness's layer in step 3 means a domain the shared list used to hand both agents —
   `sentry.io`, seeded by installing claude — leaves no trace in `allow.legacy` and is simply gone
   from a codex run. That is the intended fix, but an unannounced first failure after upgrade reads
   as a regression.

The remainder is deliberately called legacy, not hand-authored. The old append-only file cannot
distinguish direct edits from historical `--allow` values or config entries removed before upgrade.
Preserving the remainder avoids silently narrowing an existing installation; provenance and
`net deny` give the user a removal path.

Leave the old cache directory untouched as a backup and never read it again after the marker exists.
Do not import its mode: modes are now per-run and future runs derive them from config. Do not import
its deny log; denial history is diagnostic rather than policy.

If migration stops before the marker rename, the next invocation recomputes `allow.legacy` from the
same old source without overwriting `allow.local`. If no old file exists, create the empty legacy file
and marker so a stale cache file appearing later is never imported unexpectedly.

## Implementation sequence

### 1. Build the Rust policy core

In `src/net.rs`:

- replace `DEFAULT_ALLOWLIST`, `NetPolicy`, seeding, and append helpers with normalization,
  provenance, `PolicyStore`, the transaction lock, atomic file replacement, migration, run
  publication, and lease cleanup;
- keep mode parsing/resolution and denied-domain parsing;
- drop `truncate_deny_log` from the run path and move truncation behind the no-published-run check;
- make filesystem edges return errors instead of silently ignoring policy writes.

Completion criterion: pure layer tests, migration tests, writer-lock tests, and lease-lifecycle tests
all pass without starting a container, and no code path can take the policy lock twice in one
process.

### 2. Make proxy reload exact

In `proxy/main.go` and `proxy/egress/policy.go`:

- accept ordered allowlist paths;
- reload by path on every decision;
- preserve fail-closed behavior and matching semantics.

Completion criterion: Go tests prove multi-layer union, missing-layer fail-closed behavior, and reload
after same-mtime atomic replacement for both allowlist and mode.

### 3. Wire per-run policy into the run path

In `src/run.rs`:

- add XDG state resolution and the `ContainerConfig` field;
- create `PolicyRun` before fallible image/proxy work;
- change proxy arguments, mounts, env, name, and guards;
- give `stop_on_signal` the lock-free run removal alongside `proxy.stop()`, so the SIGTERM path
  cleans up before `process::exit` without touching the policy lock;
- point each proxy at its own mode and launch layers.

Completion criterion: argument golden tests show a read-only policy mount, a separate writable log
mount, the six ordered layer paths, and no policy mount in the agent container; drop and explicit
signal cleanup tests leave no published run directory.

### 4. Finish CLI semantics

In `src/cli.rs`, `src/harness.rs`, and `src/net.rs`:

- remove install-time egress seeding and update stale comments;
- implement `deny` and `status --domains`;
- make live mode commands operate on active run files;
- update usage text to state the exact lifetime of flags and machine-local commands.

Completion criterion: CLI-facing unit tests cover all-or-nothing batches, all-source provenance,
per-harness isolation, config removal on relaunch, session ephemerality, concurrent local additions,
and per-run mode isolation.

### 5. Update documentation and release metadata

Update the active sources of truth and every user-facing stale path or behavior reference:

- `AGENTS.md` egress-security and persistence invariants;
- `README.md` install, usage, configuration, and multi-harness text — "Harnesses share one egress
  allowlist" is no longer true;
- `docs/sandbox-design.md`, including the instruction to edit `~/.cache/vhrn/net/allowlist`: that
  hand-edited file is gone, and `[net].allow` plus `vhrn net allow` replace it;
- `docs/adding-a-harness.md`;
- the `USAGE` text in `src/cli.rs` — the install line still says `seed egress`, and the `net`
  subcommand block needs `deny`, the `--domains` flag, and the reworded `net denied`;
- the container guide in `src/persist.rs` only where wording changes;
- `CHANGELOG.md`, under `[Unreleased]` only. The version bump is the release runbook's job
  (`docs/runbooks/release.md` steps 1–2 promote `[Unreleased]` and edit `Cargo.toml` at tag time),
  so do not touch `Cargo.toml` or `Cargo.lock` in this change.

The changelog records `net deny` and provenance status as Added; layered/per-run policy, the state
path, per-run modes, and the policy dir dropping from `0777` to `0755` as Changed; install-time
seeding as Removed; and persistent `--allow` plus cross-run `--open-net` as Fixed.

Completion criterion: `rg` finds no live documentation or usage text claiming that install seeds
egress, policy lives under the cache, harnesses share one allowlist, or either wrapper flag affects
another run.

## Tests

Rust unit coverage must include:

- normalization and rejection, including fixtures shared conceptually with Go normalization;
- ordered union, stable output, duplicates with multiple provenance sources, and empty layers;
- base/Claude/Codex isolation and expected default counts;
- config removal affecting a new run while an existing snapshot remains unchanged;
- session domains existing only in their originating run;
- two simultaneous `allow.local` read-modify-writes retaining both additions under the file lock;
- `net deny` validation atomicity and removal from local, legacy, or both;
- migration with defaults, every harness, current config, historical session/config residue, invalid
  legacy lines, a pre-existing local file, and interruption before the marker;
- XDG state resolution for set, unset, and empty `XDG_STATE_HOME`;
- an active lease surviving cleanup regardless of age, and the same run being removed after its
  lease is released;
- policy-run cleanup on ordinary return and an injected error before proxy startup;
- run removal never taking the policy lock: a test that holds the lock and *then* drops a `PolicyRun`
  must complete rather than hang, and removal must be idempotent when run twice;
- deny-log truncation happening only when no run is published, and a second run leaving a first run's
  denial record intact;
- a comma in the resolved state root rejected at startup;
- per-run mode isolation and live all-run mode commands;
- proxy and agent mount/argument golden tests.

Go coverage must include:

- multi-file union, including duplicates across files and an entry present only in the last file —
  matching is order-independent on this side, so do not write a test that merely asserts the paths
  were visited in order;
- any required layer missing or unreadable produces deny-all under enforce;
- missing mode produces enforce;
- same-mtime replacement of an allowlist file is visible on the next check;
- same-mtime replacement of a mode file is visible on the next check;
- existing label-anchored hostname matching and HTTP/CONNECT behavior remain unchanged.

## Verification

Run the repository checks independently:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

```sh
cd proxy
gofmt -l .
go vet ./...
go test ./...
```

Then exercise the live path with locally built images:

1. Build and install the CLI, images, and proxy; install both harnesses with `--local`.
2. Confirm migration creates `allow.legacy`, writes the marker last, reports ambiguous entries, and
   leaves the old cache directory untouched.
3. Start Claude and Codex concurrently. Confirm each proxy's layer list contains only its own harness
   file and that the policy mount is read-only.
4. Run `vhrn net allow example.test`, then retry in both existing sessions. Both must succeed without
   restart.
5. Immediately run `vhrn net deny example.test`, preserving file mtimes if necessary to exercise the
   old failure mode. Both sessions must block it on their next request.
6. Launch Claude with `--allow docs.rs`; confirm Codex does not gain it and a later plain Claude run
   does not retain it.
7. Launch Codex with `--open-net`; confirm an already-running Claude proxy remains in enforce mode.
   Run `vhrn net guard` and confirm it updates every active run.
8. Add a config domain and launch; remove it and launch again. The first existing run keeps its
   launch snapshot, while the new run omits the domain.
9. Keep a run alive while forcing its directory timestamps older than a day; a policy operation must
   retain it because its lease is locked.
10. Kill one wrapper with SIGTERM and confirm immediate cleanup, and that the wrapper exits rather
    than hanging on the policy lock. Kill another with SIGKILL, run `vhrn net status`, and confirm
    lease cleanup removes only the crashed run — its orphaned proxy and agent containers outlive the
    wrapper, and reaping the run directory correctly drops that proxy to deny-all.
11. With two runs active, confirm launching the second leaves the first's `vhrn net denied` output
    intact, and that the log is truncated only once every run has exited.

## Non-goals

- Harness login/state, sandbox copies, tools layers, gitconfig, and Codex session databases remain in
  the cache hierarchy.
- `denied.log` remains shared, truncated only when no run is published. Partitioning or retaining
  per-run denial logs is a separate CLI design.
- Orphaned proxy and agent containers left by a SIGKILLed wrapper are not reaped. Unchanged from
  today.
- Layering remains additive. Removing a base, harness, config, or session domain requires changing
  that source and starting a new run where applicable; `net deny` is not a negative override layer.
- Project-local vhrn configuration remains out of scope and remains prohibited by the host-owned
  configuration boundary.
