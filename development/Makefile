SHELL := /bin/bash
.DEFAULT_GOAL := help

DOCKER_MAKEFILE := $(abspath $(dir $(lastword $(MAKEFILE_LIST)))docker/Makefile)

%:
	@+$(MAKE) --no-print-directory -f "$(DOCKER_MAKEFILE)" "$@"
