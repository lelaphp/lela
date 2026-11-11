//! The C type model, `zend_ffi_type`: one arena of types per request,
//! a [`TypeId`] in place of php's pointer, so php's identity tests
//! (`type1 == type2`) are number tests, and a type the parser completes
//! later (`struct s;` then `struct s { … };`) is completed in place where
//! every pointer to it already points. Types are never freed before the
//! request ends, which is what php's `zend_ffi_remember_type` arranges
//! for a type a `FFI\CData` still names.

use crate::ffi::Layout;
use php_ext::abi::EngineState;

/// A type of the arena, php's `zend_ffi_type *`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, EngineState)]
pub(crate) struct TypeId(u32);

impl TypeId {
    pub(crate) const VOID: TypeId = TypeId(0);
    pub(crate) const CHAR: TypeId = TypeId(1);
    pub(crate) const BOOL: TypeId = TypeId(2);
    pub(crate) const SINT8: TypeId = TypeId(3);
    pub(crate) const UINT8: TypeId = TypeId(4);
    pub(crate) const SINT16: TypeId = TypeId(5);
    pub(crate) const UINT16: TypeId = TypeId(6);
    pub(crate) const SINT32: TypeId = TypeId(7);
    pub(crate) const UINT32: TypeId = TypeId(8);
    pub(crate) const SINT64: TypeId = TypeId(9);
    pub(crate) const UINT64: TypeId = TypeId(10);
    pub(crate) const FLOAT: TypeId = TypeId(11);
    pub(crate) const DOUBLE: TypeId = TypeId(12);
    pub(crate) const LONG_DOUBLE: TypeId = TypeId(13);
    pub(crate) const PTR: TypeId = TypeId(14);
}

/// `ZEND_FFI_ATTR_*`, the bits of a type's `attr`.
pub(crate) mod attr {
    pub(crate) const CONST: u32 = 1 << 0;
    pub(crate) const INCOMPLETE_TAG: u32 = 1 << 1;
    pub(crate) const VARIADIC: u32 = 1 << 2;
    pub(crate) const INCOMPLETE_ARRAY: u32 = 1 << 3;
    pub(crate) const VLA: u32 = 1 << 4;
    pub(crate) const UNION: u32 = 1 << 5;
    pub(crate) const PACKED: u32 = 1 << 6;
    pub(crate) const MS_STRUCT: u32 = 1 << 7;
    pub(crate) const GCC_STRUCT: u32 = 1 << 8;
    pub(crate) const PERSISTENT: u32 = 1 << 9;
    pub(crate) const STORED: u32 = 1 << 10;
    pub(crate) const STRUCT_ATTRS: u32 = UNION | PACKED | MS_STRUCT | GCC_STRUCT;
    pub(crate) const ENUM_ATTRS: u32 = PACKED;
    pub(crate) const ARRAY_ATTRS: u32 = CONST | VLA | INCOMPLETE_ARRAY;
    pub(crate) const FUNC_ATTRS: u32 = VARIADIC;
    pub(crate) const POINTER_ATTRS: u32 = CONST;
}

/// An integer kind, what an enumeration is stored as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EngineState)]
pub(crate) enum Int {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
}

impl Int {
    /// The predefined type of this kind.
    pub(crate) fn id(self) -> TypeId {
        match self {
            Int::U8 => TypeId::UINT8,
            Int::I8 => TypeId::SINT8,
            Int::U16 => TypeId::UINT16,
            Int::I16 => TypeId::SINT16,
            Int::U32 => TypeId::UINT32,
            Int::I32 => TypeId::SINT32,
            Int::U64 => TypeId::UINT64,
            Int::I64 => TypeId::SINT64,
        }
    }
}

/// How a scalar is read and written in memory: the kind after an
/// enumeration is resolved to the integer it is stored as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scalar {
    Float,
    Double,
    LongDouble,
    Int(Int),
    Bool,
    Char,
}

/// `zend_ffi_type.kind` and the union beside it.
#[derive(Debug, Clone, EngineState)]
pub(crate) enum Kind {
    Void,
    Float,
    Double,
    LongDouble,
    Int(Int),
    Enum { tag: Option<Vec<u8>>, of: Int },
    Bool,
    Char,
    Pointer(TypeId),
    Func(Func),
    Array { of: TypeId, len: i64 },
    Struct(Record),
}

/// A function type: its return, its parameters (none for `(void)` and
/// for `()`), and its calling convention as libffi numbers it.
#[derive(Debug, Clone, EngineState)]
pub(crate) struct Func {
    pub(crate) ret: TypeId,
    pub(crate) args: Option<Vec<TypeId>>,
    pub(crate) abi: i32,
}

/// A struct or a union: its tag and its fields in declaration order,
/// an anonymous bit field keyed by no name.
#[derive(Debug, Clone, Default, EngineState)]
pub(crate) struct Record {
    pub(crate) tag: Option<Vec<u8>>,
    pub(crate) fields: Vec<(Option<Vec<u8>>, Field)>,
}

impl Record {
    /// The field named `name`.
    pub(crate) fn field(&self, name: &[u8]) -> Option<&Field> {
        self.fields
            .iter()
            .find(|(key, _)| key.as_deref() == Some(name))
            .map(|(_, field)| field)
    }
}

/// `zend_ffi_field`.
#[derive(Debug, Clone, Copy, EngineState)]
pub(crate) struct Field {
    pub(crate) offset: usize,
    pub(crate) is_const: bool,
    pub(crate) is_nested: bool,
    pub(crate) first_bit: u8,
    pub(crate) bits: u8,
    pub(crate) ty: TypeId,
}

/// `zend_ffi_type`.
#[derive(Debug, Clone, EngineState)]
pub(crate) struct Type {
    pub(crate) kind: Kind,
    pub(crate) size: usize,
    pub(crate) align: u32,
    pub(crate) attr: u32,
}

impl Type {
    /// `ZEND_FFI_TYPE_*`, what `FFI\CType::getKind()` answers.
    pub(crate) fn code(&self) -> i64 {
        match &self.kind {
            Kind::Void => 0,
            Kind::Float => 1,
            Kind::Double => 2,
            Kind::LongDouble => 3,
            Kind::Int(int) => int_code(*int),
            Kind::Enum { .. } => 12,
            Kind::Bool => 13,
            Kind::Char => 14,
            Kind::Pointer(_) => 15,
            Kind::Func(_) => 16,
            Kind::Array { .. } => 17,
            Kind::Struct(_) => 18,
        }
    }

    /// `type->kind < ZEND_FFI_TYPE_POINTER`: a value php reads into a
    /// PHP scalar.
    pub(crate) fn is_scalar(&self) -> bool {
        self.code() < 15
    }

    /// How the value is read and written, an enumeration's as its
    /// integer; none for anything but a scalar.
    pub(crate) fn scalar(&self) -> Option<Scalar> {
        Some(match &self.kind {
            Kind::Float => Scalar::Float,
            Kind::Double => Scalar::Double,
            Kind::LongDouble => Scalar::LongDouble,
            Kind::Int(int) | Kind::Enum { of: int, .. } => Scalar::Int(*int),
            Kind::Bool => Scalar::Bool,
            Kind::Char => Scalar::Char,
            _ => return None,
        })
    }

    pub(crate) fn pointee(&self) -> Option<TypeId> {
        match self.kind {
            Kind::Pointer(to) => Some(to),
            _ => None,
        }
    }

    pub(crate) fn element(&self) -> Option<(TypeId, i64)> {
        match self.kind {
            Kind::Array { of, len } => Some((of, len)),
            _ => None,
        }
    }

    pub(crate) fn func(&self) -> Option<&Func> {
        match &self.kind {
            Kind::Func(func) => Some(func),
            _ => None,
        }
    }

    pub(crate) fn record(&self) -> Option<&Record> {
        match &self.kind {
            Kind::Struct(record) => Some(record),
            _ => None,
        }
    }
}

/// `ZEND_FFI_TYPE_UINT8` and on for an integer kind.
pub(crate) fn int_code(int: Int) -> i64 {
    match int {
        Int::U8 => 4,
        Int::I8 => 5,
        Int::U16 => 6,
        Int::I16 => 7,
        Int::U32 => 8,
        Int::I32 => 9,
        Int::U64 => 10,
        Int::I64 => 11,
    }
}

/// `sizeof(long double)` and its alignment: a double's on arm64, the
/// x87 format's sixteen bytes on x86-64.
#[cfg(target_arch = "aarch64")]
const LONG_DOUBLE: (usize, u32) = (8, 8);
#[cfg(not(target_arch = "aarch64"))]
const LONG_DOUBLE: (usize, u32) = (16, 16);

/// The arena: the predefined types at their fixed numbers, then every type
/// a declaration made.
#[derive(Debug, Clone, EngineState)]
pub(crate) struct Types {
    all: Vec<Type>,
    /// The types a declaration made and php's `zend_ffi_type_dtor`
    /// freed before the declarations were done, which
    /// [`Types::emallocs`] does not count.
    freed: std::collections::HashSet<TypeId>,
    /// Each type and a type it holds that it owns, php's
    /// `ZEND_FFI_TYPE_IS_OWNED` bit on the nested pointer, which
    /// [`Types::free`] frees with it.
    owns: std::collections::HashSet<(TypeId, TypeId)>,
}

impl Default for Types {
    fn default() -> Types {
        let plain = |kind, size, align| Type {
            kind,
            size,
            align,
            attr: 0,
        };
        Types {
            all: vec![
                plain(Kind::Void, 1, 1),
                plain(Kind::Char, 1, 1),
                plain(Kind::Bool, 1, 1),
                plain(Kind::Int(Int::I8), 1, 1),
                plain(Kind::Int(Int::U8), 1, 1),
                plain(Kind::Int(Int::I16), 2, 2),
                plain(Kind::Int(Int::U16), 2, 2),
                plain(Kind::Int(Int::I32), 4, 4),
                plain(Kind::Int(Int::U32), 4, 4),
                plain(Kind::Int(Int::I64), 8, 8),
                plain(Kind::Int(Int::U64), 8, 8),
                plain(Kind::Float, 4, 4),
                plain(Kind::Double, 8, 8),
                plain(Kind::LongDouble, LONG_DOUBLE.0, LONG_DOUBLE.1),
                plain(Kind::Pointer(TypeId::VOID), 8, 8),
            ],
            freed: std::collections::HashSet::new(),
            owns: std::collections::HashSet::new(),
        }
    }
}

/// `zend_ffi_types`: the names `FFI_G(types)` resolves without a
/// declaration.
pub(crate) fn predefined(name: &[u8]) -> Option<TypeId> {
    Some(match name {
        b"void" => TypeId::VOID,
        b"char" => TypeId::CHAR,
        b"bool" => TypeId::BOOL,
        b"int8_t" => TypeId::SINT8,
        b"uint8_t" => TypeId::UINT8,
        b"int16_t" => TypeId::SINT16,
        b"uint16_t" => TypeId::UINT16,
        b"int32_t" => TypeId::SINT32,
        b"uint32_t" => TypeId::UINT32,
        b"int64_t" | b"intptr_t" | b"ssize_t" | b"ptrdiff_t" | b"off_t" => TypeId::SINT64,
        b"uint64_t" | b"uintptr_t" | b"size_t" => TypeId::UINT64,
        b"float" => TypeId::FLOAT,
        b"double" => TypeId::DOUBLE,
        b"long double" => TypeId::LONG_DOUBLE,
        b"va_list" | b"__builtin_va_list" | b"__gnuc_va_list" => TypeId::PTR,
        _ => return None,
    })
}

/// How many predefined types the arena opens with, each at the same number
/// in every arena.
const BUILTINS: u32 = 15;

impl TypeId {
    /// The number this type has in an arena `other`'s types were laid
    /// into from `base` on; a predefined one keeps its own.
    fn moved(self, base: u32) -> TypeId {
        if self.0 < BUILTINS {
            self
        } else {
            TypeId(self.0 - BUILTINS + base)
        }
    }
}

impl Types {
    /// `other`'s types laid into this arena after its own, every number
    /// within them moved with them: what a scope `ffi.preload` made at
    /// startup is in a request's arena. The answer moves a number of
    /// `other`'s to its number here.
    pub(crate) fn import(&mut self, other: &Types) -> impl Fn(TypeId) -> TypeId + use<> {
        let base = u32::try_from(self.all.len()).unwrap_or(u32::MAX);
        let moved = move |id: TypeId| id.moved(base);
        for ty in other.all.iter().skip(BUILTINS as usize) {
            let mut ty = ty.clone();
            match &mut ty.kind {
                Kind::Pointer(to) => *to = moved(*to),
                Kind::Array { of, .. } => *of = moved(*of),
                Kind::Func(func) => {
                    func.ret = moved(func.ret);
                    if let Some(args) = &mut func.args {
                        for arg in args {
                            *arg = moved(*arg);
                        }
                    }
                }
                Kind::Struct(record) => {
                    for (_, field) in &mut record.fields {
                        field.ty = moved(field.ty);
                    }
                }
                _ => {}
            }
            self.all.push(ty);
        }
        moved
    }

    pub(crate) fn get(&self, id: TypeId) -> &Type {
        // Every `TypeId` is one this arena handed out, and the arena only
        // grows, so the index is always in it.
        match self.all.get(id.0 as usize) {
            Some(ty) => ty,
            None => &self.all[0],
        }
    }

    pub(crate) fn get_mut(&mut self, id: TypeId) -> Option<&mut Type> {
        self.all.get_mut(id.0 as usize)
    }

    /// How many types the arena holds, the number the next is made at.
    pub(crate) fn len(&self) -> usize {
        self.all.len()
    }

    /// What php `emalloc`s for each type made from the `from`th on:
    /// `sizeof(zend_ffi_type)`, 88; for a function with parameters its
    /// `args` table, `sizeof(HashTable)` and the packed buckets of
    /// `zend_hash_init(args, 4, ...)`; for a struct with fields the
    /// buckets of `fields`, eight or the next power of two, then each
    /// `zend_ffi_field` and its name's `zend_string`.
    pub(crate) fn emallocs(&self, from: usize) -> Vec<usize> {
        const TYPE: usize = 88;
        let mut sizes = Vec::new();
        for (at, ty) in self.all.iter().enumerate().skip(from) {
            if self
                .freed
                .contains(&TypeId(u32::try_from(at).unwrap_or(u32::MAX)))
            {
                continue;
            }
            sizes.push(TYPE);
            match &ty.kind {
                Kind::Func(f) if f.args.as_ref().is_some_and(|args| !args.is_empty()) => {
                    sizes.push(56);
                    sizes.push(8 * 16 + 2 * 4);
                }
                Kind::Struct(record) if !record.fields.is_empty() => {
                    let slots = record.fields.len().next_power_of_two().max(8);
                    sizes.push(slots * 32 + slots * 2 * 4);
                    for (name, _) in &record.fields {
                        sizes.push(24);
                        if let Some(name) = name {
                            sizes.push(24 + name.len() + 1);
                        }
                    }
                }
                _ => {}
            }
        }
        sizes
    }

    /// `zend_ffi_type_dtor` over a type a declaration owned and did not
    /// keep, and over the types it owns in turn, a pointer's or an
    /// array's target, a function's return and parameters, a struct's
    /// fields: still in the arena, which never shrinks, and no longer
    /// counted as php's heap.
    pub(crate) fn free(&mut self, id: TypeId) {
        let mut next = vec![id];
        while let Some(id) = next.pop() {
            if !self.freed.insert(id) {
                continue;
            }
            let held: Vec<TypeId> = match &self.get(id).kind {
                Kind::Pointer(to) => vec![*to],
                Kind::Array { of, .. } => vec![*of],
                Kind::Func(f) => std::iter::once(f.ret)
                    .chain(f.args.iter().flatten().copied())
                    .collect(),
                Kind::Struct(record) => record.fields.iter().map(|(_, f)| f.ty).collect(),
                _ => Vec::new(),
            };
            next.extend(held.into_iter().filter(|ty| self.owns.contains(&(id, *ty))));
        }
    }

    /// `ZEND_FFI_TYPE_MAKE_OWNED` on the pointer `owner` holds to `ty`,
    /// where `by_dcl` says the declaration it came from owned `ty`.
    pub(crate) fn own(&mut self, owner: TypeId, ty: TypeId, by_dcl: bool) {
        if by_dcl {
            self.owns.insert((owner, ty));
        }
    }

    /// The ownership of `ty` moved from `from` to `to`, as
    /// `zend_ffi_add_anonymous_field` moves a nested struct's fields.
    pub(crate) fn move_owned(&mut self, from: TypeId, to: TypeId, ty: TypeId) {
        if self.owns.remove(&(from, ty)) {
            self.owns.insert((to, ty));
        }
    }

    /// A new type, `emalloc(sizeof(zend_ffi_type))` filled in.
    pub(crate) fn add(&mut self, ty: Type) -> TypeId {
        let id = TypeId(u32::try_from(self.all.len()).unwrap_or(u32::MAX));
        self.all.push(ty);
        id
    }

    /// `void *`'s kind: a new pointer type to `to`.
    pub(crate) fn pointer_to(&mut self, to: TypeId, attr: u32) -> TypeId {
        self.add(Type {
            kind: Kind::Pointer(to),
            size: 8,
            align: 8,
            attr,
        })
    }

    /// `zend_ffi_is_compatible_type` (`Z/ext/ffi/ffi.c:310`).
    pub(crate) fn is_compatible(&self, mut dst: TypeId, mut src: TypeId) -> bool {
        loop {
            if dst == src {
                return true;
            }
            let (d, s) = (self.get(dst), self.get(src));
            if d.code() == s.code() {
                if d.code() < 15 {
                    return true;
                }
                match (&d.kind, &s.kind) {
                    (Kind::Pointer(d), Kind::Pointer(s)) => {
                        dst = *d;
                        src = *s;
                        let (d, s) = (self.get(dst), self.get(src));
                        if matches!(d.kind, Kind::Void) || matches!(s.kind, Kind::Void) {
                            return true;
                        }
                        if let (Kind::Func(d), Kind::Func(s)) = (&d.kind, &s.kind) {
                            return self.func_ptr_compatible(dst, d, src, s);
                        }
                    }
                    (Kind::Array { of: d, len: dl }, Kind::Array { of: s, len: sl })
                        if dl == sl || *dl == 0 =>
                    {
                        dst = *d;
                        src = *s;
                    }
                    _ => return false,
                }
            } else if let (Kind::Pointer(d), Kind::Array { of: s, .. }) = (&d.kind, &s.kind) {
                dst = *d;
                src = *s;
                if matches!(self.get(dst).kind, Kind::Void) {
                    return true;
                }
            } else {
                return false;
            }
        }
    }

    /// `zend_ffi_func_ptr_are_compatible` (`Z/ext/ffi/ffi.c:267`).
    fn func_ptr_compatible(&self, dst_id: TypeId, dst: &Func, src_id: TypeId, src: &Func) -> bool {
        if dst.abi != src.abi {
            return false;
        }
        let variadic = |id| self.get(id).attr & attr::VARIADIC;
        if variadic(dst_id) != variadic(src_id) {
            return false;
        }
        let none = Vec::new();
        let (da, sa) = (
            dst.args.as_ref().unwrap_or(&none),
            src.args.as_ref().unwrap_or(&none),
        );
        if da.len() != sa.len() {
            return false;
        }
        if !self.is_compatible(dst.ret, src.ret) {
            return false;
        }
        da.iter().zip(sa).all(|(d, s)| self.is_compatible(*d, *s))
    }

    /// `zend_ffi_is_same_type` (`Z/ext/ffi/ffi.c:2231`).
    pub(crate) fn is_same(&self, mut one: TypeId, mut two: TypeId) -> bool {
        loop {
            if one == two {
                return true;
            }
            let (first, second) = (self.get(one), self.get(two));
            if first.code() != second.code() {
                return false;
            }
            if first.code() < 15 {
                return true;
            }
            match (&first.kind, &second.kind) {
                (Kind::Pointer(to_one), Kind::Pointer(to_two)) => {
                    one = *to_one;
                    two = *to_two;
                    if matches!(self.get(one).kind, Kind::Void)
                        || matches!(self.get(two).kind, Kind::Void)
                    {
                        return true;
                    }
                }
                (
                    Kind::Array {
                        of: of_one,
                        len: len_one,
                    },
                    Kind::Array {
                        of: of_two,
                        len: len_two,
                    },
                ) if len_one == len_two => {
                    one = *of_one;
                    two = *of_two;
                }
                _ => return false,
            }
        }
    }

    /// `zend_ffi_same_types` (`Z/ext/ffi/ffi.c:3156`): one type
    /// structurally, field for field.
    pub(crate) fn same_types(&self, old: TypeId, ty: TypeId) -> bool {
        if old == ty {
            return true;
        }
        let (before, now) = (self.get(old), self.get(ty));
        if before.code() != now.code()
            || before.size != now.size
            || before.align != now.align
            || before.attr != now.attr
        {
            return false;
        }
        match (&before.kind, &now.kind) {
            (Kind::Enum { of: p, .. }, Kind::Enum { of: q, .. }) => p == q,
            (Kind::Array { of: p, len: was }, Kind::Array { of: q, len: is }) => {
                was == is && self.same_types(*p, *q)
            }
            (Kind::Pointer(p), Kind::Pointer(q)) => self.same_types(*p, *q),
            (Kind::Struct(p), Kind::Struct(q)) => {
                p.fields.len() == q.fields.len()
                    && p.fields.iter().zip(&q.fields).all(|((pk, pf), (qk, qf))| {
                        pk == qk
                            && pf.offset == qf.offset
                            && pf.is_const == qf.is_const
                            && pf.is_nested == qf.is_nested
                            && pf.first_bit == qf.first_bit
                            && pf.bits == qf.bits
                            && self.same_types(pf.ty, qf.ty)
                    })
            }
            (Kind::Func(p), Kind::Func(q)) => {
                let none = Vec::new();
                let (pa, qa) = (
                    p.args.as_ref().unwrap_or(&none),
                    q.args.as_ref().unwrap_or(&none),
                );
                p.abi == q.abi
                    && pa.len() == qa.len()
                    && self.same_types(p.ret, q.ret)
                    && pa.iter().zip(qa).all(|(a, b)| self.same_types(*a, *b))
            }
            _ => true,
        }
    }

    /// `zend_ffi_ctype_name` (`Z/ext/ffi/ffi.c:1523`) into php's 256-byte
    /// buffer, written from three quarters in: none where a name runs out
    /// of either end of it.
    pub(crate) fn name(&self, id: TypeId) -> Option<Vec<u8>> {
        let mut buf = NameBuf::new();
        let mut ty = self.get(id);
        let mut is_ptr = false;
        let name: &[u8] = loop {
            match &ty.kind {
                Kind::Void => break b"void",
                Kind::Float => break b"float",
                Kind::Double => break b"double",
                Kind::LongDouble => break b"long double",
                Kind::Int(Int::U8) => break b"uint8_t",
                Kind::Int(Int::I8) => break b"int8_t",
                Kind::Int(Int::U16) => break b"uint16_t",
                Kind::Int(Int::I16) => break b"int16_t",
                Kind::Int(Int::U32) => break b"uint32_t",
                Kind::Int(Int::I32) => break b"int32_t",
                Kind::Int(Int::U64) => break b"uint64_t",
                Kind::Int(Int::I64) => break b"int64_t",
                Kind::Enum { tag, .. } => {
                    buf.prepend(tag.as_deref().unwrap_or(b"<anonymous>"));
                    break b"enum ";
                }
                Kind::Bool => break b"bool",
                Kind::Char => break b"char",
                Kind::Pointer(to) => {
                    if !buf.prepend(b"*") {
                        return None;
                    }
                    is_ptr = true;
                    ty = self.get(*to);
                }
                Kind::Func(func) => {
                    if is_ptr {
                        is_ptr = false;
                        if !buf.prepend(b"(") || !buf.append(b")") {
                            return None;
                        }
                    }
                    if !buf.append(b"(") || !buf.append(b")") {
                        return None;
                    }
                    ty = self.get(func.ret);
                }
                Kind::Array { of, len } => {
                    if is_ptr {
                        is_ptr = false;
                        if !buf.prepend(b"(") || !buf.append(b")") {
                            return None;
                        }
                    }
                    if !buf.append(b"[") {
                        return None;
                    }
                    if ty.attr & attr::VLA != 0 {
                        if !buf.append(b"*") {
                            return None;
                        }
                    } else if ty.attr & attr::INCOMPLETE_ARRAY == 0
                        && !buf.append(len.to_string().as_bytes())
                    {
                        return None;
                    }
                    if !buf.append(b"]") {
                        return None;
                    }
                    ty = self.get(*of);
                }
                Kind::Struct(record) => {
                    buf.prepend(record.tag.as_deref().unwrap_or(b"<anonymous>"));
                    break if ty.attr & attr::UNION != 0 {
                        b"union "
                    } else {
                        b"struct "
                    };
                }
            }
        };
        if !buf.prepend(name) {
            return None;
        }
        let mut out = buf.front;
        out.reverse();
        out.extend_from_slice(&buf.back);
        Some(out)
    }

    /// `zend_ffi_get_type` (`Z/ext/ffi/ffi.c:435`): how libffi passes a
    /// value of the type; none for a function or an array.
    pub(crate) fn layout(&self, id: TypeId) -> Option<Layout> {
        let ty = self.get(id);
        Some(match &ty.kind {
            Kind::Float => Layout::Float,
            Kind::Double | Kind::LongDouble => Layout::Double,
            Kind::Int(int) | Kind::Enum { of: int, .. } => int_layout(*int),
            Kind::Pointer(_) => Layout::Pointer,
            Kind::Void => Layout::Void,
            Kind::Bool => Layout::U8,
            Kind::Char => Layout::I8,
            Kind::Struct(_) => {
                let mut elements = Vec::new();
                self.struct_elements(id, &mut elements);
                Layout::Struct {
                    size: ty.size,
                    align: u16::try_from(ty.align).unwrap_or(u16::MAX),
                    elements,
                }
            }
            Kind::Func(_) | Kind::Array { .. } => return None,
        })
    }

    /// `zend_ffi_face_struct_add_fields` (`Z/ext/ffi/ffi.c:351`): a
    /// struct's fields flattened into libffi's element list, the small
    /// integers as unsigned, a nested struct's fields in its place, and a
    /// union's first field alone.
    fn struct_elements(&self, id: TypeId, out: &mut Vec<Layout>) {
        let ty = self.get(id);
        let Kind::Struct(record) = &ty.kind else {
            return;
        };
        for (_, field) in &record.fields {
            let field_ty = self.get(field.ty);
            match &field_ty.kind {
                Kind::Float => out.push(Layout::Float),
                Kind::Double | Kind::LongDouble => out.push(Layout::Double),
                Kind::Int(Int::I8 | Int::U8) | Kind::Bool | Kind::Char => out.push(Layout::U8),
                Kind::Int(Int::I16 | Int::U16) => out.push(Layout::U16),
                Kind::Int(Int::I32 | Int::U32) => out.push(Layout::U32),
                Kind::Int(Int::I64 | Int::U64) => out.push(Layout::U64),
                Kind::Pointer(_) => out.push(Layout::Pointer),
                Kind::Struct(_) => self.struct_elements(field.ty, out),
                _ => out.push(Layout::Void),
            }
            if ty.attr & attr::UNION != 0 {
                break;
            }
        }
    }
}

/// The libffi type of an integer kind.
fn int_layout(int: Int) -> Layout {
    match int {
        Int::U8 => Layout::U8,
        Int::I8 => Layout::I8,
        Int::U16 => Layout::U16,
        Int::I16 => Layout::I16,
        Int::U32 => Layout::U32,
        Int::I32 => Layout::I32,
        Int::U64 => Layout::U64,
        Int::I64 => Layout::I64,
    }
}

/// `zend_ffi_ctype_name_buf`: 192 bytes to prepend into, 64 to append
/// into, as `buf.start = buf.end = buf.buf + 192` leaves php's 256. A
/// prepend that does not fit still moves the start, as php's does, so
/// nothing prepends after it.
struct NameBuf {
    front: Vec<u8>,
    room: isize,
    back: Vec<u8>,
}

impl NameBuf {
    fn new() -> NameBuf {
        NameBuf {
            front: Vec::new(),
            room: 192,
            back: Vec::new(),
        }
    }

    /// `zend_ffi_ctype_name_prepend`: whether it fit.
    fn prepend(&mut self, text: &[u8]) -> bool {
        self.room -= isize::try_from(text.len()).unwrap_or(isize::MAX);
        if self.room < 0 {
            return false;
        }
        self.front.extend(text.iter().rev());
        true
    }

    /// `zend_ffi_ctype_name_append`: whether it fit.
    fn append(&mut self, text: &[u8]) -> bool {
        if self.back.len() + text.len() > 64 {
            return false;
        }
        self.back.extend_from_slice(text);
        true
    }
}
