FROM node:lts-slim

# Install essential CLI tools, build dependencies, and procps (required for pgrep lock checks)
RUN apt-get update && apt-get install -y \
    git \
    curl \
    jq \
    less \
    build-essential \
    sudo \
    procps \
    && rm -rf /var/lib/apt/lists/*

# Create a non-root user with passwordless sudo so Claude can safely install system packages
RUN useradd -ms /bin/bash dev && \
    echo "dev ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers

# Install Claude Code globally
RUN npm install -g @anthropic-ai/claude-code

# Copy and configure the self-healing lock management script
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

USER dev
WORKDIR /workspace

# Ensure the lock check runs on every boot while defaulting to launching 'claude'
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["claude"]

