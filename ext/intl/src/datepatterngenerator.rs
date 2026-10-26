//! `dateformat/datepatterngenerator_*`: `IntlDatePatternGenerator` over
//! an `icu::DateTimePatternGenerator`, which the shim holds
//! ([`crate::ffi::dateformat::Dtpg`]); the best pattern for a skeleton in
//! the generator's locale.

use php_ext::abi::{CloneObj, Instance, ObjectHandlers, OrFalse, Raw, This};
use php_ext::attr::php_class;
use php_ext::call::{clone_with, lookup_class, new_object};
use php_ext::raising::{Thrown, arity, fatal, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, PhpString};

use crate::error::{self, Carrier, IntlError};
use crate::ffi::cpp::{CName, Text};
use crate::ffi::dateformat::{self as df, Dtpg};
use crate::ffi::{self, Code, c_string};

/// `IntlDatePatternGenerator_object` past its `zend_object`: the error
/// and the generator, none until a constructor makes one.
#[derive(Default)]
pub(crate) struct GeneratorObject {
    error: IntlError,
    dtpg: Option<Dtpg>,
}

impl Carrier for GeneratorObject {
    fn error(&mut self) -> &mut IntlError {
        &mut self.error
    }
}

/// `IntlDatePatternGenerator_handlers`: `std_object_handlers` with php's
/// clone.
pub(crate) struct Handlers;
pub(crate) static HANDLERS: Handlers = Handlers;

impl ObjectHandlers for Handlers {
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_generator)
    }
}

/// `IntlDatePatternGenerator_object_clone`: the members and the
/// generator's clone, or php's `Error` once the copy is made.
fn clone_generator(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    let copy = object
        .native::<GeneratorObject>(req)
        .and_then(|o| o.dtpg.as_deref())
        .map(|d| df::php_intl_dtpg_clone(d));
    let message = match &copy {
        None => "Cannot clone uninitialized IntlDatePatternGenerator",
        Some(None) => "Failed to clone IntlDatePatternGenerator",
        Some(Some(_)) => "",
    };
    let payload = GeneratorObject {
        error: IntlError::default(),
        dtpg: copy.flatten(),
    };
    let new = clone_with(req, &object, payload)?;
    if message.is_empty() {
        return Ok(new);
    }
    new.give_up(req);
    Err(throw_error(req, ce.error, message))
}

/// `dtpg_ctor`: `Ok(false)` for php's `FAILURE` where nothing was thrown.
fn construct(
    req: &mut Request,
    object: PhpObject,
    locale: &Raw<Option<PhpString>>,
) -> Result<bool, Thrown> {
    arity(req, 0, Some(1))?;
    let locale = locale.hold(req)?;
    error::reset_object::<GeneratorObject>(req, object);
    if object
        .native::<GeneratorObject>(req)
        .is_some_and(|o| o.dtpg.is_some())
    {
        let message = "Cannot call constructor twice";
        error::errors_set::<GeneratorObject>(req, object, Code::ILLEGAL_ARGUMENT, message)?;
        return Ok(false);
    }
    let given = locale.as_ref().map_or(&b""[..], PhpString::as_bytes);
    if crate::formatter::locale_too_long(req, given.len())? {
        return Ok(false);
    }
    let name = if given.is_empty() {
        c_string(&crate::default_locale(req))
    } else {
        c_string(given)
    };
    let mut status = Code::ZERO;
    let dtpg = df::php_intl_dtpg_create(CName::from(name.as_c_str()), &mut status);
    if let Some(o) = object.native_mut::<GeneratorObject>(req) {
        o.error.code = status;
        o.dtpg = dtpg;
    }
    if status.is_failure() {
        error::set(req, status, "Error creating DateTimePatternGenerator")?;
        return Ok(false);
    }
    Ok(true)
}

/// `IntlDatePatternGenerator::create`: null where the constructor failed.
fn create(
    req: &mut Request,
    locale: &Raw<Option<PhpString>>,
) -> Result<Option<Owner<Instance<IntlDatePatternGenerator>>>, Thrown> {
    let Some(class) = lookup_class(req, b"IntlDatePatternGenerator")? else {
        return Err(fatal(
            req,
            "InternalError: IntlDatePatternGenerator is not linked",
        ));
    };
    let object = new_object(req, class)?;
    match construct(req, *object, locale) {
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

/// `INTL_METHOD_CHECK_STATUS`: the code on the object and the request,
/// a failure's message set and `false` answered.
fn check(req: &mut Request, object: PhpObject, code: Code, message: &str) -> Result<bool, Thrown> {
    error::errors_set_code::<GeneratorObject>(req, object, code);
    match error::check_object::<GeneratorObject>(req, object, message) {
        Ok(()) => Ok(true),
        Err(None) => Ok(false),
        Err(Some(thrown)) => Err(thrown),
    }
}

/// `getBestPattern`: the skeleton cleaned, its best pattern in UTF-8.
fn best_pattern(
    req: &mut Request,
    object: PhpObject,
    skeleton: &[u8],
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    error::reset_object::<GeneratorObject>(req, object);
    let units = ffi::to_utf16(skeleton);
    let taken = object
        .native_mut::<GeneratorObject>(req)
        .and_then(|o| o.dtpg.take());
    let Some(mut dtpg) = taken else {
        return Err(throw_error(
            req,
            ce.error,
            "Found unconstructed IntlDatePatternGenerator",
        ));
    };
    let (mut stage, mut status) = (0, Code::ZERO);
    let made = units.as_ref().ok().and_then(|units| {
        df::php_intl_dtpg_best_pattern(
            &mut dtpg,
            Text::from(units.as_slice()),
            &mut stage,
            &mut status,
        )
    });
    if let Some(o) = object.native_mut::<GeneratorObject>(req) {
        o.dtpg = Some(dtpg);
    }
    let code = units.as_ref().err().copied().unwrap_or(Code::ZERO);
    if !check(req, object, code, "Skeleton is not a valid UTF-8 string")? {
        return Ok(OrFalse::False);
    }
    let failed = [
        (1, "Error getting cleaned skeleton"),
        (2, "Error retrieving pattern"),
    ];
    for (at, message) in failed {
        let code = if stage == at { status } else { Code::ZERO };
        if !check(req, object, code, message)? {
            return Ok(OrFalse::False);
        }
    }
    let converted = made.map_or(Ok(Vec::new()), |text| text.to_utf8());
    let code = converted.as_ref().err().copied().unwrap_or(Code::ZERO);
    if !check(req, object, code, "Error converting result to UTF-8")? {
        return Ok(OrFalse::False);
    }
    PhpString::new(req, &converted.unwrap_or_default()).map(OrFalse::Value)
}

/// `class IntlDatePatternGenerator`: the best pattern for a skeleton in
/// a locale, cloned with the object.
///
/// ```php
/// $g = new IntlDatePatternGenerator('de_DE');
/// var_dump($g->getBestPattern('YYYYMMMddjjmm'), (clone $g)->getBestPattern('yMd'), IntlDatePatternGenerator::create('en_US')->getBestPattern('jjmm'));
/// var_dump($g->getBestPattern("jj\x80"), intl_get_error_message());
/// $u = (new ReflectionClass('IntlDatePatternGenerator'))->newInstanceWithoutConstructor();
/// try { clone $u; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $u->getBestPattern('y'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $g->__construct(); } catch (IntlException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(19) "dd. MMM YYYY, HH:mm"
/// string(5) "d.M.y"
/// string(8) "h:mm a"
/// bool(false)
/// string(102) "IntlDatePatternGenerator::getBestPattern(): Skeleton is not a valid UTF-8 string: U_INVALID_CHAR_FOUND"
/// Cannot clone uninitialized IntlDatePatternGenerator
/// Found unconstructed IntlDatePatternGenerator
/// IntlDatePatternGenerator::__construct(): Cannot call constructor twice
/// ```
#[php_class(not_serializable, size = 80, payload = GeneratorObject, handlers = crate::datepatterngenerator::HANDLERS)]
pub(crate) struct IntlDatePatternGenerator;

#[php_class]
#[allow(non_snake_case)]
impl IntlDatePatternGenerator {
    #[php(arity = manual, before_args = error::reset)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] locale: Raw<Option<PhpString>>,
    ) -> Result<(), Thrown> {
        let object = this.object();
        error::constructing(req, |req| construct(req, object, &locale).map(|_| ()))
    }

    #[php(arity = manual, before_args = error::reset)]
    fn create(
        req: &mut Request,
        #[php(default = null)] locale: Raw<Option<PhpString>>,
    ) -> Result<Option<Owner<Instance<IntlDatePatternGenerator>>>, Thrown> {
        create(req, &locale)
    }

    #[php(before_args = error::reset)]
    fn getBestPattern(
        req: &mut Request,
        this: This<Self>,
        skeleton: PhpString,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        best_pattern(req, this.object(), skeleton.as_bytes())
    }
}
