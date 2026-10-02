/*
 * php's `ext/fileinfo/php_libmagic.h`, which the vendored `config.h`
 * includes after `php.h`: the names it gives libmagic on a POSIX host,
 * and php's PCRE glue from `ext/pcre/php_pcre.h`, the shim's own over
 * libpcre2-8 (shim.c). `abort()` is php's fatal; the shim keeps it the
 * C library's, which ends the process as php's bailout ends the request.
 */
#ifndef FILEINFO_SHIM_PHP_LIBMAGIC_H
#define FILEINFO_SHIM_PHP_LIBMAGIC_H

#include "php.h"

#define FINFO_LSEEK_FUNC lseek
#define FINFO_READ_FUNC read

#ifndef PREG_OFFSET_CAPTURE
# define PREG_OFFSET_CAPTURE (1<<8)
#endif

/* `pcre2.h`'s names libmagic reads, for the 8-bit library. */
#define PCRE2_CASELESS 0x00000008u
#define PCRE2_MULTILINE 0x00000400u
typedef const uint8_t *PCRE2_SPTR;
typedef struct pcre2_real_code_8 pcre2_code;
typedef struct pcre2_real_match_data_8 pcre2_match_data;
typedef struct pcre2_real_match_context_8 pcre2_match_context;
int pcre2_match_8(const pcre2_code *, PCRE2_SPTR, size_t, size_t, uint32_t, pcre2_match_data *, pcre2_match_context *);
#define pcre2_match pcre2_match_8

/* `php_pcre.h`'s compiled-pattern cache and matchers, the shim's. */
typedef struct _pcre_cache_entry pcre_cache_entry;
pcre_cache_entry *pcre_get_compiled_regex_cache(zend_string *regex);
pcre_cache_entry *pcre_get_compiled_regex_cache_ex(zend_string *regex, bool locale_aware);
pcre2_code *php_pcre_pce_re(pcre_cache_entry *pce);
pcre2_match_data *php_pcre_create_match_data(uint32_t num_subpats, pcre2_code *re);
void php_pcre_free_match_data(pcre2_match_data *match_data);
pcre2_match_context *php_pcre_mctx(void);
void php_pcre_match_impl(pcre_cache_entry *pce, zend_string *subject_str, zval *return_value, zval *subpats, int global, zend_long flags, zend_long start_offset);
zend_string *php_pcre_replace_impl(pcre_cache_entry *pce, zend_string *subject_str, const char *subject, size_t subject_len, zend_string *replace_str, size_t limit, size_t *replace_count);

zend_string *convert_libmagic_pattern(const char *val, size_t len, uint32_t options);

#endif
