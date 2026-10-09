/*
 * timelib's allocator pointed at the request's heap, as php's
 * ext/date/config0.m4 writes this header (ADR-0014, design §5.14).
 * php's includes "zend.h" for the five functions and estrndup; here
 * they are declared, and php_heap defines them: under a lend they
 * allocate on the running request's heap, outside one on the system
 * allocator. The symbols resolve at link time; timelib_sys depends on
 * no crate.
 */
#include <inttypes.h>
#include <stdint.h>
#include <stddef.h>

void *emalloc(size_t size);
void *ecalloc(size_t count, size_t size);
void *erealloc(void *ptr, size_t size);
char *estrdup(const char *s);
char *estrndup(const char *s, size_t length);
void efree(void *ptr);

#define timelib_malloc  emalloc
#define timelib_realloc erealloc
#define timelib_calloc  ecalloc
#define timelib_strdup  estrdup
#define timelib_strndup estrndup
#define timelib_free    efree
