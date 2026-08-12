# Windows `melt` CLI packaging and export reliability

## Objective

Make Windows exports work identically for daemon-managed and self-launched GUI
instances. The final bundle must contain the `melt.exe` CLI, its complete DLL
closure, MLT data/modules, and a verified resolver contract.

## Root cause

Commit `2f0b57ec0` moved Windows from the unavailable prebuilt MLT archive to
MSYS2 packages and replaced the vendor deployment script with manual copying.
That copied MLT modules and DLLs but omitted the standalone `melt.exe` CLI.
The resolver fixes in `3727b1133`/`5f19b6934` therefore fall back to PATH in an
installed bundle, where MSYS2 is not present.

## Phases

1. **Inventory and contract**: identify the MSYS2 `melt.exe` location, its PE
   dependency closure, MLT module/data paths, and both resolver paths.
2. **Windows packaging**: copy `melt.exe` beside `Snapflow.exe`, recursively
   bundle dependencies rooted at `melt.exe`, and assert the final tree/ZIP
   contains the executable, MLT repository, data, and frei0r plugins.
3. **Resolver hardening**: ensure daemon and GUI paths resolve the same absolute
   packaged executable and return a clear missing-tool error instead of a vague
   PATH failure.
4. **Tests and review**: add focused resolver/package-contract tests, run two
   review passes, and resolve findings.
5. **Verification**: run local Go/Rust tests, then trigger the Windows workflow
   from main and validate a daemon-managed and native GUI export on Windows.

## Contract

- `Snapflow/snapflow.exe`
- `Snapflow/melt.exe`
- root-level `lib/mlt/*.dll`
- root-level `share/mlt/` including profiles
- root-level `lib/frei0r-1/*.dll`
- every DLL imported by `snapflow.exe`, MLT modules, frei0r modules, or
  `melt.exe` must be beside the executable or in the explicitly supported
  repository layout.

## Verification matrix

| Scenario | Expected result |
|---|---|
| daemon-managed launch | `MELT_BIN` is an absolute existing `Snapflow/melt.exe` path |
| self-launched GUI | resolver finds `melt.exe` beside the running GUI |
| missing melt in development | export fails with an actionable missing-tool error |
| packaged MLT modules | XML/MLT module repository loads |
| packaged melt closure | `melt.exe` starts without MSYS2 PATH |
| final ZIP | passes archive integrity and contains `Snapflow/melt.exe`, MLT data/modules, frei0r, and required DLLs |
