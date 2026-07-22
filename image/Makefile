# Build the base and harness images the vhrn CLI drives, with Apple `container` or
# Docker. Run from the repo root as `make -C image` (or `cd image && make`); the proxy
# image builds separately (proxy/Makefile). Override the engine with ENGINE=docker.

ENGINE ?= $(shell if command -v container >/dev/null 2>&1; then echo container; \
		  elif command -v docker >/dev/null 2>&1; then echo docker; fi)
ifeq ($(strip $(ENGINE)),)
  $(error No container engine found; install one or pass ENGINE=docker)
endif

TAG ?= latest

# Shared base image (debian + tooling + egress entrypoint, no agent).
BASE_IMAGE ?= vhrn-base
BASE_REF   := $(BASE_IMAGE):$(TAG)
BASE_DIR   ?= base

# The claude harness image (FROM vhrn-base + Claude Code).
CLAUDE_IMAGE ?= vhrn-claude
CLAUDE_REF   := $(CLAUDE_IMAGE):$(TAG)
CLAUDE_DIR   ?= claude

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
.PHONY: build build-base build-claude clean

# Build the base image and its harnesses (the proxy image builds separately).
build: build-base build-claude

build-base:
	$(ENGINE) build $(BUILD_ARGS) --tag $(BASE_REF) --file $(BASE_DIR)/Dockerfile $(BASE_DIR)

# The harness image builds FROM the base, so require it first.
build-claude: build-base
	$(ENGINE) build --build-arg BASE=$(BASE_REF) --tag $(CLAUDE_REF) --file $(CLAUDE_DIR)/Dockerfile $(CLAUDE_DIR)

clean:
	-$(RM_IMAGE) $(CLAUDE_REF)
	-$(RM_IMAGE) $(BASE_REF)
