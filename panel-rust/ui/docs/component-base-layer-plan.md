# Component base-layer plan (working doc)

**Cross-reference:** the physical reorganization of `base/` into
`form/`/`buttons/`/`feedback/`/`text/`/`layout/`/`terminal/` subfolders (plus
`icon.slint`/`hover.slint` staying at `base/` root) is decided and documented
in `panel-rust/ui/docs/base-layer-folder-structure.md` — **Option A, direct
hard cutover, no barrel shim**. That doc owns the file-move mechanics (full
ordered `git mv` list, every touched import line). This doc cites the
resulting **new** paths (e.g. `base/text/base_text.slint` instead of the old
flat `base/base_text.slint`) wherever a component is referenced below, since
a reader landing on this doc first should see the folder move as already
decided, not still open.

**Naming note:** `base/base_text.slint`'s exported component is being
renamed `BaseText` → `Text` as part of the same reorg (verified against the
current file: it presently reads `export component BaseText inherits Text`,
so this is a real rename, not doc catch-up). Every reference to this
component below reads `Text (was BaseText)`. See
`base-layer-folder-structure.md`'s naming-note section for the full
before/after on its two current call sites.

## §0. Base-layer usage rule (target end state)

Going forward, files under `pages/`, `components/`, and `chat-view/` are
**required** to consume a `base/` component (`Text`, `Button`, `IconButton`,
`Icon`, `Card` once it exists, `Badge`, `StatusDot`, `Divider` once it
exists, etc.) — or `extend`/wrap one — instead of instantiating Slint's raw
primitives directly (`Text {}`, a `Rectangle {}` + `TouchArea {}` pair
standing in for a button, a raw `Image {}`, etc.). This is not optional
cleanup: it's the target contract for the base layer once this plan lands.
The only accepted exception is a **documented one-off** where no base/
component fits the need — the codebase already has exactly one legitimate
precedent for this: `pages/settings/components/agent_logo.slint`'s raw
`Image {}` (4x) for brand logos, which must keep their original color and
therefore can't route through `Icon`'s `colorize`-able treatment. Any new
raw-primitive usage outside `base/` going forward should carry a comment
like that file's, explaining why no base/ component applies — silent raw
usage is a rule violation, not a style preference.

This rule is why §2b/§4 below flag **partial adoption** (files that already
import a base/ component for part of their needs but still drop back to raw
primitives nearby) as a **higher-priority "quick win"** fix than the
greenfield gaps in §6 — the base component in those cases already exists and
is already imported in the same file, so there's no new-component work
blocking the fix, only a mechanical swap.

Grounded in a full read of `tokens/*.slint` + `base/*.slint` (31 files) and a
grep audit of everything else under `panel-rust/ui/` (pages/, components/,
chat-view/, plus app.slint, chat_area.slint, chat_view_stack.slint,
settings_pane_preview.slint, preview.slint, types.slint — 33 files, ~12.3k
lines). Not exhaustive — counts are `rg -c` totals, examples capped ~5/file.

## 1. Current state

### tokens/

| file | role | notes |
|---|---|---|
| `colors.slint` | `ColorScale` — raw hex only, dark/light pairs per role | Only file allowed to hold a raw hex literal per its own doc comment. Doc comment cites `ui_html/index.html`'s CSS as source of truth. |
| `typography.slint` | `Typography` — `font-mono`, `font-scale-base` | Only 2 properties. No size/weight scale at all — see gap analysis. |
| `metrics.slint` | `Metrics` — radius presets, `space-1..10`, `pad-xs..2xl`, `gap-sm/md/lg`, `icon-sm/md/lg`, `pulse-period`, `radius-corner-cut` | Has a real spacing/icon scale. Has no equivalent type-scale. |
| `theme.slint` | `Theme` — merges the three into `bg-*`/`text-*`/`border-*`/`radius-*`/`font-*`, resolves dark/light + radius preset | Only file allowed to reference `ColorScale`. Two raw hex literals live here too (`selection-bg: #60a5fa80`, `md-link: #60a5fa`, `md-heading: #eab308`) — technically fine per the "only theme.slint" rule but worth noting they're not routed through `ColorScale` like everything else. |
| `cursor_host.slint` | `CursorHost` — shared `kind` string every interactive component writes to | Working as designed, well adopted. |
| `selection_focus.slint` | `SelectionFocus` — focused-count for selectable TextInputs | Working as designed. |
| `strings.slint` | `Strings` — all `@tr()` UI copy | Working as designed, well adopted. |

### base/ (31 files)

New-path column reflects the decided Option A reorg
(`base-layer-folder-structure.md`); `icon.slint`/`hover.slint` don't move.

| component | old path | new path | current variant props | what's missing |
|---|---|---|---|---|
| Badge | badge.slint | `base/feedback/badge.slint` | label, bg, fg, border-col, border-w, font-size, badge-font-family, badge-font-weight, pad-h, pad-v | No size scale (font-size is a raw 9px default); every prop freely overridable so "Badge" has no enforced look |
| **Card** | **does not exist — proposed** | `base/layout/card.slint` (structural/shell shape, fits the `layout/` group) | n/a | The "bordered container box wrapping content" shape is redeclared independently in 5 files (`message_card.slint`, `permission_card.slint`, `terminal_card.slint`, `local_terminal_card.slint`, `pages/settings/components/agent_card.slint`) with no shared base — see §3 for the full write-up |
| Text (was BaseText) | base_text.slint | `base/text/base_text.slint` | **none** — fixed font-family/size(13px)/color/wrap | New/WIP (untracked). No weight, size, mono, or tone variant at all — see §3. Component identifier renamed `BaseText` → `Text` as part of the reorg — see cross-reference note at top of doc. |
| Button | button.slint | `base/buttons/button.slint` | label, primary(bool), btn-width, btn-height, font-size, font-weight, bg, fg, border-col, border-w, busy | No size scale (only one fixed 72×26 default); "primary" is the only tone axis |
| CollapseExpand | collapse_expand.slint | `base/layout/collapse_expand.slint` | open, animate-width | layout-only, no typography/color surface |
| ContextRing | context_ring.slint | `base/layout/context_ring.slint` | ratio, used-tokens, limit-tokens, size | `ring-color` threshold logic (90%/70%) is hand-inlined per-instance, not a shared "tone by ratio" helper |
| DynamicModal | dynamic_modal.slint | `base/layout/dynamic_modal.slint` | title, is-mcp, show-icon, modal-width, modal-height, open | **Hardcodes `background: #00000040`** — raw hex scrim color not in Theme, breaking the "only theme.slint holds hex" rule from inside base/ itself |
| ExpandablePanel | expandable_panel.slint | `base/layout/expandable_panel.slint` | collapsed-max-height, collapsed-by-default, title | font-size 9px raw literal, not tokenized |
| FadeIn | fade_in.slint | `base/feedback/fade_in.slint` | (none, internal `shown`) | no Theme reference at all (fine, it's pure animation) |
| FilterSearchBar | filter_search_bar.slint | `base/form/filter_search_bar.slint` | query, placeholder, bar-accessible-label, show-icon, icon-trailing, leading-text, bar-bg, bar-height, bar-font-size, bar-border-width, bar-border-color | Good Theme/Metrics usage already — reference example |
| HoverSurface | hover.slint | `base/hover.slint` (unchanged, stays at root) | idle-bg, selected-bg, hover-bg, selected, button-accessible-label, interactive | Good generic base for clickable rows — reference example |
| Icon | icon.slint | `base/icon.slint` (unchanged, stays at root) | size, color | Good, well adopted (43 direct call sites); single `size` prop drives both width/height so it's aspect-ratio-safe by construction, but `size` is a raw `length` with no tie to `Metrics.icon-sm/md/lg` — see §6 |
| IconButton | icon_button.slint | `base/buttons/icon_button.slint` | button-accessible-label, btn-width, btn-height, active, enabled | Fixed 24×24 default, no size scale; unlike Icon, `btn-width`/`btn-height` are two independent props with **no aspect-ratio enforcement** — a caller can set mismatched values — see §6 |
| LabeledSelect | labeled_select.slint | `base/form/labeled_select.slint` | label, options, value, placeholder, searchable, select-accessible-label | Label font-size 10px raw, not tokenized |
| LinkText | link_text.slint | `base/text/link_text.slint` | text, font-size, mono | font-size default 12px raw |
| MarkdownView | markdown_view.slint | `base/text/markdown_view.slint` | blocks, lines, fallback-text, row-width | Heavy Theme usage (good), but `heading-size()` hardcodes 18/16/14/13px inline instead of a shared heading scale |
| MentionPickRow | mention_pick_row.slint | `base/form/mention_pick_row.slint` | highlighted, show-icon, icon-source, title, title-font-family, title-font-weight, subtitle, status-dot, action-label | font-sizes raw (11px/10px) |
| NamePromptDialog | name_prompt_dialog.slint | `base/layout/name_prompt_dialog.slint` | open, title, field-accessible-label, confirm-label, initial-value | font sizes raw everywhere (11px/10px) |
| NavItem | nav_item.slint | `base/layout/nav_item.slint` | label, active, show-icon, show-label, centered, icon-source, icon-size, label-font-size, label-font-weight-active/inactive, content-padding-left, content-spacing, active-color, inactive-color, fill-width, show-underline | Richest variant vocabulary in base/ — good exemplar of an active/inactive color+weight pair, but still a raw 11px font-size default |
| ResetOnFlipToggle | reset_on_flip_toggle.slint | `base/form/reset_on_flip_toggle.slint` | checked, enabled, toggle-accessible-label-on/off | wraps Toggle, no independent surface |
| SelectableChip | selectable_chip.slint | `base/layout/selectable_chip.slint` | label | font-size 10px raw |
| Select | select.slint | `base/form/select.slint` | options, value, placeholder, select-accessible-label, searchable | font sizes raw (11px/10px/9px); **overlaps SearchableDropdown** — two independently-built "click to open popup, pick one" primitives |
| SettingsRow | settings_row.slint | `base/layout/settings_row.slint` | label, hint, row-height, label-font-family, hint-font-family | font-size raw (11px/9px) |
| SettingsSection family | settings_section.slint | `base/layout/settings_section.slint` | SettingsSectionHeader(subsection bool), SettingsRowGroup, SettingsField(label, error), SettingsDivider | `SettingsField.error` hardcodes raw `#ef4444` with a comment admitting "no dedicated Theme.text-danger token exists yet" — real token gap |
| Spinner | spinner.slint | `base/feedback/spinner.slint` | size, stroke, stroke-width | Good, uses Metrics.pulse-period |
| StatusDot | status_dot.slint | `base/feedback/status_dot.slint` | status(string enum: connected/error/disconnected/other), size | Good Theme usage, but status string is a free-form string re-typed at every call site (no shared enum/type) |
| TerminalHeader | terminal_header.slint | `base/terminal/terminal_header.slint` | title, status-text, status-color | font sizes raw (11px/10px/9px) |
| TerminalLogBlock | terminal_log_block.slint | `base/terminal/terminal_log_block.slint` | text, content-padding, font-size, scrollable, text-overflow | font-size default raw 10px |
| TextField | text_field.slint | `base/form/text_field.slint` | text, field-accessible-label, field-font-size, placeholder | font-size raw default 11px |
| TextPillButton | text_pill_button.slint | `base/buttons/text_pill_button.slint` | label | font-size raw 10px |
| TextUtil | text_util.slint | `base/text/text_util.slint` | (callbacks only, no visual props) | n/a — string-ops helper global, not a component |
| ThinScrollbar | thin_scrollbar.slint | `base/layout/thin_scrollbar.slint` | horizontal, viewport-*/visible-*, thumb-width, active | Good, no typography surface needed |
| Toggle | toggle.slint | `base/form/toggle.slint` | checked, enabled, toggle-accessible-label | Fixed 32×18, no size variant |

## 2. Common properties to formalize

Redeclared inconsistently across the audited files (not invented — every
item below is backed by the table/grep data above):

- **Type scale.** `Metrics` has `icon-sm/md/lg` and `pad-xs..2xl`/`gap-sm/md/lg` but **no equivalent for font-size**. Every base/ component (and every page/component file) picks its own raw px default: 9, 10, 11, 12, 13, 14, 16, 18px all appear as literals across base/ alone, and `MarkdownView.heading-size()` hardcodes an 18/16/14/13px ladder inline. This is the single biggest missing token layer.
- **Weight scale.** Bare ints (400/600/700) used directly everywhere; no named vocabulary (regular/medium/semibold/bold), though usage already converges on exactly those three values almost everywhere audited.
- **Tone/color-role scale.** Three different naming conventions for the same "what color role is this" concept: `fg`/`bg` (Button, Badge), `idle-bg`/`hover-bg`/`selected-bg` (HoverSurface and its descendants), `active-color`/`inactive-color` (NavItem). No shared primary/secondary/muted/on-primary tone enum backing any of them.
- **Accessible-label prop naming.** Same concept, four different prop names depending on which component: `button-accessible-label` (Button, IconButton, HoverSurface, SelectableChip, TextPillButton) vs `field-accessible-label` (TextField, NamePromptDialog) vs `bar-accessible-label` (FilterSearchBar) vs `select-accessible-label` (Select, LabeledSelect, SearchableDropdown).
- **Status-color mapping.** `StatusDot` and `MentionPickRow.status-dot` both re-derive "connected/error/disconnected/other → green/red/yellow" as a free string, not a shared enum — currently harmless since both route through the same three Theme tokens, but the mapping logic itself is duplicated, not centralized.
- **Danger/error text color.** No `Theme.text-danger` token exists; `SettingsField.error` (base/) and other call sites (see §3, chat_area.slint's `#ef4444` uses) each hardcode the same raw hex independently instead of sharing one token.

## §2a. Naming convention: short, concern-grouped property names

Every *newly proposed* variant property in this plan follows one rule:
**one short word per property, chosen from a fixed set of concern
families** — `size`, `weight`, `bg`, `border`, `bordered`, `radius`, `pad`,
`shadow`, `variant`, `status`, `mono`. Each family name signals which
concern it governs (background/role, border, corner radius, spacing,
elevation, weight/size) the way Tailwind's utility prefixes do (`bg-`,
`text-`, `border-`, `rounded-`, `p-`, `shadow-`) — but translated into a
**single Slint identifier per concern**, not a literal multi-segment
`family-subfield` name, because (a) Slint doesn't have a colon/prefix
grammar to lean on the way a CSS-class string does, and (b) verbose
compound names (`bg-tone`, `text-weight`, `border-tone`, `content-padding`)
add characters without adding clarity once the property is already scoped
to one component. All existing base/ and tokens/ props already use
kebab-case for genuine two-word names (`btn-width`, `font-weight`,
`border-col`, `bar-accessible-label`, `radius-corner-cut` — spot-checked
against `base/button.slint`, `tokens/metrics.slint`), so this plan's new
props stay kebab-case wherever a name needs two words, but default to a
single short word wherever the concern is unambiguous alone.

**Before/after mapping** (old proposed name in an earlier pass of this doc
→ final short name; enum *values* like `primary`/`danger`/`sm`/`md`/`lg`
are unchanged from whatever was already grounded — only identifiers move):

| Component | Old proposed name | Final short name | Note |
|---|---|---|---|
| Card | `tone` | `bg` | New component, no existing prop to collide with. |
| Card | (implicit, via `border-col` only) | `border` (semantic enum) + `border-col` (kept, raw color escape) | Adds a `default\|strong\|danger\|success` enum alongside the existing raw-color override, doesn't replace it. |
| Card | `content-padding` | `pad` | |
| Card | `elevation` (length) | `shadow` (enum `none\|sm`) | |
| Card | `radius` (length default) | `radius` (enum `sm\|md\|lg`) | Same identifier, vocabulary changes from a raw length to a named scale. |
| Card | *(not proposed)* | *(no `align` prop added)* | See "Card content alignment" finding in §3 — grepped, no real pattern found. |
| Text (was BaseText) | `size`, `weight`, `mono` | unchanged | Already short/single-word. |
| Text (was BaseText) | `tone-override` | *(dropped)* | Redundant with the native inherited `color` property — see §3. |
| Button | *(bare `primary: bool)` axis only)* | `variant` (new enum `primary\|secondary\|danger\|success`) + `size` (new enum `sm\|md\|lg`) | **`bg` was deliberately not used here** — `base/button.slint` already declares `in property <color> bg` (a raw color, with 6 real call-site overrides: `permission_card.slint:117`, `local_terminal_card.slint:57`, `tool_group_view.slint:109`, `agents_view.slint:353,364,626`). A semantic string `bg: "primary"` would collide with — and break the type of — that existing, adopted prop. `variant` avoids the collision while still expressing the same "which semantic role" concern; `bg`/`fg`/`border-col`/`border-w` stay exactly as they are today as the raw escape hatches. |
| IconButton | *(none proposed)* | `size` (new enum `sm\|md\|lg`, replacing the independent `btn-width`/`btn-height` pair as the primary API — see §6's aspect-ratio gap) | `btn-width`/`btn-height` kept as raw overrides for the rare non-square case. |
| Icon | *(none proposed)* | `size` (new enum `sm\|md\|lg`, tied to `Metrics.icon-sm/md/lg`) | `color` (native) unchanged. |
| Badge | *(none proposed — free bg/fg override today)* | `variant` (new enum `primary\|secondary\|danger\|success\|warning\|neutral`) + `size` (new enum `sm\|md\|lg`) | See "Badge/StatusDot tone vocabulary" below — grounded via StatusDot's existing mapping, not via an existing Badge call site (Badge itself isn't yet used for status framing anywhere — this is a proposed *adoption target*, flagged honestly as such). |
| StatusDot | `status` (existing) | **unchanged** — kept as `status`, not renamed to `variant` | Real, adopted prop (`mention_pick_row.slint:106`, `agent_card.slint:56` set it today) — renaming it would break working call sites for a purity-only gain. Its 4 values (`connected\|error\|disconnected\|other`) are documented below as already mapping onto the same success/danger/warning family the other components use. |
| StatusDot | *(none proposed for `size`)* | **not converted** to the shared `size` enum | `base/status_dot.slint` already declares `in property <length> size: 8px` (a raw length, not a string). Converting it to the shared `sm\|md\|lg` string enum would be a breaking type change for a component whose real sizes are tiny and rarely vary — not worth the churn; flagged here so it isn't silently "missed" rather than deliberately skipped. |
| Toggle | *(none proposed)* | `size` (new enum `sm\|md\|lg`) | No tone/variant axis found in evidence — Toggle is binary on/off, not semantically toned. |
| Select / SearchableDropdown | *(none proposed)* | `size` (new enum `sm\|md\|lg`) | Same — no tone axis found; these are functional pickers. |
| TextField | *(none proposed)* | `size` (new enum `sm\|md\|lg`) | Same. |
| Divider *(proposed new component)* | *(none)* | optional `bg` (color override, default `Theme.border-default`) | No size/variant scale warranted — every found divider instance is uniformly `height: 1px`; no evidence of a thickness or tone scale. |
| ModalScrim *(proposed new component)* | *(none)* | `shade` (new enum `sm\|md\|lg`) | Grounded in 3 distinct alpha levels found in real scrim call sites: `#00000040` (`base/dynamic_modal.slint:22`), `#00000080` (`chat_area.slint:1335,1355`), `#00000088` (`app.slint:974`) — mapped `sm=0x40, md=0x80, lg=0x88`. |

## §2b. Variant enums per component

Two shared families run across *every* component where the underlying
evidence supports them: **`size`** (`sm\|md\|lg`, occasionally `xs`/`xl`
where real usage spans that wide — see Text below) and **`variant`**
(`primary\|secondary\|danger\|success`, the semantic-role family, named
`variant` rather than `bg` on Button specifically due to the real
`bg`-collision documented in §2a; new components without a pre-existing
`bg` prop, like Card, use `bg` directly). Every component also keeps
Slint's own native properties on the element it inherits (`Text`'s
`font-size`/`color`/`wrap`, `Rectangle`'s `border-radius`/`background`,
etc.) as raw escape hatches — the custom enum props are a convenience
default layered on top, never a removal of the ability to set the raw
property directly at a call site for a genuine one-off.

**Components with `size` only** (no semantic-role axis found in evidence):
Toggle, Select, SearchableDropdown, TextField, Icon, IconButton.

**Components with `size` + a semantic-role axis** (`variant` on Button;
`bg` on Card; `variant` on Badge; `status` — unchanged name — on
StatusDot): Button, Card, Badge, StatusDot (size intentionally *not*
converted on StatusDot, see §2a table).

**Components with neither** (pure layout/structural, no typography or
tone surface of their own — unchanged from the existing audit):
CollapseExpand, ContextRing, ExpandablePanel, FadeIn, ThinScrollbar,
DynamicModal (aside from its scrim-color gap, already tracked in §1/§6).

**Text (was BaseText)** — `size: xs|sm|md|lg|xl` (5-step, not the plainer
3-step `sm|md|lg` used elsewhere — grounded in real raw `font-size` usage
spanning far wider than 3 buckets: 7-10px cluster for meta/caption text
(e.g. `sidebar_thread_row.slint:288,298` at 7-8px), 11-13px cluster
matching the current 13px default (`md`), and an 18-22px heading cluster
(`onboarding_guide_view.slint:102` at 22px, `MarkdownView.heading-size()`'s
18/16/14px ladder) — a 3-step scale would force headings and captions into
the same bucket as body text, which the real data doesn't support).
`weight: regular|medium|semibold|bold` (grounded: `font-weight` raw values
found are exactly 400/500/600/700 — see the full `font-weight` file:line
list in §3). `mono: bool` (existing proposal, kept).

**Button** — `size: sm|md|lg` (new; currently one fixed 72×26 default with
no scale, `btn-width`/`btn-height` raw overrides seen ranging 28-72px wide,
16-26px tall across call sites — e.g. `local_terminal_card.slint:53-54`
(44×16), `pages/settings/views/agents_view.slint:349-350` (28×26),
`pages/settings/views/agents_view.slint:622-623` (52×20) — real spread
justifying 3 buckets). `variant: primary|secondary|danger|success` (new —
see the important caveat below: `danger`/`success` are *proposed*, not
*already duplicated*, values).

*Danger/success grounding, checked honestly.* Grepped every
`Strings.kill`/`delete`/`remove`/`reject`/`deny` Button call site
(`components/local_terminal_card.slint:50-57`, `pages/settings/views/
agents_view.slint:265-296` two-step delete-confirm flow, `pages/settings/
views/mcp_servers_view.slint:374-385` "Remove", `components/
permission_card.slint:155-196` allow/reject option rows) — **none of them
currently use a distinct red color.** Every one reuses the same
`bg: Theme.bg-card` / `bg: Theme.bg-primary-container` / `is-allow ?
Theme.bg-primary : Theme.bg-primary-container` treatment as ordinary
primary/secondary actions (evidence: `local_terminal_card.slint:57`,
`pages/settings/views/agents_view.slint:626`, `permission_card.slint:117`).
So `variant: danger` is a **genuinely new value** grounded in "these
destructive-action call sites exist and currently have no distinct visual
treatment" (a real gap, cited above), not in "this red-button pattern is
duplicated" (it isn't — nothing is currently red). Likewise `success`:
`permission_card`'s "allow" option uses the same `Theme.bg-primary` as any
other primary button, not `Theme.status-success` — so `success` is also a
proposed-not-yet-adopted value. This is a real, additional finding this
pass surfaced beyond the original font-size/font-weight/hex sweep: **no
destructive or confirmatory action anywhere in the audited files is
currently color-coded**, which §6 now calls out explicitly.

**IconButton** — `size: sm|md|lg` (new, replacing the independent
`btn-width`/`btn-height` pair as the primary sizing API, tied to
`Metrics.icon-sm/md/lg` the same way Icon's new `size` is — this is the
same fix already recommended in §6 for the aspect-ratio gap, just now named
per the shared vocabulary). No `variant` axis — `active`/`enabled` bools
already cover IconButton's state surface, no color-role duplication found.

**Icon** — `size: sm|md|lg` (new, tied to `Metrics.icon-sm/md/lg`, already
recommended in §6). `color` (native, unchanged) stays the raw escape hatch.

**Card** *(proposed new component — see §3 for the full property list and
evidence table)* — `size` is **not applicable** (Card's dimensions are
driven by its content, not a fixed scale). `bg: primary|secondary|danger|
success` (new; grounded in the 5 real conditional-background rows in §3 —
`message_card`'s bubble-role switch, `permission_card`'s
`Theme.bg-approval`, the terminal cards' `Theme.bg-terminal`, `agent_card`'s
active/inactive dim). `border: default|strong|danger|success` +
`bordered: bool` (grounded: 3 of 5 cards add a 1px ring, 2 don't).
`radius: sm|md|lg`, `pad: <length>`, `shadow: none|sm` (all grounded per
§3's table). No `align` (checked, see "Card content alignment" finding).

**Badge** — `size: sm|md|lg` (new; currently one fixed 9px default, no
scale — same pattern as Button). `variant: primary|secondary|danger|
success|warning|neutral` (new — like Button's `danger`/`success`, this is a
**proposed adoption target**, not a currently-duplicated Badge pattern:
Badge itself is barely used for status framing today. The grounding is
indirect — the *hand-rolled* warning/error bands this variant would let
Badge absorb already exist and are duplicated: `chat_area.slint:771-782`
(`#422006`/`#eab308` warning), `chat_area.slint:855-865` (`#b91c1c`/
`#ef4444` error), `pages/settings/views/mcp_servers_view.slint:512-525`
(`#2a1215`/`#ef4444` error) — three real hand-rolled banners with the same
shape, none of them going through Badge today. `variant` formalizes the
tone vocabulary Badge would need to actually replace them.)

**StatusDot** — `status: connected|error|disconnected|other` (existing
prop, name unchanged per §2a; now explicitly documented as mapping onto the
same tone family as everything else: `connected → success`, `error`/
`disconnected → danger`, `other → warning` — this mapping was already
implicit in `base/status_dot.slint`'s own ternary, just not named as a
shared enum before now). `size` intentionally not converted — see §2a.

**Toggle / Select / SearchableDropdown / TextField** — `size: sm|md|lg`
each (new; all four currently have exactly one fixed default with no
scale — Toggle 32×18, Select/SearchableDropdown/TextField all a single raw
font-size default of 9-11px). No `variant` axis found for any of the four —
none of them expresses a semantic tone/role today, only a functional state
(checked/unchecked, open/closed, focused/unfocused), which is out of scope
for a background-role enum.

**Divider** *(proposed new component, see §3)* — no `size`/`variant`
warranted; every real divider instance found is `height: 1px` uniformly.
Optional `bg` color override, defaulting to `Theme.border-default`.

**ModalScrim** *(proposed new component, see §3)* — `shade: sm|md|lg`,
grounded in the 3 distinct real alpha values found (`#00000040`,
`#00000080`, `#00000088` — see §2a table for exact file:line citations).

## §2c. Partial adoption — files that import base/ but still bypass it nearby

Cross-referencing the 28 files that import at least one `base/` component
against the same raw-primitive greps used for §3/§4 (`font-size:` and
`TouchArea {}`, since a raw font-size or hand-rolled TouchArea sitting next
to an already-imported `Text`/`Button`/`HoverSurface` is the clearest
signature of "this file adopted the base layer for part of its needs and
bypassed it for the rest," the same pattern originally flagged for
`onboarding_guide_view.slint` alone) finds **23 files** showing this
pattern (union of the two signals; every file with the TouchArea signal is
already in the font-size-signal set) — a materially larger list than the
single file called out in the first pass of this doc. This is the
"quick win" bucket per §0: the base component already exists and is
already imported in these files, so the fix is a mechanical swap, not new
component work.

`app.slint`, `chat_area.slint`, `chat-view/components/execution/
api_call_view.slint`, `chat-view/components/message/agent_bubble.slint`,
`chat-view/components/message/queued_message_bar.slint`, `chat-view/
components/message/tool_event_row.slint`, `chat-view/components/message/
tool_group_view.slint`, `chat-view/components/onboarding_components.slint`,
`chat-view/components/onboarding_guide_view.slint`, `components/
chat_input_layout.slint`, `components/local_terminal_card.slint`,
`components/message_card.slint`, `components/permission_card.slint`,
`components/sidebar.slint`, `components/sidebar_thread_row.slint`,
`components/terminal_card.slint`, `pages/settings/components/
agent_card.slint`, `pages/settings/settings_page.slint`, `pages/settings/
views/agents_view.slint`, `pages/settings/views/mcp_servers_view.slint`,
`pages/settings/views/skills_view.slint`, `pages/skills/skill_view.slint`,
`settings_pane_preview.slint`.

Of these, **10** also hand-roll at least one raw `TouchArea {}` despite
already importing `Button`/`IconButton`/`HoverSurface` in the same file
(the exact file:line list is the "toucharea" grouping already produced for
§3/§4's TouchArea count): `app.slint`, `chat_area.slint`, `chat-view/
components/message/agent_bubble.slint`, `chat-view/components/message/
tool_group_view.slint`, `chat-view/components/onboarding_guide_view.slint`,
`components/chat_input_layout.slint`, `components/permission_card.slint`,
`components/sidebar.slint`, `components/terminal_card.slint`,
`settings_pane_preview.slint` — each of these is a case where the file
already knows how to reach for `HoverSurface`/`Button` for *some* of its
clickable elements but still hand-rolls the `Rectangle` + `TouchArea` +
accessible-role boilerplate for others nearby, i.e. a real missed
extension/inheritance opportunity, not just a missing-component gap.

## 3. Component inventory

### Existing base/ components needing variant props added

**Text (was BaseText)** (base_text.slint, WIP/untracked) — currently zero variants; every consumer (onboarding_guide_view.slint uses it 26x) still overrides font-size/font-weight/color manually per instance, so adopting it hasn't actually removed any raw-literal duplication yet. Proposed (see §2b for the full naming rationale and the shared `size`/`weight` vocabulary this draws from):
```
in property <string> size: "md";   // xs|sm|md|lg|xl -> a real Typography scale
in property <string> weight: "regular"; // regular|medium|semibold|bold
in property <bool> mono: false;
```
No separate color-override sentinel is proposed — `Text` inherits Slint's native `color` property directly from `Text` (the built-in element), so `Text { color: Theme.md-link; }` already works as the raw escape hatch without needing a redundant custom prop wrapping the same concern (an earlier draft of this doc proposed a `tone-override` sentinel prop; dropped as unnecessary once the native property is the same thing).

Total raw inline `Text {}` blocks elsewhere this could absorb: **111** (see file:line list below).

**Select vs SearchableDropdown** — two parallel "click box, popup list, pick one" implementations (select.slint 206 lines, searchable_dropdown.slint 510 lines) with different visual chrome and no shared base. Proposed: extract a shared `DropdownPopup` shell (backdrop/positioning/keyboard-nav skeleton) both build on, or explicitly deprecate one in favor of the other's feature set (SearchableDropdown already has keyboard nav + live filter + optimistic selection that Select lacks).

**Card** — no base/ primitive exists today for the "bordered container box with a default padding, wrapping other content" shape, despite it being redeclared independently 5 times:

| file:line | background | radius | border | drop-shadow | content padding | root content layout |
|---|---|---|---|---|---|---|
| `panel-rust/ui/components/message_card.slint:15,29-44` | conditional (`Theme.bg-user-bubble`/`bg-agent-bubble`/`bg-tertiary`/`bg-primary-container`) | `Theme.radius-lg` (+ one corner cut via `Metrics.radius-corner-cut`) | conditional 0/1px `Theme.border-strong` | none | `Metrics.space-2/4/5` (conditional) | VerticalLayout |
| `panel-rust/ui/components/permission_card.slint:11,40-48` | `Theme.bg-approval` | `Theme.radius-md` | 1px `Theme.border-approval` | yes (`drop-shadow-blur: 8px`, raw hex `#000000.with-alpha(0.28)`) | `Metrics.pad-md/pad-sm` | VerticalLayout |
| `panel-rust/ui/components/terminal_card.slint:29,34-41` | `Theme.bg-terminal` | `Theme.radius-md` | 1px `Theme.border-terminal` | none | `Metrics.space-4` | VerticalLayout |
| `panel-rust/ui/components/local_terminal_card.slint:20,28-32` | `Theme.bg-terminal` | `Theme.radius-md` | 1px `Theme.border-terminal` | none | `Metrics.space-4` | VerticalLayout |
| `panel-rust/ui/pages/settings/components/agent_card.slint:37,98-121` | conditional `Theme.bg-primary-container`/`bg-tertiary` + `opacity` dim | `Theme.radius-md` | none | none | `Metrics.space-6` | VerticalLayout |

Total occurrence count: **5** direct card components (plus their 2 overlay siblings, `TerminalOverlay`/`LocalTerminalOverlay`, which repeat the same shape a 6th/7th time), and **28** total `border-radius: Theme.radius-md/lg` hits outside base/+tokens/ (see `rg -c` above) — so the container-radius/background/border triad this Card would centralize recurs well beyond just the five `*_card.slint` files (also `sidebar.slint:800,835`, `chat_input_layout.slint:611,748,1168,1213`, `queued_message_bar.slint:31`, `tool_event_row.slint:56`, `tool_group_view.slint:58`, `onboarding_components.slint:39`, `onboarding_guide_view.slint` 8x, `chat_area.slint:1353,1639`, `settings_page.slint:167`).

Proposed variant properties (grounded in the 5 rows above — every axis listed is one that already varies between real call sites, none invented; property names below use the short tailwind-informed vocabulary from §2b):
```
in property <string> bg: "secondary";  // primary|secondary|danger|success — background role; every card already conditionally picks a Theme.bg-* role
in property <string> radius: "md";     // sm|md|lg -> Theme.radius-sm/md/lg; md default, message_card wants lg + a corner-cut override
in property <bool> bordered: false;    // 3 of 5 cards add a 1px Theme.border-* ring, 2 don't
in property <string> border: "default"; // default|strong|danger|success -- border tone family, paired with `bordered`
in property <color> border-col: Theme.border-default; // raw escape hatch, kept alongside the semantic `border` enum for one-off overrides
in property <string> shadow: "none";   // none|sm -- 4 of 5 cards have no drop-shadow; permission_card's 8px blur is the one outlier ("sm")
in property <length> pad: Metrics.pad-md; // space-2/4/5/6 all appear as call-site-specific paddings today
```
Content layout direction is deliberately **not** proposed as a variant — see the "Card layout direction" finding below. Content *alignment* was also checked and is likewise not proposed — see "Card content alignment" immediately after.

**Card layout direction.** Grepped each of the 5 card files' root child layout (first layout element directly inside the `Rectangle`): all five — `message_card.slint:46`, `permission_card.slint:75`, `terminal_card.slint:40` (and its `TerminalOverlay:110`), `local_terminal_card.slint:39` (and its `LocalTerminalOverlay:117`), `agent_card.slint:121` — use `VerticalLayout` as the root content wrapper. `HorizontalLayout` only ever appears *nested inside* that outer VerticalLayout, for sub-rows (a header row, a label+control row), never as the top-level content direction. So there is no real call site today where the same card shape recurs in both orientations — the "should direction be a Card variant" signal is negative. Recommendation: Card should own background/radius/border/elevation/padding only and expose content via `@children` inside a fixed internal `VerticalLayout` (same convention `base/collapse_expand.slint`'s `body := VerticalLayout { @children }` already establishes), not a `direction` enum — orientation stays the caller's concern via ordinary nested layouts, consistent with every real usage found.

**Card content alignment.** Checked separately from direction: grepped each of the 5 card files' root `VerticalLayout` for its own `alignment` property and for any per-child centering/end-alignment hack (`x: (parent.width - self.width)/2`, a nested `HorizontalLayout { alignment: center/end; }` used purely to fake a Card-level alignment). Findings —
`message_card.slint:46-56` (no `alignment:` on the root layout — default), `permission_card.slint:75-83` (`alignment: start;` **is** set, but this is the *main-axis* (vertical) packing property — it stops the layout from vertically stretch-filling a taller card, it is not a horizontal content-alignment choice), `terminal_card.slint:40-42` (no `alignment:`), `local_terminal_card.slint:39-41` (no `alignment:`), `agent_card.slint:121-132` (no `alignment:`, explicit `width: parent.width; height: parent.height;` fill instead, with a comment explaining why). No `x: (parent.width - self.width)/2` centering hack and no per-child `HorizontalLayout { alignment: center/end; }` wrapper exists in any of the 5 root layouts purely to fake a Card-level alignment — the `horizontal-stretch`/`vertical-alignment` hits found nearby (`permission_card.slint:91-92,182-183` etc.) are ordinary internal row-layout technique (label vs. spacer inside a *nested* `HorizontalLayout`), not a Card-level alignment override. **Conclusion: no `align` variant needed** — all 5 cards let their `VerticalLayout` children stretch full-width by default; the one `alignment: start` outlier (`permission_card`) is an unrelated vertical-packing choice, not a horizontal-alignment pattern, and doesn't recur elsewhere.

**Typography.slint** — add the actual type-scale referenced above (see §2), consumed by Text (was BaseText) and, over time, by base/ components that still hardcode a font-size default.

### Proposed new base/ components (3+-times-copy-pasted leaf patterns)

| proposed component | pattern it replaces | occurrence count | example file:line |
|---|---|---|---|
| `Divider` (promote `SettingsDivider` out of settings-only scope, or add a general alias) | `Rectangle { height: 1px; background: Theme.border-default; }` hand-rolled outside base/ despite `SettingsDivider` already existing in settings_section.slint | 8+ (rg count: 11 `height: 1px` hits total, most paired with a border-color) | `panel-rust/ui/components/terminal_card.slint:205`, `panel-rust/ui/components/local_terminal_card.slint:151`, `panel-rust/ui/settings_pane_preview.slint:53`, `panel-rust/ui/components/chat_input_layout.slint:1108`, `panel-rust/ui/chat_area.slint:762,1399,1556`, `panel-rust/ui/chat-view/components/message/tool_group_view.slint:154`, `panel-rust/ui/pages/skills/skill_view.slint:164` |
| Reuse existing `StatusDot` instead of re-deriving `border-radius: self.width/2` circular dots inline | small colored circle badges | 3 | `panel-rust/ui/components/terminal_card.slint:151`, `panel-rust/ui/chat_area.slint:1289`, `panel-rust/ui/chat-view/components/message/tool_event_row.slint:115` |
| `ModalScrim`/`Backdrop` (fixed-color, semi-transparent full-bleed touch-catcher) | hand-rolled `Rectangle { background: #000000xx; } TouchArea { clicked => close(); }` pairs, including inside base/ itself (dynamic_modal.slint) | 4+ hex-scrim sites, `TouchArea` count 25 total (not all are scrims but several are) | `panel-rust/ui/base/dynamic_modal.slint:22`, `panel-rust/ui/chat_area.slint:1335,1355`, `panel-rust/ui/app.slint:974` |
| Type-scale-aware `Badge`/status-chip variant reuse (badge already exists — push adoption, not a new component) | ad hoc raw-hex status/warning/error banners | see §4 chat_area.slint / mcp_servers_view.slint rows | `panel-rust/ui/chat_area.slint:771-782` (warning band `#422006`/`#eab308`), `:855-865` (error band `#b91c1c`/`#ef4444`), `panel-rust/ui/pages/settings/views/mcp_servers_view.slint:512-525` (`#2a1215`/`#ef4444`) |

Total occurrence counts backing the above (rest-of-`ui/`, excluding base/ and tokens/, `rg -c` sums):

- `font-size:` — **179** across 27 files
- `font-weight:` — **71** across 22 files
- raw hex color literals (`#rrggbb[aa]`) — **24** across 8 files
- hand-rolled `TouchArea {}` (not via Button/IconButton/HoverSurface) — **25** across 12 files
- plain `Text {}` elements (not Text (was BaseText)) — **111** across 20 files
- raw `Npx` literals (spacing/sizing/radius, superset incl. font-size) — **1067** across many files
- raw `border-radius: Npx` (not `Theme.radius-*`) — **13**, e.g. `panel-rust/ui/components/sidebar_thread_row.slint:185`, `panel-rust/ui/components/sidebar.slint:340`, `panel-rust/ui/chat-view/components/execution/api_call_view.slint:34,58`, `panel-rust/ui/chat-view/components/message/agent_bubble.slint:50`, `panel-rust/ui/chat-view/components/message/tool_group_view.slint:237`, `panel-rust/ui/chat-view/components/onboarding_components.slint:78` (12px pill radius, new file, doesn't use `Theme.radius-lg`), `panel-rust/ui/chat-view/components/onboarding_guide_view.slint:56,291`, `panel-rust/ui/settings_pane_preview.slint:128`
- `@image-url` — **71**, but **43** already route through `Icon {}` (good adoption); `agent_logo.slint` uses raw `Image {}` 4x directly for brand logos, which is a legitimate exception (logos must keep original color, not `colorize`-able)

## 4. Files that can shrink (worst offenders, ranked)

Per §0/§2c: files marked **quick win** below already import the relevant
base/ component and are in the 23-file partial-adoption list — the fix is a
mechanical swap, no new base/ component required. Files without that tag
need a base/ gap (§6) filled first before they can fully adopt.

| rank | file | signal | rough estimate | adoption status |
|---|---|---|---|---|
| 1 | `panel-rust/ui/chat_area.slint` (1678 lines) | 14 font-size, 9 hex colors, 6 hand-rolled TouchArea, 14 plain Text, 141 raw px | Largest single file in ui/; the status-band (warning/error) blocks (~771-782, 855-865) alone are ~25 lines each that could collapse to one `Badge`(`variant: warning/danger`) call once that variant lands; 3 divider Rectangles, 2 modal-scrim blocks — **~150-250 lines** collapsible | **quick win** — already imports `Icon`/`IconButton`/`Spinner`/`MentionPickRow`/`ThinScrollbar`, still hand-rolls TouchArea + raw Text nearby (§2c) |
| 2 | `panel-rust/ui/chat-view/components/onboarding_guide_view.slint` (589 lines) | 25 font-size, 9 font-weight, 145 raw px (highest in the whole audit), 2 raw hex `#22C55E` duplicating `Theme.status-success` | Already imports `Text (was BaseText)`/`OnboardingPreviewCard` etc. but still hand-declares typography per Text — once Text (was BaseText) gets a size/weight scale this file's own font-size/font-weight lines could mostly disappear — **~100-150 lines** | **quick win** — the base component (`Text`) is already imported and used 26x in this exact file; this is the original partial-adoption example that motivated §2c's wider audit |
| 3 | `panel-rust/ui/pages/settings/views/mcp_servers_view.slint` (757 lines) | 20 font-size, 10 font-weight, 3 hex colors, 62 raw px | Already imports SettingsRow/SettingsSection (22 uses) — good partial adoption — but its own error-banner block (~512-525) duplicates the chat_area.slint error-band pattern instead of sharing a component — **~60-100 lines** | **quick win** for the font-size/weight lines (in §2c's 23-file list); the error-banner block needs the new Badge `variant: danger` (§2b) first |
| 4 | `panel-rust/ui/pages/settings/views/agents_view.slint` (653 lines) | 14 font-size, 8 font-weight, 53 raw px | Partial SettingsRow adoption (18 uses) but still has independent raw-Text blocks — **~50-80 lines** | **quick win** (§2c list) |
| 5 | `panel-rust/ui/settings_pane_preview.slint` (183 lines) | 12 font-size, 6 font-weight, 12 plain Text, 41 raw px, 1 divider | Does **not** import SettingsRow/SettingsSection at all despite being a settings-shaped view — hand-rolls its own row/divider/label shapes from scratch — **~40-60 lines**, high value-per-line since the file is small | **quick win** for `Button` (already imported, §2c also flags a raw TouchArea here) — the row/divider shapes still need §6's `Divider` gap filled |
| 6 | `panel-rust/ui/components/sidebar.slint` (944 lines) | 10 font-size, 10 plain Text, 93 raw px, 2 hex (8-color accent swatch array + 1px divider), 12 @image-url | Accent-color swatch array (`#f43f5e, #f97316, ...` 8 literals) is a legitimate small palette, not necessarily a token candidate, but the 10 raw Text blocks and divider are — **~40-70 lines** | **quick win** — already imports `Icon`/`IconButton`/`HoverSurface`/`NavItem`/`Toggle`/`FilterSearchBar`/`NamePromptDialog`/`TextUtil`, still has raw font-size + TouchArea nearby (§2c) |
| 7 | `panel-rust/ui/components/chat_input_layout.slint` (1380 lines) | 10 font-size, 9 plain Text, 5 hand-rolled TouchArea, 77 raw px, 2 hex (drop-shadow colors), 1 divider (already comments that it intentionally copies the divider pattern rather than sharing it) | **~40-70 lines** | **quick win** — heaviest base/ importer in the codebase (9 import lines per `base-layer-folder-structure.md`), still bypasses it for TouchArea/Text nearby (§2c) |
| 8 | `panel-rust/ui/components/sidebar_thread_row.slint` (529 lines) | 9 font-size, 8 plain Text, 81 raw px, 1 hex (`accent-color: #6366f1` default), 1 raw border-radius | **~30-50 lines** | **quick win** (§2c list — imports `Icon`/`IconButton`/`HoverSurface`/`Spinner`) |

## 5. Theme inheritance model

How base/ components currently pull from `tokens/theme.slint`, and which
don't:

**Correctly theme-driven today** (no hardcoded colors/sizes beyond a raw
font-size default): FilterSearchBar, HoverSurface, Icon, IconButton,
NavItem, StatusDot, Spinner, ThinScrollbar, Toggle, Badge, TerminalHeader,
TerminalLogBlock, MentionPickRow, SelectableChip, TextPillButton, Select,
SearchableDropdown, SettingsRow, SettingsSection family (all colors route
through `Theme.*`; radius routes through `Theme.radius-sm/md`, which itself
is preset-aware via `Theme.radius-preset` — so a theme swap or radius-preset
swap propagates through every one of these automatically).

**Hardcodes a value instead of pulling from Theme/tokens:**
- `base/dynamic_modal.slint:22` — `background: #00000040` (scrim) is a raw hex, not a Theme token. Also `border-radius: 0px` hardcoded on the card body instead of `Theme.radius-*`.
- `base/settings_section.slint:94` — `SettingsField.error` text color is raw `#ef4444` (its own comment admits there's no `Theme.text-danger` token yet — this is the actual root cause, not a mistake local to this file).
- `base/base_text.slint` — technically theme-driven (`Theme.text-primary`, `Theme.font-sans`, `Theme.font-scale`) but has no variant surface, so every consumer re-hardcodes overrides instead of the component absorbing them.
- Several base/ components (ExpandablePanel, LabeledSelect, LinkText, MentionPickRow, NamePromptDialog, NavItem, SelectableChip, Select, SettingsRow, TerminalHeader, TerminalLogBlock, TextField, TextPillButton) pull colors correctly from Theme but hardcode their **font-size default** as a raw px literal — theme-correct for color, not yet theme-correct for type scale, because no type-scale token exists to pull from (see §2/§3).

Net: the *color* half of the theme-inheritance model is solid — one theme
swap (`Theme.theme`) or radius-preset swap does propagate everywhere via
`bg-*`/`text-*`/`border-*`/`radius-*`. The *type-scale* half doesn't exist
yet, so there's nothing for base/ components to inherit even when they want
to.

## 6. Gap analysis

**Missing:**
- A `Typography` type scale (`text-xs/sm/md/lg/xl` or similar) parallel to `Metrics`'s spacing/icon scale — the single biggest structural gap. Nothing else in this plan (Text (was BaseText) variants, Button/Badge/Toggle size variants, MarkdownView's heading ladder) can be cleanly formalized until this exists.
- A `Theme.text-danger` (and arguably `text-success`/`text-warning`) token — `SettingsField.error`, and every ad hoc warning/error banner in chat_area.slint/mcp_servers_view.slint, currently reinvent the same 2-3 hex values independently.
- A general-purpose `Divider` in base/ that isn't scoped to "settings" naming (`SettingsDivider` exists but the pattern is reused well beyond settings pages).
- A `ModalScrim`/`Backdrop` primitive — `DynamicModal` (base/) and chat_area.slint/app.slint each hand-roll their own semi-transparent full-bleed + click-to-close `Rectangle`+`TouchArea` pair with slightly different alpha values (`#00000040`, `#00000080`, `#00000088`).
- A `Card` primitive — see §3. 5 independently-declared card components (7 counting the 2 overlay siblings) plus 28 total `Theme.radius-md/lg` container hits elsewhere with no shared base.
- An aspect-ratio-aware icon/hit-target size token. `base/icon.slint` is already safe (one `size` prop drives both width and height, so glyphs can't be stretched non-uniformly) but takes a raw `length` rather than an `sm`/`md`/`lg` enum tied to `Metrics.icon-sm/md/lg` (12/16/20px), which already exists and goes unused by Icon's own default. `base/icon_button.slint` is the real gap: `btn-width`/`btn-height` are two fully independent `length` props with no aspect-ratio coupling at all, so a caller can (accidentally or not) produce a non-square hit target — every other sized base/ component in this audit (Button, Toggle, StatusDot, Badge's icon-adjacent sizing) has the same "two independent dimension props, no enum" shape. Recommendation: give Icon and IconButton a shared `size: "sm" | "md" | "lg"` variant resolving through `Metrics.icon-*`, with IconButton deriving a fixed square hit target from the same token (plus its own hit-padding) instead of accepting independent width/height.
- **No destructive/confirmatory action is currently color-coded** (found while grounding §2b's Button `variant: danger|success` values): every `Strings.kill`/`delete`/`remove`/`reject` Button call site (`local_terminal_card.slint:50-57`, `agents_view.slint:265-296`, `mcp_servers_view.slint:374-385`) and every `permission_card.slint` allow/reject row (`155-196`) reuses the same primary/secondary `bg` treatment as any ordinary action — none reference `Theme.status-error`/`Theme.status-success`. This is a real, previously-uncaught gap (not surfaced by the original font-size/font-weight/hex sweep, since it's about a *missing* color distinction, not a duplicated one) — a `variant`/`bg` semantic enum on Button/Card/Badge (§2b) is what would let this be fixed in one place going forward, but the underlying `Theme.text-danger`-style token gap (above) needs to extend to a background-safe danger/success pair too before that enum has anything real to point at.

**Duplicated:**
- Card shape (background/radius/border/padding, occasionally drop-shadow) redeclared 5x independently (7x counting overlay siblings) with no shared base/ primitive — see §3.
- Select vs SearchableDropdown (two independent dropdown-popup implementations).
- Circular status-dot geometry (`border-radius: self.width/2`) reimplemented 3x instead of reusing `StatusDot`.
- Divider rectangle reimplemented 8+ times instead of reusing/promoting `SettingsDivider`.
- Warning/error status-banner blocks in chat_area.slint and mcp_servers_view.slint (same 2-color raw-hex shape, independently authored).
- Text (was BaseText) adoption in onboarding_guide_view.slint hasn't actually reduced duplication yet (26 uses, but still 25 raw font-size + 9 raw font-weight overrides alongside it) because Text (was BaseText) has no variant props to absorb them into.

**Inconsistent:**
- Two independently-named "which color role" vocabularies: `fg`/`bg` (Button/Badge) vs `idle-bg`/`hover-bg`/`selected-bg` (HoverSurface family) vs `active-color`/`inactive-color` (NavItem) — three components all expressing "primary vs secondary/muted" with no shared enum.
- Four different accessible-label prop names for the same concept (`button-accessible-label`/`field-accessible-label`/`bar-accessible-label`/`select-accessible-label`).
- `DESIGN_SYSTEM.md`'s aspirational file list (`base/input.slint`, `base/card.slint`, `base/dialog.slint`, `base/tabs.slint`, `base/tooltip.slint`, `base/scroll-area.slint`) doesn't match the actual base/ directory (`text_field.slint`, no card/dialog/tabs/tooltip primitives exist, `thin_scrollbar.slint` instead of `scroll-area.slint`) — the doc is a planning sketch, not current-state documentation; worth a follow-up pass to reconcile once this plan's rollout starts.

**Priority note — partial adoption beats greenfield gaps.** Per §0/§2c, the
23-file partial-adoption list (files that already import a base/ component
but still bypass it nearby for `Text`/`TouchArea`) should be fixed *before*
most of the "Missing"/"Duplicated" items above, wherever the two overlap —
e.g. `chat_area.slint`'s hand-rolled TouchAreas (item above, "Duplicated")
sit in a file that already imports `IconButton`/`HoverSurface`-adjacent
components, so that specific fix needs no new base/ component, unlike the
`Card`/`Divider`/`ModalScrim` gaps which are blocked on new components
existing first. §4's "quick win" tags mark exactly this distinction per
file.

**Suggested rollout order (non-breaking, Slint supports incremental opt-in):**

1. Add `Typography` type scale to `tokens/typography.slint` (additive — no existing file references it yet, so this cannot break anything).
2. Add `Theme.text-danger` (+ success/warning if useful) to `tokens/theme.slint` (additive).
3. Give `Text` (renamed from `BaseText`, see the naming note at the top of this doc) `size`/`weight`/`mono` variant props with defaults matching its current fixed look (13px/400/sans) — every existing call site keeps working unchanged since defaults are unchanged; new call sites opt in incrementally.
4. Migrate the worst-offender files (§4, chat_area.slint and onboarding_guide_view.slint first) file-by-file, swapping raw `Text { font-size: ... }` blocks for `Text { size: ... }` calls against the base component — Slint lets a consuming file's own component still layer extra properties on top of a base import (e.g. `Text { size: "sm"; color: Theme.md-link; }` — the component's own declared properties remain settable at the call site even though the shared base sets the defaults), so this doesn't require every call site to fit the shared vocabulary exactly, only to start from it.
5. Add `Divider` (or a top-level re-export of `SettingsDivider` under a neutral name), `ModalScrim`, and `Card` to base/; swap in the 8+ divider sites, 4+ scrim sites, and the 5 `*_card.slint` files (each keeps its own conditional/domain logic — item text, buttons, badges — as children passed into `Card`, per the layout-direction finding above).
6. Only after the above land: decide whether to unify Select/SearchableDropdown or formally deprecate one — this is the riskiest/most structural change (both have real callers with different feature needs) and should not block the additive token/Text (was BaseText) work above.
