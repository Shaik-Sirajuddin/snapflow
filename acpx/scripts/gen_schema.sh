#!/usr/bin/env bash
# Regenerates docs/schema/acpx-wire.schema.json from acpx-proto's
# #[derive(JsonSchema)] wire types (see acpx-proto/src/schema.rs and
# src/bin/gen_schema.rs). Run this after changing any type in
# acpx-proto/src/{jsonrpc,session,agent}.rs, then run
# `cargo test -p acpx-proto` (or the workspace suite) -- schema_test.rs
# fails the build if the committed file and the derived types disagree.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$repo_root/docs/schema/acpx-wire.schema.json"

mkdir -p "$(dirname "$out")"
cargo run -q -p acpx-proto --bin gen-schema >"$out"

echo "wrote $out"
