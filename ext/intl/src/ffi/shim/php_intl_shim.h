// What crosses the shim beside ICU's own types (src/ffi/cpp.rs): a UChar
// slice the shim reads for one call, and the UnicodeString a function
// answers, which Rust owns and gives back to php_intl_ustr_delete.

#ifndef PHP_INTL_SHIM_H
#define PHP_INTL_SHIM_H

#include <unicode/unistr.h>

struct php_intl_text {
	const UChar *ptr;
	int32_t len;
};

static inline icu::UnicodeString php_intl_string(php_intl_text text)
{
	return icu::UnicodeString(text.ptr, text.len);
}

// A string answered across the shim: a copy on the heap, or null where
// the string is bogus.
static inline icu::UnicodeString *php_intl_answer(const icu::UnicodeString &text)
{
	return text.isBogus() ? nullptr : new icu::UnicodeString(text);
}

#endif
