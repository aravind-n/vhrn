# Adding a harness

A harness is a spec (`src/harness.rs`) plus a thin `FROM vhrn-base` Dockerfile under
`image/<harness>/`, and an entry in the CI publish matrix
(`.github/workflows/_build-images.yml`) so its image lands on ghcr. The spec carries the
image name, in-container command, shell alias, default egress domains, and the
persistence descriptors (state dir, synced config, bootstrap credentials). No fork of
the CLI is required.
