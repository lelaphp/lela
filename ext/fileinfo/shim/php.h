/*
 * The Zend and php names php's patched libmagic includes through its
 * `config.h` (`libmagic/config.h:1`), defined over the C library and
 * libpcre2-8 so the vendored copy compiles byte for byte outside php-src
 * (third-engine-design.md §7.2), as ext/mbstring's shim does for libmbfl.
 * The allocators are `malloc` and kin where php's are `emalloc` and kin.
 * A `php_stream` here is the shim's own: a magic file opened by path for
 * `apprentice.c`, or the bytes the binding read through the engine's
 * streams, with the stat and the descriptor the engine's stream had.
 * What php raises through `php_error_docref` is kept, in order, for the
 * binding to raise once libmagic returns. Nothing here is libmagic's.
 */
#ifndef FILEINFO_SHIM_PHP_H
#define FILEINFO_SHIM_PHP_H

#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <sys/param.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>
#include <dirent.h>

/* What php's `configure` finds on the hosts the engine builds for, and
 * `ext/fileinfo/config.m4` asks of them. */
#define HAVE_STDINT_H 1
#define HAVE_INTTYPES_H 1
#define HAVE_UNISTD_H 1
#define HAVE_DIRENT_H 1
#define HAVE_SYS_TIME_H 1
#define HAVE_SYS_WAIT_H 1
#define HAVE_SYS_MMAN_H 1
#define HAVE_SYS_IOCTL_H 1
#define HAVE_UTIME_H 1
#define HAVE_UTIME 1
#define HAVE_UTIMES 1
#define HAVE_STRNDUP 1
#define HAVE_STRCASESTR 1
#define HAVE_STRTOF 1
#define HAVE_MKSTEMP 1
#define HAVE_MMAP 1
#define HAVE_WCHAR_H 1
#define HAVE_WCTYPE_H 1
#define HAVE_MBRTOWC 1
#define HAVE_WCWIDTH 1
#define HAVE_FORK 1
#define HAVE_STRUCT_TM_TM_GMTOFF 1
#define HAVE_STRUCT_TM_TM_ZONE 1
#define HAVE_ASCTIME_R 1
#define HAVE_CTIME_R 1
#define HAVE_FUNC_ATTRIBUTE_VISIBILITY 1

/* `main/php.h` gives libmagic php's own `strlcpy` and `strlcat`, which
 * not every C library has. */
#define HAVE_STRLCPY 1
#define HAVE_STRLCAT 1
#undef strlcpy
#undef strlcat
#define strlcpy php_strlcpy
#define strlcat php_strlcat
size_t php_strlcpy(char *dst, const char *src, size_t siz);
size_t php_strlcat(char *dst, const char *src, size_t siz);

typedef int64_t zend_long;
typedef uint64_t zend_ulong;
typedef off_t zend_off_t;
typedef struct stat zend_stat_t;
typedef int php_socket_t;
typedef int zend_result;

#define SUCCESS 0
#define FAILURE -1
#define ZEND_LONG_FMT "%" PRId64
#define ZEND_NONSTRING __attribute__((nonstring))
#define EXPECTED(c) __builtin_expect(!!(c), 1)
#define UNEXPECTED(c) __builtin_expect(!!(c), 0)

#define E_ERROR 1
#define E_WARNING 2
#define E_NOTICE 8
#define E_DEPRECATED 8192

#define REPORT_ERRORS 8
#define PHP_STREAM_AS_FD 1

/* `Zend/zend_types.h`'s `zend_string`, the fields libmagic reads. */
typedef struct _zend_string {
	uint32_t refcount;
	uint32_t type_info;
	zend_ulong h;
	size_t len;
	char val[1];
} zend_string;

#define ZSTR_VAL(s) (s)->val
#define ZSTR_LEN(s) (s)->len
zend_string *zend_string_alloc(size_t len, bool persistent);
zend_string *zend_string_init(const char *str, size_t len, bool persistent);
void zend_string_release(zend_string *s);
#define zend_string_release_ex(s, persistent) zend_string_release(s)
#define ZSTR_INIT_LITERAL(s, persistent) zend_string_init((s), sizeof(s) - 1, (persistent))
char *zend_str_tolower_dup(const char *source, size_t length);
const char *php_memnstr(const char *haystack, const char *needle, size_t needle_len, const char *end);

#define emalloc(size) malloc(size)
#define ecalloc(n, size) calloc((n), (size))
#define erealloc(p, size) realloc((p), (size))
#define efree(p) free(p)
#define estrdup(s) strdup(s)
#define estrndup(s, n) strndup((s), (n))
#define pemalloc(size, persistent) malloc(size)
#define pefree(p, persistent) free(p)
void *safe_emalloc(size_t nmemb, size_t size, size_t offset);

/* php 8.5.12's `cdf.c` asks for its tables with `femalloc`,
 * `safe_femalloc` and `safe_ferealloc`, each fallible. They are php_heap's
 * `_safe_ferealloc`, the lend's fallible entry, which the binding makes
 * every libmagic call that reaches `cdf.c` under: the tables are the
 * request's heap's, and one past the memory limit is refused with NULL,
 * the delayed OOM recorded. Every site passes `fallible` true. Outside a
 * lend it is the system's `realloc`. */
void *_safe_ferealloc(void *ptr, size_t nmemb, size_t size, size_t offset);
#define femalloc(size, fallible) _safe_ferealloc(NULL, 1, (size), 0)
#define safe_femalloc(nmemb, size, offset, fallible) \
	_safe_ferealloc(NULL, (nmemb), (size), (offset))
#define safe_ferealloc(ptr, nmemb, size, offset, fallible) \
	_safe_ferealloc((ptr), (nmemb), (size), (offset))

/* `cdf.c` and `readcdf.c`, which free those tables, are compiled with
 * `FILEINFO_SHIM_LENT_FREE`: their `efree` is php_heap's, which under the
 * lend gives a block of the heap back to it and any other to the system
 * allocator. */
#ifdef FILEINFO_SHIM_LENT_FREE
#undef efree
void efree(void *ptr);
#endif

size_t spprintf(char **pbuf, size_t max_len, const char *format, ...);
size_t vspprintf(char **pbuf, size_t max_len, const char *format, va_list ap);

/* `php_error_docref`: the message kept for the binding to raise. */
void php_error_docref(const char *docref, int type, const char *format, ...);
_Noreturn void zend_error_noreturn(int type, const char *format, ...);

#define php_localtime_r localtime_r
#define php_gmtime_r gmtime_r
#define php_asctime_r asctime_r
#define php_ctime_r ctime_r

/* A minimal zval and array: what `php_pcre_match_impl` hands
 * `softmagic.c` back, `[[match, offset]]` under `PREG_OFFSET_CAPTURE`. */
#define IS_NULL 1
#define IS_FALSE 2
#define IS_LONG 4
#define IS_STRING 6
#define IS_ARRAY 7
struct _zend_array;
typedef struct _zval_struct {
	union {
		zend_long lval;
		zend_string *str;
		struct _zend_array *arr;
	} value;
	int type;
} zval;
typedef struct _zend_array {
	uint32_t count;
	zval items[2];
} HashTable;
#define Z_TYPE(zv) (zv).type
#define Z_TYPE_P(zv) (zv)->type
#define Z_LVAL(zv) (zv).value.lval
#define Z_LVAL_P(zv) (zv)->value.lval
#define Z_STRLEN_P(zv) (zv)->value.str->len
#define Z_ARRVAL(zv) (zv).value.arr
#define Z_ARRVAL_P(zv) (zv)->value.arr
#define ZVAL_NULL(zv) ((zv)->type = IS_NULL)
zval *zend_hash_index_find(const HashTable *ht, zend_ulong h);
void zval_ptr_dtor(zval *zv);

/* The shim's stream. */
typedef struct _php_stream php_stream;
typedef struct {
	zend_stat_t sb;
} php_stream_statbuf;
typedef struct {
	char d_name[MAXPATHLEN];
} php_stream_dirent;

php_stream *php_stream_open_wrapper(const char *path, const char *mode, int options, zend_string **opened);
php_stream *php_stream_opendir(const char *path, int options, void *context);
php_stream_dirent *php_stream_readdir(php_stream *dir, php_stream_dirent *entry);
int php_stream_closedir(php_stream *dir);
int php_stream_close(php_stream *stream);
ssize_t php_stream_read(php_stream *stream, char *buf, size_t size);
ssize_t php_stream_write(php_stream *stream, const char *buf, size_t size);
char *php_stream_get_line(php_stream *stream, char *buf, size_t maxlen, size_t *returned_len);
int php_stream_stat(php_stream *stream, php_stream_statbuf *ssb);
int php_stream_stat_path_ex(const char *path, int flags, php_stream_statbuf *ssb, void *context);
int php_stream_cast(php_stream *stream, int castas, void **ret, int show_err);
#define php_sys_stat stat
#define VCWD_ACCESS access
#define zend_stat stat
#define zend_fstat fstat

/* The binding's half: a stream over bytes read elsewhere, and the
 * diagnostics kept since the last drain. */
php_stream *fileinfo_shim_stream(const unsigned char *bytes, size_t len, int fd, uint32_t mode, int64_t size);
int fileinfo_shim_take_warning(char *param, size_t param_len, char *message, size_t message_len);

#endif
