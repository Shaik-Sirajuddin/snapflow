ARG BASE_IMAGE=mltframework/shotcut-build:qt6.10.3-ubuntu22.04
FROM ${BASE_IMAGE}

# The base image supplies the Qt/Shotcut C++ toolchain. For this first cache
# experiment, the host Rust/Go toolchains are bind-mounted by the Makefile;
# this avoids adding a second multi-GB toolchain layer and measures the
# build-local behavior directly. A later reproducible image can bake these in.

ENTRYPOINT []
WORKDIR /
