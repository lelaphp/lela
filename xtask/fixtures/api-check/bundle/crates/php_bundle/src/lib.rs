/// standard depends on random, so this order is not topological.
pub fn enabled() {
    php_ext_standard::extension();
    php_ext_random::extension();
}
