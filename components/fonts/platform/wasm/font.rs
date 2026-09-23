/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Worker WASM font backend: pure-Rust font parsing and metrics via `skrifa`,
//! since the native FreeType/CoreText/DirectWrite backends are unavailable.

use app_units::Au;
use euclid::default::{Point2D, Rect, Size2D};
use fonts_traits::{FontIdentifier, FontTemplateDescriptor, LocalFontIdentifier};
use read_fonts::TableProvider;
use read_fonts::types::Tag;
use skrifa::instance::{Location, Size};
use skrifa::{FontRef, MetadataProvider};
use webrender_api::{FontInstanceFlags, FontVariation};

use crate::font::{FontMetrics, FontTableMethods, FractionalPixel, PlatformFontMethods};
use crate::{FontData, GlyphId};

#[derive(Clone)]
pub struct FontTable {
    data: FontData,
}

impl FontTableMethods for FontTable {
    fn buffer(&self) -> &[u8] {
        self.data.as_ref()
    }
}

#[derive(Clone)]
pub struct PlatformFont {
    data: FontData,
    index: u32,
    /// The requested size in pixels; zero when no size was requested.
    size: f32,
    location: Location,
    variations: Vec<FontVariation>,
}

impl PlatformFont {
    fn new(data: FontData, index: u32, requested_size: Option<Au>) -> Result<Self, &'static str> {
        FontRef::from_index(data.as_ref(), index).map_err(|_| "Could not parse font data")?;
        Ok(Self {
            data,
            index,
            size: requested_size.map_or(0.0, |size| size.to_f32_px()),
            location: Location::default(),
            variations: Vec::new(),
        })
    }

    fn font_ref(&self) -> FontRef<'_> {
        // Validated when this font was created.
        FontRef::from_index(self.data.as_ref(), self.index).expect("font data was validated")
    }

    fn skrifa_size(&self) -> Size {
        if self.size > 0.0 {
            Size::new(self.size)
        } else {
            Size::unscaled()
        }
    }
}

impl PlatformFontMethods for PlatformFont {
    fn new_from_local_font_identifier(
        font_identifier: LocalFontIdentifier,
        pt_size: Option<Au>,
        _synthetic_bold: bool,
    ) -> Result<PlatformFont, &'static str> {
        let data_and_index = font_identifier
            .font_data_and_index()
            .ok_or("Font is not in the Worker font registry")?;
        Self::new(data_and_index.data, data_and_index.index, pt_size)
    }

    fn new_from_data(
        font_identifier: FontIdentifier,
        data: &FontData,
        requested_size: Option<Au>,
        _synthetic_bold: bool,
    ) -> Result<PlatformFont, &'static str> {
        Self::new(data.clone(), font_identifier.index(), requested_size)
    }

    fn copy_with_variations(
        mut self,
        _font_identifier: &FontIdentifier,
        variations: &[FontVariation],
    ) -> Result<Self, &'static str> {
        self.variations = variations.to_vec();
        let settings: Vec<_> = variations
            .iter()
            .map(|variation| (Tag::from_u32(variation.tag), variation.value))
            .collect();
        self.location = self.font_ref().axes().location(settings);
        Ok(self)
    }

    fn descriptor(&self) -> FontTemplateDescriptor {
        match self.font_ref().os2() {
            Ok(os2) => Self::descriptor_from_os2_table(&os2),
            Err(_) => FontTemplateDescriptor::default(),
        }
    }

    fn glyph_index(&self, codepoint: char) -> Option<GlyphId> {
        self.font_ref()
            .charmap()
            .map(codepoint)
            .map(|glyph| glyph.to_u32())
    }

    fn glyph_h_advance(&self, glyph: GlyphId) -> Option<FractionalPixel> {
        let font = self.font_ref();
        font.glyph_metrics(self.skrifa_size(), &self.location)
            .advance_width(glyph.into())
            .map(f64::from)
    }

    fn glyph_h_kerning(&self, _glyph0: GlyphId, _glyph1: GlyphId) -> FractionalPixel {
        // Kerning is applied by the HarfRust shaper (GPOS and `kern`).
        0.0
    }

    fn metrics(&self) -> FontMetrics {
        let font = self.font_ref();
        let size = self.skrifa_size();
        let metrics = font.metrics(size, &self.location);
        let em_size = if self.size > 0.0 {
            self.size
        } else {
            f32::from(metrics.units_per_em)
        };
        let ascent = metrics.ascent;
        let descent = -metrics.descent;
        let advance_of = |character| {
            self.glyph_index(character)
                .and_then(|glyph| self.glyph_h_advance(glyph))
        };
        let max_advance = metrics.max_width.unwrap_or(em_size);
        let average_advance = metrics
            .average_width
            .map(f64::from)
            .or_else(|| advance_of('0'))
            .unwrap_or(f64::from(max_advance));
        let underline = metrics.underline;
        let underline_size = underline.map_or(em_size / 14.0, |decoration| decoration.thickness);
        let (strikeout_size, strikeout_offset) = match metrics.strikeout {
            Some(decoration) if decoration.thickness != 0.0 && decoration.offset != 0.0 => {
                (decoration.thickness, decoration.offset)
            },
            // OpenType's suggested default for Roman fonts, as the FreeType backend does.
            _ => (underline_size, em_size * 409.0 / 2048.0 + 0.5 * underline_size),
        };

        FontMetrics {
            underline_size: Au::from_f32_px(underline_size),
            underline_offset: Au::from_f32_px(underline.map_or(-underline_size, |decoration| {
                decoration.offset
            })),
            strikeout_size: Au::from_f32_px(strikeout_size),
            strikeout_offset: Au::from_f32_px(strikeout_offset),
            leading: Au::from_f32_px(metrics.leading),
            x_height: Au::from_f32_px(metrics.x_height.unwrap_or(0.5 * em_size)),
            em_size: Au::from_f32_px(em_size),
            ascent: Au::from_f32_px(ascent),
            descent: Au::from_f32_px(descent),
            max_advance: Au::from_f32_px(max_advance),
            average_advance: Au::from_f64_px(average_advance),
            // Servo's `line_gap` is the full line height (ascent + descent +
            // gap), as in the FreeType and DirectWrite backends; `leading` is
            // just the gap.
            line_gap: Au::from_f32_px(ascent + descent + metrics.leading),
            zero_horizontal_advance: advance_of('0').map(Au::from_f64_px),
            ic_horizontal_advance: advance_of('\u{6C34}').map(Au::from_f64_px),
            space_advance: Au::from_f64_px(advance_of(' ').unwrap_or(average_advance)),
        }
    }

    fn table_for_tag(&self, tag: Tag) -> Option<FontTable> {
        let data = self.font_ref().table_data(tag)?;
        Some(FontTable {
            data: FontData::from_bytes(data.as_bytes()),
        })
    }

    fn typographic_bounds(&self, glyph: GlyphId) -> Rect<f32> {
        let font = self.font_ref();
        let Some(bounds) = font
            .glyph_metrics(self.skrifa_size(), &self.location)
            .bounds(glyph.into())
        else {
            return Rect::default();
        };
        Rect::new(
            Point2D::new(bounds.x_min, bounds.y_min),
            Size2D::new(bounds.x_max - bounds.x_min, bounds.y_max - bounds.y_min),
        )
    }

    fn webrender_font_instance_flags(&self) -> FontInstanceFlags {
        FontInstanceFlags::empty()
    }

    fn variations(&self) -> &[FontVariation] {
        &self.variations
    }
}
