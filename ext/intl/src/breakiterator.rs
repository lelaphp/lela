//! `breakiterator/`: `IntlBreakIterator` over an `icu::BreakIterator`,
//! its rule-based and code point subclasses, the `foreach` over its
//! boundaries (`_breakiterator_get_iterator`) and `IntlPartsIterator`,
//! the `IntlIterator` over the text between them
//! (`breakiterator_iterators.cpp`). The C++ php wrote over ICU's classes
//! is [`crate::ffi::breakiterator`]'s shim.

use std::cmp::Ordering;

use php_ext::abi::{
    ClassIterator, CloneObj, DebugInfo, Instance, ObjectHandlers, OrFalse, Raw, Std, This,
};
use php_ext::attr::php_class;
use php_ext::call::{call_method, clone_with, internal_iterator, lookup_class, new_object};
use php_ext::coerce::coerce_param;
use php_ext::decl::Type;
use php_ext::raising::{Thrown, arg_value_error, fatal, throw, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value};

use crate::error::{self, Carrier, IntlError};
use crate::ffi::breakiterator::{Breaker, Step};
use crate::ffi::{Code, ParseError, c_string, to_utf8, to_utf16};
use crate::iterator::{self, IntlIterator, Walk};

/// `BreakIterator::DONE`.
const DONE: i32 = -1;

/// `BreakIterator_object`: the error and the iterator, which holds the
/// text it walks and the compiled rules it was built over.
#[derive(Default)]
pub(crate) struct BreakObject {
    error: IntlError,
    biter: Option<Breaker>,
}

impl Carrier for BreakObject {
    fn error(&mut self) -> &mut IntlError {
        &mut self.error
    }
}

/// Core's `IteratorAggregate`, named for `implements`.
#[php_class(extern)]
pub(crate) struct IteratorAggregate;

/// `BreakIterator_handlers`: equality by ICU's `operator==`, the clone
/// a clone of the ICU iterator, the debug dump, and the `foreach`.
pub(crate) static HANDLERS: Handlers = Handlers;

/// The handlers of `IntlBreakIterator` and its subclasses.
pub(crate) struct Handlers;

/// `class IntlBreakIterator implements IteratorAggregate`: made by its
/// factories, never by `new`; a `foreach` walks the boundaries.
///
/// ```php
/// try { new IntlBreakIterator; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { (new ReflectionMethod('IntlBreakIterator', '__construct'))->invoke(IntlBreakIterator::createWordInstance()); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// $bi = IntlBreakIterator::createWordInstance('en');
/// var_dump(get_class($bi), $bi->getText(), $bi->setText('Hi there!'), $bi->getText());
/// foreach ($bi as $k => $b) echo "$k:$b,";
/// echo "\n", implode(' ', [$bi->first(), $bi->next(), $bi->next(2), $bi->current(), $bi->last(), $bi->previous()]), "\n";
/// var_dump($bi->following(1), $bi->preceding(4), $bi->isBoundary(3), $bi->isBoundary(4), $bi->getLocale(ULOC_VALID_LOCALE));
/// var_dump($bi->getLocale(5), intl_get_error_message(), $bi->getErrorCode(), $bi->getErrorMessage());
/// try { $bi->following(PHP_INT_MAX); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// echo IntlBreakIterator::DONE, ' ', IntlBreakIterator::WORD_IDEO_LIMIT, ' ', IntlBreakIterator::SENTENCE_SEP, "\n";
/// $c = clone $bi; var_dump($c == $bi, $c->getText());
/// var_dump(IntlBreakIterator::createWordInstance('en') == IntlBreakIterator::createLineInstance('en'));
/// foreach (['createCodePointInstance', 'createTitleInstance', 'createCharacterInstance', 'createSentenceInstance', 'createLineInstance'] as $f) echo get_class(IntlBreakIterator::$f()), "\n";
/// var_dump(substr_count(print_r(IntlBreakIterator::createCharacterInstance(), true), "=>"));
/// var_dump(get_class($bi->getIterator()), get_class($bi->getPartsIterator()));
/// ```
/// ```expect
/// Call to private IntlBreakIterator::__construct() from global scope
/// An object of this type cannot be created with the new operator
/// string(26) "IntlRuleBasedBreakIterator"
/// NULL
/// bool(true)
/// string(9) "Hi there!"
/// 0:0,1:2,2:3,3:8,4:9,
/// 0 2 8 8 9 8
/// int(2)
/// int(3)
/// bool(true)
/// bool(false)
/// string(2) "en"
/// bool(false)
/// string(77) "IntlBreakIterator::getLocale(): invalid locale type: U_ILLEGAL_ARGUMENT_ERROR"
/// int(0)
/// string(12) "U_ZERO_ERROR"
/// IntlBreakIterator::following(): Argument #1 ($offset) must be between -2147483648 and 2147483647
/// -1 500 100
/// bool(true)
/// string(9) "Hi there!"
/// bool(false)
/// IntlCodePointBreakIterator
/// IntlRuleBasedBreakIterator
/// IntlRuleBasedBreakIterator
/// IntlRuleBasedBreakIterator
/// IntlRuleBasedBreakIterator
/// int(3)
/// string(16) "InternalIterator"
/// string(17) "IntlPartsIterator"
/// ```
#[php_class(implements(IteratorAggregate), not_serializable, size = 96, payload = BreakObject, handlers = crate::breakiterator::HANDLERS)]
pub(crate) struct IntlBreakIterator;

/// `class IntlRuleBasedBreakIterator extends IntlBreakIterator`, over
/// rules ICU compiles or compiled ones.
///
/// ```php
/// $rules = '$LN = [[:letter:] [:number:]]; $S = [.;,:]; !!forward; $LN+ {1}; $S+ {42}; !!reverse; $LN+ {1}; $S+ {42}; !!safe_forward; !!safe_reverse;';
/// $rbbi = new IntlRuleBasedBreakIterator($rules);
/// $rbbi->setText('sdfkjsdf88á.... ,;');
/// do { echo $rbbi->current(), ':', $rbbi->getRuleStatus(), ','; } while ($rbbi->next() != IntlBreakIterator::DONE);
/// echo "\n";
/// var_dump(strlen($rbbi->getRules()) > 0, $rbbi->getRuleStatusVec());
/// $copy = new IntlRuleBasedBreakIterator($rbbi->getBinaryRules(), true);
/// var_dump($copy->getBinaryRules() === $rbbi->getBinaryRules());
/// try { new IntlRuleBasedBreakIterator('[['); } catch (IntlException $e) { echo $e->getMessage(), "\n"; }
/// try { new IntlRuleBasedBreakIterator('xx', true); } catch (IntlException $e) { echo $e->getMessage(), "\n"; }
/// try { $rbbi->__construct('.;'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { (new ReflectionClass('IntlRuleBasedBreakIterator'))->newInstanceWithoutConstructor()->getRules(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 0:0,12:1,16:42,17:0,19:42,
/// bool(true)
/// array(1) {
///   [0]=>
///   int(42)
/// }
/// bool(true)
/// IntlRuleBasedBreakIterator::__construct(): unable to create RuleBasedBreakIterator from rules (parse error on line 1, offset 1)
/// IntlRuleBasedBreakIterator::__construct(): unable to create instance from compiled rules
/// IntlRuleBasedBreakIterator object is already constructed
/// Found unconstructed BreakIterator
/// ```
#[php_class(extends(IntlBreakIterator), not_serializable, handlers = crate::breakiterator::HANDLERS)]
pub(crate) struct IntlRuleBasedBreakIterator;

/// `class IntlCodePointBreakIterator extends IntlBreakIterator`: a break
/// at every code point, php's own ICU iterator.
///
/// ```php
/// $cp = IntlBreakIterator::createCodePointInstance();
/// $cp->setText("a\u{1F600}é");
/// var_dump($cp->getLastCodePoint(), $cp->next(), $cp->getLastCodePoint(), $cp->next(), dechex($cp->getLastCodePoint()));
/// var_dump($cp->last(), $cp->previous(), $cp->getLastCodePoint(), $cp->isBoundary(2), $cp->current());
/// foreach ($cp->getPartsIterator() as $k => $p) echo "$k:", bin2hex($p), ",";
/// echo "\n";
/// $c = clone $cp; var_dump($c == $cp);
/// ```
/// ```expect
/// int(-1)
/// int(1)
/// int(97)
/// int(5)
/// string(5) "1f600"
/// int(7)
/// int(5)
/// int(233)
/// bool(false)
/// int(1)
/// 0:61,1:f09f9880,2:c3a9,
/// bool(true)
/// ```
#[php_class(extends(IntlBreakIterator), not_serializable, handlers = crate::breakiterator::HANDLERS)]
pub(crate) struct IntlCodePointBreakIterator;

/// `class IntlPartsIterator extends IntlIterator`: the text between one
/// boundary and the next, keyed in sequence or by either boundary.
///
/// ```php
/// $bi = IntlBreakIterator::createWordInstance('en');
/// $bi->setText('foo bar');
/// foreach ([IntlPartsIterator::KEY_SEQUENTIAL, IntlPartsIterator::KEY_LEFT, IntlPartsIterator::KEY_RIGHT] as $t) {
///     foreach ($bi->getPartsIterator($t) as $k => $v) echo "$k=$v|";
///     echo "\n";
/// }
/// $pi = $bi->getPartsIterator();
/// var_dump($pi->getBreakIterator() === $bi, $pi->getRuleStatus(), get_class($pi));
/// try { $bi->getPartsIterator(3); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 0=foo|1= |2=bar|
/// 0=foo|3= |4=bar|
/// 3=foo|4= |7=bar|
/// bool(true)
/// int(200)
/// string(17) "IntlPartsIterator"
/// IntlBreakIterator::getPartsIterator(): Argument #1 ($type) must be one of IntlPartsIterator::KEY_SEQUENTIAL, IntlPartsIterator::KEY_LEFT, or IntlPartsIterator::KEY_RIGHT
/// ```
#[php_class(extends(IntlIterator), not_serializable, size = 80, beside(152), handlers = crate::iterator::HANDLERS)]
pub(crate) struct IntlPartsIterator;

type Answer<T> = Result<T, Thrown>;

#[php_class]
impl IntlBreakIterator {
    const DONE: i64 = -1;
    const WORD_NONE: i64 = 0;
    const WORD_NONE_LIMIT: i64 = 100;
    const WORD_NUMBER: i64 = 100;
    const WORD_NUMBER_LIMIT: i64 = 200;
    const WORD_LETTER: i64 = 200;
    const WORD_LETTER_LIMIT: i64 = 300;
    const WORD_KANA: i64 = 300;
    const WORD_KANA_LIMIT: i64 = 400;
    const WORD_IDEO: i64 = 400;
    const WORD_IDEO_LIMIT: i64 = 500;
    const LINE_SOFT: i64 = 0;
    const LINE_SOFT_LIMIT: i64 = 100;
    const LINE_HARD: i64 = 100;
    const LINE_HARD_LIMIT: i64 = 200;
    const SENTENCE_TERM: i64 = 0;
    const SENTENCE_TERM_LIMIT: i64 = 100;
    const SENTENCE_SEP: i64 = 100;
    const SENTENCE_SEP_LIMIT: i64 = 200;

    #[php(tentative, before_args = error::reset)]
    fn createCharacterInstance(
        req: &mut Request,
        #[php(default = null)] locale: Option<PhpString>,
    ) -> Answer<Option<Owner<Instance<IntlBreakIterator>>>> {
        factory(req, 0, locale)
    }

    #[php(tentative, before_args = error::reset)]
    fn createCodePointInstance(
        req: &mut Request,
    ) -> Answer<Owner<Instance<IntlCodePointBreakIterator>>> {
        let made = Breaker::open(5, c"")
            .map_err(|_| fatal(req, "InternalError: IntlCodePointBreakIterator"))?;
        made_object(req, made).map(Owner::into_instance)
    }

    #[php(tentative, before_args = error::reset)]
    fn createLineInstance(
        req: &mut Request,
        #[php(default = null)] locale: Option<PhpString>,
    ) -> Answer<Option<Owner<Instance<IntlBreakIterator>>>> {
        factory(req, 2, locale)
    }

    #[php(tentative, before_args = error::reset)]
    fn createSentenceInstance(
        req: &mut Request,
        #[php(default = null)] locale: Option<PhpString>,
    ) -> Answer<Option<Owner<Instance<IntlBreakIterator>>>> {
        factory(req, 3, locale)
    }

    #[php(tentative, before_args = error::reset)]
    fn createTitleInstance(
        req: &mut Request,
        #[php(default = null)] locale: Option<PhpString>,
    ) -> Answer<Option<Owner<Instance<IntlBreakIterator>>>> {
        factory(req, 4, locale)
    }

    #[php(tentative, before_args = error::reset)]
    fn createWordInstance(
        req: &mut Request,
        #[php(default = null)] locale: Option<PhpString>,
    ) -> Answer<Option<Owner<Instance<IntlBreakIterator>>>> {
        factory(req, 1, locale)
    }

    #[php(private)]
    fn __construct(req: &mut Request, this: This<Self>) -> Answer<()> {
        let ce = crate::throwables::Throwables::of(req)?;
        let _ = this;
        Err(throw(
            req,
            ce.exception,
            "An object of this type cannot be created with the new operator",
        ))
    }

    #[php(tentative, before_args = error::reset)]
    fn current(req: &mut Request, this: This<Self>) -> Answer<i64> {
        step(req, this.object(), Step::Current, None)
    }

    #[php(tentative, before_args = error::reset)]
    fn first(req: &mut Request, this: This<Self>) -> Answer<i64> {
        step(req, this.object(), Step::First, None)
    }

    #[php(tentative, before_args = error::reset)]
    fn following(req: &mut Request, this: This<Self>, offset: i64) -> Answer<i64> {
        step(req, this.object(), Step::Following, Some(offset))
    }

    #[php(tentative, before_args = error::reset)]
    fn getErrorCode(req: &mut Request, this: This<Self>) -> i64 {
        i64::from(error::object_code::<BreakObject>(req, this.object()).0)
    }

    #[php(tentative, before_args = error::reset)]
    fn getErrorMessage(req: &mut Request, this: This<Self>) -> Answer<Owner<PhpString>> {
        let message = error::object_message::<BreakObject>(req, this.object());
        PhpString::new(req, &message)
    }

    #[php(tentative, before_args = error::reset)]
    fn getLocale(
        req: &mut Request,
        this: This<Self>,
        r#type: i64,
    ) -> Answer<OrFalse<Owner<PhpString>>> {
        let object = this.object();
        if r#type != 0 && r#type != 1 {
            error::set(req, Code::ILLEGAL_ARGUMENT, "invalid locale type")?;
            return Ok(OrFalse::False);
        }
        let (name, status) = with(req, object, |b| {
            b.locale(i32::try_from(r#type).unwrap_or(0))
        })?;
        if !status_ok(req, object, status, "Call to ICU method has failed")? {
            return Ok(OrFalse::False);
        }
        PhpString::new(req, &name).map(OrFalse::Value)
    }

    #[php(tentative, before_args = error::reset)]
    fn getPartsIterator(
        req: &mut Request,
        this: This<Self>,
        #[php(default = IntlPartsIterator::KEY_SEQUENTIAL)] r#type: Raw<PhpString>,
    ) -> Answer<Owner<Instance<IntlPartsIterator>>> {
        let object = this.object();
        // `Z_PARAM_LONG`, where the stub declares `string $type`.
        let key = coerce_param(req, &r#type.value(), &Type::Int, 1)?
            .as_int()
            .unwrap_or(-1);
        let Some(key) = Key3::of(key) else {
            let tail = "be one of IntlPartsIterator::KEY_SEQUENTIAL, IntlPartsIterator::KEY_LEFT, or IntlPartsIterator::KEY_RIGHT";
            return Err(arg_value_error(req, 1, tail));
        };
        with(req, object, |_| ())?;
        let walk = Parts {
            object: Some(Owner::take(req, object)),
            key,
            right: 0,
        };
        iterator::made(req, b"IntlPartsIterator", Box::new(walk)).map(Owner::into_instance)
    }

    #[php(tentative, before_args = error::reset)]
    fn getText(req: &mut Request, this: This<Self>) -> Answer<Option<PhpString>> {
        let object = this.object();
        with(req, object, |_| ())?;
        Ok(object
            .native::<BreakObject>(req)
            .and_then(|o| o.biter.as_ref()?.text()))
    }

    #[php(tentative, before_args = error::reset)]
    fn isBoundary(req: &mut Request, this: This<Self>, offset: i64) -> Answer<bool> {
        step(req, this.object(), Step::IsBoundary, Some(offset)).map(|n| n != 0)
    }

    #[php(tentative, before_args = error::reset)]
    fn last(req: &mut Request, this: This<Self>) -> Answer<i64> {
        step(req, this.object(), Step::Last, None)
    }

    #[php(tentative, before_args = error::reset)]
    fn next(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] offset: Option<i64>,
    ) -> Answer<i64> {
        match offset {
            None => step(req, this.object(), Step::Next, None),
            Some(n) => step(req, this.object(), Step::NextBy, Some(n)),
        }
    }

    #[php(tentative, before_args = error::reset)]
    fn preceding(req: &mut Request, this: This<Self>, offset: i64) -> Answer<i64> {
        step(req, this.object(), Step::Preceding, Some(offset))
    }

    #[php(tentative, before_args = error::reset)]
    fn previous(req: &mut Request, this: This<Self>) -> Answer<i64> {
        step(req, this.object(), Step::Previous, None)
    }

    /// `setText(string $text): bool`: ICU walks the string in place, so
    /// the iterator holds it, a clone holds it again, and the text it
    /// held before is released.
    ///
    /// ```php
    /// $it = IntlBreakIterator::createWordInstance('en');
    /// $it->setText(str_repeat('ab ', 2) . 'cd');
    /// $ballast = [];
    /// for ($i = 0; $i < 16; $i++) $ballast[] = str_repeat("\xCC", 8);
    /// $c = clone $it;
    /// unset($it);
    /// for ($i = 0; $i < 16; $i++) $ballast[] = str_repeat("\xDD", 8);
    /// echo implode('|', iterator_to_array($c->getPartsIterator())), ' ', $c->getText(), "\n";
    /// $m = memory_get_usage();
    /// $c->setText('xy z');
    /// echo implode('|', iterator_to_array($c->getPartsIterator())), ' ', memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// ab| |ab| |cd ab ab cd
    /// xy| |z -40
    /// ```
    #[php(tentative, before_args = error::reset)]
    fn setText(req: &mut Request, this: This<Self>, text: PhpString) -> Answer<bool> {
        let object = this.object();
        let set = with_taken(req, object, |req, b| b.set_text(req, text))?;
        match set {
            Ok(old) => {
                if let Some(old) = old {
                    old.give_up(req);
                }
            }
            Err((at, status)) => {
                let message = if at == 1 {
                    "error opening UText"
                } else {
                    "error calling BreakIterator::setText()"
                };
                status_ok(req, object, status, message)?;
                return Ok(false);
            }
        }
        error::set_code(req, Code::ZERO);
        Ok(true)
    }

    fn getIterator(
        req: &mut Request,
        this: This<Self>,
    ) -> Answer<Owner<Instance<crate::iterator::Iterator>>> {
        internal_iterator(req, &this.object()).map(Owner::into_instance)
    }
}

#[php_class]
impl IntlRuleBasedBreakIterator {
    /// `__construct(string $rules, bool $compiled = false)`: compiled
    /// rules are read by ICU in place, so the object, and each clone of
    /// it, holds the string it was built over (php 8.5.12's
    /// `compiled_rules`), whatever becomes of the caller's.
    ///
    /// ```php
    /// $src = new IntlRuleBasedBreakIterator('$S = [,]; !!forward; [^,]+ {1}; $S {42};');
    /// $it = new IntlRuleBasedBreakIterator($src->getBinaryRules() . '', true);
    /// $ballast = [];
    /// for ($i = 0; $i < 16; $i++) $ballast[] = str_repeat("\xCC", strlen($src->getBinaryRules()));
    /// unset($src);
    /// $c = clone $it;
    /// unset($it);
    /// $c->setText('ab,cd');
    /// echo implode(' ', iterator_to_array($c->getPartsIterator())), ' ', $c->getRuleStatus(), "\n";
    /// ```
    /// ```expect
    /// ab , cd 1
    /// ```
    #[php(before_args = error::reset)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        rules: PhpString,
        #[php(default = false)] compiled: bool,
    ) -> Answer<()> {
        let ce = crate::throwables::Throwables::of(req)?;
        let object = this.object();
        if object
            .native::<BreakObject>(req)
            .is_some_and(|o| o.biter.is_some())
        {
            return Err(throw_error(
                req,
                ce.error,
                "IntlRuleBasedBreakIterator object is already constructed",
            ));
        }
        let prefix = "IntlRuleBasedBreakIterator::__construct(): ";
        let made = if compiled {
            Breaker::from_binary(req, rules)
                .map_err(|_| format!("{prefix}unable to create instance from compiled rules"))
        } else {
            let Ok(text) = to_utf16(rules.as_bytes()) else {
                return Err(throw(
                    req,
                    ce.intl_exception,
                    &format!("{prefix}rules were not a valid UTF-8 string"),
                ));
            };
            let mut pe = ParseError::default();
            let made = Breaker::from_rules(&text, &mut pe);
            error::set_code(req, made.as_ref().err().copied().unwrap_or(Code::ZERO));
            made.map_err(|_| {
                format!(
                    "{prefix}unable to create RuleBasedBreakIterator from rules ({})",
                    error::parse_error(&pe)
                )
            })
        };
        match made {
            Ok(biter) => {
                match object.native_mut::<BreakObject>(req) {
                    Some(o) => o.biter = Some(biter),
                    None => biter.close(req),
                }
                Ok(())
            }
            Err(message) => Err(throw(req, ce.intl_exception, &message)),
        }
    }

    #[php(tentative, before_args = error::reset)]
    fn getBinaryRules(req: &mut Request, this: This<Self>) -> Answer<OrFalse<Owner<PhpString>>> {
        let object = this.object();
        let rules = with(req, object, Breaker::binary_rules)?;
        if rules.len() > i32::MAX as usize - 1 {
            let code = error::object_code::<BreakObject>(req, object);
            error::errors_set::<BreakObject>(req, object, code, "the rules are too large")?;
            return Ok(OrFalse::False);
        }
        PhpString::new(req, &rules).map(OrFalse::Value)
    }

    #[php(tentative, before_args = error::reset)]
    fn getRules(req: &mut Request, this: This<Self>) -> Answer<OrFalse<Owner<PhpString>>> {
        let object = this.object();
        let (rules, _) = with(req, object, Breaker::rules)?;
        match to_utf8(&rules) {
            Ok(text) => PhpString::new(req, &text).map(OrFalse::Value),
            Err(code) => {
                error::errors_set::<BreakObject>(
                    req,
                    object,
                    code,
                    "Error converting result to UTF-8 string",
                )?;
                Ok(OrFalse::False)
            }
        }
    }

    #[php(tentative, before_args = error::reset)]
    fn getRuleStatus(req: &mut Request, this: This<Self>) -> Answer<i64> {
        step(req, this.object(), Step::RuleStatus, None)
    }

    #[php(tentative, before_args = error::reset)]
    fn getRuleStatusVec(req: &mut Request, this: This<Self>) -> Answer<OrFalse<Owner<PhpArray>>> {
        let object = this.object();
        match with(req, object, Breaker::rule_status_vec)? {
            Ok(all) => {
                let mut array = PhpArray::new(req)?;
                for n in all {
                    array.append(req, Owner::int(i64::from(n)))?;
                }
                Ok(OrFalse::Value(array))
            }
            Err(code) => {
                error::errors_set::<BreakObject>(
                    req,
                    object,
                    code,
                    "failed obtaining the status values",
                )?;
                Ok(OrFalse::False)
            }
        }
    }
}

#[php_class]
impl IntlCodePointBreakIterator {
    #[php(tentative, before_args = error::reset)]
    fn getLastCodePoint(req: &mut Request, this: This<Self>) -> Answer<i64> {
        step(req, this.object(), Step::LastCodePoint, None)
    }
}

#[php_class]
impl IntlPartsIterator {
    const KEY_SEQUENTIAL: i64 = 0;
    const KEY_LEFT: i64 = 1;
    const KEY_RIGHT: i64 = 2;

    #[php(tentative, before_args = error::reset)]
    fn getBreakIterator(
        req: &mut Request,
        this: This<Self>,
    ) -> Answer<Instance<IntlBreakIterator>> {
        let data = iterator::data(req, this.object())?;
        match data.and_then(|v| v.as_object()) {
            Some(object) => Ok(Instance::of(object)),
            None => Err(fatal(
                req,
                "InternalError: IntlPartsIterator holds no break iterator",
            )),
        }
    }

    #[php(tentative, before_args = error::reset)]
    fn getRuleStatus(req: &mut Request, this: This<Self>) -> Answer<i64> {
        let data = iterator::data(req, this.object())?;
        let Some(object) = data.and_then(|v| v.as_object()) else {
            return Err(fatal(
                req,
                "InternalError: IntlPartsIterator holds no break iterator",
            ));
        };
        let status = call_method(req, &object, b"getrulestatus", &[])?;
        let n = status.as_int().unwrap_or(0);
        status.give_up(req);
        Ok(n)
    }
}

/// `_breakiter_factory`: the locale's iterator of `kind`, or null with the
/// request's error set.
fn factory(
    req: &mut Request,
    kind: i32,
    locale: Option<PhpString>,
) -> Answer<Option<Owner<Instance<IntlBreakIterator>>>> {
    let locale = match locale {
        Some(l) => c_string(l.as_bytes()),
        None => c_string(&crate::default_locale(req)),
    };
    let made = Breaker::open(kind, &locale);
    error::set_code(req, made.as_ref().err().copied().unwrap_or(Code::ZERO));
    if let Ok(biter) = made {
        return made_object(req, biter).map(|o| Some(o.into_instance()));
    }
    error::set_custom_msg(req, "error creating BreakIterator")?;
    Ok(None)
}

/// `breakiterator_object_create`: a new object of the class the
/// iterator's dynamic class picks, holding it.
fn made_object(req: &mut Request, biter: Breaker) -> Answer<Owner<PhpObject>> {
    let class: &[u8] = match biter.kind() {
        1 => b"IntlRuleBasedBreakIterator",
        2 => b"IntlCodePointBreakIterator",
        _ => b"IntlBreakIterator",
    };
    let Some(id) = lookup_class(req, class)? else {
        return Err(fatal(req, "InternalError: IntlBreakIterator is not linked"));
    };
    let object = new_object(req, id)?;
    match object.native_mut::<BreakObject>(req) {
        Some(o) => o.biter = Some(biter),
        None => biter.close(req),
    }
    Ok(object)
}

/// `BREAKITER_METHOD_FETCH_OBJECT`, then `f` over the iterator: the
/// object's error reset, php's `Error` where it holds none.
fn with<T>(req: &mut Request, object: PhpObject, f: impl FnOnce(&mut Breaker) -> T) -> Answer<T> {
    let ce = crate::throwables::Throwables::of(req)?;
    error::reset_object::<BreakObject>(req, object);
    match object
        .native_mut::<BreakObject>(req)
        .and_then(|o| o.biter.as_mut())
    {
        Some(biter) => Ok(f(biter)),
        None => Err(throw_error(
            req,
            ce.error,
            "Found unconstructed BreakIterator",
        )),
    }
}

/// [`with`] where `f` reaches the request: the iterator taken out of the
/// object for the call and put back after.
fn with_taken<T>(
    req: &mut Request,
    object: PhpObject,
    f: impl FnOnce(&mut Request, &mut Breaker) -> T,
) -> Answer<T> {
    let ce = crate::throwables::Throwables::of(req)?;
    error::reset_object::<BreakObject>(req, object);
    let taken = object
        .native_mut::<BreakObject>(req)
        .and_then(|o| o.biter.take());
    let Some(mut biter) = taken else {
        return Err(throw_error(
            req,
            ce.error,
            "Found unconstructed BreakIterator",
        ));
    };
    let answer = f(req, &mut biter);
    match object.native_mut::<BreakObject>(req) {
        Some(o) => o.biter = Some(biter),
        None => biter.close(req),
    }
    Ok(answer)
}

/// `INTL_METHOD_CHECK_STATUS` over `status`, which the object keeps.
fn status_ok(req: &mut Request, object: PhpObject, status: Code, message: &str) -> Answer<bool> {
    if let Some(o) = object.native_mut::<BreakObject>(req) {
        o.error.code = status;
    }
    match error::check_object::<BreakObject>(req, object, message) {
        Ok(()) => Ok(true),
        Err(None) => Ok(false),
        Err(Some(thrown)) => Err(thrown),
    }
}

/// `_breakiter_no_args_ret_int32` and `_breakiter_int32_ret_int32`: an
/// offset held to `int32_t` first.
fn step(req: &mut Request, object: PhpObject, which: Step, offset: Option<i64>) -> Answer<i64> {
    if which != Step::IsBoundary {
        with(req, object, |_| ())?;
    }
    let arg = match offset.map(i32::try_from) {
        None => 0,
        Some(Ok(n)) => n,
        Some(Err(_)) => {
            return Err(arg_value_error(
                req,
                1,
                "be between -2147483648 and 2147483647",
            ));
        }
    };
    with(req, object, |b| i64::from(b.step(which, arg)))
}

/// `parts_iter_key_type`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Key3 {
    Sequential,
    Left,
    Right,
}

impl Key3 {
    fn of(n: i64) -> Option<Key3> {
        [Key3::Sequential, Key3::Left, Key3::Right]
            .get(usize::try_from(n).ok()?)
            .copied()
    }
}

/// `zoi_break_iter_parts`: the break iterator walked, one owner, how the
/// parts are keyed and the right boundary php keeps beside the index.
struct Parts {
    /// The break iterator walked, an owner of it until the walk is
    /// released.
    object: Option<Owner<PhpObject>>,
    key: Key3,
    right: i64,
}

impl Walk for Parts {
    /// `_breakiterator_parts_move_forward`: the text from the current
    /// boundary to the next.
    fn forward(
        &mut self,
        req: &mut Request,
        _: PhpObject,
        index: &mut i64,
    ) -> Answer<Option<Owner<Value>>> {
        let Some(o) = self
            .walked()
            .and_then(|object| object.native_mut::<BreakObject>(req))
        else {
            return Ok(None);
        };
        let Some(biter) = o.biter.as_mut() else {
            return Ok(None);
        };
        let cur = biter.step(Step::Current, 0);
        if cur == DONE {
            return Ok(None);
        }
        let next = biter.step(Step::Next, 0);
        if next == DONE {
            return Ok(None);
        }
        match self.key {
            Key3::Left => *index = i64::from(cur),
            Key3::Right => {
                *index = i64::from(next);
                self.right = i64::from(next);
            }
            Key3::Sequential => {}
        }
        let text = biter.text();
        let text = text.as_ref().map_or(&[][..], PhpString::as_bytes);
        let (from, to) = (
            usize::try_from(cur).unwrap_or(0),
            usize::try_from(next).unwrap_or(0),
        );
        let part = text.get(from..to.max(from)).unwrap_or_default().to_vec();
        Ok(Some(PhpString::new(req, &part)?.into_value()))
    }

    /// `_breakiterator_parts_rewind`: back to the first boundary.
    fn reset(&mut self, req: &mut Request, _: PhpObject) -> Answer<bool> {
        if let Some(biter) = self
            .walked()
            .and_then(|object| object.native_mut::<BreakObject>(req))
            .and_then(|o| o.biter.as_mut())
        {
            biter.step(Step::First, 0);
        }
        Ok(true)
    }

    /// `_breakiterator_parts_get_current_key`.
    fn key(&self, index: i64) -> Option<i64> {
        Some(if self.key == Key3::Right && index == 0 {
            self.right
        } else {
            index
        })
    }

    fn data(&self) -> Option<Value> {
        self.walked().map(|object| object.as_value())
    }

    fn release(&mut self, req: &mut Request) {
        if let Some(object) = self.object.take() {
            object.give_up(req);
        }
    }

    /// `ZVAL_UNDEF(&wrapping_obj)` (php 8.5.12): the parts iterator is
    /// no cycle with itself.
    fn wraps_object(&self) -> bool {
        false
    }
}

impl Parts {
    /// The break iterator walked, while the walk holds it.
    fn walked(&self) -> Option<PhpObject> {
        self.object.as_deref().copied()
    }
}

/// The `foreach` over a break iterator's boundaries,
/// `breakiterator_iterator_funcs`: the position after each move, keyed by
/// the engine's count.
#[derive(Default)]
struct Boundaries {
    current: Option<i64>,
    index: i64,
}

impl Boundaries {
    /// `_breakiter_prolog`, then `f` over the iterator: the errors reset,
    /// and the object's set where it holds none.
    fn prolog<T>(
        req: &mut Request,
        object: PhpObject,
        f: impl FnOnce(&mut Breaker) -> T,
    ) -> Answer<Option<T>> {
        error::errors_reset::<BreakObject>(req, object);
        if let Some(biter) = object
            .native_mut::<BreakObject>(req)
            .and_then(|o| o.biter.as_mut())
        {
            return Ok(Some(f(biter)));
        }
        let message =
            "The BreakIterator object backing the PHP iterator is not properly constructed";
        error::errors_set::<BreakObject>(req, object, Code(27), message)?;
        Ok(None)
    }
}

impl ClassIterator for Boundaries {
    fn rewind(&mut self, req: &mut Request, object: PhpObject) -> Answer<()> {
        let first = Boundaries::prolog(req, object, |b| b.step(Step::First, 0))?;
        self.current = first.map(i64::from);
        self.index = 0;
        Ok(())
    }

    fn valid(&mut self, _: &mut Request, _: PhpObject) -> Answer<bool> {
        Ok(self.current.is_some())
    }

    fn current(&mut self, req: &mut Request, _: PhpObject) -> Answer<Owner<Value>> {
        let current = self.current.map_or_else(Value::null, Value::int);
        Ok(Owner::take(req, current))
    }

    fn key(&mut self, _: &mut Request, _: PhpObject) -> Answer<Owner<Value>> {
        Ok(Owner::int(self.index))
    }

    fn move_forward(&mut self, req: &mut Request, object: PhpObject) -> Answer<()> {
        self.index += 1;
        self.current = None;
        let next = Boundaries::prolog(req, object, |b| b.step(Step::Next, 0))?;
        self.current = next.filter(|&n| n != DONE).map(i64::from);
        Ok(())
    }
}

impl ObjectHandlers for Handlers {
    /// `BreakIterator_compare_objects`: equal where ICU's iterators are,
    /// or both unconstructed.
    fn compare(&self, req: &mut Request, left: &Value, right: &Value) -> Answer<Option<Ordering>> {
        let (Some(one), Some(two)) = (left.as_object(), right.as_object()) else {
            return Std.compare(req, left, right);
        };
        let (Some(one), Some(two)) = (
            one.native::<BreakObject>(req),
            two.native::<BreakObject>(req),
        ) else {
            return Std.compare(req, left, right);
        };
        let same = match (&one.biter, &two.biter) {
            (Some(one), Some(two)) => one.equals(two),
            (None, None) => true,
            _ => false,
        };
        Ok(Some(if same {
            Ordering::Equal
        } else {
            Ordering::Greater
        }))
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_break)
    }

    /// `BreakIterator_get_debug_info`: whether it holds an iterator, the
    /// text and the iterator's C++ type.
    fn get_debug_info(&self, req: &mut Request, object: PhpObject) -> Answer<Option<DebugInfo>> {
        let (valid, text, kind) = match object.native::<BreakObject>(req) {
            Some(BreakObject { biter: Some(b), .. }) => (
                true,
                b.text().map_or(Value::null(), Value::string),
                b.type_name(),
            ),
            _ => (false, Value::null(), ""),
        };
        let mut table = PhpArray::new(req)?;
        table.set_str(req, b"valid", Owner::bool(valid))?;
        if valid {
            table.set_str(req, b"text", Owner::take(req, text))?;
            let kind = PhpString::new(req, kind.as_bytes())?.into_value();
            table.set_str(req, b"type", kind)?;
        }
        Ok(Some(DebugInfo::from(table)))
    }

    /// `BreakIterator_objects_free`: the iterator deleted and what it
    /// read given up, then the error's message freed, before
    /// `zend_object_std_dtor`.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let biter = object
            .native_mut::<BreakObject>(req)
            .and_then(|o| o.biter.take());
        if let Some(biter) = biter {
            biter.close(req);
        }
        error::free::<BreakObject>(req, object);
    }

    /// `_breakiterator_get_iterator`.
    fn get_iterator(
        &self,
        req: &mut Request,
        object: PhpObject,
        by_ref: bool,
    ) -> Answer<Option<Box<dyn ClassIterator>>> {
        let ce = crate::throwables::Throwables::of(req)?;
        if by_ref {
            return Err(throw(
                req,
                ce.exception,
                "Iteration by reference is not supported",
            ));
        }
        if object
            .native::<BreakObject>(req)
            .is_none_or(|o| o.biter.is_none())
        {
            return Err(throw(
                req,
                ce.exception,
                "The BreakIterator is not properly constructed",
            ));
        }
        Ok(Some(Box::<Boundaries>::default()))
    }

    /// `zoi_with_current`.
    fn iterator_size(&self) -> usize {
        128
    }
}

/// `BreakIterator_clone_obj`: a clone of ICU's iterator over the same
/// text.
fn clone_break(req: &mut Request, object: PhpObject) -> Answer<Owner<PhpObject>> {
    let ce = crate::throwables::Throwables::of(req)?;
    let copy = match object.native::<BreakObject>(req) {
        Some(BreakObject { biter: Some(b), .. }) => b.try_clone(req),
        _ => {
            return Err(throw_error(
                req,
                ce.error,
                "Cannot clone uninitialized BreakIterator",
            ));
        }
    };
    let Some(copy) = copy else {
        return Err(throw_error(req, ce.error, "Failed to clone BreakIterator"));
    };
    let payload = BreakObject {
        error: IntlError::default(),
        biter: Some(copy),
    };
    clone_with(req, &object, payload)
}
