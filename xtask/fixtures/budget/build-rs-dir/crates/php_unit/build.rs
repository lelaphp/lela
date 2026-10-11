fn main() {
    let schema = std::path::Path::new(env!( "CARGO_MANIFEST_DIR" )).join("ops.toml");
    println!("cargo:rerun-if-changed={}", schema.display());
}
