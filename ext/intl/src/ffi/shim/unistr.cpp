// The UnicodeString a shim function answers, read and freed from Rust
// (src/ffi/cpp.rs's UStr), as php's C++ reads one with getBuffer() and
// length() before intl_charFromString.

#include "php_intl_shim.h"

using icu::UnicodeString;

extern "C" {

const UChar *php_intl_ustr_buffer(const UnicodeString *text) { return text->getBuffer(); }

int32_t php_intl_ustr_length(const UnicodeString *text) { return text->length(); }

void php_intl_ustr_delete(UnicodeString *text) { delete text; }

}
