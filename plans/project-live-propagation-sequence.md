# Live project propagation lifecycle

This sequence describes the intended ownership model: one external registration
per Snapflow GUI process, with project selection represented as an update to
that registration. The registration is released only when the GUI process exits.

## Ownership recommendation

Use a global process-level registration, not one registration per project.

| Concern | Owner | Cardinality | Lifetime |
| --- | --- | --- | --- |
| GUI process identity, PID, nonce, SAP endpoint | `SnapshotdRegistration` | one per Snapflow process | process start → process exit |
| Active project path/id/generation | registration state | zero or one per process | project open/switch → next switch/close |
| Daemon-owned headless process | `ProcessInstance` | one per live daemon child | child start → child exit |
| Project registry row | `Project` | one per project path | persistent |

The process-level model matches the existing GUI architecture: one Snapflow
process owns one SAP endpoint and can change its selected project. A per-project
registration would incorrectly imply that switching projects requires a new GUI
process or multiple SAP owners. It is appropriate only if the product later
supports multiple independent GUI processes, each with its own endpoint.

The daemon may still keep project-level lookup indexes, but those indexes should
point to the process-level instance. In other words:

```text
GUI process (PID 23299)
  └── external instance (one registration, one SAP socket)
        └── active project: record.mlt → another project on switch
```

```mermaid
sequenceDiagram
    autonumber
    participant GUI as Snapflow GUI process
    participant Panel as panel-rust
    participant Reg as SnapshotdRegistration worker
    participant D as snapshotd daemon
    participant SAP as GUI SAP socket
    participant MCP as MCP/SSH client

    Note over GUI,D: Process startup: registration is process-scoped
    GUI->>Panel: create panel/plugin
    Panel->>Reg: start(initialProjectPath)
    Reg->>D: registerExternalInstance(pid, nonce, SAP socket)
    D-->>Reg: instanceId + lease/heartbeat interval
    Reg->>GUI: publish discovery descriptor

    Note over MCP,SAP: Open or switch project on the same GUI instance
    MCP->>D: project.open(path or projectId)
    D->>D: discover/resolve existing external owner
    D->>SAP: sap.hello + project.select(project)
    SAP-->>D: project selected/opened
    D-->>MCP: opened/reused, same instanceId/PID
    D-->>Panel: project inventory/open state update
    Panel->>Reg: update(projectPath, reason, generation)
    Reg->>D: updateOpenProject(instanceId, projectPath, reason, generation)
    D-->>Reg: lease renewed + project ownership updated
    Reg->>GUI: keep descriptor with new project path

    Note over Panel,D: Recovery if a row was closed or expired
    Reg->>D: heartbeat(instanceId)
    alt heartbeat succeeds
        D-->>Reg: lease renewed
    else row missing/closed
        D-->>Reg: error or inactive result
        Reg->>D: registerExternalInstance(current PID/nonce/SAP)
        D-->>Reg: new instanceId + lease
        Reg->>D: updateOpenProject(new instanceId, current project)
        D-->>Reg: project ownership restored
    end

    Note over GUI,D: Panel reload/close must not release GUI ownership
    Panel-->>Reg: panel teardown
    Note over Reg,D: Registration worker remains alive and does not unregister

    Note over GUI,D: Actual process shutdown
    GUI->>Panel: process exit
    Panel->>Reg: shutdown
    Reg->>D: unregisterExternalInstance(instanceId)
    D-->>Reg: closed
    Reg-->>D: remove discovery descriptor/socket
```

## Invariants

- A project switch changes `projectPath`, `projectId`, `generation`, and
  lifecycle reason; it does not create a second GUI process.
- `SnapshotdRegistration` must outlive panel/plugin recreation and be owned by
  the Snapflow process lifecycle.
- If registration is absent during project selection, registration is recreated
  immediately before `updateOpenProject` is sent.
- `unregisterExternalInstance` is allowed only during actual GUI process exit.
