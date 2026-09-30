//! `php`, the binary (design §4.1, §4.10; ADR-0008): php's CLI host,
//! [`php_sapi_cli::main`].
#![deny(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    php_sapi_cli::main()
}
