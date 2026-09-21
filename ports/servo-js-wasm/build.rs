fn main() {
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        // A long-lived Worker instance needs C++ constructors exactly once.
        // Export the synthetic initializer so the JavaScript host can run it
        // after instantiation, before calling any other wasm export.
        println!("cargo:rustc-link-arg=--export=__wasm_call_ctors");
    }
}
