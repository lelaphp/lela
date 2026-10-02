//! libsodium is the library the pinned php links (design §7.2, revision
//! 72; #1540): found through `pkg-config libsodium`, which on an Apple
//! silicon Mac is Homebrew's keg, and linked as the shared library the
//! oracle links, never vendored. A miss fails the build rather than
//! falling back to a library the oracle does not link.
//!
//! The link line is safe against the SDK's `/usr/lib`, which the SDK's
//! libxml2 puts ahead of the kegs (#1510): neither it nor the SDK ships
//! a libsodium, so `-l sodium` finds the keg's whatever the order. The
//! version `pkg-config` reports is the headers' version `PHP_MINFO`
//! prints, `SODIUM_VERSION_STRING`, handed to the crate as
//! `SODIUM_HEADERS_VERSION`.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    match pkg_config::Config::new()
        .atleast_version("1.0.18")
        .probe("libsodium")
    {
        Ok(library) => {
            println!(
                "cargo::rustc-env=SODIUM_HEADERS_VERSION={}",
                library.version
            );
        }
        Err(error) => {
            println!(
                "cargo::error=pkg-config libsodium: {error}; install the libsodium the oracle links, Homebrew's `libsodium` keg on a Mac (design §7.2)"
            );
        }
    }
}
