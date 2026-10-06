//! The request leak sensor (#1992) over what only the CLI host
//! registers: `STDIN`, `STDOUT` and `STDERR`, which `do_cli` registers
//! under `Core` as resources whose one owner is the constant table. A
//! debug binary arms the sensor from `PHP_SENSE_LEAKS`, so a request that
//! keeps a block, or gives one up twice, ends in the `InternalError`
//! naming it.
#![cfg(debug_assertions)]

use std::process::{Command, Stdio};

/// stdout and stderr of an armed `php -n -r <code>`, stdin empty.
fn armed(code: &str) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_php"))
        .env("PHP_SENSE_LEAKS", "1")
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
/// the table gives up at teardown (#2098).
#[test]
fn reflection_copies_the_host_s_constants_into_its_array() {
    assert_eq!(
        armed(
            "$c = (new ReflectionExtension('Core'))->getConstants(); var_dump($c['STDIN'] === STDIN); unset($c);"
        ),
        ("bool(true)\n".to_owned(), String::new())
    );
}
