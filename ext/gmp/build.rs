//! libgmp is the library the pinned php links (design §7.2, revision
//! 72): found through `pkg-config gmp`, which on an Apple silicon
//! Mac is Homebrew's keg, and linked as the shared library the oracle
//! links, never vendored. A miss fails the build rather than falling
//! back to a library the oracle does not link.
//!
//! The link line is safe against the SDK's `/usr/lib`, which the SDK's
//! libxml2 puts ahead of the kegs: neither it nor the SDK ships
//! a libgmp, so `-l gmp` finds the keg's whatever the order.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if let Err(error) = pkg_config::Config::new()
        .atleast_version("6.0")
        .probe("gmp")
    {
        println!(
            "cargo::error=pkg-config gmp: {error}; install the libgmp the oracle links, Homebrew's `gmp` keg on a Mac (design §7.2)"
        );
    }
}
