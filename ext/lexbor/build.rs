//! lexbor, compiled from the sources vendored at `lexbor/` (design §7.2,
//! §12's row on vendored C, #731): `php-src/ext/lexbor/lexbor` at the tag
//! `oracle.toml` pins, byte for byte, with its `LICENSE` and `NOTICE`. The
//! file list is the one `config.m4` compiles, read off the vendored
//! `config.m4` itself, so the tree holds files php's build leaves out (the
//! `ports/windows_nt` sources, `core/utils.c`) and compiles none of them; the flags are
//! `PHP_LEXBOR_CFLAGS`, the crate root on the include path. Nothing is
//! discovered on the system. The extension's process start points
//! lexbor's allocator at the request's heap with `lexbor_memory_setup`,
//! as php's `PHP_MINIT_FUNCTION(lexbor)` points it at `emalloc` and
//! `efree` (`src/ffi/mod.rs`).

fn main() {
    let m4 = std::fs::read_to_string("config.m4").unwrap_or_default();
    let mut build = cc::Build::new();
    // php compiles C and C++ with -DNDEBUG unless it is built with
    // --enable-debug or --enable-debug-assertions (Z/configure.ac:840), the
    // two builds that set ZEND_DEBUG, as debug assertions are this build's:
    // assert() is live exactly where Rust's debug_assertions is (#1903).
    if std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_none() {
        build.define("NDEBUG", None);
    }
    build
        .include(".")
        .opt_level(2)
        .flag_if_supported("-Wno-implicit-fallthrough")
        .warnings(false);
    let mut sources = 0;
    for word in m4.split_whitespace() {
        if let Some(path) = word.strip_prefix("$LEXBOR_DIR/")
            && std::path::Path::new(path)
                .extension()
                .is_some_and(|e| e == "c")
        {
            build.file(format!("lexbor/{path}"));
            sources += 1;
        }
    }
    if sources == 0 {
        println!("cargo:warning=config.m4 names no lexbor source");
    }
    build.compile("lexbor");
    println!("cargo:rerun-if-changed=lexbor");
    println!("cargo:rerun-if-changed=config.m4");
    println!("cargo:rerun-if-changed=build.rs");
}
