//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

/// php registers the extension as `Zend OPcache`, and the crate's name is
/// no name php answers to.
#[test]
fn the_extension_is_loaded_under_php_s_name() {
    let engine = Engine::builder()
        .extensions(&[php_ext_opcache::extension()])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture(
        "<?php var_dump(extension_loaded('Zend OPcache'), extension_loaded('opcache'));",
        sink,
    );
    assert_eq!(served.stdout, b"bool(true)\nbool(false)\n");
}

/// `opcache.enable_file_override`: `file_exists`, `is_file` and
/// `is_readable` answer `true` for a file the cache holds though it was
/// deleted since, as php's `accel_file_exists` answers it from the
/// cache's hash, and the call of a file the cache does not hold reaches
/// the native; without the directive, every call does.
#[test]
fn a_cached_file_deleted_since_exists_under_the_file_override() {
    let dir = std::env::temp_dir().join(format!("opcache-override-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let probe = r"<?php
$f = __DIR__ . '/held.inc';
file_put_contents($f, '<?php return 1;');
include $f;
unlink($f);
var_dump(file_exists($f), is_file($f), is_readable($f), file_exists($f . 'x'));
";
    let served = |overriding: &str| {
        let engine = Engine::builder()
            .extensions(&[php_ext_standard::extension(), php_ext_opcache::extension()])
            .ini("opcache.enable_cli", "1")
            .ini("opcache.file_update_protection", "0")
            .ini("opcache.enable_file_override", overriding)
            .build();
        let served = engine.serve_capture_in(&dir, b"override.php", probe, Capture::default());
        String::from_utf8_lossy(&served.stdout).into_owned()
    };
    let on = served("1");
    let off = served("0");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(on, "bool(true)\nbool(true)\nbool(true)\nbool(false)\n");
    assert_eq!(off, "bool(false)\nbool(false)\nbool(false)\nbool(false)\n");
}
