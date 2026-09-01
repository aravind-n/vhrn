# vhrn-codex: the codex harness image — the shared vhrn-base plus OpenAI Codex.
# BASE is an arg so the Makefile/CI can pin a tagged or digest-pinned base ref.
ARG BASE=vhrn-base
FROM ${BASE}

# Codex CLI via OpenAI's installer. Keep its package store outside ~/.codex,
# which vhrn replaces with the persistent runtime state mount.
USER dev
RUN export HOME=/home/dev CODEX_HOME=/home/dev/.local/share/codex; \
    curl -fsSL https://chatgpt.com/codex/install.sh | sh
USER root

CMD ["codex"]
