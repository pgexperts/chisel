// chisel-ffi — UniFFI binding surface for the Chisel engine.
//
// Role in system: the ONLY crate that speaks the C ABI. It wraps chisel::Chisel
// in a Mutex (the engine is Send + !Sync) and re-presents a flat, synchronous
// API to UniFFI, which generates Swift. All ergonomics (async actor, closures)
// live in the Swift package, not here.

uniffi::setup_scaffolding!();

mod error;
pub use error::{ChiselError, FfiResult};

mod types;
pub use types::{
    Argon2Params, ChiselKey, Counters, DefragOptions, DefragStats, DrainInsertion, Options, Stats,
    TagDropProgress,
};

mod database;
pub use database::NativeDatabase;

// Compile-time proof that the engine can move between threads, which the
// Mutex<Option<Chisel>> wrapper in database.rs relies on. If a future engine
// change (mmap, raw OS handle) makes Chisel !Send, THIS line fails to compile
// with a clear message, and the fallback is a dedicated-thread actor (see spec
// §3.3). Cheap insurance against a wrong assumption.
const _: () = {
    fn assert_send<T: Send>() {}
    let _ = assert_send::<chisel::Chisel>;
};

/// Returns the crate version, e.g. "0.1.0". Smoke test that the whole
/// Rust→UniFFI→Swift pipeline is wired end to end.
#[uniffi::export]
pub fn library_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
