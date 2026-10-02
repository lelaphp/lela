/*
 * The definitions behind shim/php.h and shim/php_libmagic.h: the Zend
 * string and allocator names, `spprintf`, php's diagnostics kept for the
 * binding, the shim's streams, and php's PCRE glue over libpcre2-8 as
 * ext/pcre links it. Each is the part of php libmagic reaches, written to
 * what php's own does where the bytes are observable: the pattern libmagic
 * hands `convert_libmagic_pattern` (`ext/fileinfo/php_libmagic.c`), the
 * match limits of `pcre.backtrack_limit` and `pcre.recursion_limit` at
 * their defaults, and the `Compilation failed` warning `pcre_get_compiled_regex_cache`
 * raises.
 */
#include "php_libmagic.h"

#include <errno.h>
#include <fcntl.h>

/* ---------------------------------------------------------------- strings */

zend_string *zend_string_alloc(size_t len, bool persistent)
{
	(void) persistent;
	zend_string *s = malloc(offsetof(zend_string, val) + len + 1);
	if (s == NULL) {
		abort();
	}
	s->refcount = 1;
	s->type_info = 0;
	s->h = 0;
	s->len = len;
	return s;
}

zend_string *zend_string_init(const char *str, size_t len, bool persistent)
{
	zend_string *s = zend_string_alloc(len, persistent);
	memcpy(s->val, str, len);
	s->val[len] = '\0';
	return s;
}

void zend_string_release(zend_string *s)
{
	free(s);
}

char *zend_str_tolower_dup(const char *source, size_t length)
{
	char *dup = malloc(length + 1);
	if (dup == NULL) {
		abort();
	}
	for (size_t i = 0; i < length; i++) {
		unsigned char c = (unsigned char) source[i];
		dup[i] = (c >= 'A' && c <= 'Z') ? (char) (c + ('a' - 'A')) : (char) c;
	}
	dup[length] = '\0';
	return dup;
}

const char *php_memnstr(const char *haystack, const char *needle, size_t needle_len, const char *end)
{
	if (needle_len == 0) {
		return haystack;
	}
	for (const char *p = haystack; p + needle_len <= end; p++) {
		if (*p == *needle && memcmp(p, needle, needle_len) == 0) {
			return p;
		}
	}
	return NULL;
}

void *safe_emalloc(size_t nmemb, size_t size, size_t offset)
{
	size_t total;
	if (__builtin_mul_overflow(nmemb, size, &total) || __builtin_add_overflow(total, offset, &total)) {
		abort();
	}
	return malloc(total);
}

size_t php_strlcpy(char *dst, const char *src, size_t siz)
{
	size_t len = strlen(src);
	if (siz != 0) {
		size_t n = len >= siz ? siz - 1 : len;
		memcpy(dst, src, n);
		dst[n] = '\0';
	}
	return len;
}

size_t php_strlcat(char *dst, const char *src, size_t siz)
{
	size_t dlen = strnlen(dst, siz);
	if (dlen == siz) {
		return siz + strlen(src);
	}
	return dlen + php_strlcpy(dst + dlen, src, siz - dlen);
}

size_t vspprintf(char **pbuf, size_t max_len, const char *format, va_list ap)
{
	char *buf = NULL;
	int len = vasprintf(&buf, format, ap);
	if (len < 0 || buf == NULL) {
		*pbuf = NULL;
		return 0;
	}
	if (max_len != 0 && (size_t) len > max_len) {
		len = (int) max_len;
		buf[len] = '\0';
	}
	*pbuf = buf;
	return (size_t) len;
}

size_t spprintf(char **pbuf, size_t max_len, const char *format, ...)
{
	va_list ap;
	va_start(ap, format);
	size_t len = vspprintf(pbuf, max_len, format, ap);
	va_end(ap);
	return len;
}

/* ------------------------------------------------------------ diagnostics */

/* What `php_error_docref` was asked to raise during one call into
 * libmagic, on the thread making it, oldest first, each with the
 * parameter `php_error_docref1` names, a path the stream layer could not
 * open. The binding drains it after every call, so nothing is carried
 * from one request to another. */
#define KEPT 16
struct kept_warning {
	char *param;
	char *message;
};
static _Thread_local struct kept_warning kept[KEPT];
static _Thread_local int kept_count;

static void keep(const char *param, const char *format, va_list ap)
{
	if (kept_count == KEPT) {
		return;
	}
	char *message = NULL;
	vspprintf(&message, 0, format, ap);
	if (message == NULL) {
		return;
	}
	kept[kept_count].param = param != NULL ? strdup(param) : NULL;
	kept[kept_count].message = message;
	kept_count++;
}

void php_error_docref(const char *docref, int type, const char *format, ...)
{
	(void) docref;
	(void) type;
	va_list ap;
	va_start(ap, format);
	keep(NULL, format, ap);
	va_end(ap);
}

static void php_error_docref1(const char *param, const char *format, ...)
{
	va_list ap;
	va_start(ap, format);
	keep(param, format, ap);
	va_end(ap);
}

/* The oldest kept warning, its message and its parameter copied out and
 * cut to the buffers: -1 for none, 1 where it names a parameter, 0 where
 * it does not. */
int fileinfo_shim_take_warning(char *param, size_t param_len, char *message, size_t message_len)
{
	if (kept_count == 0) {
		return -1;
	}
	struct kept_warning first = kept[0];
	kept_count--;
	memmove(kept, kept + 1, (size_t) kept_count * sizeof(kept[0]));
	php_strlcpy(message, first.message, message_len);
	free(first.message);
	if (first.param == NULL) {
		return 0;
	}
	php_strlcpy(param, first.param, param_len);
	free(first.param);
	return 1;
}

_Noreturn void zend_error_noreturn(int type, const char *format, ...)
{
	(void) type;
	(void) format;
	abort();
}

/* ---------------------------------------------------------------- streams */

struct _php_stream {
	/* A magic file opened by path, `apprentice.c`'s. */
	FILE *file;
	/* A directory of magic files. */
	DIR *dir;
	/* Bytes the binding read through the engine's stream. */
	const unsigned char *bytes;
	size_t len;
	size_t pos;
	/* The engine's stream's descriptor, -1 for none, and its stat. */
	int fd;
	zend_stat_t sb;
};

static php_stream *stream_new(void)
{
	php_stream *stream = calloc(1, sizeof(*stream));
	if (stream == NULL) {
		abort();
	}
	stream->fd = -1;
	return stream;
}

php_stream *fileinfo_shim_stream(const unsigned char *bytes, size_t len, int fd, uint32_t mode, int64_t size)
{
	php_stream *stream = stream_new();
	stream->bytes = bytes;
	stream->len = len;
	stream->fd = fd;
	stream->sb.st_mode = (mode_t) mode;
	stream->sb.st_size = (off_t) size;
	return stream;
}

php_stream *php_stream_open_wrapper(const char *path, const char *mode, int options, zend_string **opened)
{
	if (opened != NULL) {
		*opened = NULL;
	}
	FILE *file = fopen(path, mode);
	if (file == NULL) {
		if (options & REPORT_ERRORS) {
			php_error_docref1(path, "Failed to open stream: %s", strerror(errno));
		}
		return NULL;
	}
	php_stream *stream = stream_new();
	stream->file = file;
	if (fstat(fileno(file), &stream->sb) != 0) {
		memset(&stream->sb, 0, sizeof(stream->sb));
	}
	return stream;
}

php_stream *php_stream_opendir(const char *path, int options, void *context)
{
	(void) context;
	DIR *dir = opendir(path);
	if (dir == NULL) {
		if (options & REPORT_ERRORS) {
			php_error_docref1(path, "Failed to open directory: %s", strerror(errno));
		}
		return NULL;
	}
	php_stream *stream = stream_new();
	stream->dir = dir;
	return stream;
}

php_stream_dirent *php_stream_readdir(php_stream *dir, php_stream_dirent *entry)
{
	struct dirent *read = dir->dir != NULL ? readdir(dir->dir) : NULL;
	if (read == NULL) {
		return NULL;
	}
	php_strlcpy(entry->d_name, read->d_name, sizeof(entry->d_name));
	return entry;
}

int php_stream_closedir(php_stream *dir)
{
	return php_stream_close(dir);
}

int php_stream_close(php_stream *stream)
{
	if (stream == NULL) {
		return 0;
	}
	if (stream->file != NULL) {
		fclose(stream->file);
	}
	if (stream->dir != NULL) {
		closedir(stream->dir);
	}
	free(stream);
	return 0;
}

ssize_t php_stream_read(php_stream *stream, char *buf, size_t size)
{
	if (stream->file != NULL) {
		size_t n = fread(buf, 1, size, stream->file);
		return ferror(stream->file) ? -1 : (ssize_t) n;
	}
	size_t left = stream->len - stream->pos;
	size_t n = size < left ? size : left;
	memcpy(buf, stream->bytes + stream->pos, n);
	stream->pos += n;
	return (ssize_t) n;
}

ssize_t php_stream_write(php_stream *stream, const char *buf, size_t size)
{
	if (stream->file == NULL) {
		return -1;
	}
	size_t n = fwrite(buf, 1, size, stream->file);
	return ferror(stream->file) ? -1 : (ssize_t) n;
}

char *php_stream_get_line(php_stream *stream, char *buf, size_t maxlen, size_t *returned_len)
{
	if (stream->file == NULL || fgets(buf, (int) maxlen, stream->file) == NULL) {
		return NULL;
	}
	if (returned_len != NULL) {
		*returned_len = strlen(buf);
	}
	return buf;
}

int php_stream_stat(php_stream *stream, php_stream_statbuf *ssb)
{
	memcpy(&ssb->sb, &stream->sb, sizeof(ssb->sb));
	return 0;
}

int php_stream_stat_path_ex(const char *path, int flags, php_stream_statbuf *ssb, void *context)
{
	(void) flags;
	(void) context;
	return stat(path, &ssb->sb) == 0 ? SUCCESS : FAILURE;
}

int php_stream_cast(php_stream *stream, int castas, void **ret, int show_err)
{
	(void) castas;
	(void) show_err;
	int fd = stream->file != NULL ? fileno(stream->file) : stream->fd;
	if (fd < 0) {
		return FAILURE;
	}
	if (ret != NULL) {
		*(int *) ret = fd;
	}
	return SUCCESS;
}

/* ------------------------------------------------------------------- PCRE */

/* `pcre2.h`'s declarations for what the glue calls, in the 8-bit library
 * pcre2-sys builds. */
typedef struct pcre2_real_compile_context_8 pcre2_compile_context;
typedef struct pcre2_real_general_context_8 pcre2_general_context;
pcre2_code *pcre2_compile_8(PCRE2_SPTR, size_t, uint32_t, int *, size_t *, pcre2_compile_context *);
void pcre2_code_free_8(pcre2_code *);
pcre2_match_data *pcre2_match_data_create_from_pattern_8(const pcre2_code *, pcre2_general_context *);
void pcre2_match_data_free_8(pcre2_match_data *);
size_t *pcre2_get_ovector_pointer_8(pcre2_match_data *);
pcre2_match_context *pcre2_match_context_create_8(pcre2_general_context *);
int pcre2_set_match_limit_8(pcre2_match_context *, uint32_t);
int pcre2_set_depth_limit_8(pcre2_match_context *, uint32_t);
int pcre2_get_error_message_8(int, uint8_t *, size_t);

/* One compiled pattern, by the pattern string php would key its cache by. */
struct _pcre_cache_entry {
	char *key;
	size_t key_len;
	pcre2_code *re;
};

/* php's PCRE cache holds 4,096 patterns per thread (`PCRE_CACHE_SIZE`);
 * the few libmagic compiles are kept for the thread's life as php keeps
 * them for the process's. */
#define CACHED 4096
static _Thread_local pcre_cache_entry *cached[CACHED];
static _Thread_local int cached_count;
static _Thread_local pcre2_match_context *mctx;

pcre2_match_context *php_pcre_mctx(void)
{
	if (mctx == NULL) {
		mctx = pcre2_match_context_create_8(NULL);
		if (mctx != NULL) {
			/* `pcre.backtrack_limit` and `pcre.recursion_limit`. */
			pcre2_set_match_limit_8(mctx, 1000000);
			pcre2_set_depth_limit_8(mctx, 100000);
		}
	}
	return mctx;
}

/* `pcre_get_compiled_regex_cache` over the `~pattern~flags` form
 * `convert_libmagic_pattern` makes: the delimiter `~`, `i` and `m` the
 * only modifiers it writes. */
pcre_cache_entry *pcre_get_compiled_regex_cache_ex(zend_string *regex, bool locale_aware)
{
	(void) locale_aware;
	for (int i = 0; i < cached_count; i++) {
		if (cached[i]->key_len == regex->len && memcmp(cached[i]->key, regex->val, regex->len) == 0) {
			return cached[i];
		}
	}
	const char *p = regex->val;
	size_t len = regex->len;
	if (len < 2 || p[0] != '~') {
		return NULL;
	}
	/* The closing delimiter: the last `~` not escaped. */
	size_t end = 1;
	while (end < len && p[end] != '~') {
		if (p[end] == '\\' && end + 1 < len) {
			end++;
		}
		end++;
	}
	if (end >= len) {
		php_error_docref(NULL, E_WARNING, "No ending delimiter '~' found");
		return NULL;
	}
	uint32_t options = 0;
	for (size_t i = end + 1; i < len; i++) {
		switch (p[i]) {
			case 'i': options |= PCRE2_CASELESS; break;
			case 'm': options |= PCRE2_MULTILINE; break;
			default:
				php_error_docref(NULL, E_WARNING, "Unknown modifier '%c'", p[i]);
				return NULL;
		}
	}
	int error;
	size_t offset;
	pcre2_code *re = pcre2_compile_8((PCRE2_SPTR) (p + 1), end - 1, options, &error, &offset, NULL);
	if (re == NULL) {
		uint8_t text[256];
		pcre2_get_error_message_8(error, text, sizeof(text));
		php_error_docref(NULL, E_WARNING, "Compilation failed: %s at offset %zu", text, offset);
		return NULL;
	}
	pcre_cache_entry *entry = malloc(sizeof(*entry));
	if (entry == NULL) {
		abort();
	}
	entry->key = malloc(len);
	if (entry->key == NULL) {
		abort();
	}
	memcpy(entry->key, p, len);
	entry->key_len = len;
	entry->re = re;
	if (cached_count < CACHED) {
		cached[cached_count++] = entry;
	}
	return entry;
}

pcre_cache_entry *pcre_get_compiled_regex_cache(zend_string *regex)
{
	return pcre_get_compiled_regex_cache_ex(regex, true);
}

pcre2_code *php_pcre_pce_re(pcre_cache_entry *pce)
{
	return pce->re;
}

pcre2_match_data *php_pcre_create_match_data(uint32_t num_subpats, pcre2_code *re)
{
	(void) num_subpats;
	return pcre2_match_data_create_from_pattern_8(re, NULL);
}

void php_pcre_free_match_data(pcre2_match_data *match_data)
{
	pcre2_match_data_free_8(match_data);
}

/* `php_pcre_match_impl` as `softmagic.c` calls it, one match from the
 * start under `PREG_OFFSET_CAPTURE`: the count in `return_value` and,
 * for a match, `subpats[0]` as `[match, offset]`. */
void php_pcre_match_impl(pcre_cache_entry *pce, zend_string *subject_str, zval *return_value, zval *subpats, int global, zend_long flags, zend_long start_offset)
{
	(void) global;
	(void) flags;
	(void) start_offset;
	return_value->type = IS_LONG;
	return_value->value.lval = 0;
	pcre2_match_data *data = pcre2_match_data_create_from_pattern_8(pce->re, NULL);
	if (data == NULL) {
		return_value->type = IS_FALSE;
		return;
	}
	int count = pcre2_match_8(pce->re, (PCRE2_SPTR) subject_str->val, subject_str->len, 0, 0, data, php_pcre_mctx());
	if (count < -1) {
		return_value->type = IS_FALSE;
	} else if (count >= 0) {
		size_t *ovector = pcre2_get_ovector_pointer_8(data);
		HashTable *outer = calloc(1, sizeof(HashTable));
		HashTable *pair = calloc(1, sizeof(HashTable));
		if (outer == NULL || pair == NULL) {
			abort();
		}
		pair->count = 2;
		pair->items[0].type = IS_STRING;
		pair->items[0].value.str = zend_string_init(subject_str->val + ovector[0], ovector[1] - ovector[0], 0);
		pair->items[1].type = IS_LONG;
		pair->items[1].value.lval = (zend_long) ovector[0];
		outer->count = 1;
		outer->items[0].type = IS_ARRAY;
		outer->items[0].value.arr = pair;
		subpats->type = IS_ARRAY;
		subpats->value.arr = outer;
		return_value->value.lval = 1;
	}
	pcre2_match_data_free_8(data);
}

zval *zend_hash_index_find(const HashTable *ht, zend_ulong h)
{
	return h < ht->count ? (zval *) &ht->items[h] : NULL;
}

void zval_ptr_dtor(zval *zv)
{
	if (zv->type == IS_STRING) {
		zend_string_release(zv->value.str);
	} else if (zv->type == IS_ARRAY) {
		HashTable *ht = zv->value.arr;
		for (uint32_t i = 0; i < ht->count; i++) {
			zval_ptr_dtor(&ht->items[i]);
		}
		free(ht);
	}
	zv->type = IS_NULL;
}

/* `php_pcre_replace_impl` with no limit: every match replaced by the
 * replacement, which `ascmagic.c`'s literals hold no reference in. */
zend_string *php_pcre_replace_impl(pcre_cache_entry *pce, zend_string *subject_str, const char *subject, size_t subject_len, zend_string *replace_str, size_t limit, size_t *replace_count)
{
	(void) subject_str;
	(void) limit;
	pcre2_match_data *data = pcre2_match_data_create_from_pattern_8(pce->re, NULL);
	if (data == NULL) {
		return NULL;
	}
	size_t cap = subject_len + 1;
	char *out = malloc(cap);
	if (out == NULL) {
		abort();
	}
	size_t out_len = 0;
	size_t at = 0;
	size_t count = 0;
	while (at <= subject_len) {
		int rc = pcre2_match_8(pce->re, (PCRE2_SPTR) subject, subject_len, at, 0, data, php_pcre_mctx());
		if (rc < 0) {
			break;
		}
		size_t *ovector = pcre2_get_ovector_pointer_8(data);
		size_t need = out_len + (ovector[0] - at) + replace_str->len + 1;
		if (need > cap) {
			cap = need * 2;
			out = realloc(out, cap);
			if (out == NULL) {
				abort();
			}
		}
		memcpy(out + out_len, subject + at, ovector[0] - at);
		out_len += ovector[0] - at;
		memcpy(out + out_len, replace_str->val, replace_str->len);
		out_len += replace_str->len;
		count++;
		if (ovector[1] == ovector[0]) {
			if (ovector[0] < subject_len) {
				out[out_len++] = subject[ovector[0]];
			}
			at = ovector[0] + 1;
		} else {
			at = ovector[1];
		}
	}
	pcre2_match_data_free_8(data);
	if (at < subject_len) {
		size_t need = out_len + (subject_len - at) + 1;
		if (need > cap) {
			out = realloc(out, need);
			if (out == NULL) {
				abort();
			}
		}
		memcpy(out + out_len, subject + at, subject_len - at);
		out_len += subject_len - at;
	}
	zend_string *result = zend_string_init(out, out_len, 0);
	free(out);
	if (replace_count != NULL) {
		*replace_count = count;
	}
	return result;
}

/* `ext/fileinfo/php_libmagic.c`'s `convert_libmagic_pattern`: the
 * pattern between `~` delimiters, `~` and NUL escaped, `i` and `m` as
 * the options ask. */
zend_string *convert_libmagic_pattern(const char *val, size_t len, uint32_t options)
{
	size_t i, j;
	for (i = j = 0; i < len; i++) {
		switch (val[i]) {
			case '~': j += 2; break;
			case '\0': j += 4; break;
			default: j++; break;
		}
	}
	zend_string *t = zend_string_alloc(j + 4, 0);
	char *out = t->val;
	j = 0;
	out[j++] = '~';
	for (i = 0; i < len; i++, j++) {
		switch (val[i]) {
			case '~':
				out[j++] = '\\';
				out[j] = '~';
				break;
			case '\0':
				out[j++] = '\\';
				out[j++] = 'x';
				out[j++] = '0';
				out[j] = '0';
				break;
			default:
				out[j] = val[i];
				break;
		}
	}
	out[j++] = '~';
	if (options & PCRE2_CASELESS) {
		out[j++] = 'i';
	}
	if (options & PCRE2_MULTILINE) {
		out[j++] = 'm';
	}
	out[j] = '\0';
	t->len = j;
	return t;
}
