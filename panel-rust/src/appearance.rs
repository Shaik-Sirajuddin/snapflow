//! Host-owned appearance selectors projected into panel state.
//!
//! The host selects only dark/light, language, bundled font, scale, and
//! density. The panel keeps ownership of its Material palette and component
//! tokens, avoiding arbitrary host CSS/color injection.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorScheme {
    Dark,
    Light,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostAppearance {
    pub generation: u64,
    pub color_scheme: ColorScheme,
    pub language_tag: String,
    pub bundled_font: String,
    pub font_scale: f32,
    pub density: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AppearanceState {
    current: Option<HostAppearance>,
}

impl AppearanceState {
    /// Applies only newer host snapshots. This makes out-of-order Qt callbacks
    /// harmless and avoids resetting conversation state for appearance work.
    pub fn apply(&mut self, next: HostAppearance) -> bool {
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.generation >= next.generation)
        {
            return false;
        }
        self.current = Some(next);
        true
    }

    pub fn current(&self) -> Option<&HostAppearance> {
        self.current.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn appearance(generation: u64) -> HostAppearance {
        HostAppearance {
            generation,
            color_scheme: ColorScheme::Dark,
            language_tag: "en-US".to_owned(),
            bundled_font: "NotoSans".to_owned(),
            font_scale: 1.0,
            density: 1.0,
        }
    }

    #[test]
    fn ignores_stale_or_duplicate_host_generations() {
        let mut state = AppearanceState::default();
        assert!(state.apply(appearance(2)));
        assert!(!state.apply(appearance(2)));
        assert!(!state.apply(appearance(1)));
        assert_eq!(state.current().unwrap().generation, 2);
    }
}
