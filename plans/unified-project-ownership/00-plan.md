# Unified project ownership and `.mlt` propagation

## Objective

Make daemon-managed and self-launched GUI instances use one lifecycle contract
for registration, project open/switch, retry, close, and ownership. The daemon
must expose the currently selected `.mlt` path and truthful active/open state to
MCP even when the GUI is external.

## Implementation phases

1. Registry identity: update an existing folder record's selected MLT filename
   when a concrete `.mlt` path is observed; make live external ownership count
   as `active`/`isOpen` in project-list responses.
2. External lifecycle claim: route initial external registration and external
   project switches through `ClaimProjectOwner`, retaining conflicts as pending
   and avoiding duplicate handoff/edits while another owner is live.
3. Retry and cleanup: preserve latest external path/generation while daemon or
   SAP registration is unavailable; reconcile expired owners and promote the
   newest live pending candidate for both owner modes; close/unregister releases
   markers and invalidates pooled SAP routing.
4. Test matrix: first registration before daemon, retry after daemon start,
   daemon-managed launch/reuse, external launch/reuse, close/reopen, same-folder
   different `.mlt`, conflicting owners, stale PID/lease, and MCP status parity.

## Design decisions

- A project remains folder-keyed for durable storage; `mltFileName` is the
  selected concrete file and is updated only when a concrete path is reported.
- `project_active_owners` is the routing marker. Process/external rows remain
  lifecycle records; pending rows are retained until reconciliation promotes
  them.
- A conflicting external registration is accepted as a live lease but returned
  as pending ownership; it is never routed to edits until promotion.
- Registration callbacks remain asynchronous and retry the latest generation;
  a transient daemon/SAP failure must not replace the queued latest path with an
  older callback.

## Verification matrix

| Scenario | Expected result |
|---|---|
| GUI starts before daemon | discovery survives; latest path registers after daemon starts |
| daemon-managed launch | process row and active-owner marker agree |
| external launch | external row and active-owner marker agree |
| project A → B | old marker released; B marker/path/generation published |
| close → reopen | old lease/marker removed; reopened GUI becomes active |
| same folder `d.mlt` → `ds.mlt` | project id/root stable, `mltFileName=ds.mlt` |
| second owner | pending candidate retained; no duplicate edit routing |
| stale owner | reconciliation promotes newest live pending candidate |
| MCP list | `active`, `isOpen`, `open`, path, and instance count agree with live owner |
