# Scoped egress allowlists

Implementation plan for making egress policy derived, reversible, and easy to widen at the
smallest useful scope.

The finished behavior is:

- `vhrn net allow <domain>` grants persistent machine-wide access;
- `vhrn net allow --project <path> <domain>` grants persistent access only to that exact project;
- the matching `net deny` command undoes an allow at the same scope;
- `vhrn <harness> --allow <domain>` grants access only for that run;
- base and selected-harness requirements are explicit, immutable minimums;
- global and project changes affect matching active runs on their next connection;
- the injected container guide tells the agent how to ask the user for access;
- the proxy enforces policy and names blocked hosts, but is not an instruction channel.

The host remains the policy owner. Policy files are mounted read-only into the proxy and never
mounted into the agent container. An unreadable required policy file fails closed.

## Policy model

### Five additive layers

The effective allowlist is the normalized union of:

| Layer | Source | Scope | Lifetime |
| --- | --- | --- | --- |
| base | `BASE_ALLOWLIST` | every harness | launch snapshot |
| harness | selected `Harness::allow_domains` | selected harness | launch snapshot |
| global | `net/allow.local` | every project and harness | persistent, live |
| project | `net/projects/<key>/allow.local` | exact canonical project path | persistent, live |
| run | wrapper `--allow` | originating run | launch snapshot |

Ordering controls stable output and provenance, not precedence. Every layer is additive. Base and
harness entries are absolute minimum requirements. `net deny` revokes an entry from one mutable
scope; it is not a negative override and cannot suppress another source.

Keep the current base domains:

```text
github.com
githubusercontent.com
registry.npmjs.org
pypi.org
files.pythonhosted.org
astral.sh
```

Do not add `ghcr.io`: vhrn pulls its images on the host. With empty user layers, Claude resolves to
6 base + 5 Claude domains and Codex to 6 base + 2 Codex domains. Installation never mutates egress,
so installing one harness cannot expose its vendor domains to another.

### No egress configuration

Remove `NetConfig`, `[net].allow`, and `[net].mode` from `src/config.rs`. Host `config.toml` keeps
configuring resources, tools, and run behavior; it is no longer an egress source.

`ConfigFile` remains strict. A retained `[net]` block is therefore a hard configuration error, not
an ignored table: return a targeted diagnostic telling the user to move persistent domains with
`vhrn net allow`, remove `[net]`, and retry. Commands under `vhrn net` must not load `config.toml`, so
the migration command remains usable while the stale table exists. Run, install, and update paths
that load configuration fail until it is removed.

Future runs default to `enforce`. `--open-net` opens only its run. `vhrn net open`, `guard`, and
`report` update active runs only; with none active they print:

```text
no active runs; future runs default to enforce
```

This leaves `net allow` as the one persistent interface and removes a forgotten
`mode = "open"` as a source of durable widening.

### Project identity

Canonicalize `--project` with `std::fs::canonicalize`, require an existing directory, and use the
canonical absolute path as its exact identity. Derive a bounded storage key as the SHA-256 of its
platform path bytes. Store those exact bytes in a sibling `path` metadata file for status and
collision checking; an existing key recording another path is an error.

Moving a project gives it a new identity. Reusing a path deliberately reuses its policy. This is the
same accepted path-identity contract as agent history and memory. Internal keys and run ids are not
part of the permission UX.

Render paths in status and errors through one deterministic byte-escaping helper: wrap the result in
double quotes, leave printable ASCII other than `"` and `\` unchanged, escape those two bytes, and
render every other byte as `\xNN`. Metadata and comparisons continue using the original bytes. This
keeps spaces readable and prevents newlines, terminal escapes, or non-UTF-8 bytes from forging output.

### Domain normalization and provenance

Rust validates CLI inputs; Go repeats comparison normalization as defense in depth. Trim whitespace,
lowercase ASCII, remove one leading `*.`, and trim leading/trailing dots. Reject a result that is
empty, has an empty label, contains a byte outside ASCII alphanumeric/`-`/`_`/`.`, or has no
alphanumeric byte. This rejects URLs, ports, comments, embedded newlines, and internal wildcards.

Add Rust `idna` only for guidance. Stored policy remains ASCII. Reject non-ASCII input; when UTS #46
conversion yields a valid ASCII value, name that exact `xn--…` spelling in the error. Otherwise ask
for a valid ASCII IDNA hostname without inventing a suggestion.

Keep all provenance:

```rust
struct ResolvedDomain {
    domain: String,
    sources: Vec<LayerSource>,
}

enum LayerSource {
    Base,
    Harness(String),
    Global,
    Project(PathBuf),
    Run(String),
}
```

Use a `HashSet` only for membership and a `Vec` for byte-stable layer/source output.

## Host state and proxy inputs

### Layout and permissions

Move policy from cache to `$XDG_STATE_HOME/vhrn/net`, falling back to
`~/.local/state/vhrn/net`.

Use `XDG_STATE_HOME` only when it is a non-empty absolute path. An unset, empty, or relative value
falls back to `~/.local/state`; it is never joined to the cwd or canonical project path. Keep this
resolution in a pure helper and test all four cases, so policy cannot become repository-relative.

The resulting layout is:

```text
net/
  policy.lock
  allow.local
  projects/
    <project-key>/
      path
      allow.local
  log/
    denied.log
  runs/
    <pid>-<start-nanos>/
      lease
      harness
      project-key
      project-path
      base.allow
      harness.allow
      run.allow
      mode
```

Only the CLI mutates global/project `allow.local`. Create every required file, including empty
files, before proxy startup.

Create state, project, log, run, and published-run directories host-owned `0755`; proxy-readable
policy files `0644`; host-only metadata, `policy.lock`, and `lease` `0600`; and `denied.log` `0622`.
The proxy uid can append to the log but cannot replace policy or log pathnames. This fixes the
current `0777` policy directory; record that security change in the changelog and AGENTS invariant.

Mount the policy root at `/etc/vhrn:ro` in the proxy. Separately mount only `net/log` read-write at
`/var/log/vhrn`. The agent container gets neither.

### Direct live loading

Pass five paths in one comma-separated, space-free variable:

```text
VHRN_ALLOWLISTS=/etc/vhrn/runs/<id>/base.allow,
                /etc/vhrn/runs/<id>/harness.allow,
                /etc/vhrn/allow.local,
                /etc/vhrn/projects/<project-key>/allow.local,
                /etc/vhrn/runs/<id>/run.allow
```

`VHRN_MODE_FILE` points at the run mode.

The host state root is not encoded in `VHRN_ALLOWLISTS`: it is mounted at the fixed `/etc/vhrn`
target, and every generated list path is beneath that target with comma-free run and project keys.
Do not reject a host state path because it contains a comma.

For each CONNECT decision or plain HTTP request, the proxy reopens every required allowlist and the
mode file. Any read failure forces `enforce` with an empty union even if stored mode is `open` or
`report`. Full behavior resumes on the first completely readable decision.

Same-directory atomic replacement plus reopening by path makes global/project changes live. Existing
tunnels remain open; changes apply to subsequent connections. A retry after a denied CONNECT
necessarily creates a new decision.

## Synchronization and lifecycle

Use `std::fs::File::lock` on `policy.lock` around global/project mutations, project creation,
active-run discovery/reaping, run publication, idle-only log truncation, and live mode changes.
Release it before engine startup, waits, or network work. It is not reentrant: `Drop` and signal code
must never take it.

`write_atomic(path, contents, mode)` creates a unique same-directory temporary file with
`create_new`, writes/syncs the complete contents, applies mode, and renames it. Remove the temporary
on error. Use it for every mutable allow file, mode change, and log truncation.

Generate run ids as `<pid>-<SystemTime-nanos>`; the lease proves liveness. Under the lock:

1. Reap abandoned published runs and dot-prefixed temporary/dead directories.
2. Ensure and validate global and selected-project files.
3. Create `runs/.<id>.tmp` and exclusively lock its `lease`.
4. Write metadata, base/harness/run snapshots, and mode.
5. Rename the complete directory to `runs/<id>`.

Return `PolicyRun` holding the lease and bind it before later fallible work. Its cleanup state is an
`Arc` containing the published/dead paths and an atomic once flag; both `Drop` and the signal thread
call the same lock-free operation that renames to `.<id>.dead` and removes it. Install the signal
thread immediately after publication, before guide generation, tools-image work, or engine startup.
The signal control also owns a synchronized optional proxy-cleanup handle. Inside `start_proxy`,
construct `ProxyGuard` immediately after the engine reports a successful start, publish a clone of
its idempotent cleanup handle to the signal control, and only then inspect the IP or do other fallible
work. The local guard covers errors before `start_proxy` returns.

vhrn synchronously creates the named agent under the lifecycle gate before start/attach. On SIGTERM,
cleanup removes the owned agent before proxy and policy retirement, and kills/waits the attach client.
SIGINT remains delegated to the interactive child. SIGKILL cannot run cleanup, so leases reap stale
active-run state.
Status/live-mode operations can surface `NotFound` when run retirement races their discovery.

Reaping uses `try_lock`: `WouldBlock` is live; success means abandoned; a missing/malformed lease is
invalid. There is no age-based cleanup. A SIGKILLed wrapper may leave containers, but deleting its
policy makes the orphaned proxy fail closed; container reaping remains out of scope.

Keep one shared `denied.log`. Under the lock, truncate only when publishing a run finds no other
published run. Replace it atomically as `0622` so an old symlink is replaced rather than followed.
`net denied` reports denials since the machine last had no active vhrn run.

## CLI behavior

Remove install-time seeding. At launch, canonicalize the current project once, resolve mode as
`--open-net` or `enforce`, prepare the five layers, derive the container guide using that same path,
then start proxy and agent. `--allow` writes no persistent state.

Add `vhrn_state_from(home, xdg_state)` and `vhrn_state(home)` beside cache helpers. The pure helper
accepts only a non-empty absolute XDG value and otherwise returns `home/.local/state/vhrn`. Harness
state, session databases, disposable config, tools layers, and gitconfig stay in cache.

Mutation grammar:

```text
vhrn net allow [--project <path>] <domain>...
vhrn net deny  [--project <path>] <domain>...
```

Without `--project` target global `allow.local`; with it target that canonical project's file.

- `allow` validates path and full batch, then appends missing normalized entries in argument order
  through one atomic replacement.
- `deny` requires every entry in the selected mutable file. If any is absent, print other
  provenance and exit 1 without mutation. Otherwise remove the batch atomically and report domains
  still allowed elsewhere.
- `status` prints global/project counts and each active run's id, harness, project path, mode, and
  effective count.
- `status --domains` groups persistent project entries by path and prints active effective domains
  with all provenance.
- `open`/`guard`/`report` atomically update every active mode and never future defaults.
- `denied` retains the shared-log behavior.

Global edits reach every active run; project edits reach only matching project keys. Commands return
after replacement. Batches are validation-atomic. Exit 2 is argument shape; validation/state/I/O is
exit 1. Print success only after all writes finish.

## Agent instructions and proxy boundary

The per-harness container guide is the instruction channel. Extend guide composition with the
canonical host project path and include this guarded-run guidance:

```text
Network access is default-deny. If a required hostname is blocked, tell the user the
hostname and ask them to run:

    vhrn net allow --project '<shell-quoted-canonical-path>' <domain>

For access across every project:

    vhrn net allow <domain>

Prefer project access for a project dependency. Do not recommend opening the entire
network to resolve one blocked hostname.
```

Generate the path with a tested POSIX shell-quoting helper, including spaces and single quotes. The
agent asks the user because the host-only command is not expected to work inside the container.
Continue rebuilding and mounting the guide through each harness descriptor. The open-network variant
states that the guard is already off.

The proxy stays an enforcement component. HTTP and CONNECT denials remain 403 and name the hostname,
but generate no CLI command or project path. Preserve end-to-end TLS tunneling; add no CA, TLS
interception, SOCKS replacement, or custom request protocol. CONNECT clients expose denial bodies
inconsistently, so proxy text is diagnostic rather than instructional.

## Upgrade behavior

There is no product migration. The old policy lived at
`${XDG_CACHE_HOME:-~/.cache}/vhrn/net` (normally `~/.cache/vhrn/net`). New code never reads or
imports this old cache policy. First use creates empty global and selected
project user layers. A retained `[net]` table is rejected with the targeted migration diagnostic;
`vhrn net` remains available to add its domains before the user removes the stale table.

Document the clean reset and removed config surface prominently, with `net status --domains`,
global `net allow`, and project `net allow` examples. Leave legacy cache data inert for ordinary
users. The maintainer cutover below is explicitly not product behavior.

## Implementation sequence

### 1. Build scoped policy storage

In `src/net.rs` replace `NetPolicy`, seeding, and append helpers with normalization, provenance,
`PolicyStore`, scoped mutations, locking, atomic writes, run publication, and leases. Use existing
`sha2` for project keys and add `idna` for guidance. Create exact modes and return every filesystem
error.

Completion: pure layer, fresh-store, project-identity, concurrency, permission, and lifecycle tests
pass without containers; no double-lock path exists; cache/config are not policy inputs.

### 2. Make proxy reload exact

In `proxy/main.go` and `proxy/egress/policy.go` accept `VHRN_ALLOWLISTS` while retaining singular
`VHRN_ALLOWLIST` for standalone compatibility; reopen paths per decision; fail enforce/empty on any
read error; preserve matching and generic denials.

Completion: Go tests prove five-file union, live global/project replacement, fail-closed behavior in
all modes, recovery, and unchanged HTTP/CONNECT behavior.

### 3. Wire runs and guides

In `src/run.rs` and `src/persist.rs` resolve state/project once, publish `PolicyRun` before other
fallible work, pass five paths, use split mounts, generate the quoted guide command, and clean up in
normal/error/SIGTERM paths.

Completion: argument goldens show five layers and no agent policy mount; guide tests cover both
harnesses, ordering, guarded/open variants, spaces, and quotes; normal errors and SIGTERM at every
post-publication phase remove the run, while SIGKILL remains recoverable through lease reaping.

### 4. Finish CLI and remove config egress

In `src/cli.rs`, `src/config.rs`, `src/harness.rs`, and `src/net.rs` implement scoped allow/deny and
provenance status, retain active-only modes, remove seeding/`NetConfig`, and update help.

Completion: tests cover both scopes, atomic batches, cross-project isolation, same-project
cross-harness sharing, provenance, live mutation, remaining-source messages, and ephemeral wrapper
flags. Stale `[net]` tests prove run/install/update return the targeted error while `vhrn net`
commands remain usable.

### 5. Update active documentation

Update `AGENTS.md`, `README.md`, `docs/sandbox-design.md`, `docs/adding-a-harness.md`,
`docs/plans/completed/per-project-config.md` (remove its egress fields), CLI usage, guide text, and
`CHANGELOG.md` under `[Unreleased]`. Update `Cargo.toml`/`Cargo.lock` for `idna` without bumping the
crate version.

Record scoped allow/deny and provenance as Added; derived layers, XDG state, active-only modes, clean
reset, and permission narrowing as Changed; config egress and seeding as Removed; persistent
`--allow` and cross-run `--open-net` as Fixed.

Completion: `rg` finds no active claim that install seeds egress, config controls egress, policy
lives in cache, built-ins leak between harnesses, wrapper flags persist, or proxy responses provide
unblock commands.

### 6. Verify and cut over the maintainer

Run automated/live checks below, then perform the personal cutover only after they pass. Failure
leaves legacy policy and personal config untouched.

Completion: checks pass, live scope matches the model, cutover safety gates pass, and the report
accounts for every carried, redundant, invalid, and deleted entry.

## Test and verification matrix

Rust tests cover normalization/IDNA, stable union/provenance, default counts, deterministic canonical
project identity, exact isolation, non-directory/mismatch errors, escaped path display,
global/project live behavior, same-scope concurrent writes, independent projects, atomic deny,
remaining provenance, wrapper flag ephemerality, empty first use, ignored legacy policy, stale
`[net]` diagnostics, absolute/empty/relative XDG handling, lifecycle cleanup at each SIGTERM phase,
lease/race cleanup, idle-only log truncation/modes/symlink replacement, mount arguments, and guide
quoting.

Go tests cover five-file union, duplicates and last-file-only entries, same-mtime global/project
replacement, every required-read failure under all modes, recovery, hostname matching, and unchanged
HTTP/CONNECT denials.

Run:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test

cd proxy
gofmt -l .
go vet ./...
go test ./...
```

With local images:

1. Confirm legacy cache has no effect, a stale `[net]` block returns the targeted diagnostic, and
   `vhrn net` commands remain usable until the block is removed.
2. Start different harnesses in different projects and verify built-in isolation.
3. Add/deny a global domain and retry both without restart.
4. Add/deny a project domain and verify only matching active/future runs gain it, across harnesses.
5. Verify project deny reports a remaining global source.
6. Verify `--allow` and `--open-net` are run-only and live mode commands affect only active runs.
7. Verify both guides contain the quoted project command while proxy denials remain generic.
8. Exercise unreadable files, same-mtime replacement, run-exit races, leases, and shared logging.

### Post-implementation local cutover — Aravind only

A native host agent owns this one-off operation; the user performs no manual migration or cleanup.
It is not product behavior, a script, or CI. An agent currently running through vhrn cannot satisfy
the no-active-wrapper gate below: it must stop and hand the cutover to a native host agent rather
than attempting to dismantle its own policy. Inspected paths:

```text
old policy: /Users/aravind/.cache/vhrn/net
new policy: /Users/aravind/.local/state/vhrn/net
config:     /Users/aravind/.config/vhrn/config.toml
```

1. Stop verification sessions. Require no active runs, host vhrn wrapper, or `vhrn-proxy-*`
   container in either installed engine. An unrelated active session blocks cutover.
2. Before editing config, normalize the old allowlist plus current `[net].allow`. Record invalid
   entries, deduplicate in source order, and remove entries equal to or narrower than any base or
   registered-harness domain. Both old sources were machine-wide, so carry the remainder globally;
   do not invent project assignments.
3. For the inspected files, require this expected remainder:

   ```text
   crates.io
   golang.org
   sh.rustup.rs
   static.rust-lang.org
   agents.md
   ghcr.io
   vuln.go.dev
   ```

   `githubusercontent.com` covers its object/raw subdomains; Codex `openai.com` makes
   `developers.openai.com` an obsolete shared-list artifact. If sources changed, use the mechanical
   result and report the delta without asking for classification.
4. Reconcile new global state through one `net deny` batch and one `net allow` batch, skipping empty
   batches. Leave project layers empty.
5. Remove only `[net]` from personal config, preserving unrelated settings. Validate TOML and
   `net status --domains`; require global state to equal the derived remainder.
6. Reconfirm no wrapper/proxy. Resolve the old path to exactly
   `/Users/aravind/.cache/vhrn/net`, verify it is neither symlink nor mount point, enumerate it for
   the report, then permanently remove exactly that directory. The user explicitly authorized this
   deletion; never broaden the target.
7. From throwaway project directories, launch explicitly through `vhrn claude` and `vhrn codex`
   (not native unwrapped agent commands). Verify new state, isolated built-ins, persistent global
   remainder, empty project layers, and absent old directory. Report every
   carried/redundant/invalid/deleted entry.

## Non-goals

- User-facing session ids or `net allow --session`. Use `--allow` at launch or project scope live.
- Negative overrides; `deny` removes only from its selected mutable scope.
- Host-owned egress policy remains the only policy source.
- Automatic product migration or legacy deletion.
- TLS termination or rich CONNECT instructions.
- Closing established tunnels after revocation.
- Per-run deny logs or SIGKILLed container cleanup.
- Moving non-policy vhrn state out of cache.
