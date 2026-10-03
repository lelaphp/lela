// php's breakiterator over ICU's C++ (design §7.2, revision 75):
// `codepointiterator_internal.cpp`, php's own `BreakIterator` that steps
// a code point at a time, and the calls `breakiterator_methods.cpp` and
// `rulebasedbreakiterator_methods.cpp` make on an `icu::BreakIterator`,
// narrowed to what the Rust port reaches, behind `extern "C"`.

#include <unicode/brkiter.h>
#include <unicode/rbbi.h>
#include <unicode/uchriter.h>
#include <unicode/utext.h>

#include <cstring>
#include <new>
#include <typeinfo>

using icu::BreakIterator;
using icu::CharacterIterator;
using icu::Locale;
using icu::RuleBasedBreakIterator;
using icu::UCharCharacterIterator;
using icu::UnicodeString;

namespace PHP {

// `codepointiterator_internal.h`: a break at every code point, the last
// one stepped over kept for `getLastCodePoint`.
class CodePointBreakIterator : public BreakIterator {
  public:
    static UClassID getStaticClassID();
    CodePointBreakIterator();
    CodePointBreakIterator(const CodePointBreakIterator &other);
    CodePointBreakIterator &operator=(const CodePointBreakIterator &that);
    ~CodePointBreakIterator() override;
    bool operator==(const BreakIterator &that) const override;
    CodePointBreakIterator *clone() const override;
    UClassID getDynamicClassID() const override;
    CharacterIterator &getText() const override;
    UText *getUText(UText *fillIn, UErrorCode &status) const override;
    void setText(const UnicodeString &text) override;
    void setText(UText *text, UErrorCode &status) override;
    void adoptText(CharacterIterator *it) override;
    int32_t first() override;
    int32_t last() override;
    int32_t previous() override;
    int32_t next() override;
    int32_t current() const override;
    int32_t following(int32_t offset) override;
    int32_t preceding(int32_t offset) override;
    UBool isBoundary(int32_t offset) override;
    int32_t next(int32_t n) override;
    CodePointBreakIterator *createBufferClone(void *stackBuffer, int32_t &bufferSize,
                                              UErrorCode &status) override;
    CodePointBreakIterator &refreshInputText(UText *input, UErrorCode &status) override;
    UChar32 getLastCodePoint() { return this->lastCodePoint; }

  private:
    UText *fText;
    UChar32 lastCodePoint;
    mutable CharacterIterator *fCharIter;

    void clearCurrentCharIter() {
        delete this->fCharIter;
        this->fCharIter = nullptr;
        this->lastCodePoint = U_SENTINEL;
    }
};

UOBJECT_DEFINE_RTTI_IMPLEMENTATION(CodePointBreakIterator)

CodePointBreakIterator::CodePointBreakIterator()
    : BreakIterator(), lastCodePoint(U_SENTINEL), fCharIter(nullptr) {
    UErrorCode uec = UErrorCode();
    this->fText = utext_openUChars(nullptr, nullptr, 0, &uec);
}

CodePointBreakIterator::CodePointBreakIterator(const CodePointBreakIterator &other)
    : BreakIterator(other), fText(nullptr), lastCodePoint(U_SENTINEL), fCharIter(nullptr) {
    *this = other;
}

CodePointBreakIterator &CodePointBreakIterator::operator=(const CodePointBreakIterator &that) {
    UErrorCode uec = UErrorCode();
    if (this == &that) {
        return *this;
    }
    this->fText = utext_clone(this->fText, that.fText, false, true, &uec);
    // php copies no character iterator: getText() is deprecated.
    clearCurrentCharIter();
    this->lastCodePoint = that.lastCodePoint;
    return *this;
}

CodePointBreakIterator::~CodePointBreakIterator() {
    if (this->fText) {
        utext_close(this->fText);
    }
    clearCurrentCharIter();
}

bool CodePointBreakIterator::operator==(const BreakIterator &that) const {
    if (typeid(*this) != typeid(that)) {
        return false;
    }
    const CodePointBreakIterator &that2 = static_cast<const CodePointBreakIterator &>(that);
    return utext_equals(this->fText, that2.fText);
}

CodePointBreakIterator *CodePointBreakIterator::clone() const {
    return new CodePointBreakIterator(*this);
}

CharacterIterator &CodePointBreakIterator::getText() const {
    if (this->fCharIter == nullptr) {
        // deprecated; php sets up a bogus iterator
        static const UChar c = 0;
        this->fCharIter = new UCharCharacterIterator(&c, 0);
    }
    return *this->fCharIter;
}

UText *CodePointBreakIterator::getUText(UText *fillIn, UErrorCode &status) const {
    return utext_clone(fillIn, this->fText, false, true, &status);
}

void CodePointBreakIterator::setText(const UnicodeString &text) {
    UErrorCode uec = UErrorCode();
    this->fText = utext_openConstUnicodeString(this->fText, &text, &uec);
    clearCurrentCharIter();
}

void CodePointBreakIterator::setText(UText *text, UErrorCode &status) {
    if (U_FAILURE(status)) {
        return;
    }
    this->fText = utext_clone(this->fText, text, false, true, &status);
    clearCurrentCharIter();
}

void CodePointBreakIterator::adoptText(CharacterIterator *it) {
    UErrorCode uec = UErrorCode();
    clearCurrentCharIter();
    this->fCharIter = it;
    this->fText = utext_openCharacterIterator(this->fText, it, &uec);
}

int32_t CodePointBreakIterator::first() {
    UTEXT_SETNATIVEINDEX(this->fText, 0);
    this->lastCodePoint = U_SENTINEL;
    return 0;
}

int32_t CodePointBreakIterator::last() {
    int32_t pos = (int32_t)utext_nativeLength(this->fText);
    UTEXT_SETNATIVEINDEX(this->fText, pos);
    this->lastCodePoint = U_SENTINEL;
    return pos;
}

int32_t CodePointBreakIterator::previous() {
    this->lastCodePoint = UTEXT_PREVIOUS32(this->fText);
    if (this->lastCodePoint == U_SENTINEL) {
        return BreakIterator::DONE;
    }
    return (int32_t)UTEXT_GETNATIVEINDEX(this->fText);
}

int32_t CodePointBreakIterator::next() {
    this->lastCodePoint = UTEXT_NEXT32(this->fText);
    if (this->lastCodePoint == U_SENTINEL) {
        return BreakIterator::DONE;
    }
    return (int32_t)UTEXT_GETNATIVEINDEX(this->fText);
}

int32_t CodePointBreakIterator::current() const {
    return (int32_t)UTEXT_GETNATIVEINDEX(this->fText);
}

int32_t CodePointBreakIterator::following(int32_t offset) {
    this->lastCodePoint = utext_next32From(this->fText, offset);
    if (this->lastCodePoint == U_SENTINEL) {
        return BreakIterator::DONE;
    }
    return (int32_t)UTEXT_GETNATIVEINDEX(this->fText);
}

int32_t CodePointBreakIterator::preceding(int32_t offset) {
    this->lastCodePoint = utext_previous32From(this->fText, offset);
    if (this->lastCodePoint == U_SENTINEL) {
        return BreakIterator::DONE;
    }
    return (int32_t)UTEXT_GETNATIVEINDEX(this->fText);
}

UBool CodePointBreakIterator::isBoundary(int32_t offset) {
    // side effects, as it is supposed to have
    utext_setNativeIndex(this->fText, offset);
    return (offset == utext_getNativeIndex(this->fText));
}

int32_t CodePointBreakIterator::next(int32_t n) {
    UBool res = utext_moveIndex32(this->fText, n);
    if (res) {
        this->lastCodePoint = UTEXT_CURRENT32(this->fText);
        return (int32_t)UTEXT_GETNATIVEINDEX(this->fText);
    }
    this->lastCodePoint = U_SENTINEL;
    return BreakIterator::DONE;
}

CodePointBreakIterator *CodePointBreakIterator::createBufferClone(void *stackBuffer,
                                                                  int32_t &bufferSize,
                                                                  UErrorCode &status) {
    // RuleBasedBreakIterator::createBufferClone's, as php has it
    if (U_FAILURE(status)) {
        return nullptr;
    }
    const size_t align = sizeof(double) > sizeof(void *) ? sizeof(double) : sizeof(void *);
    if (bufferSize <= 0) {
        bufferSize = (int32_t)(sizeof(CodePointBreakIterator) + align);
        return nullptr;
    }
    char *buf = (char *)stackBuffer;
    uint32_t s = bufferSize;
    if (stackBuffer == nullptr) {
        s = 0;
    }
    size_t off = ((size_t)buf) & (align - 1);
    if (off != 0) {
        s -= (uint32_t)(align - off);
        buf += align - off;
    }
    if (s < sizeof(CodePointBreakIterator)) {
        CodePointBreakIterator *clonedBI = new CodePointBreakIterator(*this);
        status = U_SAFECLONE_ALLOCATED_WARNING;
        return clonedBI;
    }
    return new (buf) CodePointBreakIterator(*this);
}

CodePointBreakIterator &CodePointBreakIterator::refreshInputText(UText *input, UErrorCode &status) {
    if (U_FAILURE(status)) {
        return *this;
    }
    if (input == nullptr) {
        status = U_ILLEGAL_ARGUMENT_ERROR;
        return *this;
    }
    int64_t pos = utext_getNativeIndex(this->fText);
    this->fText = utext_clone(this->fText, input, false, true, &status);
    if (U_FAILURE(status)) {
        return *this;
    }
    utext_setNativeIndex(this->fText, pos);
    if (utext_getNativeIndex(fText) != pos) {
        status = U_ILLEGAL_ARGUMENT_ERROR;
    }
    return *this;
}

} // namespace PHP

using PHP::CodePointBreakIterator;

static RuleBasedBreakIterator *rules_of(BreakIterator *bi) {
    if (bi->getDynamicClassID() != RuleBasedBreakIterator::getStaticClassID()) {
        return nullptr;
    }
    return static_cast<RuleBasedBreakIterator *>(bi);
}

extern "C" {

// `BreakIterator::create*Instance`, the kind in `IntlBreakIterator`'s
// factory order: character, word, line, sentence, title; five is php's
// `CodePointBreakIterator`.
BreakIterator *php_intl_brk_open(int kind, const char *locale, UErrorCode *status) {
    Locale loc = Locale::createFromName(locale);
    switch (kind) {
    case 0:
        return BreakIterator::createCharacterInstance(loc, *status);
    case 1:
        return BreakIterator::createWordInstance(loc, *status);
    case 2:
        return BreakIterator::createLineInstance(loc, *status);
    case 3:
        return BreakIterator::createSentenceInstance(loc, *status);
    case 4:
        return BreakIterator::createTitleInstance(loc, *status);
    default:
        return new CodePointBreakIterator();
    }
}

// `new RuleBasedBreakIterator(rules, parseError, status)`, or over
// compiled rules where `pe` is null.
BreakIterator *php_intl_brk_open_rules(const void *rules, int32_t len, UParseError *pe,
                                       UErrorCode *status) {
    RuleBasedBreakIterator *rbbi;
    if (pe != nullptr) {
        UnicodeString text((const UChar *)rules, len);
        rbbi = new RuleBasedBreakIterator(text, *pe, *status);
    } else {
        rbbi = new RuleBasedBreakIterator((const uint8_t *)rules, (uint32_t)len, *status);
    }
    if (U_FAILURE(*status)) {
        delete rbbi;
        return nullptr;
    }
    return rbbi;
}

BreakIterator *php_intl_brk_clone(const BreakIterator *bi) { return bi->clone(); }

void php_intl_brk_close(BreakIterator *bi) { delete bi; }

// `getDynamicClassID`, as `breakiterator_object_create` picks the class:
// one for a rule-based iterator, two for php's code point one.
int php_intl_brk_kind(const BreakIterator *bi) {
    UClassID id = bi->getDynamicClassID();
    if (id == RuleBasedBreakIterator::getStaticClassID()) {
        return 1;
    }
    return id == CodePointBreakIterator::getStaticClassID() ? 2 : 0;
}

// `typeid(*biter).name()`, the debug dump's `type`.
const char *php_intl_brk_type(const BreakIterator *bi) { return typeid(*bi).name(); }

int php_intl_brk_equal(const BreakIterator *a, const BreakIterator *b) { return *a == *b; }

// `IntlBreakIterator::setText`: one when the UText did not open, two
// when the iterator refused it.
int php_intl_brk_set_text(BreakIterator *bi, const char *text, int64_t len, UErrorCode *status) {
    UText *ut = utext_openUTF8(nullptr, text, len, status);
    if (U_FAILURE(*status)) {
        return 1;
    }
    bi->setText(ut, *status);
    utext_close(ut);
    return U_FAILURE(*status) ? 2 : 0;
}

// The integer calls, by op: first, last, previous, next, current,
// following, preceding, isBoundary, next(n), getRuleStatus,
// getLastCodePoint.
int32_t php_intl_brk_step(BreakIterator *bi, int op, int32_t arg) {
    switch (op) {
    case 0:
        return bi->first();
    case 1:
        return bi->last();
    case 2:
        return bi->previous();
    case 3:
        return bi->next();
    case 4:
        return bi->current();
    case 5:
        return bi->following(arg);
    case 6:
        return bi->preceding(arg);
    case 7:
        return bi->isBoundary(arg);
    case 8:
        return bi->next(arg);
    case 9:
        return bi->getRuleStatus();
    default:
        if (bi->getDynamicClassID() == CodePointBreakIterator::getStaticClassID()) {
            return static_cast<CodePointBreakIterator *>(bi)->getLastCodePoint();
        }
        return U_SENTINEL;
    }
}

// `getLocale(type).getName()` into `dest`, its full length answered.
int32_t php_intl_brk_locale(const BreakIterator *bi, int type, char *dest, int32_t cap,
                            UErrorCode *status) {
    Locale locale = bi->getLocale((ULocDataLocaleType)type, *status);
    const char *name = locale.getName();
    int32_t len = (int32_t)strlen(name);
    if (len <= cap) {
        memcpy(dest, name, len);
    } else if (U_SUCCESS(*status)) {
        *status = U_BUFFER_OVERFLOW_ERROR;
    }
    return len;
}

// `RuleBasedBreakIterator::getRules` extracted into `dest`.
int32_t php_intl_brk_rules(BreakIterator *bi, UChar *dest, int32_t cap, UErrorCode *status) {
    RuleBasedBreakIterator *rbbi = rules_of(bi);
    if (rbbi == nullptr) {
        return 0;
    }
    return rbbi->getRules().extract(dest, cap, *status);
}

// `RuleBasedBreakIterator::getBinaryRules`.
const uint8_t *php_intl_brk_binary_rules(BreakIterator *bi, uint32_t *len) {
    RuleBasedBreakIterator *rbbi = rules_of(bi);
    *len = 0;
    return rbbi == nullptr ? nullptr : rbbi->getBinaryRules(*len);
}

int32_t php_intl_brk_rule_status_vec(BreakIterator *bi, int32_t *dest, int32_t cap,
                                     UErrorCode *status) {
    return bi->getRuleStatusVec(dest, cap, *status);
}
}
