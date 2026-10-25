//! `spoofchecker/`: `Spoofchecker` over ICU's `uspoof_*`
//! (`spoofchecker_class.c`, `spoofchecker_create.c`,
//! `spoofchecker_main.c`), as php builds it against ICU 58 and later: a
//! check reads `uspoof_check2UTF8` into the object's `USpoofCheckResult`, and a
//! new checker starts at `USPOOF_HIGHLY_RESTRICTIVE`.

use php_ext::abi::{CloneObj, ObjectHandlers, Ref, This, Untyped};
use php_ext::attr::php_class;
use php_ext::call::clone_with;
use php_ext::raising::{
    Level, Thrown, arg_value_error, argument_error, docref, throw, throw_error,
};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, PhpString, Value};

use crate::error;
use crate::ffi::spoofchecker::{self as uspoof, CheckResult, Checker, Set};
use crate::ffi::{self, Code, c_string};

type Answer<T> = Result<T, Thrown>;

/// The `URestrictionLevel`s `setRestrictionLevel` takes.
const LEVELS: [i64; 6] = [
    Spoofchecker::ASCII,
    Spoofchecker::SINGLE_SCRIPT_RESTRICTIVE,
    Spoofchecker::HIGHLY_RESTRICTIVE,
    Spoofchecker::MODERATELY_RESTRICTIVE,
    Spoofchecker::MINIMALLY_RESTRICTIVE,
    Spoofchecker::UNRESTRICTIVE,
];

/// The `uset_applyPattern` options `setAllowedChars` takes: none, or
/// `IGNORE_SPACE` alone or with one of the three case options.
const PATTERN_OPTIONS: [i64; 5] = [
    0,
    Spoofchecker::IGNORE_SPACE,
    Spoofchecker::IGNORE_SPACE | Spoofchecker::SIMPLE_CASE_INSENSITIVE,
    Spoofchecker::IGNORE_SPACE | Spoofchecker::CASE_INSENSITIVE,
    Spoofchecker::IGNORE_SPACE | Spoofchecker::ADD_CASE_MAPPINGS,
];

/// `Spoofchecker_object` past its `zend_object`: the checker a
/// constructor opened and the `USpoofCheckResult` it opened beside it,
/// which a clone does not carry. php's `intl_error` is left out: no
/// method of the class reads it, and only the request's is reset.
#[derive(Default)]
pub(crate) struct SpoofObject {
    uspoof: Option<Checker>,
    result: Option<CheckResult>,
}

/// `SPOOFCHECKER_METHOD_FETCH_OBJECT`: php's `Error` where no
/// constructor opened a checker.
fn constructed(req: &mut Request, object: PhpObject) -> Answer<()> {
    with(req, object, |_, _| ())
}

/// `f` over the object's checker and its `USpoofCheckResult`, or php's `Error`
/// where no constructor opened a checker.
fn with<T>(
    req: &mut Request,
    object: PhpObject,
    f: impl FnOnce(&mut Checker, Option<&mut CheckResult>) -> T,
) -> Answer<T> {
    let ce = crate::throwables::Throwables::of(req)?;
    match object.native_mut::<SpoofObject>(req) {
        Some(SpoofObject {
            uspoof: Some(checker),
            result,
        }) => Ok(f(checker, result.as_mut())),
        _ => Err(throw_error(
            req,
            ce.error,
            "Found unconstructed Spoofchecker",
        )),
    }
}

/// php's `(<code>) <name>` warning where ICU failed; whether it did.
fn warn_on_failure(req: &mut Request, status: Code) -> bool {
    if status.is_failure() {
        let message = format!("({}) {}", status.0, status.name());
        docref(req, Level::Warning, &message);
    }
    status.is_failure()
}

/// `$errorCode` assigned the checks a call answered, where it was given.
fn assign(req: &mut Request, error_code: &Ref<Untyped<Value>>, checks: i32) -> Answer<()> {
    if error_code.is_given() {
        error_code.set(req, Untyped(Value::int(i64::from(checks))))?;
    }
    Ok(())
}

/// `Spoofchecker_handlers`: `std_object_handlers` with php's clone.
pub(crate) struct Handlers;
pub(crate) static HANDLERS: Handlers = Handlers;

impl ObjectHandlers for Handlers {
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_checker)
    }
}

/// `spoofchecker_clone_obj`: `uspoof_clone` of the checker, its
/// `USpoofCheckResult` left behind, or php's `Error` once the copy is made.
fn clone_checker(req: &mut Request, object: PhpObject) -> Answer<Owner<PhpObject>> {
    let ce = crate::throwables::Throwables::of(req)?;
    let mut status = Code::ZERO;
    let copy = object
        .native::<SpoofObject>(req)
        .and_then(|o| o.uspoof.as_ref())
        .map(|checker| checker.try_clone(&mut status));
    let refusal = match &copy {
        None => Some("Cannot clone uninitialized SpoofChecker"),
        Some(_) if status.is_failure() => Some("Failed to clone SpoofChecker"),
        Some(_) => None,
    };
    let payload = SpoofObject {
        uspoof: copy.flatten(),
        ..SpoofObject::default()
    };
    let new = clone_with(req, &object, payload)?;
    let Some(refusal) = refusal else {
        return Ok(new);
    };
    new.give_up(req);
    Err(throw_error(req, ce.error, refusal))
}

/// `class Spoofchecker`: ICU's spoof checker, which tells whether a
/// string is suspicious as an identifier and whether two are confusable.
///
/// ```php
/// $s = new Spoofchecker();
/// var_dump($s->isSuspicious("\u{041F}aypal.com", $e), $e, $s->isSuspicious('paypal.com', $e), $e);
/// var_dump($s->areConfusable('google.com', 'goog1e.com', $e), $e, $s->areConfusable('a', 'b'));
/// $s->setAllowedLocales('en_US, ko_KR'); var_dump($s->isSuspicious("\u{D55C}\u{AD6D}"));
/// $s->setAllowedChars('[a-z]'); var_dump($s->isSuspicious('123'), $s->isSuspicious('abc'));
/// $s->setRestrictionLevel(Spoofchecker::ASCII); var_dump($s->isSuspicious("ab\u{0430}"));
/// $s->setChecks(-1);
/// $c = clone $s; var_dump($c->isSuspicious('123'), $c->isSuspicious("a\xFF"));
/// try { $s->__construct(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $s->setRestrictionLevel(Spoofchecker::SINGLE_SCRIPT); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { $s->setAllowedChars('[a-z]', 1024); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { $s->setAllowedChars('a-z]'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { $s->setAllowedChars("[\xFF]"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { $s->setAllowedChars('[[:nope:]]'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// $u = (new ReflectionClass('Spoofchecker'))->newInstanceWithoutConstructor();
/// try { $u->isSuspicious('x'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { clone $u; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// class Typed { public string $x = ''; }
/// $t = new Typed; $s->areConfusable('a', 'a', $t->x); var_dump($t->x);
/// resourcebundle_create('en', 'nope');
/// try { new Spoofchecker(1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(intl_get_error_code());
/// ```
/// ```expect
/// bool(true)
/// int(16)
/// bool(false)
/// int(0)
/// bool(true)
/// int(1)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
///
/// Warning: Spoofchecker::setChecks(): (1) U_ILLEGAL_ARGUMENT_ERROR in %s on line 8
/// bool(true)
/// bool(true)
/// Spoofchecker object is already constructed
/// Spoofchecker::setRestrictionLevel(): Argument #1 ($level) must be one of Spoofchecker::ASCII, Spoofchecker::SINGLE_SCRIPT_RESTRICTIVE, Spoofchecker::HIGHLY_RESTRICTIVE, Spoofchecker::MODERATELY_RESTRICTIVE, Spoofchecker::MINIMALLY_RESTRICTIVE, or Spoofchecker::UNRESTRICTIVE
/// Spoofchecker::setAllowedChars(): Argument #2 ($patternOptions) must be a valid pattern option, 0 or (SpoofChecker::IGNORE_SPACE|(<none> or SpoofChecker::CASE_INSENSITIVE or SpoofChecker::ADD_CASE_MAPPINGS or SpoofChecker::SIMPLE_CASE_INSENSITIVE))
/// Spoofchecker::setAllowedChars(): Argument #1 ($pattern) must be a valid regular expression character set pattern
/// Spoofchecker::setAllowedChars(): Argument #1 ($pattern) string conversion to unicode encoding failed (10) U_INVALID_CHAR_FOUND
/// Spoofchecker::setAllowedChars(): Argument #1 ($pattern) must be a valid regular expression character set pattern (1) U_ILLEGAL_ARGUMENT_ERROR
/// Found unconstructed Spoofchecker
/// Cannot clone uninitialized SpoofChecker
/// string(1) "1"
/// Spoofchecker::__construct() expects exactly 0 arguments, 1 given
/// int(0)
/// ```
#[php_class(not_serializable, size = 88, payload = SpoofObject, handlers = crate::spoofchecker::HANDLERS)]
pub(crate) struct Spoofchecker;

#[php_class]
#[allow(non_snake_case)]
impl Spoofchecker {
    const SINGLE_SCRIPT_CONFUSABLE: i64 = 1;
    const MIXED_SCRIPT_CONFUSABLE: i64 = 2;
    const WHOLE_SCRIPT_CONFUSABLE: i64 = 4;
    const ANY_CASE: i64 = 8;
    const SINGLE_SCRIPT: i64 = 16;
    const INVISIBLE: i64 = 32;
    const CHAR_LIMIT: i64 = 64;
    const ASCII: i64 = 0x1000_0000;
    const HIGHLY_RESTRICTIVE: i64 = 0x3000_0000;
    const MODERATELY_RESTRICTIVE: i64 = 0x4000_0000;
    const MINIMALLY_RESTRICTIVE: i64 = 0x5000_0000;
    const UNRESTRICTIVE: i64 = 0x6000_0000;
    const SINGLE_SCRIPT_RESTRICTIVE: i64 = 0x2000_0000;
    const MIXED_NUMBERS: i64 = 128;
    const HIDDEN_OVERLAY: i64 = 256;
    const IGNORE_SPACE: i64 = 1;
    const CASE_INSENSITIVE: i64 = 2;
    const ADD_CASE_MAPPINGS: i64 = 4;
    const SIMPLE_CASE_INSENSITIVE: i64 = 6;

    /// `uspoof_open`, at `USPOOF_HIGHLY_RESTRICTIVE`, with a
    /// `USpoofCheckResult` beside it; php's `IntlException` where ICU opened none.
    #[php(before_args = error::reset)]
    fn __construct(req: &mut Request, this: This<Self>) -> Answer<()> {
        let ce = crate::throwables::Throwables::of(req)?;
        let object = this.object();
        if object
            .native::<SpoofObject>(req)
            .is_some_and(|o| o.uspoof.is_some())
        {
            let message = "Spoofchecker object is already constructed";
            return Err(throw_error(req, ce.error, message));
        }
        let mut status = Code::ZERO;
        let Some(mut checker) = Checker::open(&mut status) else {
            let message = "Spoofchecker::__construct(): unable to open ICU Spoof Checker";
            return Err(throw(req, ce.intl_exception, message));
        };
        checker.set_restriction_level(crate::truncated(Spoofchecker::HIGHLY_RESTRICTIVE));
        let result = CheckResult::open(&mut status);
        if let Some(o) = object.native_mut::<SpoofObject>(req) {
            o.uspoof = Some(checker);
            o.result = result;
        }
        Ok(())
    }

    #[php(tentative, before_args = error::reset)]
    fn isSuspicious(
        req: &mut Request,
        this: This<Self>,
        string: PhpString,
        #[php(default = null)] errorCode: Ref<Untyped<Value>>,
    ) -> Answer<bool> {
        let object = this.object();
        let mut status = Code::ZERO;
        let (checks, mask) = with(req, object, |checker, mut result| {
            let checks = checker.check(string.as_bytes(), result.as_deref_mut(), &mut status);
            let mut code = status;
            (checks, uspoof::result_checks(result.as_deref(), &mut code))
        })?;
        if warn_on_failure(req, status) {
            if mask != checks {
                let message = format!(
                    "unexpected error ({checks}), does not relate to the flags passed to setChecks ({mask})"
                );
                docref(req, Level::Warning, &message);
            }
            return Ok(true);
        }
        assign(req, &errorCode, checks)?;
        Ok(checks != 0)
    }

    #[php(tentative, before_args = error::reset)]
    fn areConfusable(
        req: &mut Request,
        this: This<Self>,
        string1: PhpString,
        string2: PhpString,
        #[php(default = null)] errorCode: Ref<Untyped<Value>>,
    ) -> Answer<bool> {
        let object = this.object();
        let mut status = Code::ZERO;
        let checks = with(req, object, |checker, _| {
            checker.confusable(string1.as_bytes(), string2.as_bytes(), &mut status)
        })?;
        if warn_on_failure(req, status) {
            return Ok(true);
        }
        assign(req, &errorCode, checks)?;
        Ok(checks != 0)
    }

    #[php(tentative, before_args = error::reset)]
    fn setAllowedLocales(req: &mut Request, this: This<Self>, locales: PhpString) -> Answer<()> {
        let locales = c_string(locales.as_bytes());
        let object = this.object();
        let mut status = Code::ZERO;
        with(req, object, |checker, _| {
            checker.set_allowed_locales(&locales, &mut status);
        })?;
        warn_on_failure(req, status);
        Ok(())
    }

    #[php(tentative, before_args = error::reset)]
    fn setChecks(req: &mut Request, this: This<Self>, checks: i64) -> Answer<()> {
        let object = this.object();
        let mut status = Code::ZERO;
        with(req, object, |checker, _| {
            checker.set_checks(crate::truncated(checks), &mut status);
        })?;
        warn_on_failure(req, status);
        Ok(())
    }

    #[php(tentative, before_args = error::reset)]
    fn setRestrictionLevel(req: &mut Request, this: This<Self>, level: i64) -> Answer<()> {
        let object = this.object();
        constructed(req, object)?;
        if !LEVELS.contains(&level) {
            let message = "be one of Spoofchecker::ASCII, Spoofchecker::SINGLE_SCRIPT_RESTRICTIVE, Spoofchecker::HIGHLY_RESTRICTIVE, Spoofchecker::MODERATELY_RESTRICTIVE, Spoofchecker::MINIMALLY_RESTRICTIVE, or Spoofchecker::UNRESTRICTIVE";
            return Err(arg_value_error(req, 1, message));
        }
        with(req, object, |checker, _| {
            checker.set_restriction_level(crate::truncated(level));
        })
    }

    #[php(before_args = error::reset)]
    fn setAllowedChars(
        req: &mut Request,
        this: This<Self>,
        pattern: PhpString,
        #[php(default = 0)] patternOptions: i64,
    ) -> Answer<()> {
        let ce = crate::throwables::Throwables::of(req)?;
        let object = this.object();
        constructed(req, object)?;
        let pattern = pattern.as_bytes();
        if ffi::length(pattern.len()).is_err() {
            let message = "be less than or equal to 2147483647 bytes long";
            return Err(arg_value_error(req, 1, message));
        }
        let set_pattern = "be a valid regular expression character set pattern";
        if pattern.first() != Some(&b'[') || pattern.last() != Some(&b']') {
            return Err(arg_value_error(req, 1, set_pattern));
        }
        let units = match ffi::to_utf16(pattern) {
            Ok(units) => units,
            Err(code) => {
                let message = format!(
                    "string conversion to unicode encoding failed ({}) {}",
                    code.0,
                    code.name()
                );
                return Err(argument_error(req, ce.value_error, 1, &message));
            }
        };
        if !PATTERN_OPTIONS.contains(&patternOptions) {
            let message = "be a valid pattern option, 0 or (SpoofChecker::IGNORE_SPACE|(<none> or SpoofChecker::CASE_INSENSITIVE or SpoofChecker::ADD_CASE_MAPPINGS or SpoofChecker::SIMPLE_CASE_INSENSITIVE))";
            return Err(arg_value_error(req, 2, message));
        }
        let mut status = Code::ZERO;
        let Some(mut set) = Set::empty() else {
            return Err(throw_error(req, ce.error, "Out of memory opening a USet"));
        };
        let options = u32::try_from(patternOptions).unwrap_or(0);
        set.apply_pattern(&units, options, &mut status);
        if status.is_failure() {
            let message = format!("{set_pattern} ({}) {}", status.0, status.name());
            return Err(arg_value_error(req, 1, &message));
        }
        with(req, object, |checker, _| {
            checker.set_allowed_chars(&set, &mut status);
        })?;
        warn_on_failure(req, status);
        Ok(())
    }
}
