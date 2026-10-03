// php's intl over ICU's C++ (design §7.2, revision 75): the version rows
// `PHP_MINFO_FUNCTION(intl)` and `register_php_intl_symbols` print, which
// are ICU's compile-time macros, read here through the same headers the
// library was built with.

#include <unicode/uchar.h>
#include <unicode/uvernum.h>

extern "C" {

const char *php_intl_icu_version(void) { return U_ICU_VERSION; }

const char *php_intl_icu_data_version(void) { return U_ICU_DATA_VERSION; }

const char *php_intl_unicode_version(void) { return U_UNICODE_VERSION; }

}
