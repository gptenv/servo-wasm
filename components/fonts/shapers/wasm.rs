//! Minimal wasm text shaper.
//!
//! Worker builds do not link Servo's native HarfBuzz C++ backend. Until a
//! pure-Rust complex-script shaper is integrated, use the existing Rust ASCII
//! fast path so basic DOM/CSS text remains functional.

use crate::{Font, ShapedText, ShapingOptions};

#[derive(Debug)]
pub(crate) struct Shaper;

impl Shaper {
    pub(crate) fn new(_font: &Font) -> Self {
        Self
    }

    pub(crate) fn shape_text(
        &self,
        text: &str,
        options: &ShapingOptions,
        _font_features: &[(read_fonts::types::Tag, u32)],
    ) -> ShapedText {
        let _ = options;
        ShapedText::new(text.len(), false)
    }

    pub(crate) fn baseline(&self) -> Option<crate::FontBaseline> {
        None
    }
}
