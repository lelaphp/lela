// php's dateformat/dateformat_create.cpp, dateformat_helpers.cpp,
// dateformat_attrcpp.cpp and dateformat_format_object.cpp, the part that
// is C++ over icu::DateFormat, narrowed to what src/dateformat.rs calls:
// the calendar a formatter is made with, what it adopts and hands back as
// clones, and formatObject's one-off formatter; and
// datepatterngenerator_methods.cpp's DateTimePatternGenerator, for
// src/datepatterngenerator.rs. A UDateFormat is an
// icu::DateFormat, as php casts it; a calendar or a zone handed in is
// adopted, as php adopts it.

#include <unicode/calendar.h>
#include <unicode/datefmt.h>
#include <unicode/dtptngen.h>
#include <unicode/gregocal.h>
#include <unicode/locid.h>
#include <unicode/smpdtfmt.h>
#include <unicode/udat.h>

#include "php_intl_shim.h"

using icu::Calendar;
using icu::DateFormat;
using icu::DateTimePatternGenerator;
using icu::GregorianCalendar;
using icu::Locale;
using icu::SimpleDateFormat;
using icu::StringPiece;
using icu::TimeZone;
using icu::UnicodeString;

static DateFormat *php_intl_datefmt(UDateFormat *df) { return reinterpret_cast<DateFormat *>(df); }

static const DateFormat *php_intl_datefmt_const(const UDateFormat *df)
{
	return reinterpret_cast<const DateFormat *>(df);
}

extern "C" {

// datefmt_process_calendar_arg's calendars: the locale's own
// (UCAL_TRADITIONAL, 0) or a GregorianCalendar; null on failure.
Calendar *php_intl_datefmt_calendar_new(int32_t traditional, const char *locale, UErrorCode *status)
{
	Locale loc = Locale::createFromName(locale);
	Calendar *cal = traditional ? Calendar::createInstance(loc, *status) : new GregorianCalendar(loc, *status);
	if (U_FAILURE(*status)) {
		delete cal;
		return nullptr;
	}
	return cal;
}

void php_intl_datefmt_adopt_calendar(UDateFormat *df, Calendar *cal) { php_intl_datefmt(df)->adoptCalendar(cal); }

void php_intl_datefmt_adopt_zone(UDateFormat *df, TimeZone *zone) { php_intl_datefmt(df)->adoptTimeZone(zone); }

TimeZone *php_intl_datefmt_zone(const UDateFormat *df) { return php_intl_datefmt_const(df)->getTimeZone().clone(); }

Calendar *php_intl_datefmt_calendar(const UDateFormat *df)
{
	const Calendar *cal = php_intl_datefmt_const(df)->getCalendar();
	return cal == nullptr ? nullptr : cal->clone();
}

// datefmt_format_object past its arguments: `cal` adopted, a
// GregorianCalendar of the locale where it is null; the pattern's
// SimpleDateFormat where one is given, the styles' DateFormat otherwise;
// `zone` adopted after the calendar, and `date` formatted. Where a step
// fails, `stage` names it (1 the calendar, 2 the SimpleDateFormat, 3 the
// DateFormat) and null is answered.
UnicodeString *php_intl_datefmt_format_object(Calendar *cal, double date, TimeZone *zone, const char *pattern,
	int32_t date_style, int32_t time_style, const char *locale, int32_t *stage, UErrorCode *status)
{
	Locale loc = Locale::createFromName(locale);
	if (cal == nullptr) {
		cal = new GregorianCalendar(loc, *status);
		if (U_FAILURE(*status)) {
			delete cal;
			delete zone;
			*stage = 1;
			return nullptr;
		}
	}
	DateFormat *df;
	if (pattern != nullptr) {
		df = new SimpleDateFormat(UnicodeString::fromUTF8(StringPiece(pattern)), loc, *status);
		if (U_FAILURE(*status)) {
			delete df;
			*stage = 2;
		}
	} else {
		df = DateFormat::createDateTimeInstance((DateFormat::EStyle)date_style, (DateFormat::EStyle)time_style, loc);
		if (df == nullptr) {
			*stage = 3;
		}
	}
	if (*stage != 0) {
		delete cal;
		delete zone;
		return nullptr;
	}
	df->adoptCalendar(cal);
	df->adoptTimeZone(zone);
	UnicodeString answer;
	df->format(date, answer);
	delete df;
	return php_intl_answer(answer);
}

// DateTimePatternGenerator::createInstance(locale); null on failure.
DateTimePatternGenerator *php_intl_dtpg_create(const char *locale, UErrorCode *status)
{
	DateTimePatternGenerator *dtpg = DateTimePatternGenerator::createInstance(Locale::createFromName(locale), *status);
	if (U_FAILURE(*status)) {
		delete dtpg;
		return nullptr;
	}
	return dtpg;
}

DateTimePatternGenerator *php_intl_dtpg_clone(const DateTimePatternGenerator *dtpg) { return dtpg->clone(); }

void php_intl_dtpg_delete(DateTimePatternGenerator *dtpg) { delete dtpg; }

// getBestPattern over the skeleton getSkeleton cleans; where a step
// fails, `stage` names it (1 the skeleton, 2 the pattern) and null is
// answered.
UnicodeString *php_intl_dtpg_best_pattern(DateTimePatternGenerator *dtpg, php_intl_text text, int32_t *stage,
	UErrorCode *status)
{
	UnicodeString skeleton = dtpg->getSkeleton(php_intl_string(text), *status);
	if (U_FAILURE(*status)) {
		*stage = 1;
		return nullptr;
	}
	UnicodeString answer = dtpg->getBestPattern(skeleton, *status);
	if (U_FAILURE(*status)) {
		*stage = 2;
		return nullptr;
	}
	return php_intl_answer(answer);
}

}
