# ACPX runtime and project-switch recovery

ACPX gateways own WebSocket tasks on the Tokio runtime that creates them. A
process-global Gateway cache allowed a recreated panel bridge to reuse a
transport whose original runtime had already shut down. Project-specific
session cleanup remains valid, but it must clear actor state and reacquire the
logical session before the next call.

Verification: bridge recreation gets a fresh gateway; close clears local
session/lease state; project switch preserves thread ownership and cleanup;
transport reset remains reconnectable.
