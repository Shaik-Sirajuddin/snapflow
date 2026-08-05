# Clip speed and playback fast-forward

Implemented in parallel from the clean `main`-based worktree:

- `playback.fastForward`: transient native transport speed change.
- `edit.setClipSpeed`: persistent MLT `timewarp`, undoable replacement, filter/property preservation, and exportable project state.

Verification: Rust unit/integration tests, Go MCP adapter tests, real-FFI MCP E2E, and CMake Debug build. The real-clip success/export scenario remains a follow-up; the real-FFI test covers successful fast-forward and clip-speed routing/rejection for a missing clip.
