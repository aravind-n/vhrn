# Adding a harness

A harness is a spec (`src/harness.rs`) plus a thin `FROM vhrn-base` Dockerfile under
`image/<harness>/`, and an entry in the CI publish matrix
(`.github/workflows/_build-images.yml`) so its image lands on ghcr. The spec carries the
image name, in-container command, shell alias, default egress domains, and the
persistence descriptors (state dir, synced config, bootstrap credentials). No fork of
the CLI is required.

The host's `~/.agents` is deliberately **not** a spec field. It is the vendor-neutral config
dir, identical for every harness, so the run path mounts it as a constant — a per-harness
field there would only be something to get wrong when a harness is added. A harness that
wants an additional vendor-specific path lists it in `sync_dirs` as usual.
