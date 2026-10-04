/// The enabled set in registration order: standard before random, which
/// a topological order would refuse and the registration order does not.
pub fn enabled() -> &'static [fn()] {
    &[
        #[cfg(feature = "standard")]
        php_ext_standard::extension,
        #[cfg(feature = "random")]
        php_ext_random::extension,
        #[cfg(feature = "session")]
        php_ext_session::extension,
    ]
}
