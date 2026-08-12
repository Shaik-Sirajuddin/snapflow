SHELL := /bin/bash
.DEFAULT_GOAL := help

DEV_MAKEFILE := $(abspath $(dir $(lastword $(MAKEFILE_LIST)))development/Makefile)

%:
	@+$(MAKE) --no-print-directory -f "$(DEV_MAKEFILE)" "$@"
