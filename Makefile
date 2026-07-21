# Build the vhrn CLI (a static Go binary) and the container images it drives,
# with Apple `container` or Docker. Override the engine with ENGINE=docker.

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

# Where the CLI installs. PREFIX/BINDIR override the destination.
PREFIX   ?= /usr/local
BINDIR   ?= $(PREFIX)/bin
BIN_NAME ?= vhrn

# Go CLI build. Static (CGO off) so the binary is self-contained for curl-install.
# All build artifacts (the binary and cross-compiled releases) live under $(OUT).
GO        ?= go
CMD       ?= ./cmd/vhrn
PLATFORMS ?= darwin/arm64 darwin/amd64 linux/arm64 linux/amd64
OUT       ?= out
DIST      ?= $(OUT)/dist

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
.PHONY: build binary release test build-base build-claude build-proxy clean install uninstall

# Build every image the CLI needs: shared base, its egress proxy, and the claude harness.
build: build-base build-proxy build-claude

# The vhrn CLI: a single static binary (host arch), written under $(OUT).
binary:
	@mkdir -p $(OUT)
	CGO_ENABLED=0 $(GO) build -o $(OUT)/$(BIN_NAME) $(CMD)

# Cross-compile release binaries into $(DIST) for curl-install distribution.
release:
	@mkdir -p $(DIST)
	@for p in $(PLATFORMS); do \
	  os=$${p%/*}; arch=$${p#*/}; \
	  out=$(DIST)/$(BIN_NAME)-$$os-$$arch; \
	  echo "building $$out"; \
	  CGO_ENABLED=0 GOOS=$$os GOARCH=$$arch $(GO) build -o $$out $(CMD) || exit 1; \
	done

# CLI + proxy unit tests (the proxy is its own dependency-free module).
test:
	$(GO) test ./...
	cd $(PROXY_DIR) && $(GO) test ./...

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
	-rm -rf $(OUT)

# Build the CLI and install it into $(BINDIR) (needs sudo for /usr/local/bin).
install: binary
	sudo install -m 0755 $(OUT)/$(BIN_NAME) $(BINDIR)/$(BIN_NAME)
	@echo "Installed $(BINDIR)/$(BIN_NAME) — run '$(BIN_NAME) install <harness>' to build images."

uninstall:
	sudo rm -f $(BINDIR)/$(BIN_NAME)
