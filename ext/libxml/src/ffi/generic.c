/*
 * php_libxml_error_handler, the generic handler php sets on libxml2
 * (design §7.2, revision 32): libxml2 reports some errors straight through
 * xmlGenericError rather than its structured channel, and the handler is
 * C-variadic, which Rust cannot write. The text is printed here and handed
 * to Rust's side, which joins it to the capture running on the thread.
 */
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>

#include <libxml/xmlerror.h>
#include <libxml/globals.h>

/* Rust's side, `ffi::generic_text`. */
extern void php_libxml_generic_text(const char *text, size_t len);

static void php_libxml_generic(void *ctx, const char *msg, ...)
{
	va_list args;
	char *buf = NULL;
	(void) ctx;
	va_start(args, msg);
	int len = vasprintf(&buf, msg, args);
	va_end(args);
	if (len >= 0 && buf != NULL) {
		php_libxml_generic_text(buf, (size_t) len);
	}
	free(buf);
}

/* php's handler set on this thread, the one it replaced handed back. */
void php_libxml_generic_push(void **saved, void **saved_ctx)
{
	*saved = (void *) xmlGenericError;
	*saved_ctx = xmlGenericErrorContext;
	xmlSetGenericErrorFunc(NULL, php_libxml_generic);
}

/* The handler `php_libxml_generic_push` replaced, set again. */
void php_libxml_generic_pop(void *saved, void *saved_ctx)
{
	xmlSetGenericErrorFunc(saved_ctx, (xmlGenericErrorFunc) saved);
}
