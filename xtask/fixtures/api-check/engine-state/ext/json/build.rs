//! Writes the default depth into `OUT_DIR`, from a module of the crate it
//! reaches by `#[path]`; no string of its closure names the trait.

#[path = "src/depth.rs"]
mod depth;

fn main() {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let text = format!("const BUILT_DEPTH: u32 = {};", depth::DEPTH);
    std::fs::write(out.join("depth.rs"), text).unwrap();
}
