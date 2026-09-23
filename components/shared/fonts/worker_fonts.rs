/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! In-memory font registry for the Worker WASM target, which has no system
//! fonts. It holds the fonts bundled with the module and any fonts the host
//! registers at runtime. Registered faces are exposed as local fonts whose
//! `LocalFontIdentifier::path` is `worker-font:<index>`.

use std::cell::{Cell, RefCell};

use read_fonts::FileRef;

use crate::FontData;

/// Prefix of `LocalFontIdentifier::path` for faces in this registry.
pub const PATH_PREFIX: &str = "worker-font:";

thread_local! {
    static FONTS: RefCell<Vec<FontData>> = const { RefCell::new(Vec::new()) };
    static GENERATION: Cell<u64> = const { Cell::new(0) };
    static SERVICE_PUMP: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
}

/// Add a font file (a single face or a collection). Returns the number of
/// faces it contains, or an error if the bytes are not a font file.
pub fn register(bytes: Vec<u8>) -> Result<u32, &'static str> {
    let faces = match FileRef::new(&bytes).map_err(|_| "not an OpenType/TrueType font")? {
        FileRef::Font(_) => 1,
        FileRef::Collection(collection) => collection.len(),
    };
    if faces == 0 {
        return Err("font collection has no faces");
    }
    FONTS.with(|fonts| fonts.borrow_mut().push(FontData::from_vec(bytes)));
    GENERATION.with(|generation| generation.set(generation.get() + 1));
    Ok(faces)
}

/// The data of every registered file, in registration order.
pub fn all() -> Vec<FontData> {
    FONTS.with(|fonts| fonts.borrow().clone())
}

/// The data of the file registered at `index`.
pub fn get(index: usize) -> Option<FontData> {
    FONTS.with(|fonts| fonts.borrow().get(index).cloned())
}

/// Changes whenever a font is registered, so caches of font lookups can be
/// invalidated.
pub fn generation() -> u64 {
    GENERATION.with(Cell::get)
}

/// Install the in-process system font service's message pump.
pub fn set_service_pump(pump: Box<dyn FnMut()>) {
    SERVICE_PUMP.with(|slot| *slot.borrow_mut() = Some(pump));
}

/// Run queued system font service messages so their replies exist. Callers
/// invoke this after sending and before blocking on a reply.
pub fn process_service() {
    SERVICE_PUMP.with(|slot| {
        if let Some(pump) = slot.borrow_mut().as_mut() {
            pump();
        }
    });
}
