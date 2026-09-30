//! `php-fpm`, the binary (design §4.1; ADR-0008): [`php_sapi_fpm::main`].
#![deny(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    php_sapi_fpm::main()
}
