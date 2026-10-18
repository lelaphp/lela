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
    std::fs::create_dir_all(&dir).ok();
    let first = dir.join("first.h");
    let second = dir.join("second.h");
    std::fs::write(&first, "#define FFI_SCOPE \"S\"\nint abs(int);\n").ok();
    std::fs::write(&second, "#define FFI_SCOPE \"S\"\nlong abs(long);\n").ok();
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
            .ini("html_errors", "0")
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

/// `FFI::load` in the preload served as `opcache.preload_user`, php's
/// child under `ZEND_COMPILE_PRELOAD_IN_CHILD`, which would publish a
/// scope the parent never sees: it throws before it reads the header
/// (`Z/ext/ffi/ffi.c:3628`). The thread here takes nothing, as a test
/// that is not root cannot; what it takes is opcache's.
#[test]
fn ffi_load_refuses_in_the_preload_served_as_the_preload_user() {
    use php_ext::abi::{ExtensionBuilder, PostStartup, PreloadThread};
    use php_ext::decl::{IniDecl, Name};
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("ffi-preload-as-user");
    std::fs::create_dir_all(&dir).ok();
    let script = dir.join("preload.php");
    std::fs::write(
        &script,
        "<?php try { FFI::load(__DIR__ . '/missing.h'); } catch (FFI\\Exception $e) { echo $e->getMessage(), \"\\n\"; }",
    )
    .ok();
    let preloading = ExtensionBuilder::default()
        .ini(vec![IniDecl {
            name: Name(b"t.preload"[..].into()),
            default: Some(b""[..].into()),
            user: false,
            per_dir: false,
            system: true,
        }])
        .post_startup(|req, _| PostStartup::Preload {
            script: req.ini_get(b"t.preload").unwrap_or_default().to_vec(),
            thread: Some(PreloadThread::new(|_| true)),
        })
        .build("t");
    let engine = Engine::builder()
        .extensions(&[php_ext_ffi::extension(), preloading])
        .ini("ffi.enable", "1")
        .ini("t.preload", script.display().to_string())
        .build();
    let served = engine.serve_capture("<?php echo 'served';", Capture::default());
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "FFI::load() doesn't work in conjunction with \"opcache.preload_user\". Use \"ffi.preload\" instead.\nserved"
    );
}
