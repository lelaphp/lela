//! Under `ffi.enable=preload` in a host other than the CLI, a function
//! `opcache.preload`'s file declared uses FFI in every request after, as
//! php's `ZEND_ACC_PRELOADED` caller does
//! (`zend_ffi_validate_api_restriction`), and the request's own code is
//! refused. The witness lane serves a fence under the recipe's ini, which
//! preloads nothing, so it is held here, to the oracle's body under
//! `php-cgi -n -q -d opcache.enable=1 -d opcache.preload=...` (#1614).

#[test]
fn a_preloaded_function_calls_ffi() {
    let dir = std::env::temp_dir().join(format!("sapi-cgi-ffi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let preload = dir.join("preload.php");
    std::fs::write(
        &preload,
        "<?php\nfunction preloaded_abs(int $n): int {\n    return FFI::cdef(\"int abs(int j);\")->abs($n);\n}\n\
         class P { static function m() { return FFI::cdef(\"int abs(int j);\")->abs(-2); } }\n",
    )
    .unwrap();
    let main = dir.join("main.php");
    std::fs::write(
        &main,
        "<?php\nvar_dump(preloaded_abs(-7), P::m());\n\
         try { FFI::cdef(\"int abs(int j);\"); } catch (FFI\\Exception $e) { echo get_class($e), \": \", $e->getMessage(), \"\\n\"; }\n",
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_php-cgi"))
        .env_clear()
        .args(["-n", "-q", "-d", "opcache.enable=1", "-d"])
        .arg(format!("opcache.preload={}", preload.display()))
        .arg(&main)
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();
    // The whole of stdout: the preload's request sends no head, as the
    // oracle's does not (#1633).
    let stdout = String::from_utf8_lossy(&out.stdout);
    let body = "int(7)\nint(2)\nFFI\\Exception: FFI API is restricted by \"ffi.enable\" configuration directive\n";
    assert_eq!(stdout, body);
}
