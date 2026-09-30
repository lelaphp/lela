//! `php-fpm-witness`: `php-fpm` built with `witness-trace`, which the
//! witness lane serves a fence's request through (harness §8).
#![deny(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    php_sapi_fpm::main()
}
