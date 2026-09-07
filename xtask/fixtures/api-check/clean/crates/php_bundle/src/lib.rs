/// The enabled set in module-startup order: random before standard.
pub fn enabled() {
    php_ext_random::extension();
    php_ext_standard::extension();
}
