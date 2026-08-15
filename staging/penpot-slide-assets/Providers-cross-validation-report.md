# Providers board — cross-validation report

## Sources
- **Truth**: live Penpot file `snapflow launch`, board `Providers` (`ca27a974-130e-8085-8008-78781d3e06ce`)
- **Export**: `Providers-board.svg` / `.png` / `@2x.png`
- **Methods**: Penpot plugin geometry dump + SVG parse + Chrome file:// screenshot

## Faults found (before fix)
| Severity | Type | Detail |
|----------|------|--------|
| HIGH | viewBox_mismatch | Export used `viewBox="0 0 1280 720"` while board origin is `(24,48)` → crop/shift |

## Checks that passed
- All **10 texts** present, **no duplicate** labels
- All **7 images** embedded as data URIs
- Fonts match design families (Poppins/Orbitron/Unbounded/Syne/Silkscreen)
- After fix: **x deltas = 0** for all texts; **textLength = design bbox width**
- PNG opaque bg (corner ~ soft blue-gray, not transparent dualism)

## Expected non-faults
- SVG `y` is **baseline**, Penpot `y` is **box top** — difference is normal (ascent)
- `textLength` compresses glyphs slightly vs natural font width (matches Penpot layout boxes)

## Deliverables
- `/home/siraj/Desktop/codebases/prv/multimedia_agent/multi_media_main/staging/penpot-slide-assets/Providers-board.svg`
- `/home/siraj/Desktop/codebases/prv/multimedia_agent/multi_media_main/staging/penpot-slide-assets/Providers-board.png` (1280×720)
- `/home/siraj/Desktop/codebases/prv/multimedia_agent/multi_media_main/staging/penpot-slide-assets/Providers-board@2x.png` (2560×1440)

## Status
**Exact match on positions/fonts after viewBox + baseline recompute. Native Penpot PNG export API still HTTP-fails (exporter); validated via plugin geometry + Chrome SVG raster.**
