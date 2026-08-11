# Windows MLT root-prefix consolidation

## Objective

Use the v0.1.45 physical Windows package layout (`lib/mlt` and `share/mlt`
beside `Snapflow/`) while retaining the latest native Qt and daemon launch
hooks. Both launch modes must point to the same repository and data paths.

## v0.1.45 versus v0.1.47

- v0.1.45 packaged and checked `$OUT/lib/mlt/libmltxml.dll`, matching the
  Windows resolver observed on Nitro, but had no native Qt environment setup.
- v0.1.47 added native and daemon path setup, but packaged only
  `Snapflow/lib/mlt-7` and checked that nested path. Nitro still resolved the
  loaded DLL against the parent-level unversioned `lib/mlt`.
- The fix is the union: restore root-level unversioned packaging and point
  both native Qt and daemon-launched children there.

## Verification matrix

1. Packaging: workflow copies `/mingw64/lib/mlt` and `/mingw64/share/mlt` to
   `$OUT/lib/mlt` and `$OUT/share/mlt`, and checks `libmltxml.dll`.
2. Daemon: `appendBundledMltEnv` resolves the install-root sibling of
   `Snapflow/` and emits absolute `MLT_*` paths.
3. Native: `configureBundledMltPaths` uses the same install-root paths before
   MLT initialization.
4. Unit: `go test ./internal/procmgr` passes.
5. Review: inspect the final diff for DLL-copy pipeline preservation and path
   consistency; runtime smoke remains a release-host verification step.

The Nitro v0.1.45 trial confirmed the directory path, but exposed a second
packaging gap: module-specific FFmpeg/audio/filter dependencies were not
walked by `bundle_dlls`, so several plugins failed to load. The workflow now
walks every copied MLT/frei0r module recursively and bundles its transitive
DLL dependencies. frei0r is explicitly installed and asserted in the final
package rather than omitted as an accidental side effect of the minimal build.

The final-package boundary is now guarded independently of the intermediate
GUI ZIP. The package job promotes a misplaced dependency into `Snapflow/` when
it can be found, and fails otherwise; the final archive must contain
`avdevice-63.dll`, `avfilter-12.dll`, `lib/mlt/libmltxml.dll`, and frei0r.
Native Qt also adds the executable directory to the Windows DLL search path so
modules loaded from the sibling `lib/mlt` repository can resolve dependencies
kept beside `Snapflow.exe`, matching upstream deployment layout.
