//! libedit is the library the pinned php links (design §7.2, revision
//! 72): the platform's, `/usr/lib/libedit.3.dylib` on a Mac, found
//! through `pkg-config libedit`, whose file the package manager keeps
//! for the SDK's, and linked as the shared library the oracle links,
//! never vendored. A miss fails the build rather than falling back to a
//! library the oracle does not link.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if let Err(error) = pkg_config::Config::new().probe("libedit") {
        println!(
            "cargo::error=pkg-config libedit: {error}; the platform's libedit is the one the oracle links (design §7.2)"
        );
    }
}
