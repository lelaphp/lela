//! Writes the default depth into `OUT_DIR`, naming no trait.

fn main() {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("depth.rs"), "const DEPTH: u32 = 512;").unwrap();
}
