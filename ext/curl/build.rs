//! libcurl is the platform's (design §7.2, #1324): the library the oracle
//! links, found and linked here, never vendored, and declared in
//! `src/ffi/sys.rs`. The directory `<TARGET>_CURL_LIB_DIR` names, which
//! `.cargo/config.toml` sets for an Apple silicon Mac to Homebrew's keg,
//! the libcurl the oracle links there, is searched first, ahead of the
//! SDK's; elsewhere `pkg-config libcurl` finds it.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").unwrap_or_default();
    let key = format!("{}_CURL_LIB_DIR", target.to_uppercase().replace('-', "_"));
    println!("cargo:rerun-if-env-changed={key}");
    if let Ok(dir) = std::env::var(&key)
        && std::path::Path::new(&dir).is_dir()
    {
        println!("cargo:rustc-link-search=native={dir}");
        println!("cargo:rustc-link-lib=curl");
        return;
    }
    if let Err(error) = pkg_config::Config::new()
        .atleast_version("7.84")
        .probe("libcurl")
    {
        println!("cargo:warning=pkg-config libcurl: {error}");
        println!("cargo:rustc-link-lib=curl");
    }
}
