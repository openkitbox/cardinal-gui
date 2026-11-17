//! Offscreen screenshot rendering entry point. The real assembly lives in the
//! library so it can reach crate-private items; this binary is only a thin
//! caller that keeps the GPUI main thread to itself.
//!
//! The `[[bin]]` entry already sets `required-features`; this inner attribute
//! is a second guard so a direct `rustc` invocation cannot smuggle it in.
#![cfg(feature = "screenshot-test")]

fn main() {
    if let Err(error) = cardinal_gui::run_screenshots() {
        eprintln!("screenshots: {error}");
        std::process::exit(1);
    }
}
