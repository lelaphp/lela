//! A unit as bytes, the form the file cache keeps on disk (design §6.3):
//! this engine's own, since what php writes (`zend_file_cache.c`) is its
//! memory image and nothing a program observes but the file's presence.
//! Every field of the unit in declaration order, integers little-endian
//! at a fixed width, a sequence as its length and then its items, an
//! absent value as a zero byte and a present one as a one and the value.
//! A string in an op array's constant table is written as its bytes and interned into the
//! engine's table again as it is read, a constant array as its
//! entries, laid down again by the table ([`Intern::constant_array`]),
//! and the names its op arrays carry interned again once it is whole
//! ([`php_compile::intern_names`]), so what is read back is published
//! as a compiled unit is. Which build
//! wrote the bytes is the file's header's to say; this is only the body.

use php_heap::{Intern, Key, Zval, ZvalType};
use php_unit::decl::{
    AliasModifier, Attribute, AttributeArgument, AttributeValidator, ClassConstDecl, ClassDecl,
    ClassFlags, ClassKind, ConstDecl, ConstExpr, ConstantName, DynamicProperties, EnumBacking,
    FunctionDecl, HookKind, ImmutableArray, MethodDecl, Name, NameKind, Parameter,
    ParameterDefault, PropertyDecl, PropertyHooks, Radix, TraitAdaptation, TraitMethodRef, Type,
    Visibility,
};
use php_unit::{
    AutoGlobal, BoundVar, ConstIdx, Diagnostic, Handler, HookBody, Level, LiveKind, LiveRange,
    MapPtrOffset, MethodBody, OpArray, Opcode, OperandKind, Opline, OplineIdx, Safepoints,
    TopLevel, TryCatch, Unit, UnitCell,
};

/// The unit's bytes; none where a constant of an op array is of a kind
/// no unit keeps, which is no unit the compiler makes.
pub(crate) fn encode(unit: &Unit) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    unit.put(&mut out)?;
    Some(out)
}

/// The unit `bytes` encode, its strings, constant arrays and names laid
/// down in `strings`; none where the bytes are not one whole unit.
pub(crate) fn decode(bytes: &[u8], strings: &mut dyn Intern) -> Option<Unit> {
    let mut reader = Reader {
        bytes,
        strings,
        cell: UnitCell::default(),
    };
    let unit = Unit::take(&mut reader)?;
    if !reader.bytes.is_empty() {
        return None;
    }
    php_compile::intern_names(&unit, reader.strings).ok()?;
    Some(unit)
}

/// What is being read: the bytes left, the table the strings go into,
/// and the one cell every closure of the unit shares.
struct Reader<'a> {
    bytes: &'a [u8],
    strings: &'a mut dyn Intern,
    cell: UnitCell,
}

impl Reader<'_> {
    fn take_bytes(&mut self, n: usize) -> Option<&[u8]> {
        let (taken, rest) = self.bytes.split_at_checked(n)?;
        self.bytes = rest;
        Some(taken)
    }

    fn fixed<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take_bytes(N)?.try_into().ok()
    }

    /// A length, held to the bytes left, which no honest one exceeds.
    fn len(&mut self) -> Option<usize> {
        let n = usize::try_from(u32::take(self)?).ok()?;
        (n <= self.bytes.len()).then_some(n)
    }
}

/// One thing as bytes and back.
trait Wire: Sized {
    fn put(&self, out: &mut Vec<u8>) -> Option<()>;
    fn take(r: &mut Reader<'_>) -> Option<Self>;
}

fn put_len(n: usize, out: &mut Vec<u8>) -> Option<()> {
    u32::try_from(n).ok()?.put(out)
}

macro_rules! integer {
    ($($ty:ty),*) => {$(
        impl Wire for $ty {
            fn put(&self, out: &mut Vec<u8>) -> Option<()> {
                out.extend_from_slice(&self.to_le_bytes());
                Some(())
            }
            fn take(r: &mut Reader<'_>) -> Option<Self> {
                r.fixed().map(<$ty>::from_le_bytes)
            }
        }
    )*};
}
integer!(u8, u32, i32, u64, i64);

impl Wire for bool {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        u8::from(*self).put(out)
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        match u8::take(r)? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
}

impl Wire for f64 {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        self.to_bits().put(out)
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        u64::take(r).map(f64::from_bits)
    }
}

impl<T: Wire> Wire for Option<T> {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        match self {
            None => false.put(out),
            Some(value) => {
                true.put(out)?;
                value.put(out)
            }
        }
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        if bool::take(r)? {
            T::take(r).map(Some)
        } else {
            Some(None)
        }
    }
}

/// An extension's attribute validator, which no unit's class carries: a
/// unit's classes are a program's, and only a native class declares one
/// ([`ClassDecl::attribute_validator`]). Present, it is no unit the
/// compiler makes, so neither direction has bytes for it.
impl Wire for AttributeValidator {
    fn put(&self, _: &mut Vec<u8>) -> Option<()> {
        None
    }
    fn take(_: &mut Reader<'_>) -> Option<Self> {
        None
    }
}

impl<T: Wire> Wire for Box<T> {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        (**self).put(out)
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        T::take(r).map(Box::new)
    }
}

impl<T: Wire> Wire for Vec<T> {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        put_len(self.len(), out)?;
        self.iter().try_for_each(|item| item.put(out))
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        let n = r.len()?;
        (0..n).map(|_| T::take(r)).collect()
    }
}

/// Written as the vector it reads as, so a file the cache wrote before
/// the table stood inline reads back the same.
impl Wire for Safepoints {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        put_len(self.len(), out)?;
        self.iter().try_for_each(|item| item.put(out))
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        Vec::<OplineIdx>::take(r).map(Safepoints::from)
    }
}

impl<T: Wire> Wire for Box<[T]> {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        put_len(self.len(), out)?;
        self.iter().try_for_each(|item| item.put(out))
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        Vec::<T>::take(r).map(Vec::into_boxed_slice)
    }
}

impl<A: Wire, B: Wire> Wire for (A, B) {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        self.0.put(out)?;
        self.1.put(out)
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        Some((A::take(r)?, B::take(r)?))
    }
}

impl Wire for Name {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        put_len(self.0.len(), out)?;
        out.extend_from_slice(&self.0);
        Some(())
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        let n = r.len()?;
        r.take_bytes(n).map(|bytes| Name(bytes.into()))
    }
}

impl Wire for Box<str> {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        Name(self.as_bytes().into()).put(out)
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        let name = Name::take(r)?;
        String::from_utf8(name.0.into_vec())
            .ok()
            .map(String::into_boxed_str)
    }
}

/// The unit's cell is no bytes: whatever reads a unit gives every
/// closure of it a share of the reader's one cell, and the unit the cell
/// itself, which [`Unit::published`] binds.
impl Wire for UnitCell {
    fn put(&self, _: &mut Vec<u8>) -> Option<()> {
        Some(())
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        Some(r.cell.share())
    }
}

/// One constant of an op array: its kind, then its value. A string's bytes are interned
/// as they are read and a constant array's entries laid down again.
impl Wire for Zval {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        let kind = self.kind();
        (kind as u8).put(out)?;
        match kind {
            ZvalType::Undef | ZvalType::Null | ZvalType::False | ZvalType::True => Some(()),
            ZvalType::Long => self.as_long()?.put(out),
            ZvalType::Double => self.as_double()?.put(out),
            ZvalType::String => Name(self.as_str()?.bytes().into()).put(out),
            ZvalType::Array => {
                let array = self.as_arr()?;
                put_len(array.len(), out)?;
                array.iter().try_for_each(|(key, value)| {
                    match key {
                        Key::Long(n) => {
                            false.put(out)?;
                            n.put(out)?;
                        }
                        Key::Str(s) => {
                            true.put(out)?;
                            Name(s.bytes().into()).put(out)?;
                        }
                    }
                    value.put(out)
                })
            }
            ZvalType::ConstExpr => self.as_const_expr()?.put(out),
            ZvalType::Ptr => self.as_ptr()?.put(out),
            _ => None,
        }
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        let kind = u8::take(r)?;
        Some(match kind {
            k if k == ZvalType::Undef as u8 => Zval::UNDEF,
            k if k == ZvalType::Null as u8 => Zval::null(),
            k if k == ZvalType::False as u8 => Zval::bool(false),
            k if k == ZvalType::True as u8 => Zval::bool(true),
            k if k == ZvalType::Long as u8 => Zval::long(i64::take(r)?),
            k if k == ZvalType::Double as u8 => Zval::double(f64::take(r)?),
            k if k == ZvalType::String as u8 => {
                let bytes = Name::take(r)?;
                Zval::string(r.strings.intern(&bytes.0).ok()?)
            }
            k if k == ZvalType::Array as u8 => {
                let n = r.len()?;
                let mut entries = Vec::with_capacity(n);
                for _ in 0..n {
                    let key = if bool::take(r)? {
                        let bytes = Name::take(r)?;
                        Key::Str(r.strings.intern(&bytes.0).ok()?)
                    } else {
                        Key::Long(i64::take(r)?)
                    };
                    entries.push((key, Zval::take(r)?));
                }
                Zval::array(r.strings.constant_array(&entries).ok()?)
            }
            k if k == ZvalType::ConstExpr as u8 => Zval::const_expr(u32::take(r)?),
            k if k == ZvalType::Ptr as u8 => Zval::ptr(u32::take(r)?),
            _ => return None,
        })
    }
}

/// A struct as its fields in the order written.
macro_rules! record {
    ($($ty:ident { $($field:ident),* $(,)? })*) => {$(
        impl Wire for $ty {
            fn put(&self, out: &mut Vec<u8>) -> Option<()> {
                $(self.$field.put(out)?;)*
                Some(())
            }
            fn take(r: &mut Reader<'_>) -> Option<Self> {
                Some($ty { $($field: Wire::take(r)?,)* })
            }
        }
    )*};
}

/// A fieldless enum as the one byte its place in the list is.
macro_rules! tags {
    ($($ty:ident { $($variant:ident),* $(,)? })*) => {$(
        impl Wire for $ty {
            fn put(&self, out: &mut Vec<u8>) -> Option<()> {
                let all = [$($ty::$variant),*];
                let at = all.iter().position(|v| v == self)?;
                u8::try_from(at).ok()?.put(out)
            }
            fn take(r: &mut Reader<'_>) -> Option<Self> {
                let all = [$($ty::$variant),*];
                all.get(usize::from(u8::take(r)?)).copied()
            }
        }
    )*};
}

/// A newtype over an index.
macro_rules! index {
    ($($ty:ident),*) => {$(
        impl Wire for $ty {
            fn put(&self, out: &mut Vec<u8>) -> Option<()> {
                self.0.put(out)
            }
            fn take(r: &mut Reader<'_>) -> Option<Self> {
                u32::take(r).map($ty)
            }
        }
    )*};
}

index!(OplineIdx, ConstIdx);

tags! {
    OperandKind { Unused, Const, Tmp, Var, Cv }
    LiveKind { TmpVar, Loop, Silence, Rope, New }
    Level {
        Error, Warning, Parse, Notice, CoreError, CoreWarning, CompileError, CompileWarning,
        UserError, UserWarning, UserNotice, Strict, RecoverableError, Deprecated, UserDeprecated,
    }
    AutoGlobal { Get, Post, Cookie, Server, Env, Request, Files, Session }
    Visibility { Public, Protected, Private }
    ClassKind { Class, Interface, Trait, Enum }
    DynamicProperties { Deprecated, Allowed, Forbidden }
    EnumBacking { Pure, Int, String }
    NameKind { NotFullyQualified, FullyQualified, Relative }
    Radix { Decimal, Octal, OctalExplicit }
    HookKind { Get, Set }
}

/// An opline's handler, of which the file keeps nothing: the unit read
/// back is published as one compiled is, which resolves it from the
/// opline's other fields ([`Unit::published`]).
impl Wire for Handler {
    fn put(&self, _: &mut Vec<u8>) -> Option<()> {
        Some(())
    }
    fn take(_: &mut Reader<'_>) -> Option<Self> {
        Some(Handler::Unresolved)
    }
}

impl Wire for Opcode {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        self.num().put(out)
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        Opcode::from_u8(u8::take(r)?)
    }
}

record! {
    Opline { opcode, op1_type, op2_type, out_type, op1, op2, out, extended_value, lineno, handler }
    TryCatch { try_op, catch_op, finally_op, finally_end }
    LiveRange { var, kind, start, end }
    BoundVar { name, value, lexical }
    OpArray {
        oplines, literals, const_exprs, vars, num_cvs, num_tmps, try_catch, live_ranges,
        safepoints, filename, line_start, line_end, cache_size, cache_room, runtime_cache,
        dynamic_func_defs, bound_vars, no_receiver, strict_types, return_mask, numeric_dims,
    }
    AttributeArgument { name, value }
    Attribute { name, arguments, line, strict }
    Parameter { name, ty, by_ref, prefer_ref, promoted, variadic, default, attributes }
    FunctionDecl {
        name, parameters, return_type, returns_by_ref, is_generator, attributes, doc_comment,
        file, line, end_line, frameless,
    }
    MethodDecl { function, visibility, is_instance, is_abstract, is_final, tentative_return }
    PropertyHooks { get, set, is_abstract }
    PropertyDecl {
        name, visibility, set_visibility, is_instance, is_readonly, is_final, is_promoted,
        is_virtual, ty, default, attributes, doc_comment, hooks,
    }
    ClassConstDecl { name, visibility, is_final, is_case, ty, value, attributes, doc_comment }
    TraitMethodRef { trait_name, method }
    ClassFlags {
        is_abstract, is_final, is_readonly, is_anonymous, is_top_level, not_serializable,
        dynamic_properties, use_guards, hooked_variance,
    }
    ClassDecl {
        name, kind, flags, parent, interfaces, implementors, aliases, traits, adaptations,
        backing, constants, properties, methods, attributes, doc_comment, file, line, end_line,
        object_size, object_beside, attribute_validator,
    }
    ConstDecl { name, value, attributes }
    MethodBody { class, method, body }
    HookBody { class, property, kind, body }
    Diagnostic { level, line, message }
    Unit {
        main, functions, classes, runtime_keys, top_level, constants, methods, hooks, completed,
        dynamic, diagnostics, arms, halt_offset, loop_vars, delayed_fetches, cell,
    }
}

/// The types that name nothing, each the one byte its place here is.
fn simple_types() -> [Type; 17] {
    [
        Type::Mixed,
        Type::Void,
        Type::Never,
        Type::Null,
        Type::Bool,
        Type::False,
        Type::True,
        Type::Int,
        Type::Float,
        Type::String,
        Type::Array,
        Type::Object,
        Type::Iterable,
        Type::Callable,
        Type::CalledClass,
        Type::SelfClass,
        Type::ParentClass,
    ]
}

impl Wire for Type {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        let simple = simple_types();
        if let Some(at) = simple.iter().position(|t| t == self) {
            return u8::try_from(at).ok()?.put(out);
        }
        match self {
            Type::Named(name) => (100u8, name).put(out),
            Type::Nullable(inner) => (101u8, inner).put(out),
            Type::Union(members) => (102u8, members).put(out),
            Type::Intersection(members) => (103u8, members).put(out),
            _ => None,
        }
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        let simple = simple_types();
        let tag = u8::take(r)?;
        if let Some(simple) = simple.into_iter().nth(usize::from(tag)) {
            return Some(simple);
        }
        Some(match tag {
            100 => Type::Named(Wire::take(r)?),
            101 => Type::Nullable(Wire::take(r)?),
            102 => Type::Union(Wire::take(r)?),
            103 => Type::Intersection(Wire::take(r)?),
            _ => return None,
        })
    }
}

impl Wire for TopLevel {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        match self {
            TopLevel::Function(at) => (false, at).put(out),
            TopLevel::Class(at) => (true, at).put(out),
        }
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        Some(if bool::take(r)? {
            TopLevel::Class(u32::take(r)?)
        } else {
            TopLevel::Function(u32::take(r)?)
        })
    }
}

/// A borrowed value put as its owned form is, so a variant's fields can
/// be written as one tuple.
impl<T: Wire> Wire for &T {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        (**self).put(out)
    }
    fn take(_: &mut Reader<'_>) -> Option<Self> {
        None
    }
}

impl Wire for ConstantName {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        match self {
            ConstantName::Resolved(name) => (false, name).put(out),
            ConstantName::UnqualifiedInNamespace(name) => (true, name).put(out),
        }
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        Some(if bool::take(r)? {
            ConstantName::UnqualifiedInNamespace(Wire::take(r)?)
        } else {
            ConstantName::Resolved(Wire::take(r)?)
        })
    }
}

impl Wire for ParameterDefault {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        match self {
            ParameterDefault::None => 0u8.put(out),
            ParameterDefault::Unshown => 1u8.put(out),
            ParameterDefault::Expr(expr) => (2u8, expr).put(out),
        }
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        Some(match u8::take(r)? {
            0 => ParameterDefault::None,
            1 => ParameterDefault::Unshown,
            2 => ParameterDefault::Expr(Wire::take(r)?),
            _ => return None,
        })
    }
}

impl Wire for MapPtrOffset {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        match self {
            MapPtrOffset::Engine(at) => (false, at).put(out),
            MapPtrOffset::Request(at) => (true, at).put(out),
        }
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        let request = bool::take(r)?;
        let at = u32::take(r)?;
        Some(if request {
            MapPtrOffset::Request(at)
        } else {
            MapPtrOffset::Engine(at)
        })
    }
}

impl Wire for AliasModifier {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        match self {
            AliasModifier::Visibility(visibility) => (false, visibility).put(out),
            AliasModifier::Final => true.put(out),
        }
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        Some(if bool::take(r)? {
            AliasModifier::Final
        } else {
            AliasModifier::Visibility(Wire::take(r)?)
        })
    }
}

impl Wire for TraitAdaptation {
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        match self {
            TraitAdaptation::Insteadof {
                trait_name,
                method,
                excluded,
            } => {
                false.put(out)?;
                (trait_name, method).put(out)?;
                excluded.put(out)
            }
            TraitAdaptation::As {
                method,
                modifier,
                alias,
            } => {
                true.put(out)?;
                (method, modifier).put(out)?;
                alias.put(out)
            }
        }
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        Some(if bool::take(r)? {
            TraitAdaptation::As {
                method: Wire::take(r)?,
                modifier: Wire::take(r)?,
                alias: Wire::take(r)?,
            }
        } else {
            TraitAdaptation::Insteadof {
                trait_name: Wire::take(r)?,
                method: Wire::take(r)?,
                excluded: Wire::take(r)?,
            }
        })
    }
}

impl Wire for ConstExpr {
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per variant of the constant expression, each its fields in order"
    )]
    fn put(&self, out: &mut Vec<u8>) -> Option<()> {
        match self {
            ConstExpr::Null => 0u8.put(out),
            ConstExpr::Bool(b) => (1u8, b).put(out),
            ConstExpr::Long { value, radix } => (2u8, (value, radix)).put(out),
            ConstExpr::Double(d) => (3u8, d).put(out),
            ConstExpr::Str(bytes) => (4u8, bytes).put(out),
            ConstExpr::Constant { name, line } => (5u8, (name, line)).put(out),
            ConstExpr::ClassConstant {
                class,
                class_kind,
                name,
                line,
            } => (6u8, ((class, class_kind), (name, line))).put(out),
            ConstExpr::DynamicClassConstant {
                class,
                class_kind,
                name,
                line,
            } => (7u8, ((class, class_kind), (name, line))).put(out),
            ConstExpr::EnumCase { class, case, line } => (8u8, (class, (case, line))).put(out),
            ConstExpr::ClassName { class, line } => (9u8, (class, line)).put(out),
            ConstExpr::Unary { op, operand, line } => (10u8, (op, (operand, line))).put(out),
            ConstExpr::Binary { op, lhs, rhs, line } => (11u8, ((op, lhs), (rhs, line))).put(out),
            ConstExpr::Array { elements, line } => (12u8, (elements, line)).put(out),
            ConstExpr::Unpack(inner) => (13u8, inner).put(out),
            ConstExpr::Dim {
                container,
                dim,
                line,
            } => (14u8, (container, (dim, line))).put(out),
            ConstExpr::Prop {
                object,
                name,
                nullsafe,
                line,
            } => (15u8, ((object, name), (nullsafe, line))).put(out),
            ConstExpr::Conditional {
                cond,
                then,
                otherwise,
            } => (16u8, (cond, (then, otherwise))).put(out),
            ConstExpr::Cast { to, operand, line } => (17u8, (to, (operand, line))).put(out),
            ConstExpr::New {
                class,
                arguments,
                line,
            } => (18u8, (class, (arguments, line))).put(out),
            ConstExpr::Closure { at, name, unit } => (19u8, (at, (name, unit))).put(out),
            ConstExpr::FunctionCallable {
                name,
                name_kind,
                line,
                at,
                unit,
            } => (20u8, ((name, name_kind), (line, (at, unit)))).put(out),
            ConstExpr::MethodCallable {
                class,
                method,
                line,
            } => (21u8, (class, (method, line))).put(out),
            ConstExpr::ScopeClass => 22u8.put(out),
            ConstExpr::ImmutableArray(array) => (23u8, array.value()).put(out),
        }
    }
    fn take(r: &mut Reader<'_>) -> Option<Self> {
        Some(match u8::take(r)? {
            0 => ConstExpr::Null,
            1 => ConstExpr::Bool(Wire::take(r)?),
            2 => ConstExpr::Long {
                value: Wire::take(r)?,
                radix: Wire::take(r)?,
            },
            3 => ConstExpr::Double(Wire::take(r)?),
            4 => ConstExpr::Str(Wire::take(r)?),
            5 => ConstExpr::Constant {
                name: Wire::take(r)?,
                line: Wire::take(r)?,
            },
            6 => ConstExpr::ClassConstant {
                class: Wire::take(r)?,
                class_kind: Wire::take(r)?,
                name: Wire::take(r)?,
                line: Wire::take(r)?,
            },
            7 => ConstExpr::DynamicClassConstant {
                class: Wire::take(r)?,
                class_kind: Wire::take(r)?,
                name: Wire::take(r)?,
                line: Wire::take(r)?,
            },
            8 => ConstExpr::EnumCase {
                class: Wire::take(r)?,
                case: Wire::take(r)?,
                line: Wire::take(r)?,
            },
            9 => ConstExpr::ClassName {
                class: Wire::take(r)?,
                line: Wire::take(r)?,
            },
            10 => ConstExpr::Unary {
                op: Wire::take(r)?,
                operand: Wire::take(r)?,
                line: Wire::take(r)?,
            },
            11 => ConstExpr::Binary {
                op: Wire::take(r)?,
                lhs: Wire::take(r)?,
                rhs: Wire::take(r)?,
                line: Wire::take(r)?,
            },
            12 => ConstExpr::Array {
                elements: Wire::take(r)?,
                line: Wire::take(r)?,
            },
            13 => ConstExpr::Unpack(Wire::take(r)?),
            14 => ConstExpr::Dim {
                container: Wire::take(r)?,
                dim: Wire::take(r)?,
                line: Wire::take(r)?,
            },
            15 => ConstExpr::Prop {
                object: Wire::take(r)?,
                name: Wire::take(r)?,
                nullsafe: Wire::take(r)?,
                line: Wire::take(r)?,
            },
            16 => ConstExpr::Conditional {
                cond: Wire::take(r)?,
                then: Wire::take(r)?,
                otherwise: Wire::take(r)?,
            },
            17 => ConstExpr::Cast {
                to: Wire::take(r)?,
                operand: Wire::take(r)?,
                line: Wire::take(r)?,
            },
            18 => ConstExpr::New {
                class: Wire::take(r)?,
                arguments: Wire::take(r)?,
                line: Wire::take(r)?,
            },
            19 => ConstExpr::Closure {
                at: Wire::take(r)?,
                name: Wire::take(r)?,
                unit: Wire::take(r)?,
            },
            20 => ConstExpr::FunctionCallable {
                name: Wire::take(r)?,
                name_kind: Wire::take(r)?,
                line: Wire::take(r)?,
                at: Wire::take(r)?,
                unit: Wire::take(r)?,
            },
            21 => ConstExpr::MethodCallable {
                class: Wire::take(r)?,
                method: Wire::take(r)?,
                line: Wire::take(r)?,
            },
            22 => ConstExpr::ScopeClass,
            23 => ConstExpr::ImmutableArray(ImmutableArray::new(Wire::take(r)?)?),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit read back from its bytes is the unit written: its bytes
    /// again are the same bytes, and it dumps as the compiled one does.
    #[test]
    fn a_unit_read_back_is_the_unit_written() {
        let source = br#"<?php
namespace N;
const C = [1, 'a' => [2.5, null, true]];
function f(int $a = C, ?string ...$r): ?string {
    static $s = 3;
    return match ($a) { 1 => 'x', 2, 3 => "y$a", default => null };
}
#[Attr(1, name: [2])]
abstract class A implements \Countable {
    use T { t as protected u; }
    const K = self::class . '!';
    public int $p = 1 { get => $this->p * 2; }
    abstract function g(): static;
    function count(): int { return 0; }
}
trait T { function t($o = new \stdClass) { return fn() => static::class; } }
enum E: string { case X = 'x'; const Y = self::X; }
class B extends A { function g(): static { return $this; } }
$x = new class { public $q = [1, 2]; };
try { echo "{$x->q[0]}", __LINE__, E::X->value; } finally { echo 1 <=> 2; }
$d = []; foreach ([1, 2] as $k => $v) { if ($k < 1) { $d[$k] = $v; } echo $k, $v; }
echo (string) 0.1, PHP_EOL;
__halt_compiler(); data"#;
        let mut strings = php_heap::Strings::new();
        let options = php_compile::Options {
            filename: b"/srv/n.php",
            ..php_compile::Options::default()
        };
        let unit = php_compile::compile_source(
            &mut php_compile::syntax::Arena::new(),
            source,
            &options,
            &mut strings,
        )
        .expect("the source compiles");
        let bytes = encode(&unit).expect("the unit encodes");
        let decoded = decode(&bytes, &mut strings).expect("the bytes decode");
        assert_eq!(encode(&decoded), Some(bytes.clone()));
        assert_eq!(decoded.dump(&[]), unit.dump(&[]));
        assert_eq!(decoded.classes, unit.classes);
        assert!(
            decode(&bytes[..bytes.len() - 1], &mut strings).is_none(),
            "a short unit is none"
        );
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(
            decode(&longer, &mut strings).is_none(),
            "nor is one with bytes after it"
        );
        // The file keeps no handler: the unit read back resolves, where
        // it is published, the handlers the unit compiled resolves.
        let handlers = |unit: Unit| {
            let unit = unit.published(None);
            unit.main
                .oplines
                .iter()
                .map(|op| op.handler)
                .collect::<Vec<_>>()
        };
        let compiled = handlers(unit);
        assert!(compiled.contains(&Handler::IsSmallerCvConstJmpz));
        assert!(compiled.contains(&Handler::AssignDimCvCvOpDataCv));
        assert_eq!(handlers(decoded), compiled);
    }
}
