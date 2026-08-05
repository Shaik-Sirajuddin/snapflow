# MLT filter/clip property audit and strict MCP schema

Date: 2026-08-05  
Scope: `filter.add`, `filter.setProperty`, `filter.addKeyframe` and the
Shotcut C++ FFI implementation.

## Finding

These MCP arguments are **not hard-typed today**. The MCP adapter validates
only the envelope (`mltService: string`, `property: string`, `value: any`).
The C++ implementation then forwards the key/value directly to MLT:

| MCP operation | C++ implementation | Behaviour |
|---|---|---|
| `filter.add` | `applyJsonPropertiesToFilter()` → `Mlt::Filter::set()` | accepts every scalar key; unknown keys are stored in the MLT property bag |
| `filter.setProperty` | `filter->set()` or `filter->anim_set()` | accepts every scalar key; no service/schema lookup |
| `filter.addKeyframe` | `filter->anim_set()` | accepts every key; an unknown key can still receive an animation |

References: `shotcut/src/rustbridge/sap_ffi.cpp` functions
`applyJsonPropertiesToFilter`, `sap_filter_add`, `sap_filter_set_property`,
and `sap_filter_add_keyframe`.

`filter.list` is not a schema endpoint. It enumerates the live MLT property
bag after attachment, so it can echo a property that the renderer never reads.

## Ruled-out properties from the reported examples

### `brightness`

The checked-in MLT service definition is
`shotcut/scripts/src/mlt/src/modules/core/filter_brightness.yml`; the
implementation is `filter_brightness.c`.

| Property | Status | Evidence |
|---|---|---|
| `level` | supported; numeric; animatable | `filter_brightness.c` reads `level` with `mlt_properties_anim_get_double` |
| `alpha` | supported; numeric; animatable | implementation reads `alpha` independently of `level` |
| `start`, `end` | supported legacy numeric inputs | implementation reads both; `level` is preferred for current use |
| `threads` | supported numeric | implementation reads it for slice count |
| `rgb_only` | supported boolean | implementation reads it while selecting RGB/YUV processing |
| `opacity` | **ruled out** | not declared in the YAML and never read by `filter_brightness.c`; setting it only adds an unused property |

Therefore the previous `brightness.opacity` call was accepted by MCP/MLT's
generic property bag but could not affect rendered output. Use `alpha` for
independent alpha adjustment.

### `affine` filter

The `affine` filter is a wrapper around the `transition.affine` service. Its
definition is `modules/plus/filter_affine.yml` and the transition definition is
`modules/plus/transition_affine.yml`.

| Property | Status |
|---|---|
| `background` | supported filter property |
| `producer.*` | supported pass-through namespace |
| `transition.*` | supported pass-through namespace, but the suffix must be a real `transition.affine` property |
| `use_normalized` / `use_normalised` | supported (second is deprecated) |
| `transition.rect` | supported, animated rectangle (`X/Y:WxH[:opacity]`) |
| `transition.ox`, `transition.oy` | supported, animated offsets |
| `transition.scale_x`, `transition.scale_y` | supported, animated scale |
| `transition.rotate_*`, `transition.shear_*`, `transition.fix_*` | supported by `transition.affine` |
| `transition.geometry` | **ruled out**; no such property is declared or read by `transition_affine.c` |
| `geometry` (without the `transition.` namespace) | **ruled out for this filter**; it belongs to text/other services, not affine |

The correct keyed transform call is consequently `property:
"transition.ox"` (or `transition.rect`, etc.), not
`transition.geometry`.

## Strict schema contract to implement

Validation must happen before mutating the project, in the Rust MCP/backend
boundary. The validator should resolve a service descriptor using this order:

1. **Static MLT descriptor**: load the installed MLT YAML descriptor for the
   exact service. Accept only declared `identifier` values and declared
   namespaces (`producer.*`, `transition.*`). Enforce `type`, `values`,
   `minimum`, `maximum`, `mutable`, and `animation`.
2. **Runtime service availability**: require the exact `mltService` to appear
   in the running MLT repository (`melt-7 -query filters|producers|transitions`
   equivalent). A descriptor that is present in source but absent from the
   deployed bundle must be rejected.
3. **Dynamic plugin services**: do not pretend these have a complete static
   schema. `avfilter.*`, `sox.*`, `frei0r.*`, LADSPA/LV2/VST, OpenFX, and
   other externally discovered services require a runtime descriptor or an
   explicit `schemaUnavailable` error. Never accept arbitrary properties for
   them as a fallback.

Suggested validation errors:

```json
{
  "code": -32602,
  "message": "unsupported filter property",
  "data": {
    "mltService": "brightness",
    "property": "opacity",
    "allowed": ["level", "alpha", "start", "end", "threads", "rgb_only"],
    "reason": "not declared by the MLT service descriptor"
  }
}
```

For `filter.add`, validate every entry in `properties` atomically before
attaching the filter. For `filter.setProperty` and `filter.addKeyframe`, reject
unknown, read-only, or non-animatable properties before calling `set/anim_set`.
The validator must also validate the `transition.*`/`producer.*` suffix after
namespace expansion; accepting the prefix alone is insufficient.

## Deployed MLT service inventory

The bundled MLT 7.41 repository was queried with:

```sh
shotcut/scripts/Shotcut/Shotcut.app/bin/melt-7 -query filters
shotcut/scripts/Shotcut/Shotcut.app/bin/melt-7 -query producers
shotcut/scripts/Shotcut/Shotcut.app/bin/melt-7 -query transitions
```

The inventory is broader than the checked-in static descriptors because it
includes dynamically discovered plugins. Static schema coverage in this tree
includes the following service families:

- **Filters:** core (`brightness`, `box_blur`, `crop`, `gamma`, `mirror`,
  `panner`, `watermark`, …), plus (`affine`, `chroma`, `dynamictext`,
  `hslprimaries`, `lumakey`, `subtitle`, `text`, …), Qt (`qtext`, `qtcrop`,
  `qtblend`, `gpstext`, …), Movit, Normalize, Oldfilm, Kdenlive, Placebo,
  Vid.Stab, OpenCV, and other module YAML descriptors under
  `shotcut/scripts/src/mlt/src/modules/**/filter_*.yml`.
- **Clip/producer services:** `avformat`, `color`/`colour`, `image`/`qimage`,
  `qtext`, `kdenlivetitle`, `timewarp`, `noise`, `tone`, `subtitle`, `xml`,
  `glaxnimate`, `pango`, `pixbuf`, `framebuffer`, and the other producer YAML
  descriptors under `modules/**/producer_*.yml`.
- **Transitions:** `composite`, `luma`, `mix`, `matte`, `qtblend`, `vqm`,
  `affine`, and dynamically loaded Movit transitions.
- **Time manipulation:** `producer.timewarp` uses the required `resource`
  argument in `[speed:resource]` form (20x to 0.01x, negative for reverse);
  `link.timeremap` exposes animated `time_map`/`speed_map`, plus `image_mode`.
  These are producer/link services, not arbitrary filter properties.

The authoritative source for each static property list is the corresponding
MLT YAML file, not the MCP tool's generic JSON schema. The schema generator
should fail closed when a descriptor is missing or when the deployed MLT
version differs from the descriptor version.

## Recommended tests

1. `brightness.opacity` is rejected and does not appear in project XML.
2. `brightness.alpha` and `brightness.level` are accepted; keyframes are
   accepted only for properties marked `animation: yes`.
3. `affine.transition.ox` and `affine.transition.rect` are accepted;
   `affine.transition.geometry` is rejected.
4. Read-only properties such as `timewarp.warp_speed` are rejected by writes.
5. An unavailable dynamic plugin service returns `schemaUnavailable`, never a
   successful no-op.
6. Validation is atomic: one invalid property in `filter.add` attaches no
   filter and leaves the project unchanged.

## MCP enum audit and recommended changes

The MCP surface already correctly enumerates several closed sets:

| Field | Current enum | Assessment |
|---|---|---|
| `edit.addTrack.kind` | `video`, `audio` | correct |
| `filter.addKeyframe.interpolation` | `linear`, `smooth`, `discrete`, `hold` | correct |
| `audio.normalize.mode` | `1pass`, `2pass` | correct |
| `generator.createTitle.mode` | `simple` | correct |

The following additional enum is safe and should be added to
`snapshotd/internal/mcpadapter/tools_project.go`:

```go
mcp.WithString("projectType",
    mcp.Enum("folder", "file"),
    mcp.Description("Project storage type"),
)
```

`playback.getFrame.format` can also be safely constrained in
`tools_playback_notes.go` to the formats implemented by the C++ bridge:

```go
mcp.WithString("format",
    mcp.Enum("jpeg", "png"),
    mcp.DefaultString("jpeg"),
    mcp.Description("Image format"),
)
```

The following fields should remain strings rather than receive a static MCP
enum:

| Field | Reason |
|---|---|
| `file.export.codec` | FFmpeg exposes many codecs; the Rust backend normalizes common aliases (`h264`, `hevc`, `vp9`, `av1`, `prores`) but intentionally accepts custom installed codecs. |
| `file.export.container` | Available containers vary with the installed MLT/FFmpeg build. |
| `edit.setTrackProperties.blendMode` | qtblend uses numeric Porter-Duff values while Movit uses named modes; the valid set depends on the active transition implementation. |
| File paths, project IDs, clip IDs, names, notes, and marker colors | These are open-ended values, not closed enumerations. |

`filter.mltService` and `filter.property` require dependent schemas rather than
one global enum. The property enum must be selected after resolving the
service, for example:

```text
mltService = brightness
property    = level | alpha | start | end | threads | rgb_only

mltService = affine
property    = transition.rect | transition.ox | transition.oy |
              transition.scale_x | transition.scale_y | ...
```

These dependent enums should be generated from the MLT YAML descriptors and
enforced by the Rust validator. The MCP tool description may expose the
available service list, but correctness must not rely on a single static
global property enum.

## Conclusion

The C++ side is intentionally generic plumbing, not a typed MLT property API.
`opacity` on `brightness` and `transition.geometry` on `affine` must therefore
be ruled out by a new descriptor-backed validator. The validator should use
MLT's own YAML descriptors and runtime repository inventory, while failing
closed for dynamically discovered plugin families.

## Applied Rust safeguards

The SAP Rust dispatcher now performs service-specific validation for the
known silent-no-op cases before invoking the C++ bridge:

- `brightness`: validates `level`, `alpha`, `start`, `end`, `threads`, and
  `rgb_only`; rejects `opacity` and rejects non-numeric alpha/level values.
- `affine`: validates the supported `transition.*` property suffixes and
  rejects `transition.geometry`.
- `qtcrop`, `volume`, and `panner`: validate their known property sets.
- Unknown/dynamic services remain pass-through for compatibility with plugins
  already accepted by Snapflow; they are not falsely rejected by this static
  registry.

Validation is applied to `filter.add`, `filter.setProperty`, and
`filter.addKeyframe`. The new `filter.describe` MCP tool exposes the known
Rust schema for a requested service and returns `schemaAvailable: false` for
dynamic/unknown services. This is complementary to MCP's envelope schema:
MCP validates request shape, while Rust validates service-specific semantics.
