//! The request leak sensor over what only the CLI host
//! registers: `STDIN`, `STDOUT` and `STDERR`, which `do_cli` registers
//! under `Core` as resources whose one owner is the constant table. A
//! debug binary is armed unless `PHP_SENSE_LEAKS=0` turns the sensor off,
//! so a request that keeps a block, or gives one up twice, ends in the
//! `InternalError` naming it.
#![cfg(debug_assertions)]

use std::process::{Command, Stdio};

/// stdout and stderr of `php -n -r <code>`, armed as a debug binary is
/// by default, stdin empty.
fn armed(code: &str) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_php"))
        .env_remove("PHP_SENSE_LEAKS")
        .args(["-n", "-r", code])
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("running php: {e}"));
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `ReflectionExtension::getConstants()` copies each constant's value
/// into the array it answers, `ZVAL_COPY_OR_DUP` in `_addconstant`, so
/// the array given up leaves `STDIN`'s resource the table's count, which
/// the table gives up at teardown.
#[test]
fn reflection_copies_the_host_s_constants_into_its_array() {
    assert_eq!(
        armed(
            "$c = (new ReflectionExtension('Core'))->getConstants(); var_dump($c['STDIN'] === STDIN); unset($c);"
        ),
        ("bool(true)\n".to_owned(), String::new())
    );
}
