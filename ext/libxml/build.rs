//! libxml2 is the platform's (design §7.2, #731): found through
//! `pkg-config libxml-2.0` and linked, never vendored, so the library the
//! engine reads is the one the oracle's build reads on the same machine.
//! The declarations in `src/ffi/sys.rs` are written to 2.9, which the
//! `pkg-config` holds the library to.

fn main() {
    if let Err(error) = pkg_config::Config::new()
        .atleast_version("2.9")
        .probe("libxml-2.0")
    {
        println!("cargo:warning=pkg-config libxml-2.0: {error}");
        println!("cargo:rustc-link-lib=xml2");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
