//! `php-cgi-witness`, the witness lane's `php-cgi`: the CGI host unchanged,
//! built with `php_engine`'s `witness-trace` feature, so that a fence with
//! a `request` the lane serves through it is covered by the natives it
//! entered (harness §4.5 condition 2, §8, revision 61; #1197). A name of
//! its own keeps `target/*/php-cgi` the default build.
#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    php_sapi_cgi::main()
}
