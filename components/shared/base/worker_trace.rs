/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A cheap "current phase" breadcrumb, reported by the Worker WASM panic hook.
//! Release WASM builds are stripped of function names, so this is how a panic
//! is placed within a long operation.

use std::cell::Cell;

thread_local! {
    static PHASE: Cell<&'static str> = const { Cell::new("") };
}

/// Record the phase about to run.
pub fn set(phase: &'static str) {
    PHASE.with(|current| current.set(phase));
}

/// The most recently recorded phase.
pub fn get() -> &'static str {
    PHASE.with(Cell::get)
}
