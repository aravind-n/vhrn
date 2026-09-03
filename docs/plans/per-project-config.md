# Host-owned per-project configuration

Status: **design proposal, not built.** This restores project-specific tools and resource limits
without restoring `./.vhrn.toml`, whose repository-owned contents could configure the jail before
the container launched.

This proposal depends on the scoped-egress design in `egress-allowlist-layering.md`. Network policy
is deliberately not part of this configuration surface.

## Goals

- Let a user select tools and resource limits for one exact project from the host-owned global
  config file.
- Keep repository contents out of every decision that constructs the sandbox.
- Preserve exact, canonical project identity across config, history, sessions, and scoped egress.
- Preserve install/update prewarming for content-addressed tools images.
- Keep the resolved run configuration small and directly usable by the run path.

## Non-goals

- No config file inside a project.
- No prefix, glob, repository-root, or parent-directory inheritance.
- No `~` expansion in project keys.
- No project override for `run.blocked_dirs`.
- No network allowlist or network-mode settings. Persistent project egress belongs to
  `vhrn net allow --project <path> <domain>`; `--allow` and `--open-net` remain run-scoped wrapper
  flags.

## User-facing configuration

Overrides live under the singular `project` table in `~/.config/vhrn/config.toml`:

```toml
# ~/.config/vhrn/config.toml -- host-owned, outside every repository

[run]
blocked_dirs = ["~", "/"]

[resources]
memory = "engine"

[tools]
apt = ["jq"]

[project."/Users/me/work/payments".tools]
apt = ["jq", "postgresql-client"]
run = ["curl -fsSL https://example.internal/install.sh | sh"]

[project."/Users/me/work/payments".resources]
memory = "8g"
cpus = 4

[project."/Users/me/oss/ffmpeg".resources]
memory = "12g"
```

For fields supported here, precedence is:

```text
project."<canonical-cwd>"  >  top-level config  >  built-in defaults
```

Each optional field overlays independently. Arrays replace the lower-precedence array; they do not
append. In the example, `payments` gets both `jq` and `postgresql-client`, while an unlisted project
gets only `jq`.

Wrapper network flags are a separate, higher-precedence input to the scoped egress policy. There
are no wrapper flags for tools or resources, and agent arguments named `--memory` or `--cpus` still
pass through verbatim.

### Supported project fields

A project block may set only:

- `tools.apt`
- `tools.run`
- `resources.memory`
- `resources.cpus`

`run.blocked_dirs` remains global-only. Because blocked-directory matching is exact, applying it
only after selecting the current project's block would make it little more than a way for that
project to replace or weaken the global launch guard. Keeping it global makes the guard auditable
and ensures project selection cannot change whether that cwd is eligible to launch.

Network settings are absent by design. After the scoped-egress work lands, `[net]`, `net.allow`, and
`net.mode` are not recognized configuration. Project-specific access is stored in the host-owned
egress policy store and managed through `vhrn net`; it is not duplicated in TOML.

## Project identity and matching

The run path canonicalizes the cwd once with `std::fs::canonicalize` and uses that absolute path for
the project mount, history/session identity, scoped egress identity, and configuration lookup.
Lookup is a byte-for-byte exact match against a `project` key. A parent entry never applies to a
nested checkout.

Keys must be absolute path spellings with no `.` or `..` components. They do not expand `~`, and
glob characters have no special meaning. Users should obtain the matching spelling with `pwd -P`.
A symlink spelling does not match the canonical target path. Moving a project gives it a new
identity; reusing the old path deliberately reuses its entry.

The config parser does not canonicalize every declared key or require every configured project to
exist. That keeps config loading deterministic and lets install/update prewarm a profile for a
project that is temporarily unavailable. It validates the key's lexical shape, while the run path
supplies the canonical lookup value.

## Configuration model

Use separate parsed and resolved types so the returned run config cannot accidentally retain a map
of every project:

```rust
#[derive(serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConfigFile {
    run: RunConfig,
    tools: ToolsConfig,
    resources: ResourcesConfig,
    #[serde(rename = "project")]
    projects: BTreeMap<String, ProjectOverrides>,
}

#[derive(serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProjectOverrides {
    tools: ProjectToolsOverrides,
    resources: ProjectResourcesOverrides,
}

#[derive(serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProjectToolsOverrides {
    apt: Option<Vec<String>>,
    run: Option<Vec<String>>,
}

#[derive(serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProjectResourcesOverrides {
    memory: Option<String>,
    cpus: Option<u32>,
}

struct Config {
    run: RunConfig,
    tools: ToolsConfig,
    resources: ResourcesConfig,
}
```

The Rust member may be named `projects`, but `#[serde(rename = "project")]` pins the public TOML
spelling to the singular form shown above. The parsed types deny unknown fields, so `[projects]`, a
misspelled nested field, or a forbidden `run`/`net` section is an error instead of a silently ignored
setting. `ProjectOverrides` does not recursively contain another project map. The project-specific
leaf types merge into the existing resolved `ToolsConfig` and `ResourcesConfig` types.

Refactor loading into two pure stages:

1. Parse the one host-owned `config_dir/config.toml` into `ConfigFile` and validate its project-key
   spellings. A missing file yields the default parsed value; malformed content remains an error.
2. Resolve built-in defaults, then top-level values, then the exact optional project override into
   `Config`. Normalize and validate resource values after merging so an overridden global value is
   not used by the selected run.

The filesystem edge resolves XDG config and canonicalizes the cwd before calling these stages.
`config.rs` never searches, reads, or canonicalizes anything in the project directory.

## Tools-image lifecycle

Per-project tools create more than one effective tools profile, so install and update must not use
the directory from which the command happened to be invoked.

For `vhrn install <harness>` and each updated harness:

1. Parse the host config without selecting a cwd.
2. Produce the global tools profile plus the effective profile for every declared project, merging
   each project over the global tools fields.
3. Normalize and deduplicate profiles by their effective `apt` and ordered `run` contents.
4. Build every distinct non-empty profile against the selected harness base image. Iterate project
   keys in `BTreeMap` order so diagnostics are stable; retain all project paths associated with a
   deduplicated profile for error reporting.

As today, a tools-build failure does not undo the base harness install/update, but the command exits
nonzero. Attempt all distinct profiles so one broken profile does not prevent valid profiles from
being cached. Report the project paths associated with each failed profile. The run path still calls
`ensure_tools_image` for its selected effective profile; content-addressing makes this a cache hit
after prewarming and a safe lazy fallback if configuration changed since install/update.

Resource overrides do not participate in image identity or prewarming. They are resolved only for
the selected run and translated to the existing portable `--memory` and `--cpus` engine arguments.

## Security properties

- vhrn reads no repository file to configure the jail. A hostile clone cannot widen mounts,
  resources, tools, or egress before launch.
- The only source is the user's XDG config file outside the project mount.
- Project selection cannot weaken the global `blocked_dirs` guard.
- Project TOML cannot configure egress. Persistent mutable policy remains in the host-owned state
  store, mounted only into the proxy; run-scoped network flags remain explicit CLI actions.
- `tools.run` remains arbitrary host-authorized build input. Adding it to a project block does not
  make repository content executable by vhrn.

## Implementation sequence

1. Add `ConfigFile` and `ProjectOverrides`, using the singular TOML `project` rename and
   project-key validation.
2. Split parse and resolve operations; pass the already-canonical cwd into the resolver from the
   run path.
3. Keep `blocked_dirs` global and check it before image resolution or other host-side preparation.
4. Resolve project resources and tools into `ContainerConfig`; keep engine argument and tools hash
   assembly unchanged.
5. Change install/update prewarming to enumerate and deduplicate all effective tools profiles.
6. Update active configuration documentation and examples without adding an egress TOML surface.

## Tests

Add pure tests covering:

- singular `[project."..."]` deserialization and rejection of a plural `[projects]` typo;
- exact canonical-path selection, nested-project noninheritance, symlink-spelling nonmatch, and
  lexical rejection of relative, `.`-component, and `..`-component keys;
- field-by-field project-over-global precedence and array replacement;
- global-only `blocked_dirs` behavior, including that no project block can admit `~` or `/`;
- project resource normalization, invalid memory, and zero CPUs;
- rejection of `run` and network keys inside a project override;
- enumeration and deterministic deduplication of global/project tools profiles;
- stable association of a failed deduplicated tools profile with all affected project paths;
- unchanged configuration for a cwd with no matching project entry.

Run-path argument tests should prove that two projects can select different derived image tags and
resource flags while receiving the same global launch guard. Install/update tests should prove that
their prewarming result is independent of the caller's cwd and that a failed project profile leaves
the base install complete while returning a nonzero status.
