/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The Worker WASM target runs Servo's services (resources, storage, fonts,
//! canvas) in-process on its single thread instead of on their own threads.
//! Each registers a pump here that handles its queued messages, so that a
//! blocking receive can first let them produce the reply it is waiting for.

use std::cell::RefCell;
use std::rc::Rc;

type Pump = Rc<RefCell<Box<dyn FnMut()>>>;

thread_local! {
    static PUMPS: RefCell<Vec<Pump>> = const { RefCell::new(Vec::new()) };
}

/// Register a service's message pump. It must not block.
pub fn register(pump: Box<dyn FnMut()>) {
    PUMPS.with(|pumps| pumps.borrow_mut().push(Rc::new(RefCell::new(pump))));
}

/// Run every registered pump once. Pumps already running further up the
/// stack are skipped.
pub fn run_all() {
    let pumps: Vec<Pump> = PUMPS.with(|pumps| pumps.borrow().clone());
    for pump in pumps {
        if let Ok(mut pump) = pump.try_borrow_mut() {
            pump();
        }
    }
}
