# vhrn-claude: the claude harness image — the shared vhrn-base plus Claude Code.
# BASE is an arg so the Makefile/install can pin a tagged base ref.
ARG BASE=vhrn-base
FROM ${BASE}

# Claude Code as the self-contained native binary (no Node runtime). Installed as
# dev into ~/.local, which the runtime state-dir mount over ~/.claude does not shadow.
USER dev
RUN HOME=/home/dev curl -fsSL https://claude.ai/install.sh | bash
USER root

CMD ["claude"]
