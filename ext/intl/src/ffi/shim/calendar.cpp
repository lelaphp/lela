// php's calendar/calendar_class.cpp, calendar_methods.cpp and
// gregoriancalendar_methods.cpp, the part that is C++ over icu::Calendar
// and icu::GregorianCalendar, narrowed to what src/calendar.rs calls.
// Every function takes what src/ffi/calendar.rs declares and guards its
// own fields, days and selectors before ICU sees them, so the Rust side
// may call it safely; a zone handed in is adopted, as php adopts it.

#include <cstring>

#include <unicode/calendar.h>
#include <unicode/gregocal.h>
#include <unicode/locid.h>
#include <unicode/strenum.h>
#include <unicode/uenum.h>

#include "php_intl_shim.h"

using icu::Calendar;
using icu::GregorianCalendar;
using icu::Locale;
using icu::StringEnumeration;
using icu::TimeZone;

static bool php_intl_cal_field_ok(int32_t field) { return field >= 0 && field < UCAL_FIELD_COUNT; }

static bool php_intl_cal_dow_ok(int32_t dow) { return dow >= UCAL_SUNDAY && dow <= UCAL_SATURDAY; }

static GregorianCalendar *php_intl_cal_greg(Calendar *cal)
{
	return cal->getDynamicClassID() == GregorianCalendar::getStaticClassID()
		? static_cast<GregorianCalendar *>(cal) : nullptr;
}

extern "C" {

// Calendar::createInstance(zone, locale): the zone adopted.
Calendar *php_intl_cal_create(TimeZone *zone, const char *locale, UErrorCode *status)
{
	return Calendar::createInstance(zone, Locale::createFromName(locale), *status);
}

// new GregorianCalendar(zone, locale), the zone adopted, or from 3, 5 or
// 6 of the fields in `f`, in the default zone ICU knows; null on failure.
Calendar *php_intl_cal_greg_create(TimeZone *zone, const char *locale, int32_t n, const int32_t (*f)[6],
	UErrorCode *status)
{
	GregorianCalendar *cal;
	const int32_t *a = *f;
	if (n >= 3) {
		delete zone;
	}
	switch (n) {
		case 3: cal = new GregorianCalendar(a[0], a[1], a[2], *status); break;
		case 5: cal = new GregorianCalendar(a[0], a[1], a[2], a[3], a[4], *status); break;
		case 6: cal = new GregorianCalendar(a[0], a[1], a[2], a[3], a[4], a[5], *status); break;
		default:
			if (zone == nullptr) {
				*status = U_ILLEGAL_ARGUMENT_ERROR;
				return nullptr;
			}
			cal = new GregorianCalendar(zone, Locale::createFromName(locale), *status);
			break;
	}
	if (U_FAILURE(*status)) {
		delete cal;
		return nullptr;
	}
	return cal;
}

Calendar *php_intl_cal_clone(const Calendar *cal) { return cal->clone(); }

void php_intl_cal_delete(Calendar *cal) { delete cal; }

bool php_intl_cal_is_gregorian(Calendar *cal) { return php_intl_cal_greg(cal) != nullptr; }

const char *php_intl_cal_type(const Calendar *cal) { return cal->getType(); }

// The field calls: 0 get, 1 getActualMaximum, 2 getActualMinimum, 3
// getGreatestMinimum, 4 getLeastMaximum, 5 getMaximum, 6 getMinimum, 7
// isSet, 8 clear(field), 9 set(field, value), 10 add, 11 roll, 12
// fieldDifference(when, field). A field outside UCAL_FIELD_COUNT is
// U_ILLEGAL_ARGUMENT_ERROR.
int32_t php_intl_cal_field(Calendar *cal, int32_t which, int32_t field, int32_t value, double when,
	UErrorCode *status)
{
	if (!php_intl_cal_field_ok(field)) {
		*status = U_ILLEGAL_ARGUMENT_ERROR;
		return 0;
	}
	UCalendarDateFields f = (UCalendarDateFields)field;
	switch (which) {
		case 0: return cal->get(f, *status);
		case 1: return cal->getActualMaximum(f, *status);
		case 2: return cal->getActualMinimum(f, *status);
		case 3: return cal->getGreatestMinimum(f);
		case 4: return cal->getLeastMaximum(f);
		case 5: return cal->getMaximum(f);
		case 6: return cal->getMinimum(f);
		case 7: return cal->isSet(f);
		case 8: cal->clear(f); return 0;
		case 9: cal->set(f, value); return 0;
		case 10: cal->add(f, value, *status); return 0;
		case 11: cal->roll(f, value, *status); return 0;
		default: return cal->fieldDifference((UDate)when, f, *status);
	}
}

// set(y, m, d), set(y, m, d, h, i) or set(y, m, d, h, i, s).
void php_intl_cal_set_date(Calendar *cal, int32_t n, const int32_t (*f)[6])
{
	const int32_t *a = *f;
	switch (n) {
		case 3: cal->set(a[0], a[1], a[2]); break;
		case 5: cal->set(a[0], a[1], a[2], a[3], a[4]); break;
		default: cal->set(a[0], a[1], a[2], a[3], a[4], a[5]); break;
	}
}

// The reads without a field: 0 getFirstDayOfWeek, 1
// getMinimalDaysInFirstWeek, 2 isLenient, 3 getRepeatedWallTimeOption,
// 4 getSkippedWallTimeOption, 5 inDaylightTime, 6 isWeekend(), 7
// isGregorian's isLeapYear(value), 8 clear().
int32_t php_intl_cal_int(Calendar *cal, int32_t which, int32_t value, UErrorCode *status)
{
	switch (which) {
		case 0: return cal->getFirstDayOfWeek(*status);
		case 1: return cal->getMinimalDaysInFirstWeek();
		case 2: return cal->isLenient();
		case 3: return cal->getRepeatedWallTimeOption();
		case 4: return cal->getSkippedWallTimeOption();
		case 5: return cal->inDaylightTime(*status);
		case 6: return cal->isWeekend();
		case 7: {
			GregorianCalendar *greg = php_intl_cal_greg(cal);
			return greg != nullptr && greg->isLeapYear(value);
		}
		default: cal->clear(); return 0;
	}
}

// The writes: 0 setFirstDayOfWeek, 1 setLenient, 2
// setMinimalDaysInFirstWeek, 3 setRepeatedWallTimeOption, 4
// setSkippedWallTimeOption; a value outside what php allows is
// U_ILLEGAL_ARGUMENT_ERROR.
void php_intl_cal_set_int(Calendar *cal, int32_t which, int32_t value, UErrorCode *status)
{
	bool wall = value == UCAL_WALLTIME_FIRST || value == UCAL_WALLTIME_LAST;
	switch (which) {
		case 0:
			if (!php_intl_cal_dow_ok(value)) break;
			cal->setFirstDayOfWeek((UCalendarDaysOfWeek)value);
			return;
		case 1: cal->setLenient(value != 0); return;
		case 2:
			if (value < 1 || value > 7) break;
			cal->setMinimalDaysInFirstWeek((uint8_t)value);
			return;
		case 3:
			if (!wall) break;
			cal->setRepeatedWallTimeOption((UCalendarWallTimeOption)value);
			return;
		default:
			if (!wall && value != UCAL_WALLTIME_NEXT_VALID) break;
			cal->setSkippedWallTimeOption((UCalendarWallTimeOption)value);
			return;
	}
	*status = U_ILLEGAL_ARGUMENT_ERROR;
}

// The day-of-week calls: 0 getDayOfWeekType, 1 getWeekendTransition.
int32_t php_intl_cal_dow(const Calendar *cal, int32_t which, int32_t dow, UErrorCode *status)
{
	if (!php_intl_cal_dow_ok(dow)) {
		*status = U_ILLEGAL_ARGUMENT_ERROR;
		return 0;
	}
	if (which == 0) {
		return cal->getDayOfWeekType((UCalendarDaysOfWeek)dow, *status);
	}
	return cal->getWeekendTransition((UCalendarDaysOfWeek)dow, *status);
}

// The instants: 0 getTime, 1 setTime(when), 2 isWeekend(when), 3
// getGregorianChange, 4 setGregorianChange(when); 3 and 4 answer 0 for
// a calendar that is not Gregorian.
double php_intl_cal_time(Calendar *cal, int32_t which, double when, UErrorCode *status)
{
	GregorianCalendar *greg = php_intl_cal_greg(cal);
	switch (which) {
		case 0: return (double)cal->getTime(*status);
		case 1: cal->setTime((UDate)when, *status); return 0;
		case 2: return cal->isWeekend((UDate)when, *status);
		case 3: return greg != nullptr ? (double)greg->getGregorianChange() : 0;
		default:
			if (greg != nullptr) greg->setGregorianChange((UDate)when, *status);
			return 0;
	}
}

// Two calendars: 0 equals, 1 before, 2 after, 3 isEquivalentTo.
bool php_intl_cal_compare(const Calendar *a, const Calendar *b, int32_t which, UErrorCode *status)
{
	switch (which) {
		case 0: return a->equals(*b, *status);
		case 1: return a->before(*b, *status);
		case 2: return a->after(*b, *status);
		default: return a->isEquivalentTo(*b);
	}
}

// getLocale(type) written into `out`, NUL-terminated and cut to fit.
void php_intl_cal_locale(const Calendar *cal, int32_t type, char (*out)[ULOC_FULLNAME_CAPACITY],
	UErrorCode *status)
{
	(*out)[0] = '\0';
	if (type != ULOC_ACTUAL_LOCALE && type != ULOC_VALID_LOCALE) {
		*status = U_ILLEGAL_ARGUMENT_ERROR;
		return;
	}
	Locale locale = cal->getLocale((ULocDataLocaleType)type, *status);
	if (U_SUCCESS(*status)) {
		strncpy(*out, locale.getName(), sizeof(*out) - 1);
		(*out)[sizeof(*out) - 1] = '\0';
	}
}

TimeZone *php_intl_cal_zone(const Calendar *cal) { return cal->getTimeZone().clone(); }

void php_intl_cal_adopt_zone(Calendar *cal, TimeZone *zone) { cal->adoptTimeZone(zone); }

// The name of available locale `i`, null past the last; `count` is how
// many there are.
const char *php_intl_cal_available(int32_t i, int32_t *count)
{
	const Locale *locales = Calendar::getAvailableLocales(*count);
	return i >= 0 && i < *count ? locales[i].getName() : nullptr;
}

// Calendar::getKeywordValuesForLocale as a UEnumeration, null where ICU
// answers none.
UEnumeration *php_intl_cal_keyword_values(const char *key, const char *locale, bool common, UErrorCode *status)
{
	StringEnumeration *se = Calendar::getKeywordValuesForLocale(key, Locale::createFromName(locale), common, *status);
	if (se == nullptr) {
		return nullptr;
	}
	return uenum_openFromStringEnumeration(se, status);
}

}
