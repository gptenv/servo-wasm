/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Text shaping with HarfRust, the pure-Rust port of HarfBuzz. Used on the
//! Worker WASM target, where Servo's native HarfBuzz C++ build is not linked.
//! Adapted from Servo's earlier HarfRust backend (servo/servo#38681).

use app_units::Au;
use euclid::default::Point2D;
use harfrust::font::BuiltinFontFuncs;
use harfrust::{
    BufferClusterLevel, Feature, FontRef as HarfRustFontRef, GlyphBuffer, Language, Script,
    ShapeOptions, ShaperData, ShaperInstance, UnicodeBuffer, Variation,
};
use num_traits::Zero as _;
use read_fonts::TableProvider;
use read_fonts::types::{BigEndian, Tag};

use super::{GlyphShapingResult, unicode_script_to_iso15924_tag};
use crate::{
    Font, FontBaseline, FontData, ShapedGlyph, ShapedText, ShapingFlags, ShapingOptions,
    fixed_to_float, float_to_fixed,
};

/// HarfRust depends on its own `read-fonts` version, so tags cross over by value.
fn to_harfrust_tag(tag: Tag) -> harfrust::Tag {
    harfrust::Tag::from_be_bytes(tag.to_be_bytes())
}

pub(crate) struct HarfrustGlyphShapingResult {
    data: GlyphBuffer,
}

struct ShapedGlyphIterator<'a> {
    shaped_glyph_data: &'a HarfrustGlyphShapingResult,
    current_glyph_offset: usize,
    y_position: Au,
}

impl Iterator for ShapedGlyphIterator<'_> {
    type Item = ShapedGlyph;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_glyph_offset >= self.shaped_glyph_data.len() {
            return None;
        }
        let offset = self.current_glyph_offset;
        self.current_glyph_offset += 1;

        let glyph_info = self.shaped_glyph_data.data.glyph_infos().get(offset)?;
        let position = self.shaped_glyph_data.data.glyph_positions().get(offset)?;

        let x_offset = Au::from_f64_px(Shaper::fixed_to_float(position.x_offset));
        let y_offset = Au::from_f64_px(Shaper::fixed_to_float(position.y_offset));
        let x_advance = Au::from_f64_px(Shaper::fixed_to_float(position.x_advance));
        let y_advance = Au::from_f64_px(Shaper::fixed_to_float(position.y_advance));

        let offset = if x_offset.is_zero() && y_offset.is_zero() && y_advance.is_zero() {
            None
        } else {
            if y_advance > Au::zero() {
                self.y_position -= y_advance;
            }
            Some(Point2D::new(x_offset, self.y_position - y_offset))
        };

        Some(ShapedGlyph {
            glyph_id: glyph_info.glyph_id,
            string_byte_offset: glyph_info.cluster as usize,
            advance: x_advance,
            offset,
        })
    }
}

impl GlyphShapingResult for HarfrustGlyphShapingResult {
    fn len(&self) -> usize {
        self.data.len()
    }

    fn is_rtl(&self) -> bool {
        let glyph_infos = self.data.glyph_infos();
        match (glyph_infos.first(), glyph_infos.last()) {
            (Some(first), Some(last)) => last.cluster < first.cluster,
            _ => false,
        }
    }

    fn iter(&self) -> impl Iterator<Item = ShapedGlyph> {
        ShapedGlyphIterator {
            shaped_glyph_data: self,
            current_glyph_offset: 0,
            y_position: Au::zero(),
        }
    }
}

pub(crate) struct Shaper {
    font: *const Font,
    font_data: FontData,
    /// The index of the face in its file (non-zero only for collections).
    font_index: u32,
    /// Pixels per em, i.e. the font size.
    ppem: f64,
    shaper_data: ShaperData,
    /// Only created for variable fonts with variations set.
    shaper_instance: Option<ShaperInstance>,
}

// `Font` and `FontData` are thread-safe and the font owns its shaper.
#[allow(unsafe_code)]
unsafe impl Sync for Shaper {}
#[allow(unsafe_code)]
unsafe impl Send for Shaper {}

impl std::fmt::Debug for Shaper {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Shaper").finish_non_exhaustive()
    }
}

impl Shaper {
    pub(crate) fn new(font: &Font) -> Self {
        // A Worker `Font` only exists once its bytes were readable.
        let Ok(raw_font) = font.font_data_and_index() else {
            panic!("Font data unavailable when creating its HarfRust shaper");
        };
        let font_data = raw_font.data.clone();
        let font_index = raw_font.index;
        let ppem = font.descriptor.pt_size.to_f64_px();

        let hr_font = HarfRustFontRef::from_index(font_data.as_ref(), font_index)
            .expect("font data was validated when the font was created");
        let shaper_data = ShaperData::new(&hr_font);

        let variations = font.variations();
        let shaper_instance =
            if servo_config::pref!(layout_variable_fonts_enabled) && !variations.is_empty() {
                let variations = variations.iter().map(|variation| Variation {
                    tag: harfrust::Tag::from_u32(variation.tag),
                    value: variation.value,
                });
                Some(ShaperInstance::from_variations(&hr_font, variations))
            } else {
                None
            };

        Self {
            font: font as *const Font,
            font_data,
            font_index,
            ppem,
            shaper_data,
            shaper_instance,
        }
    }

    fn shaped_glyph_data(
        &self,
        text: &str,
        options: &ShapingOptions,
        font_features: &[(Tag, u32)],
    ) -> HarfrustGlyphShapingResult {
        let mut buffer = UnicodeBuffer::new();
        buffer.set_cluster_level(BufferClusterLevel::MonotoneCharacters);
        buffer.set_direction(if options.flags.contains(ShapingFlags::RTL_FLAG) {
            harfrust::Direction::RightToLeft
        } else {
            harfrust::Direction::LeftToRight
        });
        let script_tag = harfrust::Tag::from_u32(unicode_script_to_iso15924_tag(options.script));
        if let Some(script) = Script::from_iso15924_tag(script_tag) {
            buffer.set_script(script);
        }
        if let Ok(language) = options.language.as_str().parse::<Language>() {
            buffer.set_language(language);
        }
        buffer.push_str(text);
        buffer.guess_segment_properties();

        let features: Vec<_> = font_features
            .iter()
            .map(|(tag, value)| Feature::new(to_harfrust_tag(*tag), *value, ..))
            .collect();

        let hr_font = HarfRustFontRef::from_index(self.font_data.as_ref(), self.font_index)
            .expect("font data was validated when the font was created");
        let shaper = self
            .shaper_data
            .shaper(&hr_font)
            .instance(self.shaper_instance.as_ref())
            .build();

        let mut font_funcs = FontFuncs { font: self.font() };
        let glyph_buffer = shaper.shape(
            buffer,
            ShapeOptions::new()
                .scale(Some(Shaper::float_to_fixed(self.ppem)))
                .features(&features)
                .font_funcs(Some(&mut font_funcs)),
        );

        HarfrustGlyphShapingResult { data: glyph_buffer }
    }

    #[allow(unsafe_code)]
    fn font(&self) -> &Font {
        // SAFETY: the font owns this shaper, so it outlives it.
        assert!(!self.font.is_null());
        unsafe { &(*self.font) }
    }

    pub(crate) fn shape_text(
        &self,
        text: &str,
        options: &ShapingOptions,
        font_features: &[(Tag, u32)],
    ) -> ShapedText {
        ShapedText::with_shaped_glyph_data(
            text,
            options,
            &self.shaped_glyph_data(text, options, font_features),
        )
    }

    pub(crate) fn baseline(&self) -> Option<FontBaseline> {
        let font_ref =
            read_fonts::FontRef::from_index(self.font_data.as_ref(), self.font_index).ok()?;

        // The horizontal axis of the BASE table.
        let base_table = font_ref.base().ok()?;
        let horiz_axis = base_table.horiz_axis()?.ok()?;

        let tag_list = horiz_axis.base_tag_list()?.ok()?;
        let baseline_tags = tag_list.baseline_tags();
        let index_of = |tag: &[u8; 4]| {
            baseline_tags
                .binary_search(&BigEndian::from(Tag::new(tag)))
                .ok()
        };
        let romn_index = index_of(b"romn");
        let hang_index = index_of(b"hang");
        let ideo_index = index_of(b"ideo");
        if romn_index.is_none() && hang_index.is_none() && ideo_index.is_none() {
            return None;
        }

        // The DFLT script record's baseline coordinates.
        let script_list = horiz_axis.base_script_list().ok()?;
        let script_records = script_list.base_script_records();
        let default_record_index = script_records
            .binary_search_by_key(&Tag::from_be_bytes(*b"DFLT"), |record| {
                record.base_script_tag()
            })
            .ok()?;
        let base_script = script_records[default_record_index]
            .base_script(script_list.offset_data())
            .ok()?;
        let base_values = base_script.base_values()?.ok()?;
        let base_coords = base_values.base_coords();
        let coordinate = |index: usize| -> Option<f32> {
            base_coords
                .get(index)
                .ok()
                .map(|coord| fixed_to_float(8, coord.coordinate() as i32) as f32)
        };

        Some(FontBaseline {
            ideographic_baseline: ideo_index.and_then(coordinate).unwrap_or(0.0),
            alphabetic_baseline: romn_index.and_then(coordinate).unwrap_or(0.0),
            hanging_baseline: hang_index.and_then(coordinate).unwrap_or(0.0),
        })
    }

    fn float_to_fixed(f: f64) -> i32 {
        float_to_fixed(16, f)
    }

    fn fixed_to_float(i: i32) -> f64 {
        fixed_to_float(16, i)
    }
}

/// Glyph lookup and advances come from Servo's `Font`, so shaping agrees with
/// the metrics layout uses.
struct FontFuncs<'a> {
    font: &'a Font,
}

impl harfrust::font::FontFuncs for FontFuncs<'_> {
    fn nominal_glyph(&mut self, _builtin: &BuiltinFontFuncs, c: u32) -> Option<harfrust::GlyphId> {
        self.font
            .glyph_index(char::from_u32(c)?)
            .map(harfrust::GlyphId::new)
    }

    fn advance_width(&mut self, _builtin: &BuiltinFontFuncs, glyph: harfrust::GlyphId) -> i32 {
        Shaper::float_to_fixed(self.font.glyph_h_advance(glyph.to_u32()))
    }
}
