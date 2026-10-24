//! `pdo_stmt.c`'s fetch half: the fetch style as a type ([`Mode`], `enum
//! pdo_fetch_type` with its flags as a [`Flags`] set), decoded where php
//! verifies it (`pdo_verify_fetch_mode`); what the default style fetches
//! with ([`Target`], php's `stmt->fetch` union); a column's value as
//! `fetch_value` converts it; and a row as `do_fetch` shapes it, with
//! `fetch()`, `fetchAll()`, `fetchColumn()`, `fetchObject()` and
//! `setFetchMode()` over it.

use php_ext::abi::{Callable, ClassRef, OrFalse, Raw, True};
use php_ext::call::{autoload, call, call_array, known_method, lookup_class, new_object};
use php_ext::coerce::{callable_checked, to_long, to_string, truthy, value_name, with_string};
use php_ext::decl::Name;
use php_ext::raising::{
    Level, Thrown, arg_count_error, argument_error, docref, throw_error, while_pending,
};
use php_ext::request::{Request, Streams as _};
use php_ext::values::{
    ClassId, Key, Owner, PhpArray, PhpObject, PhpString, PropertyScope, Value, ValueRef,
};

use crate::bound::BoundTable;
use crate::connector::{BindEvent, BindKind, Orientation, SqlState};
use crate::dbh::Nulls;
use crate::error::{self, On};
use crate::stmt;

/// `enum pdo_fetch_type`, the style without its bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Default,
    Lazy,
    Assoc,
    Num,
    Both,
    Obj,
    Bound,
    Column,
    Class,
    Into,
    Func,
    Named,
    KeyPair,
}

impl Kind {
    /// The low four bits, `how & ~PDO_FETCH_FLAGS`; `None` for the three
    /// numbers php names no style.
    fn of(bits: i64) -> Option<Kind> {
        Some(match bits & 0xF {
            0 => Kind::Default,
            1 => Kind::Lazy,
            2 => Kind::Assoc,
            3 => Kind::Num,
            4 => Kind::Both,
            5 => Kind::Obj,
            6 => Kind::Bound,
            7 => Kind::Column,
            8 => Kind::Class,
            9 => Kind::Into,
            10 => Kind::Func,
            11 => Kind::Named,
            12 => Kind::KeyPair,
            _ => return None,
        })
    }
}

/// One fetch bit, the bits above the style.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Flag {
    Group = 32,
    Unique = 64,
    ClassType = 128,
    PropsLate = 256,
    Serialize = 512,
}

/// `how & PDO_FETCH_FLAGS`, as a set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Flags(i64);

impl Flags {
    fn of(bits: i64) -> Flags {
        Flags(bits & !0xF)
    }

    pub(crate) fn has(self, flag: Flag) -> bool {
        self.0 & flag as i64 != 0
    }

    fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether the set is `flag` and nothing else, php's `bits ==
    /// PDO_FETCH_GROUP`.
    fn is(self, flag: Flag) -> bool {
        self.0 == flag as i64
    }
}

/// A fetch style as a fetch reads it: the style, `None` for a number php
/// names none, and its bits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mode {
    pub(crate) kind: Option<Kind>,
    pub(crate) flags: Flags,
}

impl Mode {
    /// `how` split, the stored default standing in for
    /// `PDO_FETCH_USE_DEFAULT`, as `do_fetch` resolves it.
    fn resolve(how: i64, default: i64) -> Mode {
        let how = if Kind::of(how) == Some(Kind::Default) && Flags::of(how).is_empty() {
            default
        } else {
            how
        };
        Mode {
            kind: Kind::of(how),
            flags: Flags::of(how),
        }
    }
}

/// `pdo_verify_fetch_mode`: `how` held to php's refusals, the argument
/// `arg` named, as `fetch()` (`fetch_all` false) or `fetchAll()` asks.
fn verify(
    req: &mut Request,
    default: i64,
    how: i64,
    arg: u32,
    fetch_all: bool,
) -> Result<(), Thrown> {
    let bitmask = |req: &mut Request| {
        argument_error(
            req,
            "ValueError",
            arg,
            "must be a bitmask of PDO::FETCH_* constants",
        )
    };
    if !(0..1024).contains(&how) {
        return Err(bitmask(req));
    }
    let (mut kind, mut flags) = (Kind::of(how), Flags::of(how));
    if kind == Some(Kind::Default) {
        kind = Kind::of(default);
        flags = Flags::of(default);
    }
    if !flags.is_empty() {
        let class_flags =
            flags.has(Flag::ClassType) || flags.has(Flag::PropsLate) || flags.has(Flag::Serialize);
        if class_flags && kind != Some(Kind::Class) {
            return Err(argument_error(
                req,
                "ValueError",
                arg,
                "cannot use PDO::FETCH_CLASSTYPE, PDO::FETCH_PROPS_LATE, or PDO::FETCH_SERIALIZE fetch flags with a fetch mode other than PDO::FETCH_CLASS",
            ));
        }
        if flags.has(Flag::Serialize) {
            docref(
                req,
                Level::Deprecated,
                "The PDO::FETCH_SERIALIZE mode is deprecated",
            );
        }
    }
    match kind {
        Some(Kind::Func) if !fetch_all => Err(argument_error(
            req,
            "ValueError",
            arg,
            "PDO::FETCH_FUNC can only be used with PDOStatement::fetchAll()",
        )),
        Some(Kind::Lazy) if fetch_all => Err(argument_error(
            req,
            "ValueError",
            arg,
            "PDO::FETCH_LAZY cannot be used with PDOStatement::fetchAll()",
        )),
        Some(Kind::Into) if fetch_all => Err(argument_error(
            req,
            "ValueError",
            arg,
            "PDO::FETCH_INTO cannot be used with PDOStatement::fetchAll()",
        )),
        Some(Kind::Default) | None => Err(bitmask(req)),
        Some(_) => Ok(()),
    }
}

/// `stmt->fetch`: what the default style fetches with, the column of
/// `FETCH_COLUMN`, the class and constructor arguments of `FETCH_CLASS`
/// (an owner of the array), or the object of `FETCH_INTO` (an owner).
#[derive(Default)]
pub(crate) enum Target {
    #[default]
    None,
    Column(i64),
    Class {
        class: ClassId,
        args: Option<Owner<PhpArray>>,
    },
    Into(Owner<PhpObject>),
}

/// What one fetch reads of the statement's [`Target`], views of what it
/// owns, copied off the statement before the fetch serves PHP code.
#[derive(Clone, Copy, Default)]
struct Aim {
    column: i64,
    class: Option<(ClassId, Option<PhpArray>)>,
    into: Option<PhpObject>,
}

impl Target {
    /// `pdo_stmt_free_default_fetch_mode`.
    pub(crate) fn release(self, req: &mut Request) {
        match self {
            Target::Class {
                args: Some(args), ..
            } => args.give_up(req),
            Target::Into(object) => object.give_up(req),
            _ => {}
        }
    }

    /// What the target owns, for the cycle collector.
    pub(crate) fn visit(&self, visit: &mut dyn FnMut(Value)) {
        match self {
            Target::Class {
                args: Some(args), ..
            } => visit(Value::array(**args)),
            Target::Into(object) => visit(object.as_value()),
            _ => {}
        }
    }

    /// The views a fetch reads.
    fn aim(&self) -> Aim {
        match self {
            Target::None => Aim::default(),
            Target::Column(column) => Aim {
                column: *column,
                ..Aim::default()
            },
            Target::Class { class, args } => Aim {
                class: Some((*class, args.as_deref().copied())),
                ..Aim::default()
            },
            Target::Into(object) => Aim {
                into: Some(**object),
                ..Aim::default()
            },
        }
    }
}

/// What one fetch reads beside the statement's own target: the class
/// `fetchObject()` and `fetchAll()` name for this call, and the
/// function `fetchAll(PDO::FETCH_FUNC)` is given.
#[derive(Default)]
struct With {
    class: Option<(ClassId, Option<PhpArray>)>,
    func: Option<Callable>,
}

/// A row as `do_fetch` answers it, with its group key's bytes where it
/// was asked for one; `None` where no row came or the fetch failed.
type Fetched = Option<(Owner<Value>, Option<Vec<u8>>)>;

/// The container a row is laid into: the array of the array styles,
/// written through its owner, or the object, or nothing, of the others.
enum Out {
    Array(Owner<PhpArray>),
    Value(Owner<Value>),
}

impl Out {
    fn into_value(self) -> Owner<Value> {
        match self {
            Out::Array(array) => array.into_value(),
            Out::Value(value) => value,
        }
    }

    fn object(&self) -> Option<PhpObject> {
        match self {
            Out::Value(value) => value.as_object(),
            Out::Array(_) => None,
        }
    }
}

// ---- one value

/// `fetch_value`: column `column` of the current row, one owner the
/// caller's, the connection's null handling and stringification and the
/// bound type `ty` applied as php applies them.
pub(super) fn value(
    req: &mut Request,
    object: PhpObject,
    column: i64,
    ty: Option<BindKind>,
) -> Result<Owner<Value>, Thrown> {
    let count = stmt::read(req, object, |s| s.column_count).unwrap_or(0);
    let Ok(at) = usize::try_from(column) else {
        return Err(throw_error(
            req,
            "ValueError",
            "Column index must be greater than or equal to 0",
        ));
    };
    if at >= count {
        return Err(throw_error(req, "ValueError", "Invalid column index"));
    }
    let got = stmt::with_driver(req, object, |req, driver| driver.value(req, at, ty))?;
    let mut value = match got {
        Ok(value) => value,
        Err(failure) => {
            stmt::record(req, object, failure)?;
            Owner::null()
        }
    };
    let (nulls, stringify) = stmt::dbh_of(req, object)
        .and_then(|dbh| crate::dbh::state(req, dbh).map(|d| (d.nulls, d.stringify)))
        .unwrap_or((Nulls::Natural, false));
    if nulls == Nulls::EmptyString && value.as_str_bytes().is_some_and(<[u8]>::is_empty) {
        value.give_up(req);
        value = Owner::null();
    }
    let ty = if stringify { Some(BindKind::Str) } else { ty };
    if let Some(ty) = ty
        && !crate::is_null(&value)
    {
        // The value php converted given up, and where the conversion
        // throws given up all the same.
        match convert(req, *value, ty) {
            Ok(None) => {}
            Ok(Some(converted)) => {
                value.give_up(req);
                value = converted;
            }
            Err(thrown) => {
                value.give_up(req);
                return Err(thrown);
            }
        }
    }
    if nulls == Nulls::ToString && crate::is_null(&value) {
        value.give_up(req);
        value = crate::string(req, b"")?;
    }
    Ok(value)
}

/// The conversions `fetch_value` makes for a bound type: `None` where
/// the value stands as it is.
fn convert(req: &mut Request, value: Value, ty: BindKind) -> Result<Option<Owner<Value>>, Thrown> {
    let scalar = match ty {
        BindKind::Int => Value::int(to_long(req, &value)?),
        BindKind::Bool => Value::bool(truthy(req, &value)?),
        BindKind::Null => Value::null(),
        BindKind::Str => match value.as_ref() {
            ValueRef::Bool(false) => return crate::string(req, b"0").map(Some),
            ValueRef::Str(_) => return Ok(None),
            ValueRef::Resource(_) => match php_ext::request::Stream::from_value(req, &value) {
                Some(stream) => return Ok(Some(stream.copy_to_mem(req, None)?.into_value())),
                None => return crate::string(req, b"").map(Some),
            },
            _ => return Ok(Some(to_string(req, &value)?.into_value())),
        },
        BindKind::Lob => match value.as_str_bytes().map(<[u8]>::to_vec) {
            Some(bytes) => return memory_stream(req, &bytes).map(Some),
            None => return Ok(None),
        },
        BindKind::Stmt | BindKind::Other(_) => return Ok(None),
    };
    Ok(Some(Owner::take(req, scalar)))
}

/// `php_stream_memory_open(TEMP_STREAM_READONLY, str)`: a stream over
/// the bytes, at its start.
fn memory_stream(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    let Some(stream) = req.open_stream(b"php://memory", b"w+b", false)? else {
        return Ok(Owner::null());
    };
    stream.write(req, bytes);
    stream.seek(req, 0, php_ext::request::Whence::Set);
    Ok(stream.into_resource().into_value())
}

// ---- one row

/// `do_fetch_common`: the connector moved to the next row, the columns
/// described where they were not, and each bound column written;
/// `false` where no row came or a step failed, its error recorded.
fn common(
    req: &mut Request,
    object: PhpObject,
    orientation: Orientation,
    offset: i64,
) -> Result<bool, Thrown> {
    if !stmt::read(req, object, |s| s.executed).unwrap_or(false) {
        return Ok(false);
    }
    if !stmt::dispatch(req, object, BindEvent::FetchPre)? {
        return Ok(false);
    }
    let moved = stmt::with_driver(req, object, |req, driver| {
        driver.fetch(req, orientation, offset)
    })?;
    match moved {
        Ok(true) => {}
        Ok(false) => return Ok(false),
        Err(failure) => {
            stmt::record(req, object, failure)?;
            return Ok(false);
        }
    }
    if !stmt::read(req, object, |s| s.columns.is_some()).unwrap_or(false)
        && !stmt::describe_columns(req, object)?
    {
        return Ok(false);
    }
    if !stmt::dispatch(req, object, BindEvent::FetchPost)? {
        return Ok(false);
    }
    let bound: Vec<(i64, Value, BindKind)> = stmt::read(req, object, |s| {
        s.columns_bound
            .iter()
            .flat_map(BoundTable::iter)
            .filter(|b| b.position >= 0)
            .map(|b| (b.position, *b.value, b.ty.kind()))
            .collect()
    })
    .unwrap_or_default();
    for (position, cell, ty) in bound {
        let Some(cell) = cell.as_php_ref() else {
            continue;
        };
        let fetched = value(req, object, position, Some(ty))?;
        cell.set(req, fetched)?;
    }
    Ok(true)
}

/// A symbol-table key for a column's name, `zend_symtable_update`'s: a
/// canonical decimal is the integer, and the string made for it given
/// up ([`Key::from_string`]).
fn symtable_key(req: &mut Request, name: &[u8]) -> Result<Owner<Key>, Thrown> {
    let made = PhpString::new(req, name)?;
    Ok(Key::from_string(req, made))
}

/// `do_fetch`: the next row shaped as `how` asks, with the group key
/// where `grouped`; `None` where no row came or the fetch failed, its
/// error recorded on the statement and not yet handled. The statement is
/// fetching from the container's making on, so a `FETCH_PROPS_LATE`
/// constructor that calls `setFetchMode` is refused.
///
/// ```php
/// $db = new PDO('sqlite::memory:');
/// class C { public $a; function __construct() { global $s; try { $s->setFetchMode(PDO::FETCH_NUM); } catch (Error $e) { echo $e->getMessage(), "\n"; } } }
/// $s = $db->query('SELECT 1 AS a');
/// $s->setFetchMode(PDO::FETCH_CLASS | PDO::FETCH_PROPS_LATE, 'C');
/// var_dump($s->fetch());
/// ```
/// ```expect
/// Cannot change default fetch mode while fetching
/// object(C)#3 (1) {
///   ["a"]=>
///   int(1)
/// }
/// ```
fn shaped(
    req: &mut Request,
    object: PhpObject,
    how: i64,
    orientation: Orientation,
    offset: i64,
    grouped: bool,
    with: &With,
) -> Result<Fetched, Thrown> {
    let (default, target) = stmt::read(req, object, |s| (s.default_fetch, s.target.aim()))
        .unwrap_or((4, Aim::default()));
    let mode = Mode::resolve(how, default);
    let flags = mode.flags;
    if !common(req, object, orientation, offset)? {
        return Ok(None);
    }
    let Some(kind) = mode.kind else {
        return Ok(Some((Owner::null(), None)));
    };
    match kind {
        Kind::Bound => return Ok(Some((Owner::bool(true), None))),
        Kind::Lazy => return crate::row::lazy(req, object).map(|v| Some((v, None))),
        Kind::Column => return column_row(req, object, flags, target.column, grouped),
        _ => {}
    }
    let Some(dbh) = stmt::dbh_of(req, object) else {
        return Ok(None);
    };
    // `stmt->in_fetch` from the container's making to the row's end, so
    // an error the class refuses the fetch with, or a constructor served
    // on the row, meets a statement that is fetching.
    stmt::write(req, object, |s| s.in_fetch = true);
    let row = made(req, object, dbh, (kind, flags), target, grouped, with);
    stmt::write(req, object, |s| s.in_fetch = false);
    row
}

/// `do_fetch` from the container's making on: the container the style
/// fetches into, filled.
fn made(
    req: &mut Request,
    object: PhpObject,
    dbh: PhpObject,
    (kind, flags): (Kind, Flags),
    target: Aim,
    grouped: bool,
    with: &With,
) -> Result<Fetched, Thrown> {
    let failed = |req: &mut Request, supp: &str| -> Result<Fetched, Thrown> {
        error::raise_impl(req, dbh, On::Stmt(object), SqlState::GENERAL, Some(supp))?;
        Ok(None)
    };
    let count = stmt::read(req, object, |s| s.column_count).unwrap_or(0);
    let mut next = 0i64;
    let mut class_args = None;
    let mut class = None;
    let out = match kind {
        Kind::Default | Kind::Assoc | Kind::Both | Kind::Num | Kind::Named => {
            Out::Array(PhpArray::new(req)?)
        }
        Kind::Obj => {
            let Some(std) = lookup_class(req, b"stdClass")? else {
                return Ok(None);
            };
            Out::Value(new_object(req, std)?.into_value())
        }
        Kind::Class => {
            let named = with.class.or(target.class);
            let Some((instance, ce)) = class_object(req, object, dbh, flags, named, &mut next)?
            else {
                return Ok(None);
            };
            class = Some(ce);
            class_args = named.and_then(|(_, args)| args);
            Out::Value(instance)
        }
        Kind::Into => {
            let Some(into) = target.into else {
                return failed(req, "No fetch-into object specified.");
            };
            Out::Value(Owner::take(req, into.as_value()))
        }
        Kind::Func if with.func.is_none() => return failed(req, "No fetch function specified"),
        Kind::Func | Kind::Lazy | Kind::Bound | Kind::Column | Kind::KeyPair => {
            Out::Value(Owner::null())
        }
    };
    fill(
        req, object, kind, flags, out, next, count, grouped, with, class, class_args,
    )
}

/// `FETCH_CLASS`'s object: of the class the first column names under
/// `FETCH_CLASSTYPE` or of the one the call or the default names, made
/// without its constructor, which `FETCH_PROPS_LATE` serves now; with
/// the class. `None` where php refuses the fetch, its error raised on
/// the statement.
///
/// The class named is `zend_lookup_class`, which asks the autoloaders.
///
/// ```php
/// spl_autoload_register(function ($c) { echo "load $c\n"; if ($c === 'Lazy') { class Lazy { public $a; } } });
/// $db = new PDO('sqlite::memory:');
/// var_dump($db->query("SELECT 'Lazy' AS c, 1 AS a")->fetch(PDO::FETCH_CLASS | PDO::FETCH_CLASSTYPE));
/// ```
/// ```expect
/// load Lazy
/// object(Lazy)#4 (1) {
///   ["a"]=>
///   int(1)
/// }
/// ```
fn class_object(
    req: &mut Request,
    object: PhpObject,
    dbh: PhpObject,
    flags: Flags,
    named: Option<(ClassId, Option<PhpArray>)>,
    next: &mut i64,
) -> Result<Option<(Owner<Value>, ClassId)>, Thrown> {
    let refuse = |req: &mut Request, supp: &str| {
        error::raise_impl(req, dbh, On::Stmt(object), SqlState::GENERAL, Some(supp))
    };
    let ce = if flags.has(Flag::ClassType) {
        let name = value(req, object, *next, None)?;
        *next += 1;
        // A column that is no string leaves the class the call or the
        // default named, as `do_fetch` only looks up a string.
        let bytes = name.as_str_bytes().map(<[u8]>::to_vec);
        name.give_up(req);
        let found = match bytes {
            Some(name) => autoload(req, &name)?,
            None => named.map(|(class, _)| class),
        };
        match found {
            Some(found) => found,
            None => match lookup_class(req, b"stdClass")? {
                Some(std) => std,
                None => return Ok(None),
            },
        }
    } else if let Some((class, _)) = named {
        class
    } else {
        refuse(req, "No fetch class specified")?;
        return Ok(None);
    };
    let args = named.and_then(|(_, args)| args);
    if flags.has(Flag::Serialize) {
        if !implements_serializable(req, ce) {
            refuse(req, "cannot unserialize class")?;
            return Ok(None);
        }
        return Ok(Some((new_object(req, ce)?.into_value(), ce)));
    }
    let instance = new_object(req, ce)?;
    if flags.has(Flag::PropsLate)
        && let Err(thrown) = construct(req, *instance, args)
    {
        instance.give_up(req);
        return Err(thrown);
    }
    Ok(Some((instance.into_value(), ce)))
}

/// The body of `do_fetch` past the container's making: the group key,
/// a serialized object's payload, each column written as the style
/// writes it, and the constructor or the function served after. A
/// `FETCH_CLASS` column is written in the fetch class's scope and a
/// `FETCH_INTO` column in the executing one, `zend_update_property_ex`
/// with `ce` and with `NULL` (`ext/pdo/pdo_stmt.c:798-812, 911-914`),
/// so a parent's private property is a dynamic one and a protected
/// one is php's `Error`. A write that throws does not end the row:
/// `zend_update_property_ex` returns nothing, the later columns are
/// written with the throwable pending, each later one chained onto it,
/// and it leaves after the loop, where the constructor is not served
/// (`ext/pdo/pdo_stmt.c:911-914, 924-929`).
///
/// ```php
/// class P { protected $a; public $b; }
/// $db = new PDO('sqlite::memory:');
/// $s = $db->query("SELECT 'x' AS a, 'y' AS b");
/// $s->setFetchMode(PDO::FETCH_INTO, $p = new P);
/// try { $s->fetch(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($p->b);
/// ```
/// ```expect
/// Cannot access protected property P::$a
/// string(1) "y"
/// ```
///
/// ```php
/// class B { private $v; }
/// #[AllowDynamicProperties] class D extends B {}
/// class P { protected $v; }
/// $db = new PDO('sqlite::memory:');
/// var_dump($db->query("SELECT 'x' AS v")->fetchObject('D'));
/// $s = $db->query("SELECT 'y' AS v");
/// $s->setFetchMode(PDO::FETCH_INTO, new P);
/// try { $s->fetch(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(D)#3 (2) {
///   ["v":"B":private]=>
///   NULL
///   ["v"]=>
///   string(1) "x"
/// }
/// Cannot access protected property P::$v
/// ```
///
/// A later typed column that fails while that throwable is pending is
/// left unwritten and raises nothing (`zend_verify_property_type_error`):
///
/// ```php
/// class E { public int $a; public int $b; }
/// $db = new PDO('sqlite::memory:');
/// try { $db->query("SELECT 'x' AS a, 'y' AS b")->fetchObject('E'); } catch (TypeError $e) { echo $e->getMessage(), "|", $e->getPrevious()?->getMessage(), "\n"; }
/// ```
/// ```expect
/// Cannot assign string to property E::$a of type int|
/// ```
#[allow(clippy::too_many_arguments)]
fn fill(
    req: &mut Request,
    object: PhpObject,
    kind: Kind,
    flags: Flags,
    mut out: Out,
    next: i64,
    count: usize,
    grouped: bool,
    with: &With,
    class: Option<ClassId>,
    class_args: Option<PhpArray>,
) -> Result<Fetched, Thrown> {
    // `FETCH_FUNC`'s arguments, given up after the function is served.
    let mut arguments = Vec::new();
    let laid = columns(
        req,
        object,
        (kind, flags),
        &mut out,
        (next, count, grouped),
        with,
        (class, class_args),
        &mut arguments,
    );
    for argument in arguments {
        argument.give_up(req);
    }
    // The row is given up where its columns throw, as the VM frees what
    // the native made.
    match laid {
        Ok(key) => Ok(Some((out.into_value(), key))),
        Err(thrown) => {
            out.into_value().give_up(req);
            Err(thrown)
        }
    }
}

/// [`fill`]'s group key and columns from `next` on, laid into `out` as
/// `kind` asks, and the class's constructor or the fetch function served
/// over them; the throwable a column's write left pending is the row's
/// answer.
#[allow(clippy::too_many_arguments)]
fn columns(
    req: &mut Request,
    object: PhpObject,
    (kind, flags): (Kind, Flags),
    out: &mut Out,
    (mut next, count, grouped): (i64, usize, bool),
    with: &With,
    (class, class_args): (Option<ClassId>, Option<PhpArray>),
    arguments: &mut Vec<Owner<Value>>,
) -> Result<Option<Vec<u8>>, Thrown> {
    let mut key = None;
    if grouped {
        let fetched = value(req, object, next, None)?;
        let text = with_string(req, &fetched, <[u8]>::to_vec);
        fetched.give_up(req);
        key = Some(text?);
        next += 1;
    }
    if kind == Kind::Class && flags.has(Flag::Serialize) {
        let payload = value(req, object, next, None)?;
        let bytes = payload
            .as_str_bytes()
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        payload.give_up(req);
        if let Some(made) = out.object() {
            let text = crate::string(req, &bytes)?;
            let answered = php_ext::call::call_method(req, &made, b"unserialize", &[*text]);
            text.give_up(req);
            answered?.give_up(req);
        }
        next += 1;
    }
    let mut pending: Option<Thrown> = None;
    let mut at = next;
    while usize::try_from(at).is_ok_and(|a| a < count) {
        let fetched = value(req, object, at, None)?;
        // `stmt->columns[i].name`, read as php reads it after the value;
        // each sink below takes its own owner of it before PHP code runs.
        let name = stmt::read(req, object, |s| {
            s.columns
                .as_ref()?
                .name(usize::try_from(at).unwrap_or(usize::MAX))
        })
        .flatten();
        let Some(name) = name else {
            fetched.give_up(req);
            at += 1;
            continue;
        };
        match (kind, out.object()) {
            (Kind::Assoc, _) => set(req, out, name, fetched)?,
            (Kind::Default | Kind::Both, _) => both(req, out, name, at, fetched)?,
            (Kind::Named, _) => named(req, out, name, fetched)?,
            (Kind::Num, _) => match out {
                Out::Array(array) => array.append(req, fetched)?,
                Out::Value(_) => fetched.give_up(req),
            },
            (Kind::Class | Kind::Obj | Kind::Into, Some(made)) => {
                write_column(req, &mut pending, made, scope(kind, class), name, fetched)?;
            }
            (Kind::Func, _) => arguments.push(fetched),
            _ => fetched.give_up(req),
        }
        at += 1;
    }
    if let Some(thrown) = pending {
        return Err(thrown);
    }
    if kind == Kind::Class
        && !flags.has(Flag::PropsLate)
        && !flags.has(Flag::Serialize)
        && let (Some(made), Some(_)) = (out.object(), class)
    {
        construct(req, made, class_args)?;
    }
    if kind == Kind::Func
        && let Some(func) = &with.func
    {
        let views: Vec<Value> = arguments.iter().map(|argument| **argument).collect();
        let answered = call(req, func, &views)?;
        std::mem::replace(out, Out::Value(answered))
            .into_value()
            .give_up(req);
    }
    Ok(key)
}

/// One column of an object's row, `zend_update_property_ex`: a write
/// that throws leaves the throwable pending, chained onto one already
/// there, for the row's later columns to be written past; a fatal or an
/// `exit` is the `Err`.
fn write_column(
    req: &mut Request,
    pending: &mut Option<Thrown>,
    made: PhpObject,
    scope: PropertyScope,
    name: PhpString,
    fetched: Owner<Value>,
) -> Result<(), Thrown> {
    let name = Owner::take(req, name);
    let wrote = match pending {
        Some(standing) => while_pending(req, standing, |req| {
            made.write_prop_in(req, scope, name, fetched)
        })
        .unwrap_or(Ok(())),
        None => made.write_prop_in(req, scope, name, fetched),
    };
    let Err(thrown) = wrote else {
        return Ok(());
    };
    if thrown.exception().is_none() {
        return Err(thrown);
    }
    *pending = Some(match pending.take() {
        // The write refused past the standing throwable answers it, with
        // no owner of its own ([`while_pending`]).
        Some(earlier) if earlier.exception() == thrown.exception() => earlier,
        Some(earlier) => earlier.chain(req, thrown),
        None => thrown,
    });
    Ok(())
}

/// The scope `do_fetch` writes an object's column in: the fetch class's
/// for `FETCH_CLASS`, `NULL`'s for `FETCH_INTO` and `stdClass`'s for
/// `FETCH_OBJ`, which declares nothing.
fn scope(kind: Kind, class: Option<ClassId>) -> PropertyScope {
    match (kind, class) {
        (Kind::Class, Some(class)) => PropertyScope::Of(ClassRef(class)),
        (Kind::Into, _) => PropertyScope::Executing,
        _ => PropertyScope::Declaring,
    }
}

/// `FETCH_BOTH`'s column: the value under its name and, where the
/// number names no column already, under its number
/// (`zend_hash_index_add`).
fn both(
    req: &mut Request,
    out: &mut Out,
    name: PhpString,
    at: i64,
    fetched: Owner<Value>,
) -> Result<(), Thrown> {
    let held = Owner::take(req, *fetched);
    if let Err(thrown) = set(req, out, name, held) {
        fetched.give_up(req);
        return Err(thrown);
    }
    match out {
        Out::Array(array) if !array.contains(&Key::Int(at)) => {
            array.set(req, Owner::index(at), fetched)
        }
        _ => {
            fetched.give_up(req);
            Ok(())
        }
    }
}

/// `zend_symtable_update` on the row's array under the column's name,
/// one more owner of the statement's string; the value given up where
/// the row is no array. Every row shares the names the statement holds,
/// whatever the style that keys or names by them:
///
/// ```php
/// $db = new PDO('sqlite::memory:');
/// $q = "SELECT 1 AS first, 2 AS second, 3 AS `7` UNION SELECT 4, 5, 6";
/// foreach ([PDO::FETCH_ASSOC, PDO::FETCH_BOTH, PDO::FETCH_NAMED, PDO::FETCH_OBJ] as $mode) {
///     $s = $db->query($q);
///     $m = memory_get_usage();
///     $rows = $s->fetchAll($mode);
///     echo memory_get_usage() - $m, "\n";
///     unset($rows, $s);
/// }
/// ```
/// ```expect
/// 968
/// 968
/// 968
/// 1048
/// ```
fn set(
    req: &mut Request,
    out: &mut Out,
    name: PhpString,
    value: Owner<Value>,
) -> Result<(), Thrown> {
    let Out::Array(array) = out else {
        value.give_up(req);
        return Ok(());
    };
    array.set(req, Owner::take(req, Key::lookup(name)), value)
}

/// `FETCH_NAMED`'s column: a name seen before becomes a list of every
/// value it had, appended to where it stands once it is one.
fn named(
    req: &mut Request,
    out: &mut Out,
    name: PhpString,
    fetched: Owner<Value>,
) -> Result<(), Thrown> {
    let Out::Array(array) = out else {
        fetched.give_up(req);
        return Ok(());
    };
    let current = array.find(name.as_bytes());
    if let Some(mut list) = current.and_then(|current| current.as_array()) {
        return list.append(req, fetched);
    }
    let stored = match current {
        Some(current) => listed(req, current, fetched)?,
        None => fetched,
    };
    // `zend_hash_update`: the name as the string key it is.
    array.set(req, Owner::take(req, name).into_key(), stored)
}

/// A new list of `first`, the value a name held, and `fetched`.
fn listed(req: &mut Request, first: Value, fetched: Owner<Value>) -> Result<Owner<Value>, Thrown> {
    let mut list = match PhpArray::new(req) {
        Ok(list) => list,
        Err(thrown) => {
            fetched.give_up(req);
            return Err(thrown);
        }
    };
    let appended = match list.append(req, Owner::take(req, first)) {
        Ok(()) => list.append(req, fetched),
        Err(thrown) => {
            fetched.give_up(req);
            Err(thrown)
        }
    };
    match appended {
        Ok(()) => Ok(list.into_value()),
        Err(thrown) => {
            list.give_up(req);
            Err(thrown)
        }
    }
}

/// Whether `class` implements `Serializable`, php's `ce->unserialize`
/// being set.
fn implements_serializable(req: &mut Request, class: ClassId) -> bool {
    let Ok(Some(serializable)) = lookup_class(req, b"Serializable") else {
        return false;
    };
    match new_object(req, class) {
        Ok(probe) => {
            let is = probe.instance_of(req, serializable);
            probe.give_up(req);
            is
        }
        Err(_) => false,
    }
}

/// `pdo_call_fetch_object_constructor`: the class's constructor served
/// on `made` with `args` as its named parameters.
fn construct(req: &mut Request, made: PhpObject, args: Option<PhpArray>) -> Result<(), Thrown> {
    let constructor = Name(b"__construct".as_slice().into());
    let Some(callable) = known_method(req, made.class(), &constructor, Some(made)) else {
        return Ok(());
    };
    call_constructor(req, &callable, args)
}

/// A constructor served with `args` as its parameters, or with none, its
/// return given up: what `pdo_call_fetch_object_constructor` and
/// `pdo_stmt_construct` hand `zend_call_known_function`.
pub(super) fn call_constructor(
    req: &mut Request,
    callable: &Callable,
    args: Option<PhpArray>,
) -> Result<(), Thrown> {
    let args = args.unwrap_or_else(PhpArray::empty);
    call_array(req, callable, args)?.give_up(req);
    Ok(())
}

/// `FETCH_COLUMN`'s one value, and its group key, as `do_fetch` reads
/// the column the style and its bits name.
fn column_row(
    req: &mut Request,
    object: PhpObject,
    flags: Flags,
    column: i64,
    grouped: bool,
) -> Result<Fetched, Thrown> {
    let count = stmt::read(req, object, |s| s.column_count).unwrap_or(0);
    let colno = if (flags.has(Flag::Group) || flags.has(Flag::Unique)) && column == -1 {
        1
    } else {
        column
    };
    if colno < 0 {
        return Err(throw_error(
            req,
            "ValueError",
            "Column index must be greater than or equal to 0",
        ));
    }
    if usize::try_from(colno).is_ok_and(|c| c >= count) {
        return Err(throw_error(req, "ValueError", "Invalid column index"));
    }
    let read_at = if flags.is(Flag::Group) && column == -1 {
        1
    } else if flags.is(Flag::Group) && colno != 0 {
        0
    } else {
        colno
    };
    let fetched = value(req, object, read_at, None)?;
    if !grouped {
        return Ok(Some((fetched, None)));
    }
    let key_at = if flags.is(Flag::Group) && column > 0 {
        colno
    } else {
        0
    };
    let key = value(req, object, key_at, None).and_then(|raw| {
        let key = with_string(req, &raw, <[u8]>::to_vec);
        raw.give_up(req);
        key
    });
    match key {
        Ok(key) => Ok(Some((fetched, Some(key)))),
        Err(thrown) => {
            fetched.give_up(req);
            Err(thrown)
        }
    }
}

/// One row as the default style shapes it, for the statement's iterator.
pub(super) fn row(
    req: &mut Request,
    object: PhpObject,
    how: i64,
) -> Result<Option<Owner<Value>>, Thrown> {
    Ok(shaped(
        req,
        object,
        how,
        Orientation::Forward,
        0,
        false,
        &With::default(),
    )?
    .map(|(value, _)| value))
}

/// `pdo_do_key_pair_fetch`: the next row's two columns into `container`
/// as key and value; `false` where no row came or it had other than two
/// columns.
fn key_pair(
    req: &mut Request,
    object: PhpObject,
    orientation: Orientation,
    offset: i64,
    container: &mut Owner<PhpArray>,
) -> Result<bool, Thrown> {
    if !common(req, object, orientation, offset)? {
        return Ok(false);
    }
    if stmt::read(req, object, |s| s.column_count).unwrap_or(0) != 2 {
        if let Some(dbh) = stmt::dbh_of(req, object) {
            error::raise_impl(
                req,
                dbh,
                On::Stmt(object),
                SqlState::GENERAL,
                Some(
                    "PDO::FETCH_KEY_PAIR fetch mode requires the result set to contain exactly 2 columns.",
                ),
            )?;
        }
        return Ok(false);
    }
    let key = value(req, object, 0, None)?;
    let fetched = match value(req, object, 1, None) {
        Ok(fetched) => fetched,
        Err(thrown) => {
            key.give_up(req);
            return Err(thrown);
        }
    };
    let made = match key.as_int() {
        Some(n) => Ok(Owner::index(n)),
        None => to_string(req, &key).map(|text| Key::from_string(req, text)),
    };
    key.give_up(req);
    match made {
        Ok(key) => container.set(req, key, fetched)?,
        Err(thrown) => {
            fetched.give_up(req);
            return Err(thrown);
        }
    }
    Ok(true)
}

// ---- setting the default

/// `pdo_stmt_setup_fetch_mode`: the default style and its target set from
/// `mode` and the arguments after it, held to php's counts and types;
/// `mode_arg` is the argument `mode` was, `1` for `setFetchMode()` and
/// `2` for `PDO::query()`. As php 8.5.12 sets it, atomically: the style
/// verified against the connection's default and the target made, the
/// class's autoloader called, before the statement changes, so a refusal
/// leaves the style before it standing, and the target replaced released
/// only once the new one stands, whatever PHP code the lookup called in
/// between; `PDO::FETCH_DEFAULT` stores the connection's default.
///
/// ```php
/// class C { public $a; function __construct(public $m) {} }
/// $db = new PDO('sqlite::memory:');
/// $s = $db->query('SELECT 1 AS a');
/// $s->setFetchMode(PDO::FETCH_CLASS, 'C', ['kept']);
/// try { $s->setFetchMode(PDO::FETCH_CLASS); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// try { $s->setFetchMode(PDO::FETCH_INTO, 'x'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// $r = $s->fetch(); echo get_class($r), ' ', $r->m, ' ', $r->a, "\n";
/// $db->setAttribute(PDO::ATTR_DEFAULT_FETCH_MODE, PDO::FETCH_NUM);
/// $s = $db->query('SELECT 2 AS a');
/// $s->setFetchMode(PDO::FETCH_ASSOC);
/// $s->setFetchMode(PDO::FETCH_DEFAULT);
/// var_dump($s->fetch());
/// $o = new stdClass;
/// $s = $db->query('SELECT 3 AS a UNION ALL SELECT 4');
/// $s->setFetchMode(PDO::FETCH_INTO, $o);
/// spl_autoload_register(function ($c) use ($s) { var_dump($s->fetch()->a); eval("class $c { public \$a; }"); });
/// $s->setFetchMode(PDO::FETCH_CLASS, 'Late');
/// var_dump($o->a, get_class($s->fetch()));
/// ```
/// ```expect
/// PDOStatement::setFetchMode() expects at least 2 arguments for the fetch mode provided, 1 given
/// PDOStatement::setFetchMode(): Argument #2 must be of type object, string given
/// C kept 1
/// array(1) {
///   [0]=>
///   int(2)
/// }
/// int(3)
/// int(3)
/// string(4) "Late"
/// ```
pub(super) fn setup_mode(
    req: &mut Request,
    object: PhpObject,
    mode: i64,
    mode_arg: u32,
    args: &[Value],
) -> Result<(), Thrown> {
    let dbh_default = stmt::dbh_of(req, object)
        .and_then(|dbh| crate::dbh::state(req, dbh).map(|d| i64::from(d.default_fetch)))
        .unwrap_or(4);
    verify(req, dbh_default, mode, mode_arg, false)?;
    let flags = Flags::of(mode);
    let given = u32::try_from(args.len()).unwrap_or(u32::MAX);
    let total = mode_arg + given;
    let first = mode_arg + 1;
    let count_error = |req: &mut Request, how: &str, n: u32| {
        arg_count_error(
            req,
            &format!("{how} {n} arguments for the fetch mode provided, {total} given"),
        )
    };
    let target = match Kind::of(mode) {
        Some(
            Kind::Default
            | Kind::Lazy
            | Kind::Assoc
            | Kind::Num
            | Kind::Both
            | Kind::Obj
            | Kind::Bound
            | Kind::Named
            | Kind::KeyPair,
        ) => {
            if given != 0 {
                return Err(count_error(req, "exactly", mode_arg));
            }
            Target::None
        }
        Some(Kind::Column) => {
            if given != 1 {
                return Err(count_error(req, "exactly", first));
            }
            let Some(n) = args[0].as_int() else {
                let name = value_name(req, &args[0]);
                return Err(argument_error(
                    req,
                    "TypeError",
                    first,
                    &format!("must be of type int, {name} given"),
                ));
            };
            if n < 0 {
                return Err(argument_error(
                    req,
                    "ValueError",
                    first,
                    "must be greater than or equal to 0",
                ));
            }
            Target::Column(n)
        }
        Some(Kind::Class) if flags.has(Flag::ClassType) => {
            if given != 0 {
                return Err(count_error(req, "exactly", mode_arg));
            }
            Target::None
        }
        Some(Kind::Class) => class_target(req, args, mode_arg, total)?,
        Some(Kind::Into) => {
            if total != first {
                return Err(count_error(req, "exactly", first));
            }
            let Some(into) = args[0].as_object() else {
                let shown = value_name(req, &args[0]);
                return Err(argument_error(
                    req,
                    "TypeError",
                    first,
                    &format!("must be of type object, {shown} given"),
                ));
            };
            Target::Into(Owner::take(req, into))
        }
        Some(Kind::Func) | None => {
            return Err(argument_error(
                req,
                "ValueError",
                mode_arg,
                "must be one of the PDO::FETCH_* constants",
            ));
        }
    };
    let stored = if Kind::of(mode) == Some(Kind::Default) {
        dbh_default
    } else {
        mode
    };
    let old = match object.native_mut::<stmt::Stmt>(req) {
        Some(s) => {
            s.default_fetch = stored;
            std::mem::replace(&mut s.target, target)
        }
        None => target,
    };
    old.release(req);
    Ok(())
}

/// `setup_fetch_mode`'s `FETCH_CLASS` arm: the class the first argument
/// names and the constructor arguments the second may give, held to
/// php's counts and types.
///
/// The class named is `zend_lookup_class`, which asks the autoloaders.
///
/// ```php
/// spl_autoload_register(function ($c) { echo "load $c\n"; if ($c === 'Lazy') { class Lazy { public $a; } } });
/// $s = (new PDO('sqlite::memory:'))->query('SELECT 1 AS a');
/// $s->setFetchMode(PDO::FETCH_CLASS, 'Lazy');
/// var_dump($s->fetch());
/// ```
/// ```expect
/// load Lazy
/// object(Lazy)#4 (1) {
///   ["a"]=>
///   int(1)
/// }
/// ```
fn class_target(
    req: &mut Request,
    args: &[Value],
    mode_arg: u32,
    total: u32,
) -> Result<Target, Thrown> {
    let given = u32::try_from(args.len()).unwrap_or(u32::MAX);
    let first = mode_arg + 1;
    let count_error = |req: &mut Request, how: &str, n: u32| {
        arg_count_error(
            req,
            &format!("{how} {n} arguments for the fetch mode provided, {total} given"),
        )
    };
    if given == 0 {
        return Err(count_error(req, "at least", first));
    }
    if given > 2 {
        return Err(count_error(req, "at most", mode_arg + 2));
    }
    let Some(name) = args[0].as_str_bytes().map(<[u8]>::to_vec) else {
        let shown = value_name(req, &args[0]);
        return Err(argument_error(
            req,
            "TypeError",
            first,
            &format!("must be of type string, {shown} given"),
        ));
    };
    let Some(class) = autoload(req, &name)? else {
        return Err(argument_error(
            req,
            "TypeError",
            first,
            "must be a valid class",
        ));
    };
    let mut ctor_args = None;
    if let Some(extra) = args.get(1) {
        match extra.as_ref() {
            ValueRef::Null => {}
            ValueRef::Array(array) => {
                if !array.is_empty() {
                    if !has_constructor(req, class) {
                        return Err(argument_error(
                            req,
                            "ValueError",
                            3,
                            "must be empty when class provided in argument #2 ($class) does not have a constructor",
                        ));
                    }
                    ctor_args = Some(Owner::take(req, array));
                }
            }
            _ => {
                let shown = value_name(req, extra);
                return Err(argument_error(
                    req,
                    "TypeError",
                    mode_arg + 2,
                    &format!("must be of type ?array, {shown} given"),
                ));
            }
        }
    }
    Ok(Target::Class {
        class,
        args: ctor_args,
    })
}

/// Whether `class` declares or inherits a constructor.
fn has_constructor(req: &mut Request, class: ClassId) -> bool {
    req.symbols().method_row(class, b"__construct").is_some()
}

// ---- the methods

/// `PDOStatement::fetch`'s body, its arguments held by the method.
pub(super) fn fetch(
    req: &mut Request,
    this: PhpObject,
    how: i64,
    (orientation, offset): (i64, i64),
) -> Result<Owner<Value>, Thrown> {
    let orientation = Orientation::of(orientation);
    let (object, _) = stmt::this_stmt(req, this)?;
    stmt::clear_error(req, object);
    let default = stmt::read(req, object, |s| s.default_fetch).unwrap_or(4);
    verify(req, default, how, 1, false)?;
    if Kind::of(how) == Some(Kind::KeyPair) {
        let mut out = PhpArray::new(req)?;
        let paired = key_pair(req, object, orientation, offset, &mut out);
        if matches!(paired, Ok(true)) {
            return Ok(out.into_value());
        }
        out.give_up(req);
        paired?;
        stmt::handle_error(req, object, false)?;
        return Ok(Owner::bool(false));
    }
    if let Some((value, _)) = shaped(
        req,
        object,
        how,
        orientation,
        offset,
        false,
        &With::default(),
    )? {
        Ok(value)
    } else {
        stmt::handle_error(req, object, false)?;
        Ok(Owner::bool(false))
    }
}

/// `PDOStatement::fetch_object`'s body, its arguments held by the method.
///
/// The class named is `zend_lookup_class`, which asks the autoloaders.
///
/// ```php
/// spl_autoload_register(function ($c) { echo "load $c\n"; if ($c === 'Lazy') { class Lazy { public $a; } } });
/// var_dump((new PDO('sqlite::memory:'))->query('SELECT 1 AS a')->fetchObject('Lazy'));
/// ```
/// ```expect
/// load Lazy
/// object(Lazy)#4 (1) {
///   ["a"]=>
///   int(1)
/// }
/// ```
pub(super) fn fetch_object(
    req: &mut Request,
    this: PhpObject,
    class: Option<PhpString>,
    args: Option<PhpArray>,
) -> Result<OrFalse<Owner<PhpObject>>, Thrown> {
    let named = class.map(|name| name.as_bytes().to_vec());
    let class = match named {
        Some(name) => {
            if let Some(class) = autoload(req, &name)? {
                class
            } else {
                let shown = String::from_utf8_lossy(&name).into_owned();
                return Err(argument_error(
                    req,
                    "TypeError",
                    1,
                    &format!("must be a valid class name, {shown} given"),
                ));
            }
        }
        None => match lookup_class(req, b"stdClass")? {
            Some(class) => class,
            None => return Ok(OrFalse::False),
        },
    };
    let (object, _) = stmt::this_stmt(req, this)?;
    stmt::clear_error(req, object);
    if args.is_some_and(|a| !a.is_empty()) && !has_constructor(req, class) {
        return Err(argument_error(
            req,
            "ValueError",
            2,
            "must be empty when class provided in argument #1 ($class) does not have a constructor",
        ));
    }
    let with = With {
        class: Some((class, args)),
        func: None,
    };
    if let Some((value, _)) = shaped(req, object, 8, Orientation::Forward, 0, false, &with)? {
        match value.into_object() {
            Ok(made) => Ok(OrFalse::Value(made)),
            Err(value) => {
                value.give_up(req);
                Err(php_ext::raising::fatal(
                    req,
                    "InternalError: PDOStatement::fetchObject() made no object",
                ))
            }
        }
    } else {
        stmt::handle_error(req, object, false)?;
        Ok(OrFalse::False)
    }
}

/// `PDOStatement::fetch_column`'s body, its arguments held by the method.
pub(super) fn fetch_column(
    req: &mut Request,
    this: PhpObject,
    column: i64,
) -> Result<Owner<Value>, Thrown> {
    let (object, _) = stmt::this_stmt(req, this)?;
    stmt::clear_error(req, object);
    if !common(req, object, Orientation::Forward, 0)? {
        stmt::handle_error(req, object, false)?;
        return Ok(Owner::bool(false));
    }
    value(req, object, column, None)
}

/// `fetchAll(PDO::FETCH_CLASS, ...)`'s class, `stdClass` where none is
/// named, and its constructor arguments.
///
/// The class named is `zend_lookup_class`, which asks the autoloaders.
///
/// ```php
/// spl_autoload_register(function ($c) { echo "load $c\n"; if ($c === 'Lazy') { class Lazy { public $a; } } });
/// var_dump((new PDO('sqlite::memory:'))->query('SELECT 1 AS a')->fetchAll(PDO::FETCH_CLASS, 'Lazy'));
/// ```
/// ```expect
/// load Lazy
/// array(1) {
///   [0]=>
///   object(Lazy)#4 (1) {
///     ["a"]=>
///     int(1)
///   }
/// }
/// ```
fn all_class(
    req: &mut Request,
    second: Option<Value>,
    third: Option<Value>,
) -> Result<Option<(ClassId, Option<PhpArray>)>, Thrown> {
    let class = match second.filter(|v| !crate::is_null(v)) {
        Some(named) => {
            let Some(bytes) = named.as_str_bytes().map(<[u8]>::to_vec) else {
                let shown = value_name(req, &named);
                return Err(argument_error(
                    req,
                    "TypeError",
                    2,
                    &format!("must be of type string, {shown} given"),
                ));
            };
            match autoload(req, &bytes)? {
                Some(class) => class,
                None => {
                    return Err(argument_error(req, "TypeError", 2, "must be a valid class"));
                }
            }
        }
        None => match lookup_class(req, b"stdClass")? {
            Some(class) => class,
            None => return Ok(None),
        },
    };
    let args = match third {
        Some(args) => match args.as_array() {
            Some(args) if !args.is_empty() => {
                if !has_constructor(req, class) {
                    return Err(argument_error(
                        req,
                        "ValueError",
                        3,
                        "must be empty when class provided in argument #2 ($class) does not have a constructor",
                    ));
                }
                Some(args)
            }
            _ => None,
        },
        None => None,
    };
    Ok(Some((class, args)))
}

/// `fetchAll()`'s arguments after the style, as the style reads them: the
/// class and constructor arguments of `FETCH_CLASS`, the function of
/// `FETCH_FUNC` and the column of `FETCH_COLUMN`, which stays the
/// statement's; php's counts and types held.
fn all_arguments(
    req: &mut Request,
    object: PhpObject,
    kind: Option<Kind>,
    flags: Flags,
    given: usize,
    second: Option<Value>,
    third: Option<Value>,
) -> Result<With, Thrown> {
    let mut with = With::default();
    match kind {
        Some(Kind::Class) => match all_class(req, second, third)? {
            Some(class) => with.class = Some(class),
            None => return Ok(with),
        },
        Some(Kind::Func) => {
            if given != 2 {
                return Err(arg_count_error(
                    req,
                    &format!("exactly 2 argument for PDO::FETCH_FUNC, {given} given"),
                ));
            }
            let Some(callback) = second.filter(|v| !crate::is_null(v)) else {
                return Err(argument_error(
                    req,
                    "TypeError",
                    2,
                    "must be a callable, null given",
                ));
            };
            with.func = match callable_checked(req, &callback) {
                Ok(callable) => Some(callable),
                Err(error) => return Err(throw_error(req, "TypeError", &error)),
            };
        }
        Some(Kind::Column) => {
            if given > 2 {
                return Err(arg_count_error(
                    req,
                    &format!("at most 2 argument for the fetch mode provided, {given} given"),
                ));
            }
            let column = match second {
                Some(column) => {
                    let Some(n) = column.as_int() else {
                        let shown = value_name(req, &column);
                        return Err(argument_error(
                            req,
                            "TypeError",
                            2,
                            &format!("must be of type int, {shown} given"),
                        ));
                    };
                    if n < 0 {
                        return Err(argument_error(
                            req,
                            "ValueError",
                            2,
                            "must be greater than or equal to 0",
                        ));
                    }
                    n
                }
                None if flags.has(Flag::Group) || flags.has(Flag::Unique) => -1,
                None => 0,
            };
            let old = stmt::write(req, object, |s| {
                std::mem::replace(&mut s.target, Target::Column(column))
            })
            .unwrap_or_default();
            old.release(req);
        }
        _ => {
            if given > 1 {
                return Err(arg_count_error(
                    req,
                    &format!("exactly 1 argument for the fetch mode provided, {given} given"),
                ));
            }
        }
    }
    Ok(with)
}

/// `PDOStatement::fetch_all`'s body, its arguments held by the method.
pub(super) fn fetch_all(
    req: &mut Request,
    this: PhpObject,
    mode: Raw<i64>,
    args: &[Value],
) -> Result<Owner<PhpArray>, Thrown> {
    let given = req.frames().argument_count();
    if given > 3 {
        return Err(arg_count_error(
            req,
            &format!("at most 3 arguments, {given} given"),
        ));
    }
    let how = mode.hold(req)?;
    let second = args.first().map(Value::deref);
    let third = args
        .get(1)
        .map(Value::deref)
        .filter(|v| !matches!(v.as_ref(), ValueRef::Null));
    let (object, _) = stmt::this_stmt(req, this)?;
    let default = stmt::read(req, object, |s| s.default_fetch).unwrap_or(4);
    verify(req, default, how, 1, true)?;
    let kind = Kind::of(how);
    let mut flags = Flags::of(how);
    let with = all_arguments(req, object, kind, flags, given, second, third)?;
    let mut how = how;
    let mut kind = kind;
    if kind == Some(Kind::Default) {
        flags = Flags(flags.0 | Flags::of(default).0);
        kind = Kind::of(default);
        how = (default & 0xF) | flags.0;
    }
    stmt::clear_error(req, object);
    let mut out = PhpArray::new(req)?;
    // What a fetch that throws partway gathered is given up, as the VM
    // frees the return value of a native that threw.
    let mut gathered = gather_all(req, object, kind, how, flags, &with, &mut out);
    if gathered.is_ok() && kind == Some(Kind::KeyPair) {
        gathered = stmt::handle_error(req, object, false);
        if gathered.is_ok() {
            return Ok(out);
        }
    }
    if let Err(thrown) = gathered {
        out.give_up(req);
        return Err(thrown);
    }
    let state = stmt::read(req, object, |s| s.error_code).flatten();
    if state != Some(SqlState::NONE) {
        // `PDO_HANDLE_STMT_ERR_EX(zval_ptr_dtor(return_value);
        // RETVAL_EMPTY_ARRAY();)` (`Z/ext/pdo/pdo_stmt.c:1305`).
        out.give_up(req);
        stmt::handle_error(req, object, false)?;
        return Ok(Owner::take(req, PhpArray::empty()));
    }
    Ok(out)
}

/// `fetchAll()`'s rows gathered into `out` in the shape `kind`, `how`
/// and `flags` name.
fn gather_all(
    req: &mut Request,
    object: PhpObject,
    kind: Option<Kind>,
    how: i64,
    flags: Flags,
    with: &With,
    out: &mut Owner<PhpArray>,
) -> Result<(), Thrown> {
    if kind == Some(Kind::KeyPair) {
        while key_pair(req, object, Orientation::Forward, 0, out)? {}
        return Ok(());
    }
    if flags.has(Flag::Group) || flags.has(Flag::Unique) {
        grouped_rows(req, object, how | flags.0, flags, with, out)?;
    } else {
        while let Some((row, _)) = shaped(req, object, how, Orientation::Forward, 0, false, with)? {
            out.append(req, row)?;
        }
    }
    Ok(())
}

/// `fetchAll()` under `FETCH_GROUP` or `FETCH_UNIQUE`: each row under
/// the key its first column gives, in the order the keys are first
/// seen, a list of them per key or the last one.
fn grouped_rows(
    req: &mut Request,
    object: PhpObject,
    how: i64,
    flags: Flags,
    with: &With,
    out: &mut Owner<PhpArray>,
) -> Result<(), Thrown> {
    // Each group key as it was first seen, what the table keeps under
    // it: the last row for `FETCH_UNIQUE`, every row for `FETCH_GROUP`.
    let mut groups: Vec<(Vec<u8>, Vec<Owner<Value>>)> = Vec::new();
    loop {
        let fetched = shaped(
            req,
            object,
            how | flags.0,
            Orientation::Forward,
            0,
            true,
            with,
        );
        let (row, key) = match fetched {
            Ok(Some(fetched)) => fetched,
            Ok(None) => break,
            // What was grouped before the throw is given up with the
            // answer, as the VM frees it.
            Err(thrown) => {
                give_up_all(req, groups.into_iter().flat_map(|(_, rows)| rows));
                return Err(thrown);
            }
        };
        let key = key.unwrap_or_default();
        match groups.iter_mut().find(|(seen, _)| *seen == key) {
            Some((_, rows)) if flags.has(Flag::Unique) => {
                give_up_all(req, std::mem::replace(rows, vec![row]));
            }
            Some((_, rows)) => rows.push(row),
            None => groups.push((key, vec![row])),
        }
    }
    let mut groups = groups.into_iter();
    while let Some((key, rows)) = groups.next() {
        if let Err(thrown) = group_into(req, out, flags, &key, rows) {
            give_up_all(req, groups.flat_map(|(_, rows)| rows));
            return Err(thrown);
        }
    }
    Ok(())
}

/// One group of [`grouped_rows`] written into `out` under `key`: its
/// row for `FETCH_UNIQUE`, the list of them for `FETCH_GROUP`; every row
/// given up where a write throws.
fn group_into(
    req: &mut Request,
    out: &mut Owner<PhpArray>,
    flags: Flags,
    key: &[u8],
    rows: Vec<Owner<Value>>,
) -> Result<(), Thrown> {
    let key = match symtable_key(req, key) {
        Ok(key) => key,
        Err(thrown) => {
            give_up_all(req, rows);
            return Err(thrown);
        }
    };
    if flags.has(Flag::Unique) {
        let mut written = Ok(());
        for row in rows {
            if written.is_err() {
                row.give_up(req);
                continue;
            }
            let at = Owner::take(req, (*key).clone());
            written = out.set(req, at, row);
        }
        key.give_up(req);
        return written;
    }
    let mut group = match PhpArray::new(req) {
        Ok(group) => group,
        Err(thrown) => {
            key.give_up(req);
            give_up_all(req, rows);
            return Err(thrown);
        }
    };
    let mut appended = Ok(());
    for row in rows {
        if appended.is_err() {
            row.give_up(req);
            continue;
        }
        appended = group.append(req, row);
    }
    if let Err(thrown) = appended {
        group.give_up(req);
        key.give_up(req);
        return Err(thrown);
    }
    out.set(req, key, group.into_value())
}

/// Each owner given up.
fn give_up_all(req: &mut Request, owners: impl IntoIterator<Item = Owner<Value>>) {
    for owner in owners {
        owner.give_up(req);
    }
}

/// `PDOStatement::set_fetch_mode`'s body, its arguments held by the method.
pub(super) fn set_fetch_mode(
    req: &mut Request,
    this: PhpObject,
    mode: i64,
    args: &[Value],
) -> Result<True, Thrown> {
    let args: Vec<Value> = args.iter().map(Value::deref).collect();
    let (object, _) = stmt::this_stmt(req, this)?;
    if stmt::read(req, object, |s| s.in_fetch).unwrap_or(false) {
        return Err(throw_error(
            req,
            "Error",
            "Cannot change default fetch mode while fetching",
        ));
    }
    setup_mode(req, object, mode, 1, &args)?;
    Ok(True)
}
