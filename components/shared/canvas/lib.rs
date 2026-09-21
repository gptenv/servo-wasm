/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![crate_name = "servo_canvas_traits"]
#![crate_type = "rlib"]
#![deny(unsafe_code)]

use crossbeam_channel::Sender;
use euclid::default::Size2D;
use profile_traits::mem::ReportsChan;

use crate::canvas::CanvasId;

pub mod canvas;
// Cloudflare Workers never expose a WebGL context, and `glow` -- which
// this module depends on for its GL type conversions -- unconditionally
// pulls in `wasm-bindgen`/`web_sys` for any wasm32 target regardless of
// which of its own features are enabled. Every consumer of this module
// elsewhere in the tree is already gated behind the `webgl` Cargo feature,
// which the Worker build disables, so excluding the module itself here is
// safe.
#[cfg(not(target_arch = "wasm32"))]
#[macro_use]
pub mod webgl;

pub enum ConstellationCanvasMsg {
    Create {
        sender: Sender<Option<CanvasId>>,
        size: Size2D<u64>,
    },
    CollectMemoryReport(ReportsChan),
    Exit(Sender<()>),
}
