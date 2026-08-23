// Entry point for `cargo run --bin uniffi-bindgen -- generate ...`, used by
// build-xcframework.sh to emit the Swift bindings in library mode.
fn main() {
    uniffi::uniffi_bindgen_main()
}
