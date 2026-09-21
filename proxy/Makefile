# Build the production Rust proxy with Apple `container` or Docker.
# Run from the repo root as `make -C proxy`. Override the engine with ENGINE=docker.

ENGINE ?= $(shell if command -v container >/dev/null 2>&1; then echo container; \
		  elif command -v docker >/dev/null 2>&1; then echo docker; fi)
ifeq ($(strip $(ENGINE)),)
  $(error No container engine found; install one or pass ENGINE=docker)
endif

TAG ?=
PROXY_IMAGE ?= vhrn-proxy
PROXY_REF := $(PROXY_IMAGE)$(if $(strip $(TAG)),:$(TAG))

ifeq ($(ENGINE),docker)
RM_IMAGE := $(ENGINE) image rm
else
RM_IMAGE := $(ENGINE) image delete
endif

.DEFAULT_GOAL := build
.PHONY: build clean

build:
	$(ENGINE) build --tag $(PROXY_REF) --file Dockerfile ..

clean:
	-$(RM_IMAGE) $(PROXY_REF)
