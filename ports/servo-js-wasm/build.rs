/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

fn main() {
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        // A long-lived Worker instance needs C++ constructors exactly once.
        // Export the synthetic initializer so the JavaScript host can run it
        // after instantiation, before calling any other wasm export.
        println!("cargo:rustc-link-arg=--export=__wasm_call_ctors");
    }
}
