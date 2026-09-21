use fonts_traits::{FontTemplate, LowercaseFontFamilyName};
use style::values::computed::font::GenericFontFamily;

pub(crate) fn for_each_available_family<F>(_callback: F)
where
    F: FnMut(String),
{
}

pub(crate) fn for_each_variation<F>(_family_name: &str, _callback: F)
where
    F: FnMut(FontTemplate),
{
}

pub(crate) fn default_system_generic_font_family(
    generic: GenericFontFamily,
) -> LowercaseFontFamilyName {
    let family = match generic {
        GenericFontFamily::Monospace => "monospace",
        GenericFontFamily::Cursive => "cursive",
        GenericFontFamily::Fantasy => "fantasy",
        _ => "sans-serif",
    };
    family.into()
}

pub fn fallback_font_families(_options: crate::FallbackFontSelectionOptions) -> Vec<&'static str> {
    vec!["sans-serif", "serif", "monospace"]
}
