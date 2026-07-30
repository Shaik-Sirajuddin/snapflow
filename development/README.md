# Development commands

See [`testing.md`](testing.md) for running/writing Slint-MCP-driven end-to-end
UI tests.

```bash
make -f dev.make docker-build
make -f dev.make docker-up
make -f dev.make docker-down
make -f dev.make docker-down-v
make -f dev.make docker-rebuild
make -f dev.make docker-relaunch
make -f dev.make docker-connect
make -f dev.make docker-logs
make -f dev.make status
make -f dev.make build
make -f dev.make vnc-init
make -f dev.make vnc-up
make -f dev.make vnc-status
make -f dev.make vnc-down
make -f dev.make vnc-shared-down
make -f dev.make measure
make -f dev.make slint-hot-reload
make -f dev.make slint-hot-reload-down
```

```bash
make -f dev.make \
  WORKTREE=/absolute/path/to/worktree \
  vnc-up
```

```bash
make -f /absolute/path/to/main/dev.make \
  REPO_ROOT=/absolute/path/to/main \
  WORKTREE="$PWD" \
  vnc-up
```

```bash
make -f dev.make \
  VNC_SHARED_ROOT=/absolute/path/to/shared/state \
  vnc-init
```
