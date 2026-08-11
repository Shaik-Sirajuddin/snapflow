# Project Switch Active/Pending Ownership

The daemon keeps one authoritative instance per project. Conflicting
notifications are retained as pending candidates rather than replacing the
active instance.

```mermaid
sequenceDiagram
    participant U as User
    participant G1 as Existing GUI<br/>PID A
    participant G2 as New GUI<br/>PID B
    participant D as Daemon
    participant R as Registry

    U->>G1: Project P already open
    G1->>D: Register active(P, PID A)
    D->>R: activeByProject[P] = PID A
    R-->>D: PID A is authoritative

    U->>G2: Open same Project P
    G2->>D: Register/project-switch(P, PID B)
    D->>R: Check activeByProject[P]

    alt PID A is alive and valid
        R-->>D: PID A still active
        D->>R: Store PID B in pendingByProject[P]
        D-->>G2: Accepted as pending, do not replace PID A
        D-->>G1: Keep project operations routed to PID A
    else PID A is stale or cleared
        R-->>D: PID A invalid
        D->>R: Promote PID B to activeByProject[P]
        D-->>G2: PID B is now authoritative
    end

    U->>G1: Edit Project P
    G1->>D: Save/edit request
    D->>R: Resolve activeByProject[P]
    R-->>D: PID A
    D-->>G1: Route operation to PID A
    Note over G2: Pending PID B receives no edits yet

    D->>R: Periodic cleanup/reconciliation
    R-->>D: Validate PID A

    alt PID A remains valid
        D->>R: Keep PID A active
        D->>R: Retain valid pending candidates
    else PID A is gone
        D->>R: Validate pending PID B
        R-->>D: PID B is alive
        D->>R: Atomically promote PID B
        D-->>G2: Re-emit latest Project P state
    end
```

Each pending candidate should retain its PID/process-start identity, instance
ID/nonce, project path, lifecycle generation, and last-seen timestamp. Cleanup
must validate the active owner first, then promote the newest valid candidate
only after the active owner is confirmed stale.
