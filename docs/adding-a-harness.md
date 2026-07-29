# Adding a harness

A harness is a spec (`src/harness.rs`) plus a thin `FROM vhrn-base` Dockerfile under
`image/<harness>/`, and an entry in the CI publish matrix
(`.github/workflows/_build-images.yml`) so its image lands on ghcr. The spec carries the
image name, in-container command, shell alias, default egress domains, and the
persistence descriptors. No fork of the CLI is required.

## The rule the codex harness established

**If an agent writes its own config file, vhrn does not write it.** Inject through a layer
the agent only reads instead.

Agents record decisions in their config: which projects are trusted, which one-time
notices have been dismissed. A wrapper that derives that file each run destroys those
decisions every launch, and the user has no way to make an answer stick. Claude hit this
with `.claude.json` and the trust dialog; codex would have hit it with `config.toml`.

So the store holds the agent's file, the agent owns it, and vhrn's own configuration goes
somewhere the agent never writes — `system_config` below. Before adding a descriptor that
writes into an agent's config dir, check whether the agent writes there too.

The one exception is the container guide, which vhrn does derive each run. It is a file no
agent writes to, and `guide.in_state` exists precisely so it can sit in the state dir
without the rest of the sync going anywhere near it.

## The persistence descriptors

| Descriptor | What it decides |
| --- | --- |
| `state_dir`, `config_dir_env`, `host_config` | Where the container-owned store mounts, and which host dir seeds it |
| `sync_dirs`, `sync_files` | Disposable host config layered back on top each run |
| `credentials` | Bootstrap-only files copied into an *empty* store; never overwritten |
| `credential_env` | Host env vars forwarded when set, and hidden from spawned commands |
| `guide` | The derived guide: filename, host sources (first non-empty wins), whether it lands in the state dir, and whether it leads or trails the host's text |
| `system_config` | Mount the host's config plus vhrn's own defaults read-only at `/etc/<name>` |
| `share_history` | Mount this project's host-side history — only for a layout vhrn can reproduce exactly |
| `sessions_env`, `sessions_dir` | Partition sessions per project, for an agent that keeps one flat tree |

Two of these are about what an agent *must not* see. `credential_env` variables are hidden
from commands the agent spawns, and the host config copied in for `system_config` has its
project-trust tables stripped — the container mounts the project at its real host path, so
those tables would otherwise answer a trust prompt the user only answered on the host.

Sessions are worth the extra descriptor when an agent keeps every project's transcripts in
one tree. Mounting that tree hands a jailed agent every transcript on the machine; a shared
vhrn-owned store is smaller but still lets project A read project B. The store is keyed by
project, and the index and the transcripts it names are bound from the same tree so they
cannot end up in different partitions.

## What is not a spec field

The host's `~/.agents` is deliberately **not** one. It is the vendor-neutral config dir,
identical for every harness, so the run path mounts it as a constant — a per-harness field
there would only be something to get wrong when a harness is added. A harness that wants an
additional vendor-specific path lists it in `sync_dirs` as usual.

Nor is anything the agent installs for itself. If an agent's config dir holds both host
config and things it downloads at runtime (a remote-skill cache, say), sync only the former
— an `rsync --delete` from a host directory that is not that data's source of truth will
delete it.
