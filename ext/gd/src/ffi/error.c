/*
 * php_gd_error_method, the error method php sets on libgd (gd.c's
 * PHP_MINIT_FUNCTION): libgd hands it a va_list, which Rust cannot take.
 * The text is printed here and handed to Rust's side, the capture the
 * thread has running, which raises it once libgd returns.
 */
#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>

#include <gd.h>

/* Rust's side, `ffi::php_gd_error_text`. */
extern void php_gd_error_text(void *sink, int type, const char *text, size_t len);

static _Thread_local void *php_gd_sink;
static pthread_once_t php_gd_once = PTHREAD_ONCE_INIT;

static void php_gd_error_method(int type, const char *format, va_list args)
{
	char *buf = NULL;
	int len = vasprintf(&buf, format, args);
	if (len >= 0 && buf != NULL && php_gd_sink != NULL) {
		php_gd_error_text(php_gd_sink, type, buf, (size_t) len);
	}
	free(buf);
}

static void php_gd_set_method(void)
{
	gdSetErrorMethod(php_gd_error_method);
}

/* The thread's sink set, the one it replaced handed back. */
void *php_gd_capture(void *sink)
{
	void *saved = php_gd_sink;
	pthread_once(&php_gd_once, php_gd_set_method);
	php_gd_sink = sink;
	return saved;
}
