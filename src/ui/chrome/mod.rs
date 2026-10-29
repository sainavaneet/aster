//! Shared building blocks of the modern visual style: glyph sets, color roles,
//! state vocabulary, age formatting, and allocation-free cell writers.
//!
//! Nothing here knows about `AppState` or the client shell, so the CLI can use
//! the same vocabulary. Roles and glyph tables are resolved once when the
//! configuration or theme changes (`Chrome::derive`); the draw helpers only
//! write borrowed text into a buffer. `ui.style = "classic"` never reads any
//! of it.

// The foundation is built and tested ahead of the surfaces that draw with it
// (sidebar, tab bar, pane headers, flow view, toasts, CLI in later phases).
#![allow(dead_code)]

pub(crate) mod age;
pub(crate) mod draw;
pub(crate) mod glyphs;
pub(crate) mod roles;
pub(crate) mod state;

use crate::app::state::Palette;
use crate::config::{UiGlyphsConfig, UiStyleConfig};

pub(crate) use self::glyphs::GlyphSet;
pub(crate) use self::roles::ChromeRoles;

/// The resolved style of a client: which style, which glyphs, which colors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chrome {
    pub(crate) style: UiStyleConfig,
    pub(crate) glyphs: &'static GlyphSet,
    pub(crate) roles: ChromeRoles,
}

impl Chrome {
    /// Resolves the style for `palette`. Called on config build, config reload
    /// and whenever the palette is replaced, never per frame.
    pub(crate) fn derive(style: UiStyleConfig, glyphs: UiGlyphsConfig, palette: &Palette) -> Self {
        Self {
            style,
            glyphs: GlyphSet::for_config(glyphs),
            roles: ChromeRoles::derive(palette),
        }
    }

    pub(crate) fn is_modern(&self) -> bool {
        self.style.is_modern()
    }
}
