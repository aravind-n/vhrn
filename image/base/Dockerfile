# vhrn-base: the shared container image — debian + CLI tooling + the tamper-proof egress
# entrypoint, but no agent. Each harness image is `FROM vhrn-base` plus its own CLI,
# so the heavy, slow-changing layers are built and cached once.
FROM debian:trixie-slim

# Base packages: the entrypoint's egress firewall (nftables) and lock cleanup (procps),
# terminal support, and the file/search/network/archive/db CLIs an agent reaches for.
# This is an agent jail — ship what an agent shells out to, not human-comfort tooling
# (no editors, pagers-as-luxury, or TUIs; the agent has its own file tools).
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    git \
    nftables \
    procps \
    ncurses-term \
    less \
    jq \
    ripgrep \
    fd-find \
    zip \
    unzip \
    xz-utils \
    wget \
    rsync \
    openssh-client \
    gnupg \
    sqlite3 \
    patch \
    && ln -s "$(command -v fdfind)" /usr/local/bin/fd \
    && rm -rf /var/lib/apt/lists/*

# GitHub CLI from its own apt repo — arch-aware, so arm64 and x86_64 both work.
RUN mkdir -p -m 755 /etc/apt/keyrings \
    && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /etc/apt/keyrings/githubcli-archive-keyring.gpg \
    && chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update && apt-get install -y gh \
    && rm -rf /var/lib/apt/lists/*

# C/C++ toolchain and build drivers. clang/lld stay the default cc/c++ (symlinked below);
# build-essential adds gcc/g++ for projects that hardcode them, and cmake + ninja are the
# dominant native build drivers an agent invokes.
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang \
    lld \
    llvm \
    libc6-dev \
    make \
    pkg-config \
    build-essential \
    cmake \
    ninja-build \
    && ln -sf "$(command -v clang)" /usr/local/bin/cc \
    && ln -sf "$(command -v clang++)" /usr/local/bin/c++ \
    && rm -rf /var/lib/apt/lists/*

# Python: the system interpreter plus uv, its package/venv manager.
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3 \
    python3-venv \
    && rm -rf /var/lib/apt/lists/* \
    && curl -LsSf https://astral.sh/uv/install.sh \
       | env UV_INSTALL_DIR=/usr/local/bin UV_NO_MODIFY_PATH=1 sh

# Non-root user. UID/GID are build args to match the host user on Linux Docker;
# the base image has no uid-1000 user, so nothing needs freeing first.
ARG USER_UID=1000
ARG USER_GID=1000
RUN groupadd -g "$USER_GID" dev 2>/dev/null || true; \
    useradd -u "$USER_UID" -g "$USER_GID" -ms /bin/bash dev

# dev's user-space bins: the agent launcher (installed by the harness image) sits here,
# ahead of the standard prefixes. Tools installed via [tools] register their own PATH in
# ~/.profile, which the entrypoint sources at runtime — so no per-tool paths live here.
ENV PATH=/home/dev/.local/bin:$PATH

# virtiofs reports the bind-mount root's owner inconsistently (0:0 one moment, dev the next),
# which trips git's dubious-ownership check at random. Only the one project is ever mounted,
# so a blanket exemption weakens nothing. --system: /etc/gitconfig, not the mounted ~/.gitconfig.
RUN git config --system --add safe.directory '*'

# Entrypoint installs the egress firewall then drops to dev; needs procps for the lock check.
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# Stay root: the entrypoint sets nftables rules, then drops to the dev user itself.
WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
# Overridden by each harness image and by the command vhrn appends at run time.
CMD ["bash"]
