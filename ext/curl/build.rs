//! libcurl is the platform's (design §7.2): the library the
//! oracle links, declared and linked by `curl-sys`, never vendored; here
//! curl-sys is held to that, and a miss fails the build.
//!
//! On an Apple target curl-sys links whatever `-l curl` finds, which is
//! the SDK's libcurl unless a directory ahead of it has one; the
//! directory `<TARGET>_CURL_LIB_DIR` names, which `.cargo/config.toml`
//! sets for an Apple silicon Mac to Homebrew's keg (the libcurl the
//! oracle links there), goes on the search path here, and a variable
//! that is unset or names no directory is a failure. Elsewhere curl-sys
//! finds the library through `pkg-config libcurl` and, on a miss, builds
//! the copy it bundles, which it says through `DEP_CURL_STATIC`; that is
//! a failure too.

use std::env;
use std::path::Path;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if env::var_os("DEP_CURL_STATIC").is_some() {
        println!(
            "cargo::error=curl-sys built the libcurl it bundles because `pkg-config libcurl` found none; install the platform's libcurl, the one the oracle links (design §7.2)"
        );
        return;
    }
    if env::var("CARGO_CFG_TARGET_VENDOR").as_deref() != Ok("apple") {
        return;
    }
    let target = env::var("TARGET").unwrap_or_default();
    let key = format!("{}_CURL_LIB_DIR", target.to_uppercase().replace('-', "_"));
    println!("cargo::rerun-if-env-changed={key}");
    match env::var(&key) {
        Ok(dir) if Path::new(&dir).is_dir() => {
            println!("cargo::rustc-link-search=native={dir}");
        }
        _ => println!(
            "cargo::error={key} names no directory, so `-l curl` would find the SDK's libcurl; name the directory of the libcurl the oracle links, as `.cargo/config.toml` does for Homebrew's keg (design §7.2)"
        ),
    }
}
