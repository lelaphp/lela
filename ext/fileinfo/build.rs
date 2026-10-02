//! libmagic, compiled from the sources vendored at `libmagic/` (design
//! §7.2, #1451): `php-src/ext/fileinfo/libmagic` at the tag `oracle.toml`
//! pins, byte for byte, with php's patches applied and its `LICENSE`, and
//! `data_file.c`, the compiled magic database php builds in, which
//! `apprentice.c` includes. The file list is the one `config.m4` compiles,
//! read off the vendored `config.m4` itself; `strcasestr.c` is left out,
//! as php's configure leaves it out where the C library has the function.
//! The include path is php's, the crate root and `libmagic/`, behind
//! `shim/`, which holds the Zend and php names php's libmagic includes and
//! `shim/shim.c` behind them. Nothing is discovered on the system.

fn main() {
    let m4 = std::fs::read_to_string("config.m4").unwrap_or_default();
    let mut build = cc::Build::new();
    build
        .include("shim")
        .include("libmagic")
        .include(".")
        .opt_level(2)
        .warnings(false)
        .file("shim/shim.c");
    let mut sources = 0;
    for word in m4.split_whitespace() {
        if word.starts_with("libmagic/")
            && std::path::Path::new(word)
                .extension()
                .is_some_and(|e| e == "c")
        {
            build.file(word);
            sources += 1;
        }
    }
    if sources == 0 {
        println!("cargo:warning=config.m4 names no libmagic source");
    }
    build.compile("magic");
    for path in ["libmagic", "shim", "data_file.c", "config.m4", "build.rs"] {
        println!("cargo:rerun-if-changed={path}");
    }
}
