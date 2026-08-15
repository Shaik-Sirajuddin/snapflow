# Session Creation Request Idempotency

## Goal

Allow a client to safely retry `session/new` after a lost WebSocket response
without creating a duplicate ACPX session.

## Wire extension

Clients may include this metadata in `session/new` parameters:

```json
"_meta": {
  "com.example.client": {
    "creationRequestId": "7ee35d23-17d5-4a62-92cb-d36d857fc272",
    "workspaceId": "project-42"
  }
}
```

`creationRequestId` is a client-generated UUID that remains unchanged across
retries. `workspaceId` is optional correlation metadata and is not an
idempotency key by itself.

## Server behavior

1. Extract and validate the namespaced metadata on `session/new`.
2. Persist `creationRequestId` and `workspaceId` with the gateway session,
   scoped by tenant.
3. Add a unique tenant-scoped lookup for `creationRequestId`.
4. On a repeated `session/new` with the same tenant and ID:
   - If the original session exists, is not closed/deleted, and has not been
     used (no prompt/turn or equivalent activity), return the original
     gateway `sessionId` and do not create another backend session.
   - If it has already been used, return a deterministic idempotency-conflict
     JSON-RPC error; never silently create a second session for the same ID.
   - If the original row was deleted, treat the request as new.
5. Preserve the metadata in gateway-wide and backend-selected `session/list`
   responses under the same `_meta.com.example.client` namespace.

## Client behavior

1. Generate one `creationRequestId` per logical thread/session creation.
2. Reuse that ID for every retry of the same `session/new` operation.
3. Pass `workspaceId` when available.
4. Accept the returned gateway session ID and metadata from either the first
   response or an idempotent retry.

## “Unused” definition

The server must track whether the session has received a user turn. Creation
alone does not mark it used. A successful `session/prompt`, queue operation,
or other backend operation that begins conversation activity marks it used.
The check and the deduplicating insert must be atomic to prevent concurrent
retries from creating duplicates.

## Persistence and migration

- Add nullable `creation_request_id` and `workspace_id` columns to `sessions`.
- Add a unique index on `(tenant_id, creation_request_id)` for non-null IDs.
- Add a persisted usage marker (or use an existing state/activity revision with
  an explicit creation-only value) so the unused-session check survives a
  daemon restart.
- Keep old rows and clients fully compatible when `_meta` is absent.

## Tests

- First `session/new` stores metadata and returns it from `session/list`.
- Retrying the same unused request returns the same gateway ID and creates no
  second backend session.
- Retrying after a prompt returns the idempotency-conflict error.
- Concurrent duplicate requests create one session.
- Different tenants may reuse the same request ID without colliding.
- Deleted sessions no longer deduplicate future requests.
- Metadata remains namespaced and does not leak across tenants.
