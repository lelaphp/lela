/*
 * The part of ext/xsl's binding to libxslt that Rust cannot write
 * (design §7.2, revision 72): the generic error handler libxslt calls
 * C-variadically, as `xsl_libxslt_error_handler` in php_xsl.c receives
 * it, and the fields of the transformation context and the stylesheet
 * php's xsltprocessor.c sets and reads, through libxslt's own headers.
 */
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <libxml/xmlmemory.h>
#include <libxslt/xsltInternals.h>
#include <libxslt/transform.h>
#include <libxslt/xsltutils.h>

/* Rust's side, `ffi::error_text`: one message, and whether libxslt
 * handed it whole as the argument of a "%s" format. */
extern void php_xsl_error_text(const char *text, size_t len, int whole);

/* `xsl_libxslt_error_handler`: the "%s" argument handed on as it is, and
 * any other format printed first, as php_libxml_error_handler_va prints. */
static void php_xsl_error(void *ctx, const char *msg, ...)
{
	va_list args;
	(void) ctx;
	va_start(args, msg);
	if (strcmp(msg, "%s") == 0) {
		const char *arg = va_arg(args, const char *);
		if (arg != NULL) {
			php_xsl_error_text(arg, strlen(arg), 1);
		}
	} else {
		char *buf = NULL;
		int len = vasprintf(&buf, msg, args);
		if (len >= 0 && buf != NULL) {
			php_xsl_error_text(buf, (size_t) len, 0);
		}
		free(buf);
	}
	va_end(args);
}

/* `xsltSetGenericErrorFunc(NULL, xsl_libxslt_error_handler)`. */
void php_xsl_set_error_handler(void)
{
	xsltSetGenericErrorFunc(NULL, php_xsl_error);
}

/* `ctxt->xinclude`, `ctxt->maxTemplateDepth` and `ctxt->maxTemplateVars`. */
void php_xsl_set_limits(xsltTransformContextPtr ctxt, int xinclude, int depth, int vars)
{
	ctxt->xinclude = xinclude;
	ctxt->maxTemplateDepth = depth;
	ctxt->maxTemplateVars = vars;
}

/* `ctxt->xpathCtxt`, the context its expressions are evaluated in. */
void *php_xsl_xpath_context(xsltTransformContextPtr ctxt)
{
	return ctxt->xpathCtxt;
}

/* `sheet->doc`. */
xmlDocPtr php_xsl_sheet_doc(xsltStylesheetPtr sheet)
{
	return sheet->doc;
}

/* `xsltApplyStylesheetUser` with the profile written to `profile`, opened
 * and closed around it as php_xsl_apply_stylesheet does. */
xmlDocPtr php_xsl_apply(xsltStylesheetPtr sheet, xmlDocPtr doc, xsltTransformContextPtr ctxt, const char *profile)
{
	FILE *f = profile != NULL ? fopen(profile, "w") : NULL;
	xmlDocPtr result = xsltApplyStylesheetUser(sheet, doc, NULL, NULL, f, ctxt);
	if (f != NULL) {
		fclose(f);
	}
	return result;
}

/* `xmlFree` of what `xsltSaveResultToString` allocated. */
void php_xsl_free(void *p)
{
	xmlFree(p);
}
