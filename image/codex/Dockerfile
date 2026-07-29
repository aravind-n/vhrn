# vhrn-codex: the codex harness image — the shared vhrn-base plus OpenAI Codex.
# BASE is an arg so the Makefile/CI can pin a tagged or digest-pinned base ref.
ARG BASE=vhrn-base
FROM ${BASE}

# Populated by buildx; uname -m covers a builder that does not set it.
ARG TARGETARCH

# Codex as the self-contained musl binary (no Node runtime). Installed as dev into
# ~/.local, which the runtime state-dir mount over ~/.codex does not shadow.
#
# The version floats: harness images track the newest agent and the daily refresh
# re-probes, so nothing is pinned here. The newest rust-v* tag is resolved explicitly
# rather than through /releases/latest/download, which can point at an unrelated release
# train. The tags API is ~50 KB against megabytes for the releases API; the atom feed is
# the fallback when the anonymous API is rate-limited. Both filters anchor the whole tag,
# or an -alpha prerelease yields a version with no download behind it.
USER dev
RUN set -eux; \
    export HOME=/home/dev; \
    arch="${TARGETARCH:-$(uname -m)}"; \
    case "$arch" in \
      amd64|x86_64) arch=x86_64 ;; \
      arm64|aarch64) arch=aarch64 ;; \
      *) echo "unsupported architecture: $arch" >&2; exit 1 ;; \
    esac; \
    tag="$(curl -fsSL 'https://api.github.com/repos/openai/codex/tags?per_page=100' \
           | jq -r '.[].name' \
           | grep -E '^rust-v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -1 || true)"; \
    if [ -z "$tag" ]; then \
      tag="$(curl -fsSL 'https://github.com/openai/codex/releases.atom' \
             | grep -oE 'Repository/[0-9]+/[^<]+' | sed 's|.*/||' \
             | grep -E '^rust-v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -1 || true)"; \
    fi; \
    [ -n "$tag" ] || { echo 'could not resolve a codex rust-v release tag' >&2; exit 1; }; \
    mkdir -p "$HOME/.local/bin" /tmp/codex; \
    curl -fsSL "https://github.com/openai/codex/releases/download/${tag}/codex-${arch}-unknown-linux-musl.tar.gz" \
      | tar -xz -C /tmp/codex; \
    n="$(find /tmp/codex -type f | wc -l)"; \
    [ "$n" -eq 1 ] || { echo "expected one file in the codex archive, found $n" >&2; exit 1; }; \
    install -m 0755 "$(find /tmp/codex -type f)" "$HOME/.local/bin/codex"; \
    rm -rf /tmp/codex; \
    codex --version
USER root

CMD ["codex"]
