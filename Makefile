# Build the container images the vhrn CLI drives, with Apple `container` or Docker.
# The CLI itself is built and tested by cargo (`cargo build --release`, `cargo test`)
# and released by .github/workflows/release.yml; this Makefile owns only the image
# recipes plus a `test` convenience that runs both the Rust and proxy suites.
# Override the engine with ENGINE=docker.

ENGINE ?= $(shell if command -v container >/dev/null 2>&1; then echo container; \
		  elif command -v docker >/dev/null 2>&1; then echo docker; fi)
ifeq ($(strip $(ENGINE)),)
  $(error No container engine found; install one or pass ENGINE=docker)
endif

TAG ?= latest

# Shared base image (debian + tooling + egress entrypoint, no agent).
BASE_IMAGE ?= vhrn-base
BASE_REF   := $(BASE_IMAGE):$(TAG)
BASE_DIR   ?= image/base

# The claude harness image (FROM vhrn-base + Claude Code).
CLAUDE_IMAGE ?= vhrn-claude
CLAUDE_REF   := $(CLAUDE_IMAGE):$(TAG)
CLAUDE_DIR   ?= image/claude

# The egress proxy sidecar image (see proxy/).
PROXY_IMAGE ?= vhrn-proxy
PROXY_REF   := $(PROXY_IMAGE):$(TAG)
PROXY_DIR   ?= proxy

# Match the container user to your host UID/GID (native Linux Docker only). Applied
# to the base build, where the dev user is created.
BUILD_ARGS :=
ifeq ($(ENGINE),docker)
  ifeq ($(shell uname -s),Linux)
    BUILD_ARGS := --build-arg USER_UID=$(shell id -u) --build-arg USER_GID=$(shell id -g)
  endif
endif

ifeq ($(ENGINE),docker)
RM_IMAGE := $(ENGINE) image rm
else
RM_IMAGE := $(ENGINE) image delete
endif

.DEFAULT_GOAL := build
.PHONY: build test build-base build-claude build-proxy clean

# Build every image the CLI needs: shared base, its egress proxy, and the claude harness.
build: build-base build-proxy build-claude

# CLI (cargo) + proxy (its own dependency-free module) unit tests.
test:
	cargo test
	cd $(PROXY_DIR) && go test ./...

build-base:
	$(ENGINE) build $(BUILD_ARGS) --tag $(BASE_REF) --file $(BASE_DIR)/Dockerfile $(BASE_DIR)

# The harness image builds FROM the base, so require it first.
build-claude: build-base
	$(ENGINE) build --build-arg BASE=$(BASE_REF) --tag $(CLAUDE_REF) --file $(CLAUDE_DIR)/Dockerfile $(CLAUDE_DIR)

build-proxy:
	$(ENGINE) build --tag $(PROXY_REF) --file $(PROXY_DIR)/Dockerfile $(PROXY_DIR)

clean:
	-$(RM_IMAGE) $(CLAUDE_REF)
	-$(RM_IMAGE) $(BASE_REF)
	-$(RM_IMAGE) $(PROXY_REF)
