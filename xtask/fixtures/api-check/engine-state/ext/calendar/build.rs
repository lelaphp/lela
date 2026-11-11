//! A build script that writes the impl into `OUT_DIR`, which no source
//! holds: the sensor reads the build script's text, and one that names the
//! trait fails the crate.

fn main() {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("kept.rs"), "impl EngineState for Kept {}").unwrap();
}
