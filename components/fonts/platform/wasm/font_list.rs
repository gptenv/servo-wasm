/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Worker WASM font list: the faces in `fonts_traits::worker_fonts`, which
//! holds the bundled fonts plus any fonts the host registers.

use std::cell::RefCell;

use fonts_traits::{
    FontIdentifier, FontTemplate, FontTemplateDescriptor, LocalFontIdentifier,
    LowercaseFontFamilyName, worker_fonts,
};
use read_fonts::{FileRef, TableProvider};
use skrifa::MetadataProvider;
use skrifa::string::StringId;
use style::Atom;
use style::values::computed::font::GenericFontFamily;

use crate::font::PlatformFontMethods;
use crate::platform::font::PlatformFont;

/// Family names of the fonts bundled into the Worker build, used for the CSS
/// generic families.
const SANS_SERIF: &str = "noto sans";
const SERIF: &str = "noto serif";
const MONOSPACE: &str = "noto sans mono";

struct Face {
    family: String,
    identifier: LocalFontIdentifier,
    descriptor: FontTemplateDescriptor,
}

/// Every face in the registry, in registration order.
fn faces() -> Vec<Face> {
    let mut faces = Vec::new();
    for (file_index, data) in worker_fonts::all().iter().enumerate() {
        let Ok(file) = FileRef::new(data.as_ref()) else {
            continue;
        };
        for (face_index, font) in file.fonts().enumerate() {
            let Ok(font) = font else {
                continue;
            };
            let family = [StringId::TYPOGRAPHIC_FAMILY_NAME, StringId::FAMILY_NAME]
                .into_iter()
                .find_map(|id| font.localized_strings(id).english_or_first())
                .map(|name| name.to_string());
            let Some(family) = family else {
                continue;
            };
            let descriptor = match font.os2() {
                Ok(os2) => PlatformFont::descriptor_from_os2_table(&os2),
                Err(_) => FontTemplateDescriptor::default(),
            };
            faces.push(Face {
                family,
                identifier: LocalFontIdentifier {
                    path: Atom::from(format!("{}{file_index}", worker_fonts::PATH_PREFIX)),
                    face_index: face_index as u16,
                    named_instance_index: 0,
                },
                descriptor,
            });
        }
    }
    faces
}

pub(crate) fn for_each_available_family<F>(mut callback: F)
where
    F: FnMut(String),
{
    let mut seen = Vec::<String>::new();
    for face in faces() {
        if !seen.iter().any(|family| family.eq_ignore_ascii_case(&face.family)) {
            seen.push(face.family.clone());
            callback(face.family);
        }
    }
}

pub(crate) fn for_each_variation<F>(family_name: &str, mut callback: F)
where
    F: FnMut(FontTemplate),
{
    for face in faces() {
        if face.family.eq_ignore_ascii_case(family_name) {
            callback(FontTemplate::new(
                FontIdentifier::Local(face.identifier),
                face.descriptor,
                None,
            ));
        }
    }
}

pub(crate) fn default_system_generic_font_family(
    generic: GenericFontFamily,
) -> LowercaseFontFamilyName {
    match generic {
        GenericFontFamily::Serif => SERIF,
        GenericFontFamily::Monospace => MONOSPACE,
        _ => SANS_SERIF,
    }
    .into()
}

thread_local! {
    /// Fallback family names for the current registry generation. The names
    /// are leaked once per generation because the platform API returns
    /// `&'static str`; registrations are few and happen at startup.
    static FALLBACK_FAMILIES: RefCell<Option<(u64, Vec<&'static str>)>> =
        const { RefCell::new(None) };
}

/// Every registered family, bundled ones first, so glyphs missing from the
/// requested font can come from any registered font (e.g. host-supplied CJK
/// or emoji fonts).
pub fn fallback_font_families(_options: crate::FallbackFontSelectionOptions) -> Vec<&'static str> {
    let generation = worker_fonts::generation();
    FALLBACK_FAMILIES.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((cached_generation, families)) = cache.as_ref() &&
            *cached_generation == generation
        {
            return families.clone();
        }
        let mut families: Vec<&'static str> = vec![SANS_SERIF, SERIF, MONOSPACE];
        for_each_available_family(|family| {
            if !families
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&family))
            {
                families.push(Box::leak(family.into_boxed_str()));
            }
        });
        *cache = Some((generation, families.clone()));
        families
    })
}
