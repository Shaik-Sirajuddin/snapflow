//! Phase 0 (chat-panel-ui-theme-parity.md): design tokens extracted
//! verbatim from `ui_html/index.html`'s `:root`/`.dark` CSS custom
//! properties and its `radiusPresets` object -- plain data only, no
//! Slint dependency, so this module is independently unit-testable and
//! reviewable side-by-side against the HTML without needing a UI
//! toolkit at all.
//!
//! Naming convention: field names mirror the HTML's own two-layer
//! scheme rather than inventing new ones --
//! `--md-sys-color-*` (Material Design System role tokens, the
//! `Palette` fields below) are aliased by Tailwind's `colors.sys.*`
//! config onto semantic names (`outer`, `sidebar`, `card`, `border`,
//! `textPrimary`, ...) which is what `SysRoles` re-exposes. Every
//! `SysRoles` field is a same-value alias of exactly one `Palette`
//! field -- see `SysRoles::from_palette`'s doc comment for the mapping
//! table, sourced from `ui_html/index.html`'s `tailwind.config` block.
//!
//! Status is deliberately monochrome in the source design (verified by
//! grepping index.html for every red/green/amber/emerald Tailwind class
//! -- zero hits, and confirmed by the HTML's own code comment "no fancy
//! green/red circles" at the tool-marker render site). This module
//! therefore does NOT define a status-color map; tool-call status
//! renders as plain uppercase mono-font text (Phase 3, in
//! `message_card.slint`), not a color code.

/// A plain RGBA color, 0-255 per channel. Slint-free by design (see
/// module doc) -- Phase 1 converts these into `slint::Color` at the
/// UI boundary, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Color { r, g, b, a: 255 }
    }

    const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Color { r, g, b, a }
    }

    /// Alpha expressed as the HTML's own 0.0-1.0 float (e.g.
    /// `rgba(0, 0, 0, 0.06)`), converted to a 0-255 byte the same way
    /// browsers do (round, not truncate).
    const fn rgba_f(r: u8, g: u8, b: u8, alpha_0_to_1_times_1000: u32) -> Self {
        // const fn can't do float math portably across MSRV here, so the
        // caller passes alpha already scaled by 1000 (e.g. 0.06 -> 60)
        // and we do the divide with integer math, rounding.
        let a = ((alpha_0_to_1_times_1000 * 255 + 500) / 1000) as u8;
        Color { r, g, b, a }
    }
}

/// One theme's full set of Material Design System role colors, pulled
/// verbatim from index.html's `:root` (light) / `.dark` blocks. Field
/// names match the CSS custom property suffix after `--md-sys-color-`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub surface: Color,
    pub surface_container: Color,
    pub surface_container_low: Color,
    pub surface_variant: Color,
    pub outline: Color,
    pub outline_variant: Color,
    pub on_surface: Color,
    pub on_surface_variant: Color,
    pub on_surface_muted: Color,
    pub primary: Color,
    pub primary_container: Color,
    pub on_primary: Color,
    pub on_primary_container: Color,
    /// Flattened, opaque approximation of `--md-sys-color-chat-bg-overlay`
    /// -- the HTML uses this as a `backdrop-blur-md` glass overlay; this
    /// renderer has no blur (see the theme-parity plan's Phase 0/1 note,
    /// `renderer-software` has no GPU compositor), so it's used here as a
    /// flat fill instead. Accepted deviation, not a bug.
    pub chat_bg_overlay: Color,
}

/// "Base White" (light mode), verbatim from index.html's `:root` block.
pub const LIGHT: Palette = Palette {
    surface: Color::rgb(0xff, 0xff, 0xff),
    surface_container: Color::rgb(0xf4, 0xf4, 0xf7),
    surface_container_low: Color::rgb(0xff, 0xff, 0xff),
    surface_variant: Color::rgb(0xea, 0xea, 0xea),
    outline: Color::rgba_f(0, 0, 0, 60),
    outline_variant: Color::rgba_f(0, 0, 0, 120),
    on_surface: Color::rgb(0x12, 0x12, 0x12),
    on_surface_variant: Color::rgb(0x55, 0x55, 0x58),
    on_surface_muted: Color::rgb(0x8a, 0x8a, 0x8f),
    primary: Color::rgb(0x00, 0x00, 0x00),
    primary_container: Color::rgb(0xe5, 0xe5, 0xea),
    on_primary: Color::rgb(0xff, 0xff, 0xff),
    on_primary_container: Color::rgb(0x00, 0x00, 0x00),
    // rgba(250, 250, 252, 0.55): round(0.55 * 255) = 140.25 -> 140.
    chat_bg_overlay: Color::rgba(250, 250, 252, 140),
};

/// "Base Black" (dark mode), verbatim from index.html's `.dark` block.
pub const DARK: Palette = Palette {
    surface: Color::rgb(0x00, 0x00, 0x00),
    surface_container: Color::rgb(0x09, 0x09, 0x0b),
    surface_container_low: Color::rgb(0x09, 0x09, 0x0b),
    surface_variant: Color::rgb(0x15, 0x15, 0x18),
    outline: Color::rgba_f(255, 255, 255, 60),
    outline_variant: Color::rgba_f(255, 255, 255, 120),
    on_surface: Color::rgb(0xf2, 0xf2, 0xf7),
    on_surface_variant: Color::rgb(0xa1, 0xa1, 0xa6),
    on_surface_muted: Color::rgb(0x63, 0x63, 0x66),
    primary: Color::rgb(0xff, 0xff, 0xff),
    primary_container: Color::rgb(0x1c, 0x1c, 0x1e),
    on_primary: Color::rgb(0x00, 0x00, 0x00),
    on_primary_container: Color::rgb(0xff, 0xff, 0xff),
    chat_bg_overlay: Color::rgb(0x13, 0x13, 0x13), // dark mode's is opaque, not rgba
};

/// Resolve a theme by name (`"dark"` / `"light"`), defaulting to dark --
/// matches index.html's own `init()` (`changeTheme('dark') // start with
/// black mode`) and the existing `ChatPanel::theme` property's current
/// default in `lib.rs`.
pub fn palette_for_theme(theme: &str) -> Palette {
    if theme.eq_ignore_ascii_case("light") {
        LIGHT
    } else {
        DARK
    }
}

/// Tailwind's `colors.sys.*` semantic role names, each a same-value
/// alias of one `Palette` field -- table sourced verbatim from
/// `ui_html/index.html`'s `tailwind.config` block:
///
/// | sys.* role         | md-sys-color token       |
/// |---------------------|--------------------------|
/// | outer               | surface                  |
/// | sidebar              | surface-container         |
/// | chat                 | chat-bg-overlay           |
/// | card                 | surface-container-low     |
/// | border               | outline                   |
/// | border_strong        | outline-variant           |
/// | text_primary         | on-surface                |
/// | text_secondary       | on-surface-variant        |
/// | text_muted           | on-surface-muted          |
/// | primary              | primary                   |
/// | primary_container    | primary-container         |
/// | on_primary           | on-primary                |
/// | on_primary_container | on-primary-container      |
/// | user_bubble          | surface-variant           |
/// | user_bubble_border   | outline                   |
/// | user_bubble_text     | on-surface                |
/// | agent_bubble         | surface-container         |
/// | agent_bubble_border  | outline                   |
/// | agent_bubble_text    | on-surface                |
/// | terminal_bg          | surface-container         |
/// | terminal_border      | outline                   |
/// | terminal_text        | on-surface-variant        |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SysRoles {
    pub outer: Color,
    pub sidebar: Color,
    pub chat: Color,
    pub card: Color,
    pub border: Color,
    pub border_strong: Color,
    pub text_primary: Color,
    pub text_secondary: Color,
    pub text_muted: Color,
    pub primary: Color,
    pub primary_container: Color,
    pub on_primary: Color,
    pub on_primary_container: Color,
    pub user_bubble: Color,
    pub user_bubble_border: Color,
    pub user_bubble_text: Color,
    pub agent_bubble: Color,
    pub agent_bubble_border: Color,
    pub agent_bubble_text: Color,
    pub terminal_bg: Color,
    pub terminal_border: Color,
    pub terminal_text: Color,
}

impl SysRoles {
    pub fn from_palette(p: &Palette) -> Self {
        SysRoles {
            outer: p.surface,
            sidebar: p.surface_container,
            chat: p.chat_bg_overlay,
            card: p.surface_container_low,
            border: p.outline,
            border_strong: p.outline_variant,
            text_primary: p.on_surface,
            text_secondary: p.on_surface_variant,
            text_muted: p.on_surface_muted,
            primary: p.primary,
            primary_container: p.primary_container,
            on_primary: p.on_primary,
            on_primary_container: p.on_primary_container,
            user_bubble: p.surface_variant,
            user_bubble_border: p.outline,
            user_bubble_text: p.on_surface,
            agent_bubble: p.surface_container,
            agent_bubble_border: p.outline,
            agent_bubble_text: p.on_surface,
            terminal_bg: p.surface_container,
            terminal_border: p.outline,
            terminal_text: p.on_surface_variant,
        }
    }
}

/// Border-radius "finish" presets, verbatim from index.html's
/// `radiusPresets` object (`sm`/`md`/`lg`, all in px).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadiusScale {
    pub sm: u32,
    pub md: u32,
    pub lg: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadiusPreset {
    Sharp,
    Classic,
    Organic,
}

impl RadiusPreset {
    /// Parse the Slint-facing `radius-preset` property value
    /// (`"sharp"|"classic"|"organic"`), defaulting to `Classic` --
    /// matches index.html's `let activeRadiusPreset = 'classic'`.
    pub fn from_name(name: &str) -> Self {
        match name {
            "sharp" => RadiusPreset::Sharp,
            "organic" => RadiusPreset::Organic,
            _ => RadiusPreset::Classic,
        }
    }

    pub fn scale(self) -> RadiusScale {
        match self {
            RadiusPreset::Sharp => RadiusScale {
                sm: 0,
                md: 0,
                lg: 0,
            },
            RadiusPreset::Classic => RadiusScale {
                sm: 6,
                md: 10,
                lg: 16,
            },
            RadiusPreset::Organic => RadiusScale {
                sm: 12,
                md: 18,
                lg: 28,
            },
        }
    }
}

/// Font family names for the two embedded fonts (Phase 1 registers the
/// actual `.ttf` bytes via `slint::register_font_from_memory` -- this
/// module only names which family goes where, so Phase 1 has one place
/// to read from rather than re-deciding it inline).
pub struct FontTokens;

impl FontTokens {
    /// Body/UI text -- index.html: `fontFamily.sans: ['"Plus Jakarta
    /// Sans"', 'sans-serif']`.
    pub const SANS_FAMILY: &'static str = "Plus Jakarta Sans";
    /// Badges, timestamps, tool-call logs -- index.html:
    /// `fontFamily.mono: ['"JetBrains Mono"', 'monospace']`.
    pub const MONO_FAMILY: &'static str = "JetBrains Mono";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_and_dark_palettes_are_distinct() {
        assert_ne!(LIGHT.surface, DARK.surface);
        assert_eq!(LIGHT.surface, Color::rgb(0xff, 0xff, 0xff));
        assert_eq!(DARK.surface, Color::rgb(0x00, 0x00, 0x00));
    }

    #[test]
    fn palette_for_theme_defaults_to_dark() {
        // Matches index.html's init(): changeTheme('dark') is the
        // startup default, and the existing ChatPanel::theme property's
        // current default in lib.rs -- an unrecognized/empty theme name
        // must not silently fall back to light.
        assert_eq!(palette_for_theme("dark"), DARK);
        assert_eq!(palette_for_theme(""), DARK);
        assert_eq!(palette_for_theme("nonsense"), DARK);
    }

    #[test]
    fn palette_for_theme_light_is_case_insensitive_and_has_its_own_overlay() {
        let l1 = palette_for_theme("light");
        let l2 = palette_for_theme("Light");
        assert_eq!(l1, l2);
        assert_eq!(l1.chat_bg_overlay, Color::rgba(250, 250, 252, 140));
        // Dark mode's overlay is opaque (a = 255), light's is translucent
        // (a ~= 140) -- verifies the two modes weren't accidentally
        // aliased to the same overlay treatment.
        assert_eq!(DARK.chat_bg_overlay.a, 255);
        assert_eq!(l1.chat_bg_overlay.a, 140);
    }

    #[test]
    fn outline_alpha_rounds_the_same_way_a_browser_would() {
        // rgba(0, 0, 0, 0.06) -> alpha byte = round(0.06 * 255) = 15.3 -> 15
        assert_eq!(LIGHT.outline.a, 15);
        // rgba(0, 0, 0, 0.12) -> round(0.12 * 255) = 30.6 -> 31
        assert_eq!(LIGHT.outline_variant.a, 31);
    }

    #[test]
    fn sys_roles_are_same_value_aliases_not_new_colors() {
        let roles = SysRoles::from_palette(&DARK);
        assert_eq!(roles.outer, DARK.surface);
        assert_eq!(roles.sidebar, DARK.surface_container);
        assert_eq!(roles.chat, DARK.chat_bg_overlay);
        assert_eq!(roles.card, DARK.surface_container_low);
        assert_eq!(roles.border, DARK.outline);
        assert_eq!(roles.border_strong, DARK.outline_variant);
        assert_eq!(roles.text_primary, DARK.on_surface);
        assert_eq!(roles.text_secondary, DARK.on_surface_variant);
        assert_eq!(roles.text_muted, DARK.on_surface_muted);
        assert_eq!(roles.user_bubble, DARK.surface_variant);
        assert_eq!(roles.user_bubble_border, DARK.outline);
        assert_eq!(roles.agent_bubble, DARK.surface_container);
        assert_eq!(roles.terminal_bg, DARK.surface_container);
        assert_eq!(roles.terminal_text, DARK.on_surface_variant);
    }

    #[test]
    fn radius_presets_match_index_html_verbatim() {
        assert_eq!(
            RadiusPreset::Sharp.scale(),
            RadiusScale {
                sm: 0,
                md: 0,
                lg: 0
            }
        );
        assert_eq!(
            RadiusPreset::Classic.scale(),
            RadiusScale {
                sm: 6,
                md: 10,
                lg: 16
            }
        );
        assert_eq!(
            RadiusPreset::Organic.scale(),
            RadiusScale {
                sm: 12,
                md: 18,
                lg: 28
            }
        );
    }

    #[test]
    fn radius_preset_from_name_defaults_to_classic() {
        assert_eq!(RadiusPreset::from_name("sharp"), RadiusPreset::Sharp);
        assert_eq!(RadiusPreset::from_name("organic"), RadiusPreset::Organic);
        assert_eq!(RadiusPreset::from_name("classic"), RadiusPreset::Classic);
        assert_eq!(RadiusPreset::from_name("bogus"), RadiusPreset::Classic);
        assert_eq!(RadiusPreset::from_name(""), RadiusPreset::Classic);
    }

    #[test]
    fn font_family_names_match_index_html_verbatim() {
        assert_eq!(FontTokens::SANS_FAMILY, "Plus Jakarta Sans");
        assert_eq!(FontTokens::MONO_FAMILY, "JetBrains Mono");
    }
}
