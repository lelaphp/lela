//! `resourcebundle/`: `ResourceBundle` and the `resourcebundle_*`
//! functions over ICU's `ures_*` (`resourcebundle_class.c`,
//! `resourcebundle.c`, `resourcebundle_iterator.c`). A bundle reads its
//! elements by key or index, through `get()`, `$bundle[...]` and a
//! `foreach`; a table or an array element is a `ResourceBundle` of its
//! own. Each method php aliases to a function shares one body here with
//! it. No PHP code runs while a bundle is read, so nothing is held across
//! a call (ADR-0013).

use php_ext::abi::{
    ClassIterator, DimensionRead, Instance, IntoReturn, ObjectHandlers, OrFalse, Raw, Refusals,
    This, Union, Untyped,
};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{internal_iterator, lookup_class, new_object};
use php_ext::raising::{
    Thrown, arg_value_error, argument_error, fatal, illegal_container_offset, throw_error,
    value_error,
};
use php_ext::request::{MAXPATHLEN, Request};
use php_ext::values::{Layout, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::breakiterator::IteratorAggregate;
use crate::error::{self, Carrier, IntlError};
use crate::ffi::resourcebundle::{Bundle, Kind};
use crate::ffi::{self, Code, c_string, locale as uloc};
use crate::formatter::locale_too_long;

/// What a lookup answers where it fell back to a parent locale or the
/// default one.
const FELL_BACK: [Code; 2] = [Code::USING_FALLBACK, Code::USING_DEFAULT];

/// `ResourceBundle_object` past its `zend_object`: the error and the
/// bundle a constructor opened, or an element handed on.
#[derive(Default)]
pub(crate) struct BundleObject {
    error: IntlError,
    me: Option<Bundle>,
}

impl Carrier for BundleObject {
    fn error(&mut self) -> &mut IntlError {
        &mut self.error
    }
}

/// The object's bundle, none where no constructor opened one.
fn bundle(req: &mut Request, object: PhpObject) -> Option<&Bundle> {
    object.native::<BundleObject>(req)?.me.as_ref()
}

/// What `resourcebundle_extract_value` answers, typed as `get()` declares
/// it: a bundle for a table or an array, an integer vector's array, a
/// string or binary's bytes, an integer; null where the element was not
/// found, and php's unverified `false` where reading it failed.
type Element = Option<
    Union<
        Union<
            Owner<Instance<ResourceBundle>>,
            Union<Owner<PhpArray>, Union<Owner<PhpString>, i64>>,
        >,
        Untyped<bool>,
    >,
>;

/// php's `RETURN_FALSE` from `INTL_METHOD_CHECK_STATUS`.
const FALSE: Element = Some(Union::B(Untyped(false)));

/// `INTL_METHOD_CHECK_STATUS(source, message)` after ICU wrote `status`
/// into the object's code: `false` where it is a failure.
fn checked(
    req: &mut Request,
    object: PhpObject,
    status: Code,
    message: &str,
) -> Result<bool, Thrown> {
    error::errors_set_code::<BundleObject>(req, object, status);
    match error::check_object::<BundleObject>(req, object, message) {
        Ok(()) => Ok(true),
        Err(None) => Ok(false),
        Err(Some(thrown)) => Err(thrown),
    }
}

/// `resourcebundle_extract_value`: the element as PHP reads it, ICU's
/// reads continuing the object's code as php's write into it. A table
/// or an array is handed on to a new `ResourceBundle`, the object's
/// errors reset.
fn extract(req: &mut Request, object: PhpObject, child: Bundle) -> Result<Element, Thrown> {
    let mut status = error::object_code::<BundleObject>(req, object);
    let value = match child.kind() {
        Kind::String => {
            let units = child.string(&mut status);
            if !checked(req, object, status, "Failed to retrieve string value")? {
                return Ok(FALSE);
            }
            let converted = ffi::to_utf8(&units);
            let code = converted.as_ref().err().copied().unwrap_or(Code::ZERO);
            let message = "Error converting value to UTF-8";
            match converted {
                Ok(bytes) if checked(req, object, code, message)? => {
                    Union::B(Union::B(Union::A(PhpString::new(req, &bytes)?)))
                }
                _ => return Ok(FALSE),
            }
        }
        Kind::Binary => {
            let bytes = child.binary(&mut status);
            if !checked(req, object, status, "Failed to retrieve binary value")? {
                return Ok(FALSE);
            }
            Union::B(Union::B(Union::A(PhpString::new(req, &bytes)?)))
        }
        Kind::Int => {
            let n = child.int(&mut status);
            if !checked(req, object, status, "Failed to retrieve integer value")? {
                return Ok(FALSE);
            }
            Union::B(Union::B(Union::B(i64::from(n))))
        }
        Kind::IntVector => {
            let ints = child.int_vector(&mut status);
            if !checked(req, object, status, "Failed to retrieve vector value")? {
                return Ok(FALSE);
            }
            let size = u32::try_from(ints.len()).unwrap_or(u32::MAX);
            let mut array = PhpArray::sized(req, size, Layout::Packed)?;
            for n in ints {
                array.append(req, Owner::int(i64::from(n)))?;
            }
            Union::B(Union::A(array))
        }
        Kind::Table | Kind::Array => {
            let new = new_bundle(req)?;
            if let Some(o) = new.native_mut::<BundleObject>(req) {
                o.me = Some(child);
            }
            error::errors_reset::<BundleObject>(req, object);
            Union::A(new.into_instance())
        }
        Kind::Other => {
            return Err(fatal(
                req,
                "InternalError: a resource of a type ResourceBundle does not read",
            ));
        }
    };
    Ok(Some(Union::A(value)))
}

/// An element's offset: `Z_PARAM_STR_OR_LONG`'s two arms.
enum Offset<'a> {
    Key(&'a [u8]),
    Index(i64),
}

/// `resource_bundle_array_fetch`: the element under a key or an index,
/// null where ICU found none (or found it only by a fallback the caller
/// refused), its message set. `arg` is the argument the offset was, or
/// zero for `$bundle[...]`, which words its refusals of its own.
fn fetch(
    req: &mut Request,
    object: PhpObject,
    offset: &Offset,
    fallback: bool,
    arg: u32,
) -> Result<Element, Thrown> {
    error::reset(req);
    error::reset_object::<BundleObject>(req, object);
    let mut status = Code::ZERO;
    let (child, what) = match *offset {
        Offset::Key([]) if arg > 0 => return Err(arg_value_error(req, arg, "not be empty")),
        Offset::Key([]) => return Err(value_error(req, "Offset must not be empty")),
        Offset::Key(key) => {
            let key = c_string(key);
            let child = match bundle(req, object) {
                Some(me) => me.by_key(&key, &mut status),
                None => unconstructed(&mut status),
            };
            (child, [b"'", key.as_bytes(), b"'"].concat())
        }
        Offset::Index(n) => {
            let Ok(index) = i32::try_from(n) else {
                let range = format!("between {} and {}", i32::MIN, i32::MAX);
                return Err(if arg > 0 {
                    argument_error(req, "ValueError", arg, &format!("index must be {range}"))
                } else {
                    value_error(req, &format!("Index must be {range}"))
                });
            };
            let child = match bundle(req, object) {
                Some(me) => me.by_index(index, &mut status),
                None => unconstructed(&mut status),
            };
            (child, index.to_string().into_bytes())
        }
    };
    error::errors_set_code::<BundleObject>(req, object, status);
    let message = match child {
        _ if status.is_failure() => [b"Cannot load resource element ", what.as_slice()].concat(),
        Some(_) if !fallback && FELL_BACK.contains(&status) => {
            // php's `icuerror` here is never initialised; ICU reads it as
            // the zero it most often is.
            let locale = bundle(req, object)
                .map_or_else(|| NULL.to_vec(), |me| actual_locale(me, Code::ZERO));
            let head = b"Cannot load element ";
            [&head[..], &what, b" without fallback from to ", &locale].concat()
        }
        Some(child) => return extract(req, object, child),
        None => return Ok(None),
    };
    error::errors_set_custom_msg::<BundleObject>(req, object, message)?;
    Ok(None)
}

/// What ICU answers for a lookup on a null bundle.
fn unconstructed(status: &mut Code) -> Option<Bundle> {
    if !status.is_failure() {
        *status = Code::ILLEGAL_ARGUMENT;
    }
    None
}

/// `resourcebundle_ctor`'s arguments, counted and held where the native
/// says, the constructor's inside its [`error::constructing`] span; the
/// request's error was reset before them (`before_args`).
fn arguments(
    req: &mut Request,
    locale: &Raw<Option<PhpString>>,
    bundle: &Raw<Option<PhpString>>,
    fallback: &Raw<bool>,
) -> Result<Given, Thrown> {
    php_ext::raising::arity(req, 2, Some(3))?;
    Ok(Given {
        locale: locale.hold(req)?,
        bundle: bundle.hold(req)?,
        fallback: fallback.hold(req)?,
    })
}

/// `resourcebundle_ctor`'s arguments, held.
struct Given {
    locale: Option<PhpString>,
    bundle: Option<PhpString>,
    fallback: bool,
}

/// `resourcebundle_ctor`: `Ok(false)` for php's `FAILURE` where nothing
/// was thrown.
fn construct(req: &mut Request, object: PhpObject, given: &Given) -> Result<bool, Thrown> {
    if bundle(req, object).is_some() {
        let message = "ResourceBundle object is already constructed";
        return Err(throw_error(req, "Error", message));
    }
    let locale = given.locale.as_ref().map(PhpString::as_bytes);
    if locale_too_long(req, locale.map_or(0, <[u8]>::len))? {
        return Ok(false);
    }
    let locale = locale.map_or_else(|| crate::default_locale(req), <[u8]>::to_vec);
    let name = given.bundle.as_ref().map(PhpString::as_bytes);
    if name.is_some_and(|name| name.len() >= MAXPATHLEN) {
        return Err(argument_error(req, "ValueError", 2, "is too long"));
    }
    let package = name.map(c_string);
    // ICU writes the object's code, which a constructor that failed
    // before left set: `ures_open` then opens nothing.
    let mut status = error::object_code::<BundleObject>(req, object);
    let opened = Bundle::open(
        package.as_deref(),
        &c_string(&locale),
        given.fallback,
        &mut status,
    );
    // `ures_getLocaleByType` continues the object's code, which a
    // warning leaves as it is.
    let actual = match &opened {
        Some(me) if !given.fallback && FELL_BACK.contains(&status) => {
            Some(actual_locale(me, status))
        }
        _ => None,
    };
    if let Some(o) = object.native_mut::<BundleObject>(req) {
        o.me = opened;
    }
    if !checked(req, object, status, "Cannot load libICU resource bundle")? {
        return Ok(false);
    }
    let Some(actual) = actual else {
        return Ok(true);
    };
    let message = [
        &b"Cannot load libICU resource '"[..],
        name.unwrap_or(b"(default data)"),
        b"' without fallback from ",
        &locale,
        b" to ",
        &actual,
    ]
    .concat();
    error::errors_set_custom_msg::<BundleObject>(req, object, message)?;
    Ok(false)
}

/// `ResourceBundle_ce_ptr`'s `object_init_ex`: a `ResourceBundle` no
/// constructor ran on, whatever class the caller's is.
fn new_bundle(req: &mut Request) -> Result<Owner<PhpObject>, Thrown> {
    let Some(class) = lookup_class(req, b"ResourceBundle")? else {
        return Err(fatal(req, "InternalError: ResourceBundle is not linked"));
    };
    new_object(req, class)
}

/// What `%s` prints for a null `char *`.
const NULL: &[u8] = b"(null)";

/// `ures_getLocaleByType(ULOC_ACTUAL_LOCALE)` from `status`, as a
/// message quotes it.
fn actual_locale(me: &Bundle, mut status: Code) -> Vec<u8> {
    me.actual_locale(&mut status)
        .unwrap_or_else(|| NULL.to_vec())
}

/// `resourcebundle_create`: a fresh `ResourceBundle`, or null where its
/// constructor failed.
fn create(req: &mut Request, given: &Given) -> Result<Option<Owner<Bundles>>, Thrown> {
    let object = new_bundle(req)?;
    match construct(req, *object, given) {
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

/// `RESOURCEBUNDLE_METHOD_FETCH_OBJECT`: the object's error reset, the
/// request's having been reset before the arguments
/// (`RESOURCEBUNDLE_METHOD_INIT_VARS`, `before_args`), and php's `Error`
/// where no constructor opened a bundle.
fn fetch_object(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    error::reset_object::<BundleObject>(req, object);
    if bundle(req, object).is_none() {
        return Err(throw_error(
            req,
            "Error",
            "Found unconstructed ResourceBundle",
        ));
    }
    Ok(())
}

/// `resourcebundle_count`: `ures_getSize`.
fn count(req: &mut Request, object: PhpObject) -> Result<i64, Thrown> {
    fetch_object(req, object)?;
    Ok(bundle(req, object).map_or(0, |me| i64::from(me.size())))
}

/// `resourcebundle_locales`: the locales a bundle's package holds, ICU's
/// own for an empty name.
fn locales(req: &mut Request, name: PhpString) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let name = name.as_bytes();
    if name.len() >= MAXPATHLEN {
        return Err(argument_error(req, "ValueError", 1, "is too long"));
    }
    let package = (!name.is_empty()).then(|| c_string(name));
    let mut status = Code::ZERO;
    let opened = uloc::available(package.as_deref(), &mut status);
    if let OrFalse::False = error::or_false(error::check(req, status, "Cannot fetch locales list"))?
    {
        return Ok(OrFalse::False);
    }
    let Some(mut all) = opened else {
        return Ok(OrFalse::False);
    };
    all.reset(&mut status);
    if let OrFalse::False =
        error::or_false(error::check(req, status, "Cannot iterate locales list"))?
    {
        return Ok(OrFalse::False);
    }
    let mut out = PhpArray::new(req)?;
    while let Some(entry) = all.next(&mut status) {
        let entry = PhpString::new(req, &entry)?;
        out.append(req, entry.into_value())?;
    }
    Ok(OrFalse::Value(out))
}

/// `resourcebundle_get_error_code`: the object's code.
fn error_code(req: &mut Request, object: PhpObject) -> i64 {
    i64::from(error::object_code::<BundleObject>(req, object).0)
}

/// `resourcebundle_get_error_message`.
fn error_message(req: &mut Request, object: PhpObject) -> Result<Owner<PhpString>, Thrown> {
    let message = error::object_message::<BundleObject>(req, object);
    PhpString::new(req, &message)
}

/// `ResourceBundle_object_handlers`: no clone, the elements read as
/// dimensions, the count and the iterator.
pub(crate) struct Handlers;
pub(crate) static HANDLERS: Handlers = Handlers;

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        Refusals::NONE.cloning()
    }

    /// `resourcebundle_array_get`.
    fn read_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        read: DimensionRead,
    ) -> Result<Owner<Value>, Thrown> {
        let Some(offset) = offset.map(Value::deref) else {
            let message = "Cannot apply [] to ResourceBundle object";
            return Err(throw_error(req, "Error", message));
        };
        let element = match offset.as_ref() {
            ValueRef::Int(n) => fetch(req, object, &Offset::Index(n), true, 0)?,
            ValueRef::Str(s) => fetch(req, object, &Offset::Key(s.as_bytes()), true, 0)?,
            _ => return Err(illegal_offset(req, object, &offset, read)),
        };
        IntoReturn::into_return(element, req)
    }

    /// `resourcebundle_array_count`: php's `FAILURE` for an
    /// unconstructed bundle, which `count()` hands to the method.
    fn count_elements(&self, req: &mut Request, object: PhpObject) -> Result<Option<i64>, Thrown> {
        if let Some(me) = bundle(req, object) {
            return Ok(Some(i64::from(me.size())));
        }
        let message = "Found unconstructed ResourceBundle";
        error::errors_set::<BundleObject>(req, object, Code::ILLEGAL_ARGUMENT, message)?;
        Ok(None)
    }

    /// `resourcebundle_get_iterator`.
    fn get_iterator(
        &self,
        req: &mut Request,
        object: PhpObject,
        by_ref: bool,
    ) -> Result<Option<Box<dyn ClassIterator>>, Thrown> {
        if by_ref {
            let message = "An iterator cannot be used with foreach by reference";
            return Err(throw_error(req, "Error", message));
        }
        let (table, length) =
            bundle(req, object).map_or((false, 0), |me| (me.kind() == Kind::Table, me.size()));
        Ok(Some(Box::new(Elements {
            table,
            length,
            i: 0,
            read: None,
        })))
    }

    /// `sizeof(ResourceBundle_iterator)`.
    fn iterator_size(&self) -> usize {
        144
    }
}

/// `zend_illegal_container_offset(ce->name, offset, type)`.
fn illegal_offset(
    req: &mut Request,
    object: PhpObject,
    offset: &Value,
    read: DimensionRead,
) -> Thrown {
    let class = req.symbols().class_name(object.class()).to_vec();
    illegal_container_offset(req, &class, offset, read)
}

/// The `foreach` over a bundle's elements, `ResourceBundle_iterator`:
/// each read by its index, as php reads them, keyed by its table key or
/// its index, and kept until the walk steps on.
struct Elements {
    table: bool,
    length: i32,
    i: i32,
    /// `current` and `currentkey`, none while undefined.
    read: Option<(Owner<Value>, Vec<u8>)>,
}

impl Elements {
    /// `resourcebundle_iterator_read`: the element and its key, read
    /// where they are not; none where ICU could not open it.
    fn read(
        &mut self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<(Value, &[u8])>, Thrown> {
        if self.read.is_none() {
            let mut status = Code::ZERO;
            let child = bundle(req, object).and_then(|me| me.by_index(self.i, &mut status));
            if let Some(child) = child.filter(|_| !status.is_failure()) {
                // The key first: the element is handed on as it is read.
                let key = if self.table { child.key() } else { Vec::new() };
                let element = extract(req, object, child)?;
                self.read = Some((IntoReturn::into_return(element, req)?, key));
            }
        }
        Ok(self
            .read
            .as_ref()
            .map(|(value, key)| (**value, key.as_slice())))
    }

    /// `resourcebundle_iterator_invalidate`.
    fn invalidate(&mut self, req: &mut Request) {
        if let Some((value, _)) = self.read.take() {
            value.give_up(req);
        }
    }
}

impl ClassIterator for Elements {
    fn rewind(&mut self, req: &mut Request, _: PhpObject) -> Result<(), Thrown> {
        self.i = 0;
        self.invalidate(req);
        Ok(())
    }

    fn valid(&mut self, _: &mut Request, _: PhpObject) -> Result<bool, Thrown> {
        Ok(self.i < self.length)
    }

    fn current(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
        let value = self
            .read(req, object)?
            .map_or_else(Value::null, |(value, _)| value);
        Ok(Owner::take(req, value))
    }

    fn key(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
        let key = self.read(req, object)?.map(|(_, key)| key.to_vec());
        if !self.table {
            return Ok(Owner::int(i64::from(self.i)));
        }
        Ok(PhpString::new(req, &key.unwrap_or_default())?.into_value())
    }

    fn move_forward(&mut self, req: &mut Request, _: PhpObject) -> Result<(), Thrown> {
        self.i += 1;
        self.invalidate(req);
        Ok(())
    }

    fn release(&mut self, req: &mut Request, _: PhpObject) {
        self.invalidate(req);
    }
}

/// `class ResourceBundle implements IteratorAggregate, Countable`: a
/// bundle of ICU's resources for a locale, its elements read by key or
/// index, the tables and arrays among them bundles of their own.
///
/// ```php
/// $b = new ResourceBundle('de_DE', 'ICUDATA-region');
/// var_dump($b->get('Countries')->get('DE'), $b['Countries']['FR'], count($b) > 3, $b->getErrorCode());
/// var_dump($b->get('nope'), $b->getErrorCode(), $b->getErrorMessage(), intl_get_error_message());
/// var_dump($b->get('Countries', false) instanceof ResourceBundle, ResourceBundle::create('en', 'nope'), intl_get_error_message());
/// $v = new ResourceBundle('icuver', 'ICUDATA');
/// foreach ($v as $k => $e) { echo $k, ' ', gettype($e), "\n"; }
/// var_dump(get_class($v->getIterator()), in_array('de', ResourceBundle::getLocales('')), ResourceBundle::getLocales('/nope/x'), intl_get_error_code());
/// $it = $b->getIterator(); var_dump($it->key(), $it->current() === $it->current());
/// try { new ResourceBundle('en', 'nope'); } catch (IntlException $e) { echo $e->getMessage(), "\n"; }
/// try { $v->__construct('en', null); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { clone $v; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $v[] = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $v[1.5]; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { $v['']; } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { $v->get(1 << 40); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// $b['nope']; var_dump(intl_get_error_message());
/// function f($b) { return $b[-1]; }
/// f($b); var_dump(intl_get_error_message());
/// $u = (new ReflectionClass('ResourceBundle'))->newInstanceWithoutConstructor();
/// var_dump($u->get('x'), $u->getErrorCode());
/// try { count($u); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// foreach ($u as $x) { echo "never\n"; }
/// ```
/// ```expect
/// string(11) "Deutschland"
/// string(10) "Frankreich"
/// bool(true)
/// int(0)
/// NULL
/// int(2)
/// string(84) "ResourceBundle::get(): Cannot load resource element 'nope': U_MISSING_RESOURCE_ERROR"
/// string(12) "U_ZERO_ERROR"
/// bool(true)
/// NULL
/// string(86) "ResourceBundle::create(): Cannot load libICU resource bundle: U_MISSING_RESOURCE_ERROR"
/// CLDRVersion string
/// DataVersion string
/// ICUVersion string
/// string(16) "InternalIterator"
/// bool(true)
/// bool(false)
/// int(2)
/// string(9) "Countries"
/// bool(true)
/// ResourceBundle::__construct(): Cannot load libICU resource bundle
/// ResourceBundle object is already constructed
/// Trying to clone an uncloneable object of class ResourceBundle
/// Cannot use object of type ResourceBundle as array
/// Cannot access offset of type float on ResourceBundle
/// Offset must not be empty
/// ResourceBundle::get(): Argument #1 ($index) index must be between -2147483648 and 2147483647
/// string(69) "main(): Cannot load resource element 'nope': U_MISSING_RESOURCE_ERROR"
/// string(62) "f(): Cannot load resource element -1: U_MISSING_RESOURCE_ERROR"
/// NULL
/// int(1)
/// Found unconstructed ResourceBundle
/// ```
#[php_class(implements(IteratorAggregate, Countable), not_serializable, size = 88, payload = BundleObject, handlers = crate::resourcebundle::HANDLERS)]
pub(crate) struct ResourceBundle;

/// Core's `Countable`, named for `implements`.
#[php_class(extern)]
pub(crate) struct Countable;

type Bundles = Instance<ResourceBundle>;

#[php_class]
#[allow(non_snake_case)]
impl ResourceBundle {
    #[php(arity = manual, before_args = error::reset)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        locale: Raw<Option<PhpString>>,
        bundle: Raw<Option<PhpString>>,
        #[php(default = true)] fallback: Raw<bool>,
    ) -> Result<(), Thrown> {
        let object = this.object();
        error::constructing(req, |req| {
            let given = arguments(req, &locale, &bundle, &fallback)?;
            construct(req, object, &given).map(|_| ())
        })
    }

    #[php(tentative, arity = manual, before_args = error::reset)]
    fn create(
        req: &mut Request,
        locale: Raw<Option<PhpString>>,
        bundle: Raw<Option<PhpString>>,
        #[php(default = true)] fallback: Raw<bool>,
    ) -> Result<Option<Owner<Bundles>>, Thrown> {
        let given = arguments(req, &locale, &bundle, &fallback)?;
        create(req, &given)
    }

    #[php(tentative)]
    fn get(
        req: &mut Request,
        this: This<Self>,
        index: Union<PhpString, i64>,
        #[php(default = true)] fallback: bool,
    ) -> Result<Element, Thrown> {
        fetch(req, this.object(), &offset(&index), fallback, 1)
    }

    #[php(tentative, before_args = error::reset)]
    fn count(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        count(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    fn getLocales(
        req: &mut Request,
        bundle: PhpString,
    ) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
        locales(req, bundle)
    }

    #[php(tentative, before_args = error::reset)]
    fn getErrorCode(req: &mut Request, this: This<Self>) -> i64 {
        error_code(req, this.object())
    }

    #[php(tentative, before_args = error::reset)]
    fn getErrorMessage(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        error_message(req, this.object())
    }

    fn getIterator(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<crate::iterator::Iterator>>, Thrown> {
        internal_iterator(req, &this.object()).map(Owner::into_instance)
    }
}

/// `Z_PARAM_STR_OR_LONG`'s offset.
fn offset(index: &Union<PhpString, i64>) -> Offset<'_> {
    match index {
        Union::A(key) => Offset::Key(key.as_bytes()),
        Union::B(n) => Offset::Index(*n),
    }
}

/// `resourcebundle_create`.
///
/// ```php
/// $b = resourcebundle_create('de', 'ICUDATA-region');
/// var_dump(get_class($b), resourcebundle_create('en', 'nope'), intl_get_error_message());
/// try { resourcebundle_create('en', str_repeat('x', 1024)); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(14) "ResourceBundle"
/// NULL
/// string(85) "resourcebundle_create(): Cannot load libICU resource bundle: U_MISSING_RESOURCE_ERROR"
/// resourcebundle_create(): Argument #2 ($bundle) is too long
/// ```
#[php_function(arity = manual, before_args = error::reset)]
fn resourcebundle_create(
    req: &mut Request,
    locale: Raw<Option<PhpString>>,
    bundle: Raw<Option<PhpString>>,
    #[php(default = true)] fallback: Raw<bool>,
) -> Result<Option<Owner<Bundles>>, Thrown> {
    let given = arguments(req, &locale, &bundle, &fallback)?;
    create(req, &given)
}

/// `resourcebundle_get`.
///
/// ```php
/// $b = resourcebundle_create('de', 'ICUDATA-region');
/// var_dump(resourcebundle_get(resourcebundle_get($b, 'Countries'), 'AT'), resourcebundle_get($b, 'nope'), intl_get_error_message());
/// try { resourcebundle_get($b, ''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(11) "Österreich"
/// NULL
/// string(83) "resourcebundle_get(): Cannot load resource element 'nope': U_MISSING_RESOURCE_ERROR"
/// resourcebundle_get(): Argument #2 ($index) must not be empty
/// ```
#[php_function]
fn resourcebundle_get(
    req: &mut Request,
    bundle: Bundles,
    index: Union<PhpString, i64>,
    #[php(default = true)] fallback: bool,
) -> Result<Element, Thrown> {
    fetch(req, bundle.object(), &offset(&index), fallback, 2)
}

/// `resourcebundle_count`.
///
/// ```php
/// var_dump(resourcebundle_count(resourcebundle_create('icuver', 'ICUDATA')));
/// resourcebundle_create('en', 'nope');
/// try { resourcebundle_count(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(intl_get_error_code());
/// ```
/// ```expect
/// int(3)
/// resourcebundle_count(): Argument #1 ($bundle) must be of type ResourceBundle, int given
/// int(0)
/// ```
#[php_function(before_args = error::reset)]
fn resourcebundle_count(req: &mut Request, bundle: Bundles) -> Result<i64, Thrown> {
    count(req, bundle.object())
}

/// `resourcebundle_locales`.
///
/// ```php
/// var_dump(in_array('en_US', resourcebundle_locales('')), resourcebundle_locales('/nope/x'), intl_get_error_message());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// string(77) "resourcebundle_locales(): Cannot fetch locales list: U_MISSING_RESOURCE_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn resourcebundle_locales(
    req: &mut Request,
    bundle: PhpString,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    locales(req, bundle)
}

/// `resourcebundle_get_error_code`.
///
/// ```php
/// $b = resourcebundle_create('de', 'ICUDATA-region');
/// $b->get('nope');
/// var_dump(resourcebundle_get_error_code($b), intl_get_error_code());
/// ```
/// ```expect
/// int(2)
/// int(0)
/// ```
#[php_function(before_args = error::reset)]
fn resourcebundle_get_error_code(req: &mut Request, bundle: Bundles) -> i64 {
    error_code(req, bundle.object())
}

/// `resourcebundle_get_error_message`.
///
/// ```php
/// $b = resourcebundle_create('de', 'ICUDATA-region');
/// var_dump(resourcebundle_get_error_message($b));
/// $b->get(-1);
/// var_dump(resourcebundle_get_error_message($b));
/// ```
/// ```expect
/// string(12) "U_ZERO_ERROR"
/// string(80) "ResourceBundle::get(): Cannot load resource element -1: U_MISSING_RESOURCE_ERROR"
/// ```
#[php_function(before_args = error::reset)]
fn resourcebundle_get_error_message(
    req: &mut Request,
    bundle: Bundles,
) -> Result<Owner<PhpString>, Thrown> {
    error_message(req, bundle.object())
}

/// `php_intl.stub.php`'s `resourcebundle_*` block.
pub(crate) fn functions(builder: php_ext::abi::ExtensionBuilder) -> php_ext::abi::ExtensionBuilder {
    builder.functions(vec![
        resourcebundle_create::declared(),
        resourcebundle_get::declared(),
        resourcebundle_count::declared(),
        resourcebundle_locales::declared(),
        resourcebundle_get_error_code::declared(),
        resourcebundle_get_error_message::declared(),
    ])
}
