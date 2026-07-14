#!/usr/bin/env bash
# Regenerates docs/schema/acpx-http.openapi.json from acpx-proto's
# openapi.rs (see that module's doc comment). Run this after changing
# the HTTP transport's paths/headers or any type it $refs, then run
# `cargo test -p acpx-proto` (or the workspace suite) -- openapi_test.rs
# fails the build if the committed file and the current source disagree.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$repo_root/docs/schema/acpx-http.openapi.json"

mkdir -p "$(dirname "$out")"
cargo run -q -p acpx-proto --bin gen-openapi >"$out"

echo "wrote $out"
