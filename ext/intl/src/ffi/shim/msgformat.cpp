// php's msgformat/msgformat_helpers.cpp, the part that is C++ over
// icu::MessageFormat, narrowed to what src/msgformat.rs calls: the
// argument types the format keeps and its pattern's parts, through php's
// MessageFormatAdapter, which umsg_get_types walks in Rust; the date
// subformats umsg_set_timezone hands a zone; format() over the arguments
// Rust has typed; and parse()'s Formattables, read one at a time. A
// UMessageFormat is an icu::MessageFormat, as php casts it. Every
// function guards its own indices, so the Rust side may call it safely.

#include <unicode/datefmt.h>
#include <unicode/fmtable.h>
#include <unicode/messagepattern.h>
#include <unicode/msgfmt.h>
#include <unicode/timezone.h>
#include <unicode/umsg.h>

#include <vector>

#include "php_intl_shim.h"

U_NAMESPACE_BEGIN
// ICU declares MessageFormatAdapter a friend of MessageFormat; php uses it
// as the backdoor to the format's private members, its methods prefixed
// so they clash with none of ICU's.
class MessageFormatAdapter {
public:
	static const Formattable::Type *phpGetArgTypeList(const MessageFormat &m, int32_t &count);
	static const MessagePattern &phpGetMessagePattern(const MessageFormat &m);
};

const Formattable::Type *MessageFormatAdapter::phpGetArgTypeList(const MessageFormat &m, int32_t &count)
{
	return m.getArgTypeList(count);
}

const MessagePattern &MessageFormatAdapter::phpGetMessagePattern(const MessageFormat &m)
{
	return m.msgPattern;
}
U_NAMESPACE_END

using icu::DateFormat;
using icu::Format;
using icu::Formattable;
using icu::MessageFormat;
using icu::MessageFormatAdapter;
using icu::MessagePattern;
using icu::TimeZone;
using icu::UnicodeString;

static const MessageFormat &php_intl_msgfmt(const UMessageFormat *fmt)
{
	return *reinterpret_cast<const MessageFormat *>(fmt);
}

// One part of the pattern: MessagePattern::Part's type, argument type,
// value, and where it stands in the pattern string.
struct php_intl_msgfmt_part {
	int32_t type;
	int32_t arg_type;
	int32_t value;
	int32_t index;
	int32_t length;
};

// One argument for format(), typed as umsg_format_helper types it: a
// Formattable::Type, and the number, integer or text it carries.
struct php_intl_msgfmt_arg {
	php_intl_text name;
	int32_t type;
	double number;
	int64_t integer;
	php_intl_text text;
};

// parse()'s Formattables and their count, held until
// php_intl_msgfmt_parsed_delete.
struct php_intl_msgfmt_parsed {
	Formattable *all;
	int32_t count;
};

// One parsed Formattable as umsg_parse_helper reads it: its type, and its
// date, double, long, int64 or string, the string borrowed from it.
struct php_intl_msgfmt_value {
	int32_t type;
	double number;
	int64_t integer;
	const UChar *units;
	int32_t length;
};

extern "C" {

// MessageFormat::usesNamedArguments.
bool php_intl_msgfmt_named(const UMessageFormat *fmt) { return php_intl_msgfmt(fmt).usesNamedArguments(); }

// phpGetArgTypeList's entry at `i`, a Formattable::Type; -1 past its end.
int32_t php_intl_msgfmt_arg_type(const UMessageFormat *fmt, int32_t i)
{
	int32_t count = 0;
	const Formattable::Type *types = MessageFormatAdapter::phpGetArgTypeList(php_intl_msgfmt(fmt), count);
	return types != nullptr && i >= 0 && i < count ? types[i] : -1;
}

// MessagePattern::countParts.
int32_t php_intl_msgfmt_part_count(const UMessageFormat *fmt)
{
	return MessageFormatAdapter::phpGetMessagePattern(php_intl_msgfmt(fmt)).countParts();
}

// MessagePattern::getPart(i); false past the last part.
bool php_intl_msgfmt_part(const UMessageFormat *fmt, int32_t i, php_intl_msgfmt_part *out)
{
	const MessagePattern &mp = MessageFormatAdapter::phpGetMessagePattern(php_intl_msgfmt(fmt));
	if (i < 0 || i >= mp.countParts()) {
		return false;
	}
	const MessagePattern::Part &part = mp.getPart(i);
	*out = {part.getType(), part.getArgType(), part.getValue(), part.getIndex(), part.getLength()};
	return true;
}

// MessagePattern::getPatternString(), the string the parts index; its
// buffer lives as long as the format.
const UChar *php_intl_msgfmt_pattern(const UMessageFormat *fmt, int32_t *length)
{
	const UnicodeString &pattern = MessageFormatAdapter::phpGetMessagePattern(php_intl_msgfmt(fmt)).getPatternString();
	*length = pattern.length();
	return pattern.getBuffer();
}

// umsg_set_timezone's walk over getFormats(): the DateFormats among the
// subformats counted, each adopting a clone of `zone` where one is
// given; -1 where getFormats() answers none.
int32_t php_intl_msgfmt_date_formats(UMessageFormat *fmt, const TimeZone *zone)
{
	int32_t count = 0;
	const Format **formats = reinterpret_cast<MessageFormat *>(fmt)->getFormats(count);
	if (formats == nullptr) {
		return -1;
	}
	int32_t dates = 0;
	for (int32_t i = 0; i < count; i++) {
		DateFormat *df = dynamic_cast<DateFormat *>(const_cast<Format *>(formats[i]));
		if (df == nullptr) {
			continue;
		}
		dates++;
		if (zone != nullptr) {
			df->adoptTimeZone(zone->clone());
		}
	}
	return dates;
}

// umsg_format_helper's call: each argument a Formattable under its name,
// and format() over them; null where it fails.
UnicodeString *php_intl_msgfmt_format(const UMessageFormat *fmt, const php_intl_msgfmt_arg *args, int32_t count,
	UErrorCode *status)
{
	std::vector<Formattable> fargs(count > 0 ? count : 0);
	std::vector<UnicodeString> names(fargs.size());
	for (size_t i = 0; i < fargs.size(); i++) {
		names[i] = php_intl_string(args[i].name);
		switch (args[i].type) {
		case Formattable::kDate:
			fargs[i].setDate(args[i].number);
			break;
		case Formattable::kDouble:
			fargs[i].setDouble(args[i].number);
			break;
		case Formattable::kLong:
			fargs[i].setLong(static_cast<int32_t>(args[i].integer));
			break;
		case Formattable::kInt64:
			fargs[i].setInt64(args[i].integer);
			break;
		default:
			fargs[i].setString(php_intl_string(args[i].text));
			break;
		}
	}
	UnicodeString result;
	php_intl_msgfmt(fmt).format(names.empty() ? nullptr : names.data(), fargs.empty() ? nullptr : fargs.data(),
		static_cast<int32_t>(fargs.size()), result, *status);
	return U_FAILURE(*status) ? nullptr : php_intl_answer(result);
}

// MessageFormat::parse(source, count, status); null where it fails.
php_intl_msgfmt_parsed *php_intl_msgfmt_parse(const UMessageFormat *fmt, php_intl_text source, UErrorCode *status)
{
	int32_t count = 0;
	Formattable *all = php_intl_msgfmt(fmt).parse(php_intl_string(source), count, *status);
	if (U_FAILURE(*status) || all == nullptr) {
		delete[] all;
		return nullptr;
	}
	return new php_intl_msgfmt_parsed{all, count};
}

int32_t php_intl_msgfmt_parsed_count(const php_intl_msgfmt_parsed *parsed) { return parsed->count; }

// The Formattable at `i`; false past the end.
bool php_intl_msgfmt_parsed_at(const php_intl_msgfmt_parsed *parsed, int32_t i, php_intl_msgfmt_value *out)
{
	if (i < 0 || i >= parsed->count) {
		return false;
	}
	const Formattable &f = parsed->all[i];
	*out = {f.getType(), 0.0, 0, nullptr, 0};
	switch (f.getType()) {
	case Formattable::kDate:
		out->number = f.getDate();
		break;
	case Formattable::kDouble:
		out->number = f.getDouble();
		break;
	case Formattable::kLong:
		out->integer = f.getLong();
		break;
	case Formattable::kInt64:
		out->integer = f.getInt64();
		break;
	case Formattable::kString:
		out->units = f.getString().getBuffer();
		out->length = f.getString().length();
		break;
	default:
		break;
	}
	return true;
}

void php_intl_msgfmt_parsed_delete(php_intl_msgfmt_parsed *parsed)
{
	delete[] parsed->all;
	delete parsed;
}

}
