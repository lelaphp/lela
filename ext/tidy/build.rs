//! libtidy is the keg's tidy-html5 (design §7.2, revision 72): the
//! library the oracle links, found through `pkg-config tidy` and never
//! vendored, its declarations written in `src/ffi/`.
//!
//! The SDK's `/usr/lib` holds a libtidy of its own, an older one, and
//! the SDK's libxml2 puts `-L /usr/lib` ahead of the keg's directory on
//! the link line, where `-l tidy` would find it; so the keg's archive is
//! linked where it has one, `libtidy.a` held inside this crate's rlib,
//! whatever the order, as `.cargo/config.toml` links sqlite's.
//! A host whose directory has no archive links the shared library, and a
//! host `pkg-config` finds no libtidy on fails the build.

use std::path::Path;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_PATH");
    let library = match pkg_config::Config::new()
        .atleast_version("5.8")
        .cargo_metadata(false)
        .probe("tidy")
    {
        Ok(library) => library,
        Err(error) => {
            println!(
                "cargo::error=pkg-config tidy: {error}; install tidy-html5, the libtidy the oracle links (design §7.2)"
            );
            return;
        }
    };
    let archive = library
        .link_paths
        .iter()
        .find(|dir| Path::new(dir).join("libtidy.a").is_file());
    if let Some(dir) = archive {
        println!("cargo::rustc-link-search=native={}", dir.display());
        println!("cargo::rustc-link-lib=static=tidy");
    } else {
        for dir in &library.link_paths {
            println!("cargo::rustc-link-search=native={}", dir.display());
        }
        println!("cargo::rustc-link-lib=dylib=tidy");
    }
}
