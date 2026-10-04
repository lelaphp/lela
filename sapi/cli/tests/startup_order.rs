//! What extensions supply is installed in the order php installs it:
//! every module's at its `MINIT`, then opcache's, a Zend extension's,
//! after them (`Z/main/main.c:2353-2356`). Held on the binary, where
//! the bundle's own module order stands: ffi's `zend_post_startup_cb`
//! link is beneath opcache's and is asked first, so a failing
//! `ffi.preload` refuses startup before opcache preloads, as the oracle
//! does (#1797).

use std::process::{Command, Stdio};

#[test]
fn a_failing_ffi_preload_refuses_startup_before_opcache_preloads() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("startup-order");
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    let preload = dir.join("preload.php");
    let script = dir.join("served.php");
    let missing = dir.join("missing.h");
    for (path, source) in [
        (&preload, "<?php echo \"preloaded\\n\";"),
        (&script, "<?php echo \"served\\n\";"),
    ] {
        std::fs::write(path, source).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
    let out = Command::new(env!("CARGO_BIN_EXE_php"))
        .arg("-n")
        .args(["-d", "opcache.enable_cli=1"])
        .arg("-d")
        .arg(format!("opcache.preload={}", preload.display()))
        .args(["-d", "ffi.enable=preload"])
        .arg("-d")
        .arg(format!("ffi.preload={}", missing.display()))
        .arg(&script)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("running php: {e}"));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "\nWarning: FFI: failed pre-loading '{}', file doesn't exist in Unknown on line 0\n\n\
             Fatal error: Unable to start FFI module in Unknown on line 0\nStack trace:\n#0 {{main}}\n",
            missing.display()
        )
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(out.status.code(), Some(254));
}
