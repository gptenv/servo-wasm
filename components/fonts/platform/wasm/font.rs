use app_units::Au;
use euclid::default::{Point2D, Rect};
use fonts_traits::{FontIdentifier, FontTemplateDescriptor, LocalFontIdentifier};
use read_fonts::types::Tag;
use style::values::computed::font::{FontStyle, FontWeight, FontWidth};
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
    descriptor: FontTemplateDescriptor,
    variations: Vec<FontVariation>,
}

impl PlatformFontMethods for PlatformFont {
    fn new_from_local_font_identifier(
        _font_identifier: LocalFontIdentifier,
        _pt_size: Option<Au>,
        _synthetic_bold: bool,
    ) -> Result<PlatformFont, &'static str> {
        Err("Worker has no system font registry")
    }

    fn new_from_data(
        _font_identifier: FontIdentifier,
        data: &FontData,
        _requested_size: Option<Au>,
        _synthetic_bold: bool,
    ) -> Result<PlatformFont, &'static str> {
        Ok(Self {
            data: data.clone(),
            descriptor: FontTemplateDescriptor::new(
                FontWeight::normal(),
                FontWidth::NORMAL,
                FontStyle::NORMAL,
            ),
            variations: Vec::new(),
        })
    }

    fn copy_with_variations(
        mut self,
        _font_identifier: &FontIdentifier,
        variations: &[FontVariation],
    ) -> Result<Self, &'static str> {
        self.variations = variations.to_vec();
        Ok(self)
    }

    fn descriptor(&self) -> FontTemplateDescriptor {
        self.descriptor.clone()
    }

    fn glyph_index(&self, codepoint: char) -> Option<GlyphId> {
        Some(codepoint as u32)
    }

    fn glyph_h_advance(&self, _glyph: GlyphId) -> Option<FractionalPixel> {
        Some(10.0)
    }

    fn glyph_h_kerning(&self, _glyph0: GlyphId, _glyph1: GlyphId) -> FractionalPixel {
        0.0
    }

    fn metrics(&self) -> FontMetrics {
        FontMetrics {
            em_size: Au::from_px(10),
            ascent: Au::from_px(8),
            descent: Au::from_px(2),
            max_advance: Au::from_px(10),
            average_advance: Au::from_px(10),
            space_advance: Au::from_px(5),
            ..Default::default()
        }
    }

    fn table_for_tag(&self, _tag: Tag) -> Option<FontTable> {
        Some(FontTable {
            data: self.data.clone(),
        })
    }

    fn typographic_bounds(&self, _glyph: GlyphId) -> Rect<f32> {
        Rect::new(
            Point2D::new(0.0, -8.0),
            euclid::default::Size2D::new(10.0, 10.0),
        )
    }

    fn webrender_font_instance_flags(&self) -> FontInstanceFlags {
        FontInstanceFlags::empty()
    }

    fn variations(&self) -> &[FontVariation] {
        &self.variations
    }
}
