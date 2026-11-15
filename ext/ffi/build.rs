//! libffi is the platform's (design §7.2, revision 75): the
//! oracle links `/usr/lib/libffi.dylib`, whose declarations are the
//! SDK's `ffi.h`, and `pkg-config libffi` names that library on a Mac,
//! `-lffi` with no search path of its own, so the linker finds the
//! SDK's stub ahead of anything a keg installs. Homebrew's `libffi` keg
//! is keg-only and is never linked here. A miss fails the build rather
//! than falling back to a library the oracle does not link.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if let Err(error) = pkg_config::Config::new()
        .atleast_version("3.3")
        .probe("libffi")
    {
        println!(
            "cargo::error=pkg-config libffi: {error}; the platform's libffi, which the oracle links (design §7.2)"
        );
    }
}
