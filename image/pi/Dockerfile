# vhrn-pi: the Pi harness image — the shared vhrn-base plus Node and Pi.
# Keep Node out of vhrn-base so its runtime only ships with the harness that needs it.
# BASE is an arg so the Makefile/CI can pin a tagged or digest-pinned base ref.
ARG BASE=vhrn-base
FROM node:24-bookworm-slim AS node

FROM ${BASE}

# Copy only Node's executable entry points and global package tree from the official
# Node image. Pi lives under /usr/local, outside the runtime ~/.pi/agent mount.
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
# npm and npx are relative symlinks in the Node image. Recreate them after
# copying their target tree so Node resolves npm's own modules correctly.
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
    && npm install --global --ignore-scripts @earendil-works/pi-coding-agent

CMD ["pi"]
