# Build the vhrn sandbox image with Apple `container` or Docker.
# Override the engine with ENGINE=docker.

ENGINE ?= $(shell if command -v container >/dev/null 2>&1; then echo container; \
		  elif command -v docker >/dev/null 2>&1; then echo docker; fi)
ifeq ($(strip $(ENGINE)),)
  $(error No container engine found; install one or pass ENGINE=docker)
endif

IMAGE      ?= vhrn-sandbox
TAG        ?= latest
IMAGE_REF  := $(IMAGE):$(TAG)
DOCKERFILE ?= image/Dockerfile

# The egress proxy sidecar image (see proxy/).
PROXY_IMAGE ?= vhrn-proxy
PROXY_REF   := $(PROXY_IMAGE):$(TAG)
PROXY_DIR   ?= proxy

# Where the wrapper installs. PREFIX/BINDIR override the destination.
PREFIX   ?= /usr/local
BINDIR   ?= $(PREFIX)/bin
WRAPPER  ?= vhrn.sh
BIN_NAME ?= vhrn

# Match the container user to your host UID/GID (native Linux Docker only).
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
.PHONY: build build-box build-proxy rebuild clean install uninstall

# Build both images the wrapper needs: the box and its egress proxy sidecar.
build: build-box build-proxy

build-box:
	$(ENGINE) build $(BUILD_ARGS) --tag $(IMAGE_REF) --file $(DOCKERFILE) image

build-proxy:
	$(ENGINE) build --tag $(PROXY_REF) --file $(PROXY_DIR)/Dockerfile $(PROXY_DIR)

rebuild:
	$(ENGINE) build --no-cache $(BUILD_ARGS) --tag $(IMAGE_REF) --file $(DOCKERFILE) image
	$(ENGINE) build --no-cache --tag $(PROXY_REF) --file $(PROXY_DIR)/Dockerfile $(PROXY_DIR)

clean:
	-$(RM_IMAGE) $(IMAGE_REF)
	-$(RM_IMAGE) $(PROXY_REF)

# Install the wrapper into $(BINDIR) (needs sudo for the default /usr/local/bin).
install:
	sudo install -m 0755 $(WRAPPER) $(BINDIR)/$(BIN_NAME)
	@echo "Installed $(BINDIR)/$(BIN_NAME) — run '$(BIN_NAME)' in any project."

uninstall:
	sudo rm -f $(BINDIR)/$(BIN_NAME)
