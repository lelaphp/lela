//! libpcre2-8 is the library the pinned keg links (design §7.2, #624,
//! #717): `pcre2-sys` finds it through `pkg-config libpcre2-8` and links
//! it, the keg's archive where the workspace sets `LIBPCRE2_8_STATIC`.
//! Where `pkg-config` misses, `pcre2-sys` builds the source it bundles, a
//! release the oracle does not link; the same lookup here, emitting
//! nothing, fails the build there instead.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if let Err(error) = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libpcre2-8")
    {
        println!(
            "cargo::error=pkg-config libpcre2-8: {error}; install the libpcre2-8 the oracle links, Homebrew's `pcre2` keg on a Mac (design §7.2)"
        );
    }
}
