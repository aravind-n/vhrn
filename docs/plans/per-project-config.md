# Host-owned per-project configuration

Status: **design sketch, not built.** Written alongside the removal of the `./.vhrn.toml`
project config layer (a sandbox-escape vector — repo content configuring the jail). This is
the sanctioned way per-project settings come back: keyed off the user's own global config,
never a file inside the project.

## Why not just keep `./.vhrn.toml`

Because it is read host-side, before the container launches, a `.vhrn.toml` committed to a
repository is trusted and obeyed the first time the user runs `vhrn <harness>` in it — so a
clone can disable the egress guard or widen the persistent allowlist. Per-project
configuration is a legitimate need (one project wants Go, another wants an extra egress
domain), but the *source* of that configuration must be a file the user controls, not the
sandboxed repository.

## Design

Per-project overrides live in a keyed table inside `~/.config/vhrn/config.toml`, addressed by
the **canonicalized absolute project path** — the same key shape `blocked_dirs` already
matches against and `history_key` already derives:

```toml
# ~/.config/vhrn/config.toml — host-owned, outside any repo

[net]
mode = "enforce"                       # global default for every project

[project."/Users/me/work/payments"]
toolchains.tools = ["go@1.26"]
net.allow = ["proxy.golang.org"]       # this project only

[project."/Users/me/oss/ffmpeg"]
net.mode = "report"
```

Precedence, unchanged in spirit — every layer host-owned:

```
CLI flags  >  [project."<cwd>"]  >  top-level keys  >  built-in defaults
```

Matching is exact on the resolved cwd (`std::fs::canonicalize`), never subtree, so a parent
project's block does not silently apply to a nested checkout.

## What it may set

The existing `RunConfig` / `ToolchainsConfig` / `NetConfig` fields, scoped to the project:
`toolchains.tools`, `net.allow`, `net.mode`, `run.blocked_dirs`. Same expressiveness the old
project layer had — with the trust properties inverted, because the file lives where only the
user can write it.

## Security property

vhrn reads **nothing** from the project directory to configure itself. The project is mounted
into the jail and read by the agent *inside* the boundary; it is never an input to how the
boundary is built. A hostile clone cannot reach any config vhrn acts on.

## Sketch of the implementation

- Extend `Config` with an internal `projects: Map<String, Overrides>` deserialized from the
  `[project."…"]` tables; keep the public fields as the resolved result.
- `load_config` gains no new file source — it still reads only `config_dir/config.toml`. After
  the base merge, if a `[project."<canonical cwd>"]` block matches, overlay it. The cwd is
  passed in from the run path (which already canonicalizes it), so `config.rs` still never
  touches the project directory itself.
- Unit-testable as a pure `(toml, cwd) -> Config` merge, like the current layering.

## Open questions

- Whether `[project]` blocks should support `~` expansion in the key, or require absolute
  canonical paths only (leaning: canonical only, to match `blocked_dirs` resolution).
- Whether a glob/prefix form (`[project."/Users/me/oss/*"]`) is worth it, or whether exact
  match keeps the model simple and auditable (leaning: exact only for v1).
