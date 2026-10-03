// php's timezone/timezone_class.cpp and timezone_methods.cpp, the part
// that is C++ over icu::TimeZone, narrowed to what src/timezone.rs calls:
// the zone made, cloned, compared and read, and TimeZone's static
// lookups. Every function takes what src/ffi/timezone.rs declares and
// guards its own selectors, so the Rust side may call it safely.

#include <unicode/timezone.h>
#include <unicode/calendar.h>
#include <unicode/locid.h>
#include <unicode/strenum.h>
#include <unicode/uenum.h>

#include "php_intl_shim.h"

using icu::Calendar;
using icu::Locale;
using icu::StringEnumeration;
using icu::TimeZone;
using icu::UnicodeString;

// TimeZone::EDisplayType, php's display_types[].
static bool php_intl_tz_display_type(int32_t type)
{
	return type >= TimeZone::SHORT && type <= TimeZone::GENERIC_LOCATION;
}

extern "C" {

// TimeZone::createTimeZone: GMT's clone where the id is not understood,
// which php_intl_tz_is_unknown tells apart.
TimeZone *php_intl_tz_create(php_intl_text id)
{
	return TimeZone::createTimeZone(php_intl_string(id));
}

// createTimeZone(id) from an id php wrote in ASCII (common_date.cpp's
// UnicodeString(id, id_len, US_INV)).
TimeZone *php_intl_tz_create_invariant(const char *id)
{
	return TimeZone::createTimeZone(UnicodeString(id, -1, US_INV));
}

// 0 createDefault(), 1 getGMT(), 2 getUnknown(), each a zone of the
// caller's own.
TimeZone *php_intl_tz_special(int32_t which)
{
	switch (which) {
		case 0: return TimeZone::createDefault();
		case 1: return TimeZone::getGMT()->clone();
		default: return TimeZone::getUnknown().clone();
	}
}

bool php_intl_tz_is_unknown(const TimeZone *tz) { return *tz == TimeZone::getUnknown(); }

TimeZone *php_intl_tz_clone(const TimeZone *tz) { return tz->clone(); }

void php_intl_tz_delete(TimeZone *tz) { delete tz; }

bool php_intl_tz_equals(const TimeZone *a, const TimeZone *b) { return *a == *b; }

bool php_intl_tz_has_same_rules(const TimeZone *a, const TimeZone *b) { return a->hasSameRules(*b); }

UnicodeString *php_intl_tz_id(const TimeZone *tz)
{
	UnicodeString id;
	tz->getID(id);
	return php_intl_answer(id);
}

// 0 getRawOffset(), 1 getDSTSavings(), 2 useDaylightTime().
int32_t php_intl_tz_int(const TimeZone *tz, int32_t which)
{
	switch (which) {
		case 0: return tz->getRawOffset();
		case 1: return tz->getDSTSavings();
		default: return tz->useDaylightTime();
	}
}

void php_intl_tz_offset(const TimeZone *tz, double date, bool local, int32_t *raw, int32_t *dst,
	UErrorCode *status)
{
	tz->getOffset((UDate)date, (UBool)local, *raw, *dst, *status);
}

// getDisplayName, or null for a type outside display_types[].
UnicodeString *php_intl_tz_display_name(const TimeZone *tz, bool dst, int32_t type, const char *locale)
{
	if (!php_intl_tz_display_type(type)) {
		return nullptr;
	}
	UnicodeString name;
	tz->getDisplayName((UBool)dst, (TimeZone::EDisplayType)type, Locale::createFromName(locale), name);
	return new UnicodeString(name);
}

double php_intl_now(void) { return (double)Calendar::getNow(); }

// TimeZone's static lookups by id: 0 getCanonicalID (flag: isSystemID),
// 1 getEquivalentID(n), 2 getIanaID, 3 getWindowsID, 4
// getIDForWindowsID(region).
UnicodeString *php_intl_tz_lookup(int32_t which, php_intl_text text, int32_t n, const char *region,
	bool *flag, UErrorCode *status)
{
	UnicodeString id = php_intl_string(text), out;
	UBool system = false;
	switch (which) {
		case 0: TimeZone::getCanonicalID(id, out, system, *status); *flag = system; break;
		case 1: out = TimeZone::getEquivalentID(id, n); break;
		case 2: TimeZone::getIanaID(id, out, *status); break;
		case 3: TimeZone::getWindowsID(id, out, *status); break;
		default: TimeZone::getIDForWindowsID(id, region, out, *status); break;
	}
	return new UnicodeString(out);
}

int32_t php_intl_tz_count_equivalent(php_intl_text id)
{
	return TimeZone::countEquivalentIDs(php_intl_string(id));
}

int32_t php_intl_tz_region(php_intl_text id, char (*out)[3], UErrorCode *status)
{
	return TimeZone::getRegion(php_intl_string(id), *out, sizeof(*out), *status);
}

// The zones as a UEnumeration: 0 createEnumeration(), 1 of a country, 2
// at a raw offset, 3 createTimeZoneIDEnumeration(type, region, offset);
// null where ICU answers none.
UEnumeration *php_intl_tz_enumeration(int32_t which, const char *name, int32_t offset,
	const int32_t *raw, UErrorCode *status)
{
	StringEnumeration *se;
	switch (which) {
		case 0: se = TimeZone::createEnumeration(); break;
		case 1: se = TimeZone::createEnumeration(name); break;
		case 2: se = TimeZone::createEnumeration(offset); break;
		default:
			if (offset < UCAL_ZONE_TYPE_ANY || offset > UCAL_ZONE_TYPE_CANONICAL_LOCATION) {
				*status = U_ILLEGAL_ARGUMENT_ERROR;
				return nullptr;
			}
			se = TimeZone::createTimeZoneIDEnumeration((USystemTimeZoneType)offset, name, raw, *status);
			break;
	}
	if (se == nullptr || U_FAILURE(*status)) {
		delete se;
		return nullptr;
	}
	return uenum_openFromStringEnumeration(se, status);
}

}

extern "C" const char *php_intl_tz_data_version(UErrorCode *status)
{
	return TimeZone::getTZDataVersion(*status);
}
