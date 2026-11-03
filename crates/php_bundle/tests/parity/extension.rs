//! The parity extension: php declarations mirrored from extensions this
//! test does not link (standard, pcre, spl, json, dom), declared with the
//! attributes alone, and `proto_*` natives and classes for the shapes no
//! mirror covers. A mirror's body is faithful where `behaviour.php`
//! calls it and a stand-in where only its declaration is held.

use std::ops::ControlFlow;

use php_ext::abi::{
    Callable, ClassRef, ConstArray, ConstValue, ExtensionBuilder, False, Instance, Iterable,
    NamedVariadic, Never, OrFalse, Passed, Raw, Ref, Static, This, True, Typed, Union, Untyped,
    Variadic,
};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::coerce::{identical, loose_eq, to_string};
use php_ext::raising::{Thrown, arg_value_error, throw};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value, ValueRef};

#[php_const]
pub(crate) mod constants {
    pub(crate) const ENT_HTML401: i64 = 0;
    pub(crate) const ENT_QUOTES: i64 = 3;
    pub(crate) const ENT_SUBSTITUTE: i64 = 8;
    #[php(deprecated(since = "8.1", message = "as the constant has no effect"))]
    pub(crate) const FILE_TEXT: i64 = 0;
    /// Computed when the builder runs.
    pub(crate) fn PROTO_BUILT() -> String {
        String::from("at startup")
    }
}

#[php_const(namespace(Proto))]
pub(crate) const INSIDE: bool = true;

/// `strspn(string $string, string $characters, int $offset = 0, ?int $length = null): int`
#[php_function]
fn strspn(
    string: PhpString,
    characters: PhpString,
    #[php(default = 0)] offset: i64,
    #[php(default = null)] length: Option<i64>,
) -> i64 {
    let bytes = string.as_bytes();
    let len = i64::try_from(bytes.len()).unwrap_or(0);
    let start = if offset < 0 {
        (len + offset).max(0)
    } else {
        offset.min(len)
    };
    let end = match length {
        Some(n) if n < 0 => (len + n).max(start),
        Some(n) => (start + n).min(len),
        None => len,
    };
    let (start, end) = (
        usize::try_from(start).unwrap_or(0),
        usize::try_from(end).unwrap_or(0),
    );
    let set = characters.as_bytes();
    i64::try_from(
        bytes[start..end]
            .iter()
            .take_while(|b| set.contains(b))
            .count(),
    )
    .unwrap_or(0)
}

/// `explode(string $separator, string $string, int $limit = PHP_INT_MAX): array`,
/// faithful for a positive limit.
#[php_function]
fn explode(
    req: &mut Request,
    separator: PhpString,
    string: PhpString,
    #[php(default = PHP_INT_MAX)] limit: i64,
) -> Result<Owner<PhpArray>, Thrown> {
    if separator.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    let (separator, string) = (separator.as_bytes().to_vec(), string.as_bytes().to_vec());
    let mut pieces: Vec<&[u8]> = Vec::new();
    let mut rest: &[u8] = &string;
    let keep = usize::try_from(limit.max(1)).unwrap_or(usize::MAX);
    while pieces.len() + 1 < keep
        && let Some(at) = rest
            .windows(separator.len())
            .position(|w| w == separator.as_slice())
    {
        pieces.push(&rest[..at]);
        rest = &rest[at + separator.len()..];
    }
    pieces.push(rest);
    let mut array = PhpArray::new(req)?;
    for piece in pieces {
        let piece = PhpString::new(req, piece)?.into_value();
        array.append(req, piece)?;
    }
    Ok(array)
}

/// `htmlspecialchars(string $string, int $flags = ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401, ?string $encoding = null, bool $double_encode = true): string`,
/// whose body answers the flags it was handed, the default the engine
/// evaluated.
#[php_function]
fn htmlspecialchars(
    req: &mut Request,
    string: PhpString,
    #[php(default = ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401)] flags: i64,
    #[php(default = null)] encoding: Option<PhpString>,
    #[php(default = true)] double_encode: bool,
) -> Result<Owner<PhpString>, Thrown> {
    let _ = (string, encoding, double_encode);
    PhpString::new(req, format!("flags={flags}").as_bytes())
}

/// `enum RoundingMode`, pure.
#[php_class]
pub(crate) enum RoundingMode {
    HalfAwayFromZero,
    HalfTowardsZero,
    HalfEven,
    HalfOdd,
    TowardsZero,
    AwayFromZero,
    NegativeInfinity,
    PositiveInfinity,
}

#[php_class]
impl RoundingMode {}

/// `round(int|float $num, int $precision = 0, int|RoundingMode $mode = RoundingMode::HalfAwayFromZero): float`,
/// faithful at precision 0 for the roundings `behaviour.php` asks for.
#[php_function]
fn round(
    num: Union<i64, f64>,
    #[php(default = 0)] precision: i64,
    #[php(default = RoundingMode::HalfAwayFromZero)] mode: Union<i64, RoundingMode>,
) -> f64 {
    let _ = precision;
    #[allow(clippy::cast_precision_loss)]
    let x = match num {
        Union::A(n) => n as f64,
        Union::B(f) => f,
    };
    #[allow(clippy::float_cmp)] // a half is exact
    match mode {
        Union::B(RoundingMode::HalfEven) => {
            let r = x.round();
            if (x - x.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
                r - x.signum()
            } else {
                r
            }
        }
        Union::B(RoundingMode::TowardsZero) => x.trunc(),
        _ => x.round(),
    }
}

/// `trim(string $string, string $characters = " \n\r\t\v\0"): string`, frameless.
#[php_function(frameless(1, 2))]
fn trim(
    req: &mut Request,
    string: PhpString,
    #[php(default = " \n\r\t\x0B\0")] characters: PhpString,
) -> Result<Owner<PhpString>, Thrown> {
    let set = characters.as_bytes();
    let s = string.as_bytes();
    let start = s.iter().position(|b| !set.contains(b)).unwrap_or(s.len());
    let end = s
        .iter()
        .rposition(|b| !set.contains(b))
        .map_or(start, |e| e + 1);
    PhpString::new(req, &s[start..end])
}

/// `mkdir(string $directory, int $permissions = 0777, bool $recursive = false, $context = null): bool`,
/// declaration only: the default keeps its octal spelling.
#[php_function]
fn mkdir(
    directory: PhpString,
    #[php(default = 0777)] permissions: i64,
    #[php(default = false)] recursive: bool,
    #[php(default = null)] context: Untyped<Value>,
) -> bool {
    let _ = (directory, permissions, recursive, context);
    false
}

/// `str_replace(array|string $search, array|string $replace, string|array $subject, &$count = null): string|array`,
/// faithful for strings.
#[php_function]
fn str_replace(
    req: &mut Request,
    search: Union<PhpArray, PhpString>,
    replace: Union<PhpArray, PhpString>,
    subject: Union<PhpString, PhpArray>,
    #[php(default = null)] count: Ref<Untyped<Value>>,
) -> Result<Union<Owner<PhpString>, PhpArray>, Thrown> {
    let (Union::B(search), Union::B(replace), Union::A(subject)) = (search, replace, subject)
    else {
        return Err(arg_value_error(req, 1, "is mirrored for strings only"));
    };
    let (n, r, s) = (search.as_bytes(), replace.as_bytes(), subject.as_bytes());
    let mut out = Vec::new();
    let mut hits = 0i64;
    let mut at = 0;
    while at < s.len() {
        if !n.is_empty() && s[at..].starts_with(n) {
            out.extend_from_slice(r);
            at += n.len();
            hits += 1;
        } else {
            out.push(s[at]);
            at += 1;
        }
    }
    count.set(req, Untyped(Value::int(hits)))?;
    Ok(Union::A(PhpString::new(req, &out)?))
}

/// `in_array(mixed $needle, array $haystack, bool $strict = false): bool`, frameless.
#[php_function(frameless(2, 3))]
fn in_array(
    req: &mut Request,
    needle: Value,
    haystack: PhpArray,
    #[php(default = false)] strict: bool,
) -> Result<bool, Thrown> {
    // `ZEND_HASH_FOREACH_VAL(ht, entry)`, no owner of the table taken:
    // the argument holds it while `loose_eq` serves `__toString`.
    let found = walk::foreach(req, haystack, Hold::None, |req, row| -> walk::Step {
        let value = row.value().deref();
        let same = if strict {
            identical(req, &needle, &value)?
        } else {
            loose_eq(req, &needle, &value)?
        };
        Ok(if same {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    })?;
    Ok(found.is_some())
}

/// `array_keys(array $array, mixed $filter_value = UNKNOWN, bool $strict = false): array`
#[php_function]
fn array_keys(
    req: &mut Request,
    array: PhpArray,
    filter_value: Passed<Value>,
    #[php(default = false)] strict: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    // `ZEND_HASH_FOREACH_KEY_VAL`, no owner of the table taken: the argument
    // holds it while `loose_eq` serves `__toString`.
    let walked = walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let value = *row.value();
        let keep = match &filter_value {
            Passed::Omitted => true,
            Passed::Given(filter) => {
                let value = value.deref();
                let same = if strict {
                    identical(req, filter, &value)
                } else {
                    loose_eq(req, filter, &value)
                };
                same?
            }
        };
        if keep {
            let key = match key {
                Key::Int(n) => Value::int(n),
                Key::Str(s) => Value::string(s),
            };
            out.append(req, Owner::take(req, key))?;
        }
        Ok(ControlFlow::Continue(()))
    });
    if let Err(thrown) = walked {
        out.give_up(req);
        return Err(thrown);
    }
    Ok(out)
}

/// `array_push(array &$array, mixed ...$values): int`, the caller's array
/// changed in place.
#[php_function]
fn array_push(
    req: &mut Request,
    array: Ref<PhpArray>,
    values: Variadic<Value>,
) -> Result<i64, Thrown> {
    let Some(mut target) = array.separated(req)? else {
        return Ok(0);
    };
    for value in values {
        target.append(req, Owner::take(req, value))?;
    }
    Ok(i64::try_from(target.len()).unwrap_or(0))
}

/// `sscanf(string $string, string $format, mixed &...$vars): array|int|null`, declaration only.
#[php_function]
fn sscanf(
    string: PhpString,
    format: PhpString,
    vars: Variadic<Ref<Value>>,
) -> Option<Union<PhpArray, i64>> {
    drop((string, format, vars));
    None
}

/// `array_multisort(&$array, &...$rest): bool`, declaration only.
#[php_function]
fn array_multisort(
    #[php(prefer_ref)] array: Ref<Untyped<Value>>,
    #[php(prefer_ref)] rest: Variadic<Ref<Untyped<Value>>>,
) -> True {
    drop((array, rest));
    True
}

/// `preg_match(string $pattern, string $subject, &$matches = null, int $flags = 0, int $offset = 0): int|false`, declaration only.
#[php_function(frameless(2))]
fn preg_match(
    pattern: PhpString,
    subject: PhpString,
    #[php(default = null)] matches: Ref<Untyped<Value>>,
    #[php(default = 0)] flags: i64,
    #[php(default = 0)] offset: i64,
) -> OrFalse<i64> {
    let _ = (pattern, subject, matches, flags, offset);
    OrFalse::False
}

/// `wordwrap(string $string, int $width = 75, string $break = "\n", bool $cut_long_words = false): string`, declaration only.
#[php_function]
fn wordwrap(
    string: PhpString,
    #[php(default = 75)] width: i64,
    #[php(default = "\n")] r#break: PhpString,
    #[php(default = false)] cut_long_words: bool,
) -> PhpString {
    let _ = (width, r#break, cut_long_words);
    string
}

/// `implode(array|string $separator, ?array $array = null): string` and
/// its alias `join`, declaration only.
#[php_function(frameless(1, 2), aliases(join))]
fn implode(
    req: &mut Request,
    separator: Union<PhpArray, PhpString>,
    #[php(default = null)] array: Option<PhpArray>,
) -> Result<Owner<PhpString>, Thrown> {
    let _ = (separator, array);
    PhpString::new(req, b"")
}

/// `utf8_encode(string $string): string`, deprecated.
#[php_function(deprecated(
    since = "8.2",
    message = "visit the php.net documentation for various alternatives"
))]
fn utf8_encode(string: PhpString) -> PhpString {
    string
}

/// `password_verify(#[\SensitiveParameter] string $password, string $hash): bool`, declaration only.
#[php_function]
fn password_verify(#[php(sensitive)] password: PhpString, hash: PhpString) -> bool {
    let _ = (password, hash);
    false
}

/// A Windows-only native, which php's `#if` leaves out elsewhere.
#[cfg(windows)]
#[php_function]
fn sapi_windows_vt100_support(
    stream: Untyped<Value>,
    #[php(default = null)] enable: Option<bool>,
) -> bool {
    let _ = (stream, enable);
    false
}

/// `class php_user_filter`: untyped, `mixed` and typed properties with
/// defaults, a keyword parameter, an untyped by-reference one, tentative
/// returns.
#[php_class]
pub(crate) struct php_user_filter;

#[php_class]
impl php_user_filter {
    #[php(property)]
    const filtername: PhpString = "";
    #[php(property)]
    const params: Value = "";
    #[php(property)]
    const stream: Untyped<Value> = ConstValue::Null;

    #[php(tentative)]
    fn filter(
        this: This<Self>,
        r#in: Untyped<Value>,
        out: Untyped<Value>,
        consumed: Ref<Untyped<Value>>,
        closing: bool,
    ) -> i64 {
        let _ = (this, r#in, out, consumed, closing);
        0
    }
    #[php(tentative)]
    fn onCreate(this: This<Self>) -> bool {
        let _ = this;
        true
    }
    #[php(tentative)]
    fn onClose(this: This<Self>) {
        let _ = this;
    }
}

/// `class SplFileInfo`, `@not-serializable`, two of its methods.
#[php_class(not_serializable)]
pub(crate) struct SplFileInfo;

#[php_class]
impl SplFileInfo {
    fn __construct(this: This<Self>, filename: PhpString) {
        let _ = (this, filename);
    }
    #[php(tentative)]
    fn setFileClass(this: This<Self>, #[php(default = SplFileObject::class)] class: PhpString) {
        let _ = (this, class);
    }
}

#[php_class(extends(SplFileInfo))]
pub(crate) struct SplFileObject;

#[php_class]
impl SplFileObject {
    fn __construct(
        this: This<Self>,
        filename: PhpString,
        #[php(default = "r")] mode: PhpString,
        #[php(default = false)] useIncludePath: bool,
        #[php(default = null)] context: Untyped<Value>,
    ) {
        let _ = (this, filename, mode, useIncludePath, context);
    }
    #[php(tentative)]
    fn fgets(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let _ = this;
        PhpString::new(req, b"line")
    }
    /// `@alias SplFileObject::fgets`: its own declaration, `fgets`' entry.
    #[php(tentative, implementation = SplFileObject::fgets)]
    fn getCurrentLine(this: This<Self>) -> PhpString;
}

/// `SplTempFileObject::__construct(int $maxMemory = 2 * 1024 * 1024)`.
#[php_class(extends(SplFileObject))]
pub(crate) struct SplTempFileObject;

#[php_class]
impl SplTempFileObject {
    fn __construct(this: This<Self>, #[php(default = 2 * 1024 * 1024)] maxMemory: i64) {
        let _ = (this, maxMemory);
    }
}

/// Core's `Exception`, named for `extends`.
#[php_class(extern)]
pub(crate) struct Exception;

/// Core's `ArithmeticError`, with one constant this extension adds to it,
/// as `pdo_sqlite` adds its `PDO::SQLITE_*`.
#[php_class(extern)]
pub(crate) struct ArithmeticError;

#[php_class]
impl ArithmeticError {
    const PROTO_ADDED: i64 = 42;
}

/// `class JsonException extends Exception`.
#[php_class(extends(Exception))]
pub(crate) struct JsonException;

#[php_class]
impl JsonException {}

/// `class DOMNode`, one of its `@virtual` properties.
#[php_class]
pub(crate) struct DOMNode;

#[php_class]
impl DOMNode {
    #[php(property, virtual)]
    const nodeName: PhpString;
}

/// `enum Dom\AdjacentPosition: string`.
#[php_class(namespace(Dom), backed = string)]
pub(crate) enum AdjacentPosition {
    #[php(value = "beforebegin")]
    BeforeBegin,
    #[php(value = "afterbegin")]
    AfterBegin,
    #[php(value = "beforeend")]
    BeforeEnd,
    #[php(value = "afterend")]
    AfterEnd,
}

#[php_class]
impl AdjacentPosition {}

/// `Dom\Element`, one method.
#[php_class(namespace(Dom))]
pub(crate) struct Element;

#[php_class]
impl Element {
    fn insertAdjacentText(this: This<Self>, r#where: AdjacentPosition, data: PhpString) {
        let _ = (this, r#where, data);
    }
}

/// `proto_raw(int $n): int`: the body's own check before the declared
/// coercion.
#[php_function]
fn proto_raw(req: &mut Request, n: Raw<i64>) -> Result<i64, Thrown> {
    if matches!(n.value().as_ref(), ValueRef::Null) {
        return Err(arg_value_error(req, 1, "not be null here"));
    }
    n.hold(req)
}

/// `proto_seen(bool $flag = true): string`: presence beside a shown default.
#[php_function]
fn proto_seen(
    req: &mut Request,
    #[php(default = true)] flag: Passed<bool>,
) -> Result<Owner<PhpString>, Thrown> {
    let text = match flag {
        Passed::Given(b) => format!("given {b}"),
        Passed::Omitted => "omitted".to_owned(),
    };
    PhpString::new(req, text.as_bytes())
}

/// `proto_call(callable $callback, mixed ...$args): mixed`, and the named
/// extras beside the tail.
#[php_function]
fn proto_call(
    req: &mut Request,
    callback: Callable,
    args: NamedVariadic<Value>,
) -> Result<Owner<Value>, Thrown> {
    let named: Vec<(PhpString, Value)> = args.named().to_vec();
    let positional: Vec<Value> = args.positional.collect();
    php_ext::call::call_named(req, &callback, &positional, &named)
}

/// `proto_count(iterable $of): int`.
#[php_function]
fn proto_count(req: &mut Request, of: Iterable) -> Result<i64, Thrown> {
    match of {
        Iterable::Array(array) => Ok(i64::try_from(array.len()).unwrap_or(0)),
        Iterable::Traversable(object) => php_ext::coerce::count(req, &object.as_value()),
    }
}

/// `proto_manual(int ...$n): string`, which counts its own arguments: none
/// or two.
#[php_function(arity = manual)]
fn proto_manual(req: &mut Request, n: Variadic<i64>) -> Result<Owner<PhpString>, Thrown> {
    if !matches!(n.len(), 0 | 2) {
        return Err(php_ext::raising::arg_count_error(
            req,
            "exactly 0 or 2 arguments",
        ));
    }
    let text = format!("{:?}", n.collect::<Vec<_>>());
    PhpString::new(req, text.as_bytes())
}

/// `proto_describe(mixed $value): string`, through `to_string`.
#[php_function]
fn proto_describe(req: &mut Request, value: Value) -> Result<Owner<PhpString>, Thrown> {
    to_string(req, &value)
}

/// `proto_never(): never`.
#[php_function]
fn proto_never(req: &mut Request) -> Result<Never, Thrown> {
    let Some(&Raised(exception)) = req.engine::<Raised>() else {
        return Err(php_ext::raising::fatal(
            req,
            "InternalError: no engine state",
        ));
    };
    Err(throw(req, exception, "never returns"))
}

/// Core's `Exception` as the extension's engine start resolved it, the
/// class `proto_never` throws.
struct Raised(php_ext::values::ClassId);

/// `proto_false(): false`.
#[php_function]
fn proto_false() -> False {
    False
}

/// `enum ProtoLevel: int`, its values the discriminants.
#[php_class(backed = int)]
pub(crate) enum ProtoLevel {
    Low = 1,
    High = 2,
}

#[php_class]
impl ProtoLevel {
    const TOP: i64 = 2;
}

/// `interface ProtoCounter { public function value(): int; }`
#[php_class(interface)]
pub(crate) struct ProtoCounter;

#[php_class]
impl ProtoCounter {
    fn value(this: This<Self>) -> i64;
}

/// One payload two unrelated classes share, as dom's and date's are.
#[derive(Default)]
pub(crate) struct Tally {
    n: i64,
}

/// `abstract class ProtoBase`.
#[php_class(abstract, payload = Tally)]
pub(crate) struct ProtoBase;

#[php_class]
impl ProtoBase {
    fn grow(this: This<Self>);
    fn kind(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let _ = this;
        PhpString::new(req, b"base")
    }
}

/// `final class ProtoBox extends ProtoBase implements ProtoCounter`.
#[php_class(final, extends(ProtoBase), implements(ProtoCounter))]
pub(crate) struct ProtoBox;

#[php_class]
impl ProtoBox {
    const START: i64 = 10;
    #[php(protected)]
    const STEP: i64 = 5;
    const MIXED: Typed<Union<i64, PhpArray>, i64> = Typed::new(7);
    const LIST: ConstArray = ConstArray(&[
        (None, ConstValue::Int(1)),
        (Some(ConstValue::Str("k")), ConstValue::Str("v")),
    ]);
    #[php(property)]
    const label: PhpString = "box";
    #[php(property, protected)]
    const tag: Option<PhpString> = None;
    #[php(property)]
    const list: PhpArray = ConstArray(&[]);
    #[php(property, readonly)]
    const id: i64;

    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = self::START)] start: i64,
    ) -> Result<(), Thrown> {
        this.payload_mut(req)?.n = start;
        Ok(())
    }
    fn value(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        Ok(this.payload_mut(req)?.n)
    }
    fn grow(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        this.payload_mut(req)?.n += Self::STEP;
        Ok(())
    }
    fn add(req: &mut Request, this: This<Self>, n: i64) -> Result<Static, Thrown> {
        this.payload_mut(req)?.n += n;
        // `RETURN_OBJ_COPY`, answered as `static`: the engine takes its
        // own owner of the object returned.
        Ok(Static(this.object()))
    }
    fn make(
        req: &mut Request,
        #[php(default = 0)] n: i64,
    ) -> Result<Owner<Instance<Self>>, Thrown> {
        let Some(class) = php_ext::call::lookup_class(req, b"ProtoBox")? else {
            return Err(php_ext::raising::fatal(
                req,
                "InternalError: ProtoBox unlinked",
            ));
        };
        let made = php_ext::call::new_object(req, class)?.into_instance::<Self>();
        made.payload_mut(req)?.n = n;
        Ok(made)
    }
    #[php(protected)]
    fn hidden(this: This<Self>) {
        let _ = this;
    }
    #[php(final)]
    fn done(this: This<Self>) -> True {
        let _ = this;
        True
    }
}

/// A second class over the same payload, reaching `ProtoBox::value`'s entry.
#[php_class(payload = Tally)]
pub(crate) struct ProtoTally;

#[php_class]
impl ProtoTally {
    #[php(implementation = ProtoBox::value)]
    fn value(this: This<Self>) -> i64;
}

/// A payload laid out per class, `new = PATH`.
pub(crate) struct Laid {
    by: ClassRef,
}

impl Laid {
    fn for_class(class: ClassRef) -> Laid {
        Laid { by: class }
    }
}

#[php_class(payload = Laid, new = Laid::for_class)]
pub(crate) struct ProtoLaid;

#[php_class]
impl ProtoLaid {
    fn same(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let class = this.object().class();
        Ok(this.payload_mut(req)?.by.0 == class)
    }
}

pub(crate) fn extension() -> php_ext::abi::Extension {
    ExtensionBuilder::default()
        .functions(&[
            strspn::DECLARED,
            explode::DECLARED,
            htmlspecialchars::DECLARED,
            round::DECLARED,
            trim::DECLARED,
            mkdir::DECLARED,
            str_replace::DECLARED,
            in_array::DECLARED,
            array_keys::DECLARED,
            array_push::DECLARED,
            sscanf::DECLARED,
            array_multisort::DECLARED,
            preg_match::DECLARED,
            wordwrap::DECLARED,
            implode::DECLARED,
            join::DECLARED,
            utf8_encode::DECLARED,
            password_verify::DECLARED,
            proto_raw::DECLARED,
            proto_seen::DECLARED,
            proto_call::DECLARED,
            proto_count::DECLARED,
            proto_manual::DECLARED,
            proto_describe::DECLARED,
            proto_never::DECLARED,
            proto_false::DECLARED,
            // The stub's `#if`: the line carries the native's own `#[cfg]`.
            #[cfg(windows)]
            sapi_windows_vt100_support::DECLARED,
        ])
        .constants(constants::DECLARED)
        .constants(&[INSIDE::DECLARED])
        .class::<RoundingMode>()
        .class::<php_user_filter>()
        .class::<SplFileInfo>()
        .class::<SplFileObject>()
        .class::<SplTempFileObject>()
        .class::<ArithmeticError>()
        .class::<JsonException>()
        .class::<DOMNode>()
        .class::<AdjacentPosition>()
        .class::<Element>()
        .class::<ProtoLevel>()
        .class::<ProtoCounter>()
        .class::<ProtoBase>()
        .class::<ProtoBox>()
        .class::<ProtoTally>()
        .class::<ProtoLaid>()
        .engine_state(|start| start.class::<Exception>().map(Raised))
        .build("proto")
}
