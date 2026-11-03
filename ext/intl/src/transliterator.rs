//! `transliterator/`: `Transliterator` over ICU's `UTransliterator`, the
//! C API php calls, and the `transliterator_*` functions its methods
//! share a body with (`transliterator_methods.c`,
//! `transliterator_class.c`).

use php_ext::abi::{CloneObj, Instance, ObjectHandlers, OrFalse, This, Union};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{clone_with, lookup_class, new_object, write_prop};
use php_ext::coerce::c_str;
use php_ext::raising::{Level, Thrown, arg_value_error, docref, fatal, throw, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString};

use crate::error::{self, Carrier, IntlError};
use crate::ffi::transliterator::{Trans, ids};
use crate::ffi::{Code, ParseError, to_utf8, to_utf16};

type Answer<T> = Result<T, Thrown>;

/// `TRANSLITERATOR_FORWARD`, `UTRANS_FORWARD`.
const FORWARD: i64 = 0;
/// `TRANSLITERATOR_REVERSE`, `UTRANS_REVERSE`.
const REVERSE: i64 = 1;

/// `Transliterator_object`: the error and the ICU transliterator.
#[derive(Default)]
pub(crate) struct TransObject {
    error: IntlError,
    utrans: Option<Trans>,
}

impl Carrier for TransObject {
    fn error(&mut self) -> &mut IntlError {
        &mut self.error
    }
}

/// `Transliterator_handlers`: the clone a clone of ICU's transliterator.
pub(crate) static HANDLERS: Handlers = Handlers;

/// The handlers of `Transliterator` and its subclasses.
pub(crate) struct Handlers;

impl ObjectHandlers for Handlers {
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_trans)
    }
}

/// `class Transliterator`: made by its factories, never by `new`; its
/// readonly `id` is ICU's id of the transliterator.
///
/// ```php
/// $t = Transliterator::create('Latin-ASCII');
/// var_dump($t->id, $t->transliterate('Ærøskøbing'), $t->transliterate('façade', 2, 4), $t->getErrorCode(), $t->getErrorMessage());
/// var_dump($t->transliterate('ab', 3), $t->getErrorCode(), $t->getErrorMessage());
/// try { $t->transliterate('ab', 1, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// $r = Transliterator::createFromRules('a > b; b > a;', Transliterator::REVERSE);
/// var_dump($r->id, $r->transliterate('abc'), Transliterator::createFromRules('a >'), intl_get_error_message());
/// var_dump(Transliterator::create('Any-Hex')->createInverse()->id, Transliterator::create('nope'), intl_get_error_message());
/// var_dump(in_array('Any-Latin', Transliterator::listIDs()), Transliterator::FORWARD, Transliterator::REVERSE);
/// try { Transliterator::create('Any-Hex', 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { new Transliterator; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { (new ReflectionMethod('Transliterator', '__construct'))->invoke($t); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// $c = clone $t; var_dump($c->id, $c->transliterate('ø'));
/// $u = (new ReflectionClass('Transliterator'))->newInstanceWithoutConstructor();
/// try { $u->transliterate('x'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { clone $u; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(11) "Latin-ASCII"
/// string(11) "AEroskobing"
/// string(6) "facade"
/// int(0)
/// string(12) "U_ZERO_ERROR"
/// bool(false)
/// int(1)
/// string(159) "Transliterator::transliterate(): Neither "start" nor the "end" arguments can exceed the number of UTF-16 code units (in this case, 2): U_ILLEGAL_ARGUMENT_ERROR"
/// Transliterator::transliterate(): Argument #2 ($start) must be less than or equal to argument #3 ($end)
/// string(13) "RulesTransPHP"
/// string(3) "abc"
/// NULL
/// string(147) "Transliterator::createFromRules(): unable to create ICU transliterator from rules (parse error at offset 0, before or at "a >"): U_MISSING_OPERATOR"
/// string(7) "Hex-Any"
/// NULL
/// string(88) "Transliterator::create(): unable to open ICU transliterator with id "nope": U_INVALID_ID"
/// bool(true)
/// int(0)
/// int(1)
/// Transliterator::create(): Argument #2 ($direction) must be either Transliterator::FORWARD or Transliterator::REVERSE
/// Call to private Transliterator::__construct() from global scope
/// An object of this type cannot be created with the new operator.
/// string(11) "Latin-ASCII"
/// string(1) "o"
/// Found unconstructed transliterator
/// Cannot clone uninitialized Transliterator
/// ```
#[php_class(not_serializable, size = 80, payload = TransObject, handlers = crate::transliterator::HANDLERS)]
pub(crate) struct Transliterator;

#[php_class]
impl Transliterator {
    const FORWARD: i64 = FORWARD;
    const REVERSE: i64 = REVERSE;

    #[php(property, readonly)]
    const id: PhpString;

    #[php(private, final)]
    fn __construct(req: &mut Request, _: This<Self>) -> Answer<()> {
        let ce = crate::throwables::Throwables::of(req)?;
        Err(throw(
            req,
            ce.exception,
            "An object of this type cannot be created with the new operator.",
        ))
    }

    #[php(tentative, before_args = error::reset)]
    fn create(
        req: &mut Request,
        id: PhpString,
        #[php(default = Transliterator::FORWARD)] direction: i64,
    ) -> Answer<Option<Owner<Instance<Transliterator>>>> {
        create(req, id.as_bytes(), direction)
    }

    #[php(tentative, before_args = error::reset)]
    fn createFromRules(
        req: &mut Request,
        rules: PhpString,
        #[php(default = Transliterator::FORWARD)] direction: i64,
    ) -> Answer<Option<Owner<Instance<Transliterator>>>> {
        create_from_rules(req, rules.as_bytes(), direction)
    }

    #[php(tentative, before_args = error::reset)]
    fn createInverse(
        req: &mut Request,
        this: This<Self>,
    ) -> Answer<Option<Owner<Instance<Transliterator>>>> {
        create_inverse(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    fn listIDs(req: &mut Request) -> Answer<OrFalse<Owner<PhpArray>>> {
        list_ids(req)
    }

    #[php(tentative, before_args = error::reset)]
    fn transliterate(
        req: &mut Request,
        this: This<Self>,
        string: PhpString,
        #[php(default = 0)] start: i64,
        #[php(default = -1)] end: i64,
    ) -> Answer<OrFalse<Owner<PhpString>>> {
        transliterate(req, this.object(), string.as_bytes(), start, end, 0)
    }

    #[php(tentative, before_args = error::reset)]
    fn getErrorCode(req: &mut Request, this: This<Self>) -> i64 {
        error_code(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    fn getErrorMessage(req: &mut Request, this: This<Self>) -> Answer<Owner<PhpString>> {
        error_message(req, this.object())
    }
}

/// `transliterator_create(string $id, int $direction = Transliterator::FORWARD): ?Transliterator`.
///
/// ```php
/// var_dump(transliterator_create('Any-Upper')->transliterate('abc'), transliterator_create('[\x8F'), intl_get_error_message());
/// ```
/// ```expect
/// string(3) "ABC"
/// NULL
/// string(88) "transliterator_create(): unable to open ICU transliterator with id "[\x8F": U_INVALID_ID"
/// ```
#[php_function(before_args = error::reset)]
fn transliterator_create(
    req: &mut Request,
    id: PhpString,
    #[php(default = Transliterator::FORWARD)] direction: i64,
) -> Answer<Option<Owner<Instance<Transliterator>>>> {
    create(req, id.as_bytes(), direction)
}

/// `transliterator_create_from_rules(string $rules, int $direction = Transliterator::FORWARD): ?Transliterator`.
///
/// ```php
/// var_dump(transliterator_create_from_rules('x > y;')->transliterate('xox'), transliterator_create_from_rules("\x8F"), intl_get_error_message());
/// try { transliterator_create_from_rules('x > y;', -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "yoy"
/// NULL
/// string(101) "transliterator_create_from_rules(): String conversion of rules to UTF-16 failed: U_INVALID_CHAR_FOUND"
/// transliterator_create_from_rules(): Argument #2 ($direction) must be either Transliterator::FORWARD or Transliterator::REVERSE
/// ```
#[php_function(before_args = error::reset)]
fn transliterator_create_from_rules(
    req: &mut Request,
    rules: PhpString,
    #[php(default = Transliterator::FORWARD)] direction: i64,
) -> Answer<Option<Owner<Instance<Transliterator>>>> {
    create_from_rules(req, rules.as_bytes(), direction)
}

/// `transliterator_list_ids(): array|false`.
///
/// ```php
/// var_dump(in_array('Latin-ASCII', transliterator_list_ids()), intl_get_error_code());
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// ```
#[php_function(before_args = error::reset)]
fn transliterator_list_ids(req: &mut Request) -> Answer<OrFalse<Owner<PhpArray>>> {
    list_ids(req)
}

/// `transliterator_create_inverse(Transliterator $transliterator): ?Transliterator`.
///
/// ```php
/// var_dump(transliterator_create_inverse(Transliterator::create('Latin-Greek'))->id);
/// ```
/// ```expect
/// string(11) "Greek-Latin"
/// ```
#[php_function(before_args = error::reset)]
fn transliterator_create_inverse(
    req: &mut Request,
    transliterator: Instance<Transliterator>,
) -> Answer<Option<Owner<Instance<Transliterator>>>> {
    create_inverse(req, transliterator.object())
}

/// `transliterator_transliterate(Transliterator|string $transliterator, string $string, int $start = 0, int $end = -1): string|false`:
/// a string opens a transliterator for the call.
///
/// ```php
/// var_dump(transliterator_transliterate('Any-Hex', 'é'), transliterator_transliterate(Transliterator::create('Any-Lower'), 'ABC', 1));
/// var_dump(transliterator_transliterate('nope', 'x'));
/// try { transliterator_transliterate('Any-Hex', 'x', -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { transliterator_transliterate('Any-Hex', 'x', 0, -2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(6) "\u00E9"
/// string(3) "Abc"
///
/// Warning: transliterator_transliterate(): Could not create transliterator with ID "nope" (transliterator_transliterate(): unable to open ICU transliterator with id "nope": U_INVALID_ID) in %s on line 3
/// bool(false)
/// transliterator_transliterate(): Argument #3 ($start) must be greater than or equal to 0
/// transliterator_transliterate(): Argument #4 ($end) must be greater than or equal to -1
/// ```
#[php_function(before_args = error::reset)]
fn transliterator_transliterate(
    req: &mut Request,
    transliterator: Union<Instance<Transliterator>, PhpString>,
    string: PhpString,
    #[php(default = 0)] start: i64,
    #[php(default = -1)] end: i64,
) -> Answer<OrFalse<Owner<PhpString>>> {
    let id = match transliterator {
        Union::A(object) => {
            return transliterate(req, object.object(), string.as_bytes(), start, end, 1);
        }
        Union::B(id) => id,
    };
    let Some(made) = create(req, id.as_bytes(), FORWARD)? else {
        let message = String::from_utf8_lossy(&error::message(req)).into_owned();
        let id = String::from_utf8_lossy(c_str(id.as_bytes())).into_owned();
        let text = format!("Could not create transliterator with ID \"{id}\" ({message})");
        docref(req, Level::Warning, &text);
        return Ok(OrFalse::False);
    };
    let object = made.object();
    let answer = transliterate(req, object, string.as_bytes(), start, end, 1);
    made.give_up(req);
    answer
}

/// `transliterator_get_error_code(Transliterator $transliterator): int`.
///
/// ```php
/// $t = Transliterator::create('Any-Hex');
/// $t->transliterate("\x8F");
/// var_dump(transliterator_get_error_code($t), intl_get_error_code());
/// ```
/// ```expect
/// int(10)
/// int(0)
/// ```
#[php_function(before_args = error::reset)]
fn transliterator_get_error_code(
    req: &mut Request,
    transliterator: Instance<Transliterator>,
) -> i64 {
    error_code(req, transliterator.object())
}

/// `transliterator_get_error_message(Transliterator $transliterator): string`.
///
/// ```php
/// $t = Transliterator::create('Any-Hex');
/// var_dump(transliterator_get_error_message($t));
/// $t->transliterate('x', 5);
/// var_dump(transliterator_get_error_message($t));
/// ```
/// ```expect
/// string(12) "U_ZERO_ERROR"
/// string(159) "Transliterator::transliterate(): Neither "start" nor the "end" arguments can exceed the number of UTF-16 code units (in this case, 1): U_ILLEGAL_ARGUMENT_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn transliterator_get_error_message(
    req: &mut Request,
    transliterator: Instance<Transliterator>,
) -> Answer<Owner<PhpString>> {
    error_message(req, transliterator.object())
}

/// The procedural functions, in `php_intl.stub.php`'s order.
pub(crate) fn functions(b: php_ext::abi::ExtensionBuilder) -> php_ext::abi::ExtensionBuilder {
    b.functions(&[
        transliterator_create::DECLARED,
        transliterator_create_from_rules::DECLARED,
        transliterator_list_ids::DECLARED,
        transliterator_create_inverse::DECLARED,
        transliterator_transliterate::DECLARED,
        transliterator_get_error_code::DECLARED,
        transliterator_get_error_message::DECLARED,
    ])
}

/// The direction held to the two php names.
fn direction(req: &mut Request, direction: i64) -> Answer<i32> {
    match direction {
        FORWARD => Ok(0),
        REVERSE => Ok(1),
        _ => Err(arg_value_error(
            req,
            2,
            "be either Transliterator::FORWARD or Transliterator::REVERSE",
        )),
    }
}

/// `create_transliterator`: ICU's transliterator by id, or null with the
/// request's error set.
fn create(
    req: &mut Request,
    id: &[u8],
    dir: i64,
) -> Answer<Option<Owner<Instance<Transliterator>>>> {
    error::reset(req);
    let dir = direction(req, dir)?;
    let uid = match to_utf16(id) {
        Ok(uid) => uid,
        Err(code) => {
            error::set_code(req, code);
            error::set_custom_msg(req, "String conversion of id to UTF-16 failed")?;
            return Ok(None);
        }
    };
    match Trans::open(&uid, dir, None, &mut ParseError::default()) {
        Ok(utrans) => made(req, utrans),
        Err(code) => {
            error::set_code(req, code);
            let id = String::from_utf8_lossy(c_str(id));
            let message = format!("unable to open ICU transliterator with id \"{id}\"");
            error::set_custom_msg(req, &message)?;
            Ok(None)
        }
    }
}

/// `transliterator_create_from_rules`: ICU's transliterator over rules,
/// named `RulesTransPHP`.
fn create_from_rules(
    req: &mut Request,
    rules: &[u8],
    dir: i64,
) -> Answer<Option<Owner<Instance<Transliterator>>>> {
    let dir = direction(req, dir)?;
    let urules = match to_utf16(rules) {
        Ok(urules) => urules,
        Err(code) => {
            error::set_code(req, code);
            error::set_custom_msg(req, "String conversion of rules to UTF-16 failed")?;
            return Ok(None);
        }
    };
    let id: Vec<u16> = "RulesTransPHP".encode_utf16().collect();
    let mut pe = ParseError::default();
    match Trans::open(&id, dir, Some(&urules), &mut pe) {
        Ok(utrans) => made(req, utrans),
        Err(code) => {
            error::set_code(req, code);
            let parsed = error::parse_error(&pe);
            let message = format!("unable to create ICU transliterator from rules ({parsed})");
            error::set_custom_msg(req, &message)?;
            Ok(None)
        }
    }
}

/// `transliterator_create_inverse`.
fn create_inverse(
    req: &mut Request,
    object: PhpObject,
) -> Answer<Option<Owner<Instance<Transliterator>>>> {
    match with(req, object, Trans::inverse)? {
        Ok(utrans) => made(req, utrans),
        Err(code) => {
            error::set_code(req, code);
            error::set_custom_msg(req, "could not create inverse ICU transliterator")?;
            Ok(None)
        }
    }
}

/// `transliterator_list_ids`: every id ICU registers.
fn list_ids(req: &mut Request) -> Answer<OrFalse<Owner<PhpArray>>> {
    let mut all = match ids() {
        Ok(all) => all,
        Err(code) => {
            error::set_code(req, code);
            error::set_custom_msg(req, "Failed to obtain registered transliterators")?;
            return Ok(OrFalse::False);
        }
    };
    let mut array = PhpArray::new(req)?;
    let mut status = Code::ZERO;
    while let Some(id) = all.next(&mut status) {
        let id = PhpString::new(req, &id)?;
        array.append(req, id.into_value())?;
    }
    error::set_code(req, status);
    if status.is_failure() {
        array.give_up(req);
        error::set_custom_msg(req, "Failed to build array of registered transliterators")?;
        return Ok(OrFalse::False);
    }
    Ok(OrFalse::Value(array))
}

/// `transliterator_transliterate`'s body over a transliterator: `shift`
/// is 1 where the function's first argument moves the others.
fn transliterate(
    req: &mut Request,
    object: PhpObject,
    text: &[u8],
    start: i64,
    end: i64,
    shift: u32,
) -> Answer<OrFalse<Owner<PhpString>>> {
    if end < -1 {
        let tail = "be greater than or equal to -1";
        return Err(arg_value_error(req, 3 + shift, tail));
    }
    if start < 0 {
        let tail = "be greater than or equal to 0";
        return Err(arg_value_error(req, 2 + shift, tail));
    }
    if end != -1 && start > end {
        let tail = format!("be less than or equal to argument #{} ($end)", 3 + shift);
        return Err(arg_value_error(req, 2 + shift, &tail));
    }
    with(req, object, |_| ())?;
    let utext = match to_utf16(text) {
        Ok(utext) => utext,
        Err(code) => {
            let message = "String conversion of string to UTF-16 failed";
            return fail(req, object, code, message);
        }
    };
    let len = i64::try_from(utext.len()).unwrap_or(i64::MAX);
    if start > len || end > len {
        let message = format!(
            "Neither \"start\" nor the \"end\" arguments can exceed the number of UTF-16 code units (in this case, {len})"
        );
        return fail(req, object, Code::ILLEGAL_ARGUMENT, &message);
    }
    let limit = if end == -1 { len } else { end };
    let (start, limit) = (
        i32::try_from(start).unwrap_or(0),
        i32::try_from(limit).unwrap_or(0),
    );
    let done = with(req, object, |t| run_trans(t, &utext, start, limit))?;
    let converted = match done {
        Ok(out) => to_utf8(&out),
        Err(code) => return fail(req, object, code, "transliteration failed"),
    };
    match converted {
        Ok(bytes) => PhpString::new(req, &bytes).map(OrFalse::Value),
        Err(code) => fail(req, object, code, "Error converting value to UTF-8"),
    }
}

/// `utrans_transUChars` over a copy of `text` from `start` to `limit`,
/// the buffer grown and the copy transliterated again where the answer
/// overflows it, as `transliterator_transliterate` loops.
fn run_trans(utrans: &Trans, text: &[u16], start: i32, limit: i32) -> Result<Vec<u16>, Code> {
    let mut capacity = text.len() + 1;
    loop {
        let mut buffer = text.to_vec();
        buffer.resize(capacity.max(text.len() + 1), 0);
        let mut len = i32::try_from(text.len()).map_err(|_| Code::INDEX_OUTOFBOUNDS)?;
        let status = utrans.trans_uchars(&mut buffer, &mut len, start, limit);
        if status == Code::BUFFER_OVERFLOW {
            capacity = usize::try_from(len).unwrap_or(0) + 1;
            continue;
        }
        status.check()?;
        buffer.truncate(usize::try_from(len).unwrap_or(0));
        return Ok(buffer);
    }
}

/// `intl_errors_set(obj, code, msg)`, then `false`.
fn fail(
    req: &mut Request,
    object: PhpObject,
    code: Code,
    message: &str,
) -> Answer<OrFalse<Owner<PhpString>>> {
    error::errors_set::<TransObject>(req, object, code, message)?;
    Ok(OrFalse::False)
}

/// `transliterator_get_error_code`: the object's code, its error kept.
fn error_code(req: &mut Request, object: PhpObject) -> i64 {
    i64::from(error::object_code::<TransObject>(req, object).0)
}

/// `transliterator_get_error_message`: the object's message, its error
/// kept.
fn error_message(req: &mut Request, object: PhpObject) -> Answer<Owner<PhpString>> {
    let message = error::object_message::<TransObject>(req, object);
    PhpString::new(req, &message)
}

/// `transliterator_object_construct` over a new object: the
/// transliterator held and `id` written.
fn made(req: &mut Request, utrans: Trans) -> Answer<Option<Owner<Instance<Transliterator>>>> {
    let id = to_utf8(&utrans.id());
    let Some(class) = lookup_class(req, b"Transliterator")? else {
        return Err(fatal(req, "InternalError: Transliterator is not linked"));
    };
    let object = new_object(req, class)?;
    if let Some(o) = object.native_mut::<TransObject>(req) {
        o.utrans = Some(utrans);
    }
    let written = match id {
        Ok(id) => PhpString::new(req, &id)
            .and_then(|id| write_prop(req, &object, b"id", id.into_value()))
            .map(|()| true),
        Err(code) => {
            error::set_code(req, code);
            error::set_custom_msg(req, "internal constructor call failed").map(|()| false)
        }
    };
    match written {
        Ok(true) => Ok(Some(object.into_instance())),
        Ok(false) => {
            object.give_up(req);
            Ok(None)
        }
        Err(thrown) => {
            object.give_up(req);
            Err(thrown)
        }
    }
}

/// `TRANSLITERATOR_METHOD_FETCH_OBJECT`, then `f` over the
/// transliterator: the object's error reset, php's `Error` where it
/// holds none.
fn with<T>(req: &mut Request, object: PhpObject, f: impl FnOnce(&Trans) -> T) -> Answer<T> {
    let ce = crate::throwables::Throwables::of(req)?;
    error::reset_object::<TransObject>(req, object);
    match object
        .native::<TransObject>(req)
        .and_then(|o| o.utrans.as_ref())
    {
        Some(utrans) => Ok(f(utrans)),
        None => Err(throw_error(
            req,
            ce.error,
            "Found unconstructed transliterator",
        )),
    }
}

/// `Transliterator_clone_obj`: a clone of ICU's transliterator.
fn clone_trans(req: &mut Request, object: PhpObject) -> Answer<Owner<PhpObject>> {
    let ce = crate::throwables::Throwables::of(req)?;
    let copy = match object.native::<TransObject>(req) {
        Some(TransObject {
            utrans: Some(utrans),
            ..
        }) => utrans.try_clone(),
        _ => {
            return Err(throw_error(
                req,
                ce.error,
                "Cannot clone uninitialized Transliterator",
            ));
        }
    };
    let Ok(copy) = copy else {
        return Err(throw_error(req, ce.error, "Failed to clone Transliterator"));
    };
    let payload = TransObject {
        error: IntlError::default(),
        utrans: Some(copy),
    };
    clone_with(req, &object, payload)
}
