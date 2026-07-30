SHELL := /bin/bash
.DEFAULT_GOAL := help

PLAN_DIR := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
# This Makefile is a canonical development entrypoint. WORKTREE may point at
# another checkout, but shared allocator/build state is kept outside it.
REPO_ROOT ?= $(abspath $(PLAN_DIR)/../..)
# Auto-detect WORKTREE from the invoking shell's cwd (not this Makefile's own
# location, which is fixed to REPO_ROOT): running these targets from inside a
# git worktree checkout picks that worktree automatically. Falls back to
# REPO_ROOT when cwd isn't inside a git worktree (or has no git available).
WORKTREE ?= $(shell git -C "$(CURDIR)" rev-parse --show-toplevel 2>/dev/null || echo $(REPO_ROOT))
WORKTREE_MOUNT := $(if $(filter $(WORKTREE),$(REPO_ROOT)),,-v $(WORKTREE):$(WORKTREE))
WORKTREE_LABEL ?= $(shell git -C "$(WORKTREE)" rev-parse --abbrev-ref HEAD 2>/dev/null | sed 's#[^A-Za-z0-9_.-]#-#g')
WORKTREE_LABEL := $(if $(WORKTREE_LABEL),$(WORKTREE_LABEL),detached)
OTHER_WORKTREE ?=
IMAGE ?= snapflow-shared-build:local
BASE_IMAGE ?= mltframework/shotcut-build:qt6.10.3-ubuntu22.04
NAME := snapflow-build-$(WORKTREE_LABEL)
BUILD_DIR := $(REPO_ROOT)/shotcut-rebrand/build-local
ACTIVE_MARKER := $(BUILD_DIR)/.docker_shared_build_active
STATE_BASE ?= $(HOME)/.snapflow/state
DOCKER_STATE_ROOT ?= $(STATE_BASE)/snapflow/docker
LOCK := $(DOCKER_STATE_ROOT)/shared-build.lock
STATE_DIR := $(DOCKER_STATE_ROOT)/worktrees/$(WORKTREE_LABEL)
VNC_SCRIPT := $(REPO_ROOT)/development/scripts/vnc_worktree.sh
SHARED_VNC_SCRIPT := $(REPO_ROOT)/development/scripts/vnc_shared_init.sh
VNC_SHARED_ROOT ?= $(STATE_BASE)/snapflow/vnc-shared
# Opt-in Slint MCP server on the real-backend vnc-up instance -- see
# vnc_worktree.sh's own VNC_MCP_PORT doc comment. Empty/0 (default): no MCP
# server, unchanged from before this existed. "1": reserve a free port
# through the same registry gateway_port/mcp_addr already use. A specific
# number: pin that port.
VNC_MCP_PORT ?=
TIMINGS := $(DOCKER_STATE_ROOT)/timings.tsv
CMAKE_PYTHON_ROOT ?= $(HOME)/.local/lib/python3.12/site-packages/cmake
# Build-phase logging: quiet by default (only a short status line, full log
# only on failure); VERBOSE=1 streams live output as before.
VERBOSE ?= 0

# Make host agent installations discoverable inside development containers.
# These are read-only: ACP/agent discovery needs the host metadata and
# credentials, while container builds must not mutate the host stores. Omit a
# mount when a particular host directory is absent so a fresh machine does
# not get an implicitly-created root-owned directory from Docker.
ACP_DISCOVERY_MOUNTS := $(foreach dir,.claude .codex .grok,$(if $(wildcard $(HOME)/$(dir)),-v $(HOME)/$(dir):$(HOME)/$(dir):ro,))

DOCKER_RUN = docker run --rm --init \
  --user $(shell id -u):$(shell id -g) \
  --label app=snapflow-shared-build \
  --label worktree=$(WORKTREE_LABEL) \
  -v $(PLAN_DIR):$(PLAN_DIR):ro \
  -w $(REPO_ROOT) \
  -v $(REPO_ROOT):$(REPO_ROOT) \
  $(WORKTREE_MOUNT) \
  -v $(HOME)/.cargo:$(HOME)/.cargo \
  -v $(HOME)/.rustup:$(HOME)/.rustup \
  $(ACP_DISCOVERY_MOUNTS) \
  -v $(HOME)/.cache:$(HOME)/.cache \
  -v $(HOME)/go:$(HOME)/go \
  -v /usr/local/go:/usr/local/go:ro \
  -v $(CMAKE_PYTHON_ROOT):$(CMAKE_PYTHON_ROOT):ro \
  -v /usr/lib/x86_64-linux-gnu:/usr/lib/x86_64-linux-gnu:ro \
  -v /usr/lib/gcc:/usr/lib/gcc:ro \
  -v /usr/libexec/gcc:/usr/libexec/gcc:ro \
  -v /lib/x86_64-linux-gnu:/lib/x86_64-linux-gnu:ro \
  -v /usr/include:/usr/include:ro \
  -v /usr/share/pkgconfig:/usr/share/pkgconfig:ro \
  -v /usr/lib/qt6:/usr/lib/qt6:ro \
  -v /usr/bin/x86_64-linux-gnu-qtpaths6:/usr/bin/x86_64-linux-gnu-qtpaths6:ro \
  -v /usr/bin/x86_64-linux-gnu-qmake6:/usr/bin/x86_64-linux-gnu-qmake6:ro \
  -v /usr/bin/as:/usr/bin/as:ro -v /usr/bin/ld:/usr/bin/ld:ro \
  -v /usr/bin/g++:/usr/bin/g++:ro -v /usr/bin/gcc:/usr/bin/gcc:ro \
  -v /usr/bin/c++:/usr/bin/c++:ro -v /usr/bin/cc:/usr/bin/cc:ro \
  -v /usr/bin/x86_64-linux-gnu-g++-15:/usr/bin/g++-11:ro \
  -v /usr/bin/x86_64-linux-gnu-g++-15:/usr/bin/x86_64-linux-gnu-g++-11:ro \
  -v /usr/bin/x86_64-linux-gnu-gcc-15:/usr/bin/gcc-11:ro \
  -v /usr/bin/x86_64-linux-gnu-ar:/usr/bin/x86_64-linux-gnu-ar:ro \
  -v /usr/bin/x86_64-linux-gnu-ranlib:/usr/bin/x86_64-linux-gnu-ranlib:ro \
  -v /usr/bin/x86_64-linux-gnu-strip:/usr/bin/x86_64-linux-gnu-strip:ro \
  -v /usr/include/x86_64-linux-gnu/qt6:/usr/include/x86_64-linux-gnu/qt6:ro \
  -e PATH=$(HOME)/.cargo/bin:/usr/local/go/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
  -e HOME=$(HOME) \
  -e REPO_ROOT=$(REPO_ROOT) -e WORKTREE=$(WORKTREE) \
  -e VNC_SHARED_ROOT=$(VNC_SHARED_ROOT) \
  -e DOCKER_STATE_ROOT=$(DOCKER_STATE_ROOT) \
  -e BUILD_DIR=$(BUILD_DIR) -e ACTIVE_MARKER=$(ACTIVE_MARKER) \
  -v $(VNC_SHARED_ROOT):$(VNC_SHARED_ROOT) \
  -v $(DOCKER_STATE_ROOT):$(DOCKER_STATE_ROOT) \
  -v /tmp/.X11-unix:/tmp/.X11-unix \
  -e CARGO_HOME=$(HOME)/.cargo \
  -e RUSTUP_HOME=$(HOME)/.rustup \
  -e GOCACHE=$(HOME)/.cache/go-build \
  -e GOMODCACHE=$(HOME)/go/pkg/mod \
  -e VERBOSE=$(VERBOSE) \
  $(IMAGE)

# ============================================================================
# Rust crate builds (quiet by default)
#
# Host-side `cargo build` per crate/workspace -- as opposed to `build`/
# `build-snapflow`, which is the full Docker+cmake C++ snapflow build. Each
# target prints one "==> name" / "==> name ok" line on success; VERBOSE=1
# streams live output instead. Failures always show the log path + a grep
# filter suggestion, never the whole log. See cargo_build_quiet.sh.
# ============================================================================
CARGO_BUILD_QUIET := $(REPO_ROOT)/development/scripts/cargo_build_quiet.sh

build-panel-rust:
	@DOCKER_STATE_ROOT="$(DOCKER_STATE_ROOT)" VERBOSE="$(VERBOSE)" "$(CARGO_BUILD_QUIET)" panel-rust "$(WORKTREE)/panel-rust"

build-sap-rust:
	@DOCKER_STATE_ROOT="$(DOCKER_STATE_ROOT)" VERBOSE="$(VERBOSE)" "$(CARGO_BUILD_QUIET)" sap-rust "$(WORKTREE)/sap-rust"

build-acpx:
	@DOCKER_STATE_ROOT="$(DOCKER_STATE_ROOT)" VERBOSE="$(VERBOSE)" "$(CARGO_BUILD_QUIET)" acpx "$(WORKTREE)/acpx" -p acpx-server

build-skills-manager:
	@DOCKER_STATE_ROOT="$(DOCKER_STATE_ROOT)" VERBOSE="$(VERBOSE)" "$(CARGO_BUILD_QUIET)" skills-manager "$(WORKTREE)/lib/skills-manager"
# ============================================================================

# ============================================================================
# Slint hot-reload viewer (no C++/cmake build, no acpx-server, no real
# backend -- just this worktree's panel-rust/ui/*.slint tree, live-reloaded
# on save via `slint-viewer --auto-reload`). Generalizes the one-off
# scripts/launch_slint_vnc_ui_polish_a.sh script (hardcoded display :111 /
# port 5911 / single worktree, its own dedicated Xvnc) into a proper
# per-worktree command, kept as a standalone script (slint_hot_reload.sh)
# rather than inline recipe shell -- same pattern every other non-trivial
# operation in this Makefile already follows (vnc_worktree.sh,
# vnc_shared_init.sh, cargo_build_quiet.sh), and for a real, reproduced
# reason here specifically: an inline `@shared_env="$$(...init)"` recipe
# line reliably hung until an outer timeout killed it when run through
# this Makefile's nested `@+$(MAKE) -f ...` submake proxy chain -- see
# the script's own header comment for the full diagnosis (GNU Make
# jobserver fd inheritance into a backgrounded child, most likely). The
# script reuses the SAME shared-VNC infrastructure `vnc-up` goes through
# (vnc_shared_init.sh's `init`/`workspace-ensure`/`workspace-place`), so
# it lands on the exact VNC session + workspace slot already assigned to
# this worktree, not a second unshared Xvnc.
SLINT_HOT_RELOAD_SCRIPT := $(REPO_ROOT)/development/scripts/slint_hot_reload.sh

slint-hot-reload:
	@VNC_STATE_BASE="$(STATE_BASE)" VNC_SHARED_ROOT="$(VNC_SHARED_ROOT)" "$(SLINT_HOT_RELOAD_SCRIPT)" start "$(WORKTREE)"

slint-hot-reload-down:
	@VNC_STATE_BASE="$(STATE_BASE)" VNC_SHARED_ROOT="$(VNC_SHARED_ROOT)" "$(SLINT_HOT_RELOAD_SCRIPT)" stop "$(WORKTREE)"
# ============================================================================

.PHONY: help image docker-build docker-up docker-down docker-down-v docker-rebuild docker-relaunch docker-connect docker-logs status build build-current build-snapflow build-panel-rust build-sap-rust build-acpx build-skills-manager vnc-init vnc-up vnc-down vnc-shared-down vnc-status measure measure-one clean-measure slint-hot-reload slint-hot-reload-down

help:
	@echo "WORKTREE=$(WORKTREE)"
	@echo "WORKTREE_LABEL=$(WORKTREE_LABEL)"
	@echo "docker-build  build the shared build image"
	@echo "docker-up     start the reusable build container"
	@echo "docker-down   stop/remove the container; preserve caches"
	@echo "docker-down-v stop/remove container and plan-local state"
	@echo "docker-rebuild rebuild image, clear plan state, start container"
	@echo "docker-relaunch restart container without rebuilding image"
	@echo "docker-connect open a shell in the running container"
	@echo "docker-logs    show container logs"
	@echo "status         show container and VNC status"
	@echo "build        serialized Docker build using shared build-local"
	@echo "build-snapflow      alias for 'build' (the C++ snapflow build)"
	@echo "build-panel-rust    quiet host-side 'cargo build' for panel-rust"
	@echo "build-sap-rust      quiet host-side 'cargo build' for sap-rust"
	@echo "build-acpx          quiet host-side 'cargo build -p acpx-server'"
	@echo "build-skills-manager quiet host-side 'cargo build' for lib/skills-manager"
	@echo "vnc-init     start/reuse shared host Xvnc and workspace registry"
	@echo "vnc-up       Docker build, then launch this worktree on shared Xvnc"
	@echo "               VNC_MCP_PORT=1 enables the Slint MCP server on this"
	@echo "               real-backend instance (VNC_MCP_PORT=<port> to pin one)"
	@echo "vnc-down     stop this worktree and release its workspace"
	@echo "vnc-shared-down stop shared Xvnc after all workspaces are released"
	@echo "vnc-status   show VNC instances and port registry"
	@echo "slint-hot-reload      live-reloading .slint viewer for this worktree,"
	@echo "                        no C++/cargo build, no acpx-server -- just"
	@echo "                        panel-rust/ui/app.slint, edit+save to reload"
	@echo "slint-hot-reload-down stop this worktree's hot-reload viewer"
	@echo "measure      record first/repeat/optional-other-worktree timings"
	@echo "VERBOSE=1    stream live build output (docker build / cmake / cargo); default is quiet, full log only on failure"

image: docker-build

docker-build:
	@mkdir -p "$(DOCKER_STATE_ROOT)"
	@log="$(DOCKER_STATE_ROOT)/docker-build.log"; \
	echo "==> docker build (image=$(IMAGE))"; \
	if [ "$(VERBOSE)" = "1" ]; then \
	  docker build --build-arg BASE_IMAGE=$(BASE_IMAGE) -t $(IMAGE) -f $(PLAN_DIR)/Dockerfile $(PLAN_DIR) 2>&1 | tee "$$log"; status=$${PIPESTATUS[0]}; \
	else \
	  docker build --build-arg BASE_IMAGE=$(BASE_IMAGE) -t $(IMAGE) -f $(PLAN_DIR)/Dockerfile $(PLAN_DIR) >"$$log" 2>&1; status=$$?; \
	fi; \
	if [ "$$status" -ne 0 ]; then \
	  echo "==> docker build FAILED (exit $$status)" >&2; \
	  echo "    log: $$log" >&2; \
	  echo "    inspect: grep -inE 'error|failed' \"$$log\" | tail -n 50" >&2; \
	  exit "$$status"; \
	fi; \
	echo "==> docker build ok (log: $$log)"

docker-up: image
	-@docker rm -f $(NAME) >/dev/null 2>&1 || true
	@docker run -d --name $(NAME) --init \
	  --user $(shell id -u):$(shell id -g) \
	  --label app=snapflow-shared-build --label worktree=$(WORKTREE_LABEL) \
	  -v $(PLAN_DIR):$(PLAN_DIR):ro \
	  -w $(REPO_ROOT) -v $(REPO_ROOT):$(REPO_ROOT) $(WORKTREE_MOUNT) \
	  -v $(HOME)/.cargo:$(HOME)/.cargo -v $(HOME)/.rustup:$(HOME)/.rustup \
	  $(ACP_DISCOVERY_MOUNTS) \
	  -v $(HOME)/.cache:$(HOME)/.cache -v $(HOME)/go:$(HOME)/go \
	  -v /usr/local/go:/usr/local/go:ro \
	  -v $(CMAKE_PYTHON_ROOT):$(CMAKE_PYTHON_ROOT):ro \
	  -v /usr/lib/x86_64-linux-gnu:/usr/lib/x86_64-linux-gnu:ro \
	  -v /usr/lib/gcc:/usr/lib/gcc:ro \
	  -v /usr/libexec/gcc:/usr/libexec/gcc:ro \
	  -v /lib/x86_64-linux-gnu:/lib/x86_64-linux-gnu:ro \
	  -v /usr/include:/usr/include:ro \
	  -v /usr/share/pkgconfig:/usr/share/pkgconfig:ro \
	  -v /usr/lib/qt6:/usr/lib/qt6:ro \
	  -v /usr/bin/x86_64-linux-gnu-qtpaths6:/usr/bin/x86_64-linux-gnu-qtpaths6:ro \
	  -v /usr/bin/x86_64-linux-gnu-qmake6:/usr/bin/x86_64-linux-gnu-qmake6:ro \
	  -v /usr/bin/as:/usr/bin/as:ro -v /usr/bin/ld:/usr/bin/ld:ro \
	  -v /usr/bin/g++:/usr/bin/g++:ro -v /usr/bin/gcc:/usr/bin/gcc:ro \
	  -v /usr/bin/c++:/usr/bin/c++:ro -v /usr/bin/cc:/usr/bin/cc:ro \
	  -v /usr/bin/x86_64-linux-gnu-g++-15:/usr/bin/g++-11:ro \
	  -v /usr/bin/x86_64-linux-gnu-g++-15:/usr/bin/x86_64-linux-gnu-g++-11:ro \
	  -v /usr/bin/x86_64-linux-gnu-gcc-15:/usr/bin/gcc-11:ro \
	  -v /usr/bin/x86_64-linux-gnu-ar:/usr/bin/x86_64-linux-gnu-ar:ro \
	  -v /usr/bin/x86_64-linux-gnu-ranlib:/usr/bin/x86_64-linux-gnu-ranlib:ro \
	  -v /usr/bin/x86_64-linux-gnu-strip:/usr/bin/x86_64-linux-gnu-strip:ro \
	  -v /usr/include/x86_64-linux-gnu/qt6:/usr/include/x86_64-linux-gnu/qt6:ro \
	  -e PATH=$(HOME)/.cargo/bin:/usr/local/go/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
	  -e HOME=$(HOME) -e CARGO_HOME=$(HOME)/.cargo -e RUSTUP_HOME=$(HOME)/.rustup \
	  -e VNC_SHARED_ROOT=$(VNC_SHARED_ROOT) -e DOCKER_STATE_ROOT=$(DOCKER_STATE_ROOT) \
	  -v $(VNC_SHARED_ROOT):$(VNC_SHARED_ROOT) -v $(DOCKER_STATE_ROOT):$(DOCKER_STATE_ROOT) \
	  -v /tmp/.X11-unix:/tmp/.X11-unix \
	  -e REPO_ROOT=$(REPO_ROOT) -e WORKTREE=$(WORKTREE) \
	  -e BUILD_DIR=$(BUILD_DIR) -e ACTIVE_MARKER=$(ACTIVE_MARKER) \
	  -e GOCACHE=$(HOME)/.cache/go-build \
	  -e GOMODCACHE=$(HOME)/go/pkg/mod $(IMAGE) sleep infinity

docker-down:
	-@docker rm -f $(NAME) >/dev/null 2>&1 || true

docker-down-v: docker-down
	@rm -rf "$(STATE_DIR)"

docker-rebuild: docker-build docker-down-v docker-up

docker-relaunch: docker-down docker-up

docker-connect:
	@docker exec -it $(NAME) /bin/sh

docker-logs:
	@docker logs --tail 100 -f $(NAME)

status:
	@docker ps -a --filter "name=^$(NAME)$$" --format 'container={{.Names}} status={{.Status}} image={{.Image}}' || true
	@$(MAKE) -f "$(PLAN_DIR)/Makefile" --no-print-directory vnc-status

build: image
	@mkdir -p "$(dir $(LOCK))" "$(DOCKER_STATE_ROOT)"
	@flock -w 3600 "$(LOCK)" env \
	  REPO_ROOT="$(REPO_ROOT)" WORKTREE="$(WORKTREE)" \
	  BUILD_DIR="$(BUILD_DIR)" ACTIVE_MARKER="$(ACTIVE_MARKER)" \
	  $(DOCKER_RUN) /bin/bash "$(PLAN_DIR)/build.sh"

build-current: build

build-snapflow: build

vnc-init:
	@VNC_SHARED_ROOT="$(VNC_SHARED_ROOT)" "$(SHARED_VNC_SCRIPT)" init "$(WORKTREE)"

vnc-up: build
	@set -e; \
	 snapflow_bin="$(BUILD_DIR)/src/snapflow"; \
	 if [ ! -x "$$snapflow_bin" ] && [ -x "$(WORKTREE)/build-snapflow-latest/src/snapflow" ]; then \
	   echo "==> build artifact was not materialized at $$snapflow_bin; using worktree artifact $(WORKTREE)/build-snapflow-latest/src/snapflow"; \
	   snapflow_bin="$(WORKTREE)/build-snapflow-latest/src/snapflow"; \
	 fi; \
	 [ -x "$$snapflow_bin" ] || { echo "missing snapflow binary: $$snapflow_bin" >&2; exit 1; }; \
	 VNC_SHARED_ROOT="$(VNC_SHARED_ROOT)" VNC_SHARED=1 VNC_SKIP_BUILD=1 SNAPFLOW_BIN_OVERRIDE="$$snapflow_bin" VNC_MCP_PORT="$(VNC_MCP_PORT)" "$(VNC_SCRIPT)" start "$(WORKTREE)" $${AGENT_ID:-codex-acp}

vnc-down:
	@VNC_SHARED_ROOT="$(VNC_SHARED_ROOT)" VNC_SHARED=1 "$(VNC_SCRIPT)" clean "$(WORKTREE)"

vnc-shared-down:
	@VNC_SHARED_ROOT="$(VNC_SHARED_ROOT)" "$(SHARED_VNC_SCRIPT)" down

vnc-status:
	@VNC_SHARED_ROOT="$(VNC_SHARED_ROOT)" "$(SHARED_VNC_SCRIPT)" status
	@"$(VNC_SCRIPT)" status

measure: image
	@mkdir -p "$(DOCKER_STATE_ROOT)"
	@$(MAKE) -f "$(PLAN_DIR)/Makefile" --no-print-directory clean-measure
	@$(MAKE) -f "$(PLAN_DIR)/Makefile" --no-print-directory measure-one LABEL=first WORKTREE_TO_BUILD="$(WORKTREE)"
	@$(MAKE) -f "$(PLAN_DIR)/Makefile" --no-print-directory measure-one LABEL=repeat WORKTREE_TO_BUILD="$(WORKTREE)"
	@if [ -n "$(OTHER_WORKTREE)" ]; then \
	  $(MAKE) -f "$(PLAN_DIR)/Makefile" --no-print-directory measure-one LABEL=other WORKTREE_TO_BUILD="$(OTHER_WORKTREE)"; \
	else \
	  echo "other\tskipped\tset OTHER_WORKTREE=/absolute/path/to/worktree to measure it" >> "$(TIMINGS)"; \
	fi
	@echo "Recorded $(TIMINGS)"
	@column -t -s $$'\t' "$(TIMINGS)" 2>/dev/null || cat "$(TIMINGS)"

measure-one:
	@start=$$(date +%s%N); status=0; \
	  $(MAKE) -f "$(PLAN_DIR)/Makefile" --no-print-directory WORKTREE="$(WORKTREE_TO_BUILD)" build >/dev/null 2>"$(DOCKER_STATE_ROOT)/$(LABEL).log" || status=$$?; \
	  end=$$(date +%s%N); \
	  awk -v s=$$start -v e=$$end 'BEGIN { printf "%.3f", (e-s)/1000000000 }' > "$(DOCKER_STATE_ROOT)/$(LABEL).seconds"; \
	  printf '%s\t%s\t%s\t%s\n' "$(LABEL)" "$$(cat "$(DOCKER_STATE_ROOT)/$(LABEL).seconds")" "$$status" "$(WORKTREE_TO_BUILD)" >> "$(TIMINGS)"; \
	  echo "$(LABEL): $$(cat "$(DOCKER_STATE_ROOT)/$(LABEL).seconds")s (status $$status)"; \
	  exit 0

clean-measure:
	@rm -f "$(TIMINGS)" "$(DOCKER_STATE_ROOT)"/*.seconds "$(DOCKER_STATE_ROOT)"/*.log
	@printf 'case\tseconds\tstatus\tworktree\n' > "$(TIMINGS)"
