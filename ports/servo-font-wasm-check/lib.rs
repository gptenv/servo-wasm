//! Runtime verification for the wasm32 glyph backend added to our
//! `wr_glyph_rasterizer` fork (see docs/wasm-rendering.md). `cargo check`
//! only proves the code type-checks; this crate actually instantiates
//! `FontContext`, rasterizes real glyphs from real font files, and checks
//! the output is sane -- the same way ports/servo-js-wasm verifies the JS
//! engine rather than trusting compilation alone.
//!
//! Exercised via ports/servo-font-wasm-check/tests/wasm.test.mjs under
//! Node, since there is no wasm32-unknown-unknown `cargo test` runner
//! configured in this workspace (see that file for why).

use std::sync::Arc;

use webrender_api::units::DevicePoint;
use webrender_api::{
    ColorU, FontInstanceFlags, FontInstanceKey, FontKey, FontRenderMode, IdNamespace,
};
use wr_glyph_rasterizer::platform::font::FontContext;
use wr_glyph_rasterizer::{BaseFontInstance, FontInstance, GlyphKey, SubpixelDirection};

// Ahem is the standard web-platform-tests font: every glyph is a solid
// black square exactly matching the font's em box, which makes rasterized
// output exactly predictable rather than merely plausible.
static AHEM_TTF: &[u8] = include_bytes!("../../tests/wpt/tests/fonts/Ahem.ttf");
// Not tests/wpt/tests/fonts/pass.woff: that's a synthetic WOFF-container
// validity fixture with glyphs only for ' ' and 'P' (checked by hand before
// picking this), not representative of a real font's coverage.
static GENTIUM_WOFF: &[u8] = include_bytes!("../../tests/wpt/tests/fonts/GentiumPlus-R.woff");
static HASUBI_WOFF2: &[u8] =
    include_bytes!("../../tests/wpt/tests/fonts/hasubi-mono/HasubiMono-Regular.woff2");

fn rasterize_capital_a(font_bytes: &[u8], font_key_id: u32) -> Result<(i32, i32, Vec<u8>), i32> {
    let mut ctx = FontContext::new();
    let font_key = FontKey::new(IdNamespace(0), font_key_id);
    ctx.add_raw_font(&font_key, Arc::new(font_bytes.to_vec()), 0);

    let glyph_index = match ctx.get_glyph_index(font_key, 'A') {
        Some(idx) if idx != 0 => idx,
        _ => return Err(1), // no glyph found for 'A'
    };

    let base = Arc::new(BaseFontInstance::new(
        FontInstanceKey::new(IdNamespace(0), font_key_id),
        font_key,
        32.0,
        None,
        None,
        Vec::new(),
    ));
    let mut instance = FontInstance::new(
        base,
        ColorU::new(0, 0, 0, 255),
        FontRenderMode::Alpha,
        FontInstanceFlags::empty(),
    );
    FontContext::prepare_font(&mut instance);

    let key = GlyphKey::new(glyph_index, DevicePoint::zero(), SubpixelDirection::None);

    let dims = ctx.get_glyph_dimensions(&instance, &key).ok_or(2)?;
    if dims.width <= 0 || dims.height <= 0 {
        return Err(3); // degenerate dimensions for a visible glyph
    }
    if dims.advance <= 0.0 {
        return Err(4); // 'A' at 32px must have a positive advance width
    }

    let rasterized = ctx.rasterize_glyph(&instance, &key).map_err(|_| 5)?;
    if rasterized.width != dims.width || rasterized.height != dims.height {
        return Err(6); // dimensions disagree between the two entry points
    }

    let expected_len = (rasterized.width as usize) * (rasterized.height as usize) * 4;
    if rasterized.bytes.len() != expected_len {
        return Err(7); // bgra_pixels buffer is the wrong size
    }
    if rasterized.bytes.iter().all(|&b| b == 0) {
        return Err(8); // nothing was actually drawn
    }
    if rasterized.bytes.iter().skip(3).step_by(4).all(|&a| a == 0) {
        return Err(9); // alpha channel is entirely transparent
    }

    // A codepoint with no glyph in the font must not be confused with '\0'.
    if ctx.get_glyph_index(font_key, '\u{10FFFF}').is_some() {
        return Err(10);
    }

    Ok((rasterized.width, rasterized.height, rasterized.bytes))
}

/// Raw TrueType input (no WOFF/WOFF2 unwrapping needed). Returns 0 on
/// success, otherwise a distinct nonzero failure code.
#[unsafe(no_mangle)]
pub extern "C" fn check_ttf() -> i32 {
    match rasterize_capital_a(AHEM_TTF, 1) {
        Ok(_) => 0,
        Err(code) => code,
    }
}

/// Ahem's defining property: every glyph is a solid square exactly the
/// size of the font's em box, so at 32px, 'A' must rasterize to *exactly*
/// 32x32 fully-opaque pixels -- not just "some plausible nonzero size".
#[unsafe(no_mangle)]
pub extern "C" fn check_ttf_ahem_is_exact_square() -> i32 {
    let (width, height, bytes) = match rasterize_capital_a(AHEM_TTF, 1) {
        Ok(result) => result,
        Err(code) => return 100 + code,
    };
    if width != 32 || height != 32 {
        return 200; // Ahem's whole point is that this is exact, not approximate
    }
    if !bytes.iter().skip(3).step_by(4).all(|&a| a == 255) {
        return 201; // Ahem glyphs are solid fills, not partially transparent
    }
    0
}

/// WOFF1 input, exercising the decompress_woff1 path in add_raw_font.
#[unsafe(no_mangle)]
pub extern "C" fn check_woff1() -> i32 {
    match rasterize_capital_a(GENTIUM_WOFF, 2) {
        Ok(_) => 0,
        Err(code) => code,
    }
}

/// WOFF2 input, exercising the decompress_woff2 (Brotli) path in
/// add_raw_font.
#[unsafe(no_mangle)]
pub extern "C" fn check_woff2() -> i32 {
    match rasterize_capital_a(HASUBI_WOFF2, 3) {
        Ok(_) => 0,
        Err(code) => code,
    }
}
