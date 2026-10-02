//! libxslt and libexslt are the platform's (design §7.2, revision 72):
//! found through `pkg-config libxslt` and `pkg-config libexslt` and
//! linked, never vendored, so the library the engine runs is the one the
//! oracle's build links on the same machine. `src/ffi/shim.c` is the part
//! of the binding Rust cannot write: libxslt's error handler is C-variadic,
//! and the transformation's fields are read through the library's
//! own headers rather than a copy of its layout. It includes libxslt's
//! headers and libxml2's beside them, which `pkg-config libxml-2.0` names
//! and which are not linked here: ext/libxml links libxml2.

fn main() {
    for (library, version, fallback) in [("libxslt", "1.1", "xslt"), ("libexslt", "0.8", "exslt")] {
        if let Err(error) = pkg_config::Config::new()
            .atleast_version(version)
            .probe(library)
        {
            println!("cargo:warning=pkg-config {library}: {error}");
            println!("cargo:rustc-link-lib={fallback}");
        }
    }
    let mut build = cc::Build::new();
    build.file("src/ffi/shim.c").warnings(true);
    if let Ok(xml) = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libxml-2.0")
    {
        for path in xml.include_paths {
            build.include(path.join("libxml2")).include(path);
        }
    }
    if let Ok(dir) = pkg_config::get_variable("libxml-2.0", "includedir") {
        build.include(std::path::Path::new(&dir).join("libxml2"));
    }
    build.compile("php_xsl_shim");
    println!("cargo:rerun-if-changed=src/ffi/shim.c");
    println!("cargo:rerun-if-changed=build.rs");
}
