#!/usr/bin/env bash
# Regenerates docs/schema/acpx.openrpc.json from acpx-proto's METHODS
# registry (see acpx-proto/src/methods.rs and src/openrpc.rs). Run this
# after adding/changing a dispatched method or any type it references,
# then run `cargo test -p acpx-proto` (or the workspace suite) --
# openrpc_test.rs fails the build if the committed file and the current
# registry disagree.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$repo_root/docs/schema/acpx.openrpc.json"

mkdir -p "$(dirname "$out")"
cargo run -q -p acpx-proto --bin gen-openrpc >"$out"

echo "wrote $out"
