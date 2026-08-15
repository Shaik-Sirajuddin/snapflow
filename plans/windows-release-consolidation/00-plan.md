# Windows/Linux release consolidation

## Scope

Integrate the reviewed Windows/runtime/UI fixes into a clean branch from local
`main`, preserve the existing plan/documentation changes, squash commits by
feature, perform two independent post-integration review/fix loops, verify the
release matrix, push, and create a release tag to trigger Windows/Linux builds.

## Source worktrees

- Project-switch lifecycle: `multi_media_main-project-switch`, latest reviewed
  fix `0db74b1fc`.
- AI-mode gate: `multi_media_main-disable-ai`, root `964080110`, Shotcut
  submodule `0dfcc48f`.
- Installer synchronization: `multi_media_main-windows-installer-sync`, final
  reviewed commit `a384e6fef`.
- MLT compatibility: `multi_media_main-mlt-plugins`, final package fix
  `eb4c39da8` plus managed-launch environment fixes in its ancestry.

## Release rules

- Work only in a clean release worktree based on local `main`.
- Preserve unrelated user changes in the current worktree.
- Squash into feature commits: lifecycle, AI-mode, installer/daemon, and MLT
  packaging/runtime.
- Do not push or tag until both review gates and verification pass.
- Windows runtime execution remains a required CI/runtime gate; Linux tests can
  run locally.

## Verification matrix

| Area | Local check | Required runtime gate |
|---|---|---|
| snapshotd Go | `go test ./...` | Windows native daemon/transport tests |
| panel-rust | `cargo check`, focused tests | Windows target check/build |
| AI-mode | config tests, Qt compile if available | Launch with flag true/false |
| MLT packaging | package layout/assertions | Windows GUI create/save/open |
| installer | PowerShell syntax/static checks | Windows `irm` install + stale-lock restart |

## Open risks

- Shotcut is a submodule and requires a consistent gitlink update.
- Windows runtime tests cannot execute on the Linux host.
- Release workflow/tag naming must match the repository's existing conventions.
