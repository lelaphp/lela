/* libxml2's generic error channel, `xmlGenericErrorFunc`, is variadic, and
 * Rust defines no variadic function: this is the handler `Call::run` sets
 * for the length of one call. It formats one call's message as libxml's
 * own `xmlGenericErrorDefaultFunc` would and hands the text to
 * `dax_libxml_generic_text`, which keeps it beside the call's structured
 * errors, as php's `php_libxml_error_handler` takes it. */
#include <stdarg.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>

void dax_libxml_generic_text(void *ctx, const char *text, size_t len);

void dax_libxml_generic(void *ctx, const char *msg, ...)
{
	char stack[1024];
	va_list ap, again;
	va_start(ap, msg);
	va_copy(again, ap);
	int n = vsnprintf(stack, sizeof stack, msg, ap);
	va_end(ap);
	if (n >= 0 && (size_t) n < sizeof stack) {
		dax_libxml_generic_text(ctx, stack, (size_t) n);
	} else if (n >= 0) {
		char *heap = malloc((size_t) n + 1);
		if (heap != NULL) {
			vsnprintf(heap, (size_t) n + 1, msg, again);
			dax_libxml_generic_text(ctx, heap, (size_t) n);
			free(heap);
		}
	}
	va_end(again);
}
