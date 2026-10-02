//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let engine = Engine::builder()
        .extensions(&[php_ext_ffi::extension()])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('ffi'));", sink);
    assert_eq!(served.stdout, b"bool(true)\n");
}

/// `ffi.preload` naming a file that is not there, and a header that
/// redeclares a symbol of the scope another header made: php warns and
/// refuses to start the extension, which serves nothing and ends the
/// process with `exit(-2)`. The bytes are the oracle's under `php -n -d
/// ffi.enable=1 -d ffi.preload=...` (#1614).
#[test]
fn a_failed_preload_refuses_startup() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("ffi-preload-refused");
    let _ = std::fs::create_dir_all(&dir);
    let first = dir.join("first.h");
    let second = dir.join("second.h");
    let _ = std::fs::write(&first, "#define FFI_SCOPE \"S\"\nint abs(int);\n");
    let _ = std::fs::write(&second, "#define FFI_SCOPE \"S\"\nlong abs(long);\n");
    let missing = dir.join("missing.h");
    let both = format!("{}:{}", first.display(), second.display());
    for (list, warning) in [
        (
            missing.display().to_string(),
            format!(
                "FFI: failed pre-loading '{}', file doesn't exist",
                missing.display()
            ),
        ),
        (
            both,
            format!(
                "FFI: failed pre-loading '{}', redefinition of 'abs'",
                second.display()
            ),
        ),
    ] {
        let engine = Engine::builder()
            .extensions(&[php_ext_ffi::extension()])
            .ini("ffi.enable", "1")
            .ini("ffi.preload", &list)
            .build();
        let served = engine.serve_capture("<?php echo 'served';", Capture::default());
        assert_eq!(
            String::from_utf8_lossy(&served.stdout),
            format!(
                "\nWarning: {warning} in Unknown on line 0\n\nFatal error: Unable to start FFI module in Unknown on line 0\nStack trace:\n#0 {{main}}\n"
            )
        );
        assert_eq!(served.exit_code, -2);
    }
}
