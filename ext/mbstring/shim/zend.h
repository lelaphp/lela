/*
 * The Zend names php 8's libmbfl includes (`mbfl/mbfilter.h:88`,
 * `mbfl/mbfl_encoding.h:34`, `mbfl/mbfl_memory_device.c:34`), defined
 * over the C library so the vendored copy compiles byte for byte outside
 * php-src (third-engine-design.md §7.2). `zend_string` keeps
 * php's layout, `Zend/zend_types.h`, because libmbfl writes its header
 * fields; the allocators are php_heap's `emalloc` and kin, the request's
 * heap under a lend, as php's are `zend_mm`'s.
 * Nothing here is libmbfl's; it is the part of Zend libmbfl reads.
 */
#ifndef MBSTRING_SHIM_ZEND_H
#define MBSTRING_SHIM_ZEND_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef uint64_t zend_ulong;
typedef int64_t zend_long;

typedef struct {
	uint32_t refcount;
	uint32_t type_info;
} zend_refcounted_h;

/* `Zend/zend_types.h`'s `struct _zend_string`. */
typedef struct _zend_string {
	zend_refcounted_h gc;
	zend_ulong h;
	size_t len;
	char val[1];
} zend_string;

#define ZSTR_VAL(zstr) (zstr)->val
#define ZSTR_LEN(zstr) (zstr)->len
#define ZSTR_H(zstr) (zstr)->h
#define _ZSTR_HEADER_SIZE offsetof(zend_string, val)
#define _ZSTR_STRUCT_SIZE(len) (_ZSTR_HEADER_SIZE + (len) + 1)

#define GC_STRING 6
#define IS_STR_VALID_UTF8 (1 << 9)
#define GC_SET_REFCOUNT(p, rc) ((p)->gc.refcount = (rc))
#define GC_TYPE_INFO(p) (p)->gc.type_info
#define GC_ADD_FLAGS(p, flags) ((p)->gc.type_info |= ((flags) << 0))

/* php_heap's `emalloc` and kin (`crates/php_heap/src/lend.rs`, ADR-0014):
 * under a lend of a request's heap a block is the request's, counted as
 * php's `zend_mm` counts its own, and outside one, where the engine's
 * multibyte scanner converts with no request, the system allocator's.
 * `efree` outside a lend frees nothing, so a free asks first whether one
 * stands; the binding frees a block made under a lend under a lend too. */
void *emalloc(size_t size);
void *ecalloc(size_t nmemb, size_t size);
void *erealloc(void *ptr, size_t size);
void efree(void *ptr);
bool php_heap_lend_standing(void);
/* php 8.5.12's `_safe_ferealloc`: fallible for the one call. */
void *_safe_ferealloc(void *ptr, size_t nmemb, size_t size, size_t offset);

static inline void mbs_efree(void *ptr)
{
	if (php_heap_lend_standing()) {
		(efree)(ptr);
	} else {
		free(ptr);
	}
}
#define efree(ptr) mbs_efree(ptr)

/* `Zend/zend_alloc.h`'s `safe_ferealloc`: `_safe_ferealloc` where it is
 * fallible, `_safe_erealloc`, whose overflow no string this binding is
 * handed reaches, where it is not. */
#define safe_ferealloc(ptr, nmemb, size, offset, fallible) \
	((fallible) ? _safe_ferealloc((ptr), (nmemb), (size), (offset)) \
		: erealloc((ptr), (nmemb) * (size) + (offset)))

#define ZEND_ASSERT(c) ((void) 0)
#define ZEND_DEBUG 0
#define EMPTY_SWITCH_DEFAULT_CASE() default: break;
#define ZEND_FALLTHROUGH __attribute__((__fallthrough__))
#define ZEND_NONSTRING __attribute__((nonstring))
#define ZEND_NO_SANITIZE_ADDRESS
#define ZEND_ATTRIBUTE_UNUSED __attribute__((unused))
#define zend_always_inline inline __attribute__((always_inline))
#define EXPECTED(c) __builtin_expect(!!(c), 1)
typedef enum { SUCCESS = 0, FAILURE = -1 } zend_result;
#define UNEXPECTED(c) __builtin_expect(!!(c), 0)
#define PHP_HAVE_BUILTIN_USUB_OVERFLOW 1

#ifndef MIN
# define MIN(a, b) (((a) < (b)) ? (a) : (b))
#endif
#ifndef MAX
# define MAX(a, b) (((a) > (b)) ? (a) : (b))
#endif

/* The empty string php interns; `mb_cut_fn`s answer it, and the binding
 * never frees it. */
extern zend_string *zend_empty_string;

/* `Zend/zend_string.h`'s `zend_string_alloc`, the header as php lays it. */
static zend_always_inline zend_string *zend_string_alloc(size_t len, bool persistent)
{
	(void) persistent;
	zend_string *ret = emalloc(_ZSTR_STRUCT_SIZE(len));
	GC_SET_REFCOUNT(ret, 1);
	GC_TYPE_INFO(ret) = GC_STRING;
	ZSTR_H(ret) = 0;
	ZSTR_LEN(ret) = len;
	return ret;
}

static zend_always_inline zend_string *zend_string_init_fast(const char *str, size_t len)
{
	zend_string *ret = zend_string_alloc(len, false);
	memcpy(ZSTR_VAL(ret), str, len);
	ZSTR_VAL(ret)[len] = '\0';
	return ret;
}

/* `Zend/zend_bitset.h`'s `zend_ulong_ntz`. */
static zend_always_inline int zend_ulong_ntz(zend_ulong num)
{
	return __builtin_ctzll(num);
}

/* `Zend/zend_multiply.h`'s `zend_safe_address_guarded`: php bails out on
 * an overflow, which no string this binding is handed reaches. */
static zend_always_inline size_t zend_safe_address_guarded(size_t nmemb, size_t size, size_t offset)
{
	return nmemb * size + offset;
}

#endif /* MBSTRING_SHIM_ZEND_H */
