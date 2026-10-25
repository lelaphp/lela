/*
 * What the binding in `src/ffi/` calls that libmbfl declares `static
 * inline` in `mbfl/mbfl_encoding.h`, `mb_convert_buf`'s helpers, given
 * a symbol each; and `zend_empty_string`, the interned empty string
 * php's engine owns and libmbfl's `mb_cut_fn`s answer.
 */
#include "mbfilter.h"

static zend_string empty_string = { { 1, GC_STRING }, 0, 0, { 0 } };
zend_string *zend_empty_string = &empty_string;

/* `mb_convert_buf_init`. */
void mbs_buf_init(mb_convert_buf *buf, size_t initsize, uint32_t repl_char, unsigned int err_mode)
{
	mb_convert_buf_init(buf, initsize, repl_char, err_mode);
}

/* `mb_convert_buf_free`. */
void mbs_buf_free(mb_convert_buf *buf)
{
	mb_convert_buf_free(buf);
}

/* `mb_convert_buf_len`. */
size_t mbs_buf_len(mb_convert_buf *buf)
{
	return mb_convert_buf_len(buf);
}

/* `ZSTR_VAL(buf->str)`: the first of `mbs_buf_len` bytes written. */
const unsigned char *mbs_buf_bytes(mb_convert_buf *buf)
{
	return (const unsigned char *)ZSTR_VAL(buf->str);
}

/* `mb_convert_buf_reset`. */
void mbs_buf_reset(mb_convert_buf *buf, size_t len)
{
	mb_convert_buf_reset(buf, len);
}

/* `MB_CONVERT_BUF_ENSURE` and `mb_convert_buf_appendn`: bytes written
 * as they are, as `mbstring.c` writes a trim marker or a MIME word. */
void mbs_buf_append(mb_convert_buf *buf, const unsigned char *bytes, size_t len)
{
	unsigned char *out, *limit;
	MB_CONVERT_BUF_LOAD(buf, out, limit);
	MB_CONVERT_BUF_ENSURE(buf, out, limit, len);
	out = mb_convert_buf_appendn(out, (const char *)bytes, len);
	MB_CONVERT_BUF_STORE(buf, out, limit);
}

/* `zend_string_free` for a string libmbfl made, and nothing for the
 * interned empty one. */
void mbs_string_free(zend_string *str)
{
	if (str != zend_empty_string) {
		efree(str);
	}
}

/* `efree` for a block libmbfl answered the binding, under the lend, or
 * outside one, it was made in. */
void mbs_free(void *ptr)
{
	efree(ptr);
}
