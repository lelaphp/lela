//! uriparser, compiled from the sources vendored at `uriparser/` (design
//! §7.2, §12's row on vendored C): `php-src/ext/uri/uriparser` at
//! the tag `oracle.toml` pins, byte for byte, with its
//! `COPYING.BSD-3-Clause`. The file list is `URIPARSER_SOURCES` of php's
//! `config.m4`, read off the copy beside this file; the flags are the
//! bundled build's, `URI_STATIC_BUILD` with the ANSI interface alone
//! (`URI_NO_UNICODE`, php's `AC_DEFINE`s), `uriparser/include` on the
//! include path. Nothing is discovered on the system. The memory manager
//! is uriparser's default, the C library's `malloc` and `free`, where
//! php's `uri_parser_rfc3986.c` hands it `emalloc` and `efree`.

fn main() {
    let m4 = std::fs::read_to_string("config.m4").unwrap_or_default();
    let mut build = cc::Build::new();
    // php compiles C and C++ with -DNDEBUG unless it is built with
    // --enable-debug or --enable-debug-assertions (Z/configure.ac:840), the
    // two builds that set ZEND_DEBUG, as debug assertions are this build's:
    // assert() is live exactly where Rust's debug_assertions is.
    if std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_none() {
        build.define("NDEBUG", None);
    }
    build
        .include("uriparser/include")
        .define("URI_STATIC_BUILD", None)
        .define("URI_NO_UNICODE", "1")
        .opt_level(2)
        .warnings(false);
    let mut sources = 0;
    for word in m4.split(|c: char| c.is_whitespace() || c == '"' || c == '\\') {
        if let Some(path) = word.strip_prefix("$URIPARSER_DIR/")
            && std::path::Path::new(path)
                .extension()
                .is_some_and(|e| e == "c")
        {
            build.file(format!("uriparser/{path}"));
            sources += 1;
        }
    }
    if sources == 0 {
        println!("cargo:warning=config.m4 names no uriparser source");
    }
    build.compile("uriparser");
    println!("cargo:rerun-if-changed=uriparser");
    println!("cargo:rerun-if-changed=config.m4");
    println!("cargo:rerun-if-changed=build.rs");
}
