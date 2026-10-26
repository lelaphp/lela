//! `FFI\CData`: a C value PHP code holds, `zend_ffi_cdata`. What it
//! owns is in its type: memory it allocated and frees ([`Place::Owned`]),
//! memory someone else owns ([`Place::Borrowed`]), a pointer value it
//! holds itself in a cell of its own, php's `ptr_holder`
//! ([`Place::Holder`]), or nothing after `FFI::free` ([`Place::Freed`]).
//! The handlers are php's three tables in one: a scalar's
//! (`zend_ffi_cdata_value_handlers`, read and written as `->cdata`), any
//! other type's (`zend_ffi_cdata_handlers`, fields and elements), and a
//! freed one's (`zend_ffi_cdata_free_handlers`), chosen by the type and
//! the place as php chooses the table when it makes the object.

use std::cmp::Ordering;

use php_ext::abi::{
    ClassIterator, CloneObj, DebugInfo, DimensionRead, GcTable, ObjectHandlers, PropertyPlace,
    PropertyRead, PropertyTest,
};
use php_ext::attr::php_class;
use php_ext::call::{Function, new_object};
use php_ext::coerce::{BinaryOp, to_double, to_long, to_string, truthy, value_name, with_string};
use php_ext::raising::{Thrown, throw_error};
use php_ext::request::Request;
use php_ext::values::walk;
use php_ext::values::{
    ClosureInfo, Emalloc, Owner, PhpArray, PhpObject, PhpString, TypeTag, Value, ValueRef,
};

use std::num::NonZeroUsize;

use crate::ffi::{self, Block};
use crate::types::{Field, Int, Kind, Scalar, TypeId, attr};
use crate::{State, classes, ffi_error, types};

/// `pemalloc(size, persistent)`, zeroed: the request's heap, where
/// `memory_get_usage()` counts it as php's `emalloc` is counted, kept by
/// its address until [`free`] gives it back; the process's for a
/// persistent value, as php's `malloc`. The `Err` is the memory limit's
/// fatal.
pub(crate) fn alloc(req: &mut Request, size: usize, persistent: bool) -> Result<usize, Thrown> {
    if persistent {
        return match Block::zeroed(size) {
            Some(block) => Ok(block.release()),
            None => Err(ffi_error(req, "Out of memory")),
        };
    }
    let mut block = Emalloc::new(req, size.max(1))?;
    let addr = block.as_mut_ptr() as usize;
    req.state::<State>().heap.insert(addr, block);
    Ok(addr)
}

/// `pefree(ptr, persistent)`: memory [`alloc`] made back where it came
/// from, and any other the process's `free`, as `FFI::free` hands C
/// memory back.
pub(crate) fn free(req: &mut Request, addr: usize) {
    match req.state::<State>().heap.remove(&addr) {
        Some(block) => block.free(req),
        None => ffi::free(addr),
    }
}

/// What php `emalloc`s for each type the C declarations made from the
/// `from`th on, held by the value made of them until it is freed, as
/// php frees an owned `zend_ffi_type` with its `FFI\CData` or its
/// `FFI\CType`: [`crate::types::Types::emallocs`].
pub(crate) fn hold_types(req: &mut Request, from: usize) -> Result<Box<[usize]>, Thrown> {
    let sizes = types(req).emallocs(from);
    hold(req, &sizes)
}

/// One block of each of `sizes` on the request's heap, held until
/// [`free_types`] gives them back.
pub(crate) fn hold(req: &mut Request, sizes: &[usize]) -> Result<Box<[usize]>, Thrown> {
    let mut held = Vec::with_capacity(sizes.len());
    for size in sizes {
        held.push(alloc(req, *size, false)?);
    }
    Ok(held.into_boxed_slice())
}

/// `FFI_G(weak_types)`: the table `zend_ffi_remember_type` allocates
/// at its first use, by the addresses [`alloc`] gave it and its packed
/// buckets, how many rows it has room for, and the blocks of each type
/// it remembers. What it keeps stays on the request's heap until
/// `RSHUTDOWN`, which gives it back ([`forget_types`]).
#[derive(Debug)]
pub(crate) struct WeakTypes {
    table: usize,
    data: usize,
    slots: usize,
    types: Vec<Box<[usize]>>,
}

/// `HT_PACKED_SIZE_EX(slots, HT_MIN_MASK)`: a `zval` a slot and the two
/// `uint32_t` hash slots of the smallest mask.
fn packed_size(slots: usize) -> usize {
    slots * 16 + 2 * 4
}

/// `zend_ffi_remember_type`: one row of `FFI_G(weak_types)` for a type
/// whose owner hands it on, its held blocks kept to the request's end,
/// the table (`emalloc(sizeof(HashTable))` and `zend_hash_init(..., 0,
/// ...)`) made at the first, its packed buckets laid at its first insert
/// and doubled, `zend_hash_packed_grow`, when full.
pub(crate) fn remember(req: &mut Request, held: Box<[usize]>) -> Result<(), Thrown> {
    let mut weak = if let Some(weak) = req.state::<State>().weak_types.take() {
        weak
    } else {
        let table = alloc(req, 56, false)?;
        let data = match alloc(req, packed_size(8), false) {
            Ok(data) => data,
            Err(thrown) => {
                free(req, table);
                free_types(req, &held);
                return Err(thrown);
            }
        };
        WeakTypes {
            table,
            data,
            slots: 8,
            types: Vec::new(),
        }
    };
    if weak.types.len() == weak.slots {
        match alloc(req, packed_size(weak.slots * 2), false) {
            Ok(data) => {
                free(req, weak.data);
                weak.data = data;
                weak.slots *= 2;
            }
            Err(thrown) => {
                req.state::<State>().weak_types = Some(weak);
                free_types(req, &held);
                return Err(thrown);
            }
        }
    }
    weak.types.push(held);
    req.state::<State>().weak_types = Some(weak);
    Ok(())
}

/// `RSHUTDOWN`'s `zend_hash_destroy(FFI_G(weak_types))` and `efree`:
/// every remembered type given back, then the packed buckets and the
/// table.
pub(crate) fn forget_types(req: &mut Request) {
    let Some(weak) = req.state::<State>().weak_types.take() else {
        return;
    };
    for held in &weak.types {
        free_types(req, held);
    }
    free(req, weak.data);
    free(req, weak.table);
}

/// The types a value held given back with it.
pub(crate) fn free_types(req: &mut Request, held: &[usize]) {
    for addr in held {
        free(req, *addr);
    }
}

/// Where a `FFI\CData`'s value stands, and what of it the object owns.
#[derive(Debug, Default)]
pub(crate) enum Place {
    /// `cdata->ptr == NULL`: an object no constructor of php's filled.
    #[default]
    Null,
    /// `ZEND_FFI_FLAG_OWNED`: memory the object allocated and frees
    /// ([`free`]).
    Owned(usize),
    /// Memory someone else owns.
    Borrowed(usize),
    /// `cdata->ptr == &cdata->ptr_holder`: a pointer value the object
    /// holds in a cell of its own, and the pointee where it owns that.
    Holder {
        cell: Block,
        target: Option<NonZeroUsize>,
    },
    /// After `FFI::free`: php's free handlers, `Use after free()`.
    Freed,
}

impl Place {
    /// A cell holding the pointer value `value`.
    pub(crate) fn holder(value: usize) -> Place {
        match Block::zeroed(8) {
            Some(cell) => {
                ffi::write(cell.addr(), value);
                Place::Holder { cell, target: None }
            }
            None => Place::Null,
        }
    }

    /// `cdata->ptr`.
    pub(crate) fn ptr(&self) -> usize {
        match self {
            Place::Owned(addr) | Place::Borrowed(addr) => *addr,
            Place::Holder { cell, .. } => cell.addr(),
            Place::Null | Place::Freed => 0,
        }
    }

    /// `cdata->flags & ZEND_FFI_FLAG_OWNED`.
    pub(crate) fn owned(&self) -> bool {
        matches!(
            self,
            Place::Owned(_)
                | Place::Holder {
                    target: Some(_),
                    ..
                }
        )
    }
}

/// `zend_ffi_cdata` past its object header, and the types it owns
/// ([`hold_types`]).
#[derive(Debug, Default)]
pub(crate) struct Data {
    pub(crate) ty: TypeId,
    pub(crate) place: Place,
    pub(crate) is_const: bool,
    pub(crate) persistent: bool,
    pub(crate) held: Box<[usize]>,
}

impl Data {
    /// A value at `ptr` someone else owns.
    pub(crate) fn borrowed(ty: TypeId, ptr: usize, is_const: bool) -> Data {
        Data {
            ty,
            place: Place::Borrowed(ptr),
            is_const,
            persistent: false,
            held: Box::default(),
        }
    }

    /// A pointer value held in a cell of the object's own.
    pub(crate) fn holding(ty: TypeId, value: usize, is_const: bool) -> Data {
        Data {
            ty,
            place: Place::holder(value),
            is_const,
            persistent: false,
            held: Box::default(),
        }
    }
}

/// `final class FFI\CData`, `@not-serializable`: no constructor, no
/// methods, `sizeof(zend_ffi_cdata)` for each.
///
/// ```php
/// $c = (new ReflectionClass('FFI\CData'));
/// var_dump($c->isFinal(), $c->isIterable());
/// try { new FFI\CData; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// Instantiation of FFI\CData is not allowed
/// ```
// `size = 104`: php's `zend_ffi_cdata_new` asks `emalloc` for
// `sizeof(zend_ffi_cdata)`, 88, whole, not `zend_object_alloc`.
#[php_class(namespace(FFI), final, not_serializable, payload = Data, size = 104, handlers = crate::cdata::HANDLERS)]
pub(crate) struct CData;

#[php_class]
impl CData {}

/// The handlers of `FFI\CData`.
pub(crate) static HANDLERS: Handlers = Handlers;

/// The handlers of `FFI\CData`.
pub(crate) struct Handlers;

/// What an object's payload says, copied out of it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct View {
    pub(crate) ty: TypeId,
    pub(crate) ptr: usize,
    pub(crate) is_const: bool,
    pub(crate) life: Life,
    pub(crate) holder: bool,
    pub(crate) owned: bool,
}

/// Whether a `FFI\CData` holds a value: one php's constructors filled,
/// one `FFI::free` freed, or one no constructor of php's filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Life {
    Live,
    Freed,
    Uninit,
}

impl View {
    pub(crate) fn freed(&self) -> bool {
        self.life == Life::Freed
    }

    pub(crate) fn uninit(&self) -> bool {
        self.life == Life::Uninit
    }
}

/// The view of a `FFI\CData`'s payload.
pub(crate) fn view(req: &Request, object: PhpObject) -> View {
    match object.native::<Data>(req) {
        Some(data) => View {
            ty: data.ty,
            ptr: data.place.ptr(),
            is_const: data.is_const,
            life: match data.place {
                Place::Freed => Life::Freed,
                Place::Null => Life::Uninit,
                _ => Life::Live,
            },
            holder: matches!(data.place, Place::Holder { .. }),
            owned: data.place.owned(),
        },
        None => View {
            ty: TypeId::VOID,
            ptr: 0,
            is_const: false,
            life: Life::Uninit,
            holder: false,
            owned: false,
        },
    }
}

/// The `FFI\CData` a value is, a reference followed:
/// `Z_TYPE_P(v) == IS_OBJECT && Z_OBJCE_P(v) == zend_ffi_cdata_ce`.
pub(crate) fn as_cdata(req: &mut Request, value: &Value) -> Result<Option<PhpObject>, Thrown> {
    let class = classes(req)?.cdata;
    Ok(value
        .deref()
        .as_object()
        .filter(|object| object.class() == class))
}

/// A new `FFI\CData` holding `data`.
pub(crate) fn make(req: &mut Request, data: Data) -> Result<Owner<PhpObject>, Thrown> {
    let class = classes(req)?.cdata;
    let object = new_object(req, class)?;
    if let Some(slot) = object.native_mut::<Data>(req) {
        *slot = data;
    }
    Ok(object)
}

/// A scalar read from C memory, as `zend_ffi_cdata_to_zval` reads one.
pub(crate) fn read_scalar(
    req: &mut Request,
    scalar: Scalar,
    addr: usize,
) -> Result<Owner<Value>, Thrown> {
    let value = match scalar {
        Scalar::Float => Value::float(f64::from(ffi::read::<f32>(addr))),
        Scalar::Double | Scalar::LongDouble => Value::float(ffi::read::<f64>(addr)),
        Scalar::Int(Int::U8) => Value::int(i64::from(ffi::read::<u8>(addr))),
        Scalar::Int(Int::I8) => Value::int(i64::from(ffi::read::<i8>(addr))),
        Scalar::Int(Int::U16) => Value::int(i64::from(ffi::read::<u16>(addr))),
        Scalar::Int(Int::I16) => Value::int(i64::from(ffi::read::<i16>(addr))),
        Scalar::Int(Int::U32) => Value::int(i64::from(ffi::read::<u32>(addr))),
        Scalar::Int(Int::I32) => Value::int(i64::from(ffi::read::<i32>(addr))),
        Scalar::Int(Int::U64) => Value::int(ffi::read::<u64>(addr) as i64),
        Scalar::Int(Int::I64) => Value::int(ffi::read::<i64>(addr)),
        Scalar::Bool => Value::bool(ffi::read::<u8>(addr) != 0),
        Scalar::Char => return Ok(PhpString::new(req, &[ffi::read::<u8>(addr)])?.into_value()),
    };
    Ok(Owner::take(req, value))
}

/// The facts of a type the conversions read, copied out of the arena.
#[derive(Debug, Clone, Copy)]
struct Facts {
    scalar: Option<Scalar>,
    pointee: Option<TypeId>,
    is_struct: bool,
    size: usize,
    attr: u32,
}

fn facts(req: &mut Request, ty: TypeId) -> Facts {
    let t = types(req).get(ty);
    Facts {
        scalar: t.scalar(),
        pointee: t.pointee(),
        is_struct: t.record().is_some(),
        size: t.size,
        attr: t.attr,
    }
}

/// Whether `ty` is `char`.
fn is_char(req: &mut Request, ty: TypeId) -> bool {
    matches!(types(req).get(ty).kind, Kind::Char)
}

/// Whether `ty` is a pointer to a function.
pub(crate) fn func_pointee(req: &mut Request, ty: TypeId) -> Option<TypeId> {
    let types = types(req);
    let to = types.get(ty).pointee()?;
    types.get(to).func().map(|_| to)
}

/// How `zend_ffi_cdata_to_zval` is asked for the value: `BP_VAR_R`, a
/// PHP scalar where the type is one, or any other fetch, which is always
/// a `FFI\CData` over the place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fetch {
    Read,
    Place,
}

/// `zend_ffi_cdata_to_zval`: the value at `ptr` of type `ty`, a PHP
/// scalar for a scalar read, a string for a `const char *` read, null
/// for a null pointer, and otherwise `existing` with one more owner or a
/// new `FFI\CData` over the place; `is_ret` makes it own a returned
/// struct and hold a returned pointer, as `zend_ffi_cdata_to_zval_slow_ret`
/// does.
#[expect(clippy::too_many_arguments, reason = "zend_ffi_cdata_to_zval's eight")]
pub(crate) fn to_value(
    req: &mut Request,
    existing: Option<PhpObject>,
    ptr: usize,
    ty: TypeId,
    fetch: Fetch,
    is_const: bool,
    is_ret: bool,
    debug_union: bool,
) -> Result<Owner<Value>, Thrown> {
    let f = facts(req, ty);
    if fetch == Fetch::Read {
        if let Some(scalar) = f.scalar {
            return read_scalar(req, scalar, ptr);
        }
        if let Some(pointee) = f.pointee {
            let value = ffi::read::<usize>(ptr);
            if value == 0 {
                return Ok(Owner::null());
            }
            if debug_union {
                let text = format!("{value:#x}");
                return Ok(PhpString::new(req, text.as_bytes())?.into_value());
            }
            if f.attr & attr::CONST != 0 && is_char(req, pointee) {
                let text = ffi::c_string(value);
                return Ok(PhpString::new(req, &text)?.into_value());
            }
            if let Some(object) = existing {
                return Ok(Owner::take(req, object.as_value()));
            }
            let data = if is_ret {
                slow_ret(req, ptr, ty, f, is_const)?
            } else {
                Data::holding(ty, value, is_const)
            };
            return Ok(make(req, data)?.into_value());
        }
    }
    if let Some(object) = existing {
        return Ok(Owner::take(req, object.as_value()));
    }
    let data = if is_ret {
        slow_ret(req, ptr, ty, f, is_const)?
    } else {
        Data::borrowed(ty, ptr, is_const)
    };
    Ok(make(req, data)?.into_value())
}

/// `zend_ffi_cdata_to_zval_slow_ret`: a returned pointer held, a
/// returned struct copied into memory the object owns.
fn slow_ret(
    req: &mut Request,
    ptr: usize,
    ty: TypeId,
    f: Facts,
    is_const: bool,
) -> Result<Data, Thrown> {
    if f.pointee.is_some() {
        return Ok(Data::holding(ty, ffi::read::<usize>(ptr), is_const));
    }
    if f.is_struct {
        let addr = alloc(req, f.size, false)?;
        ffi::copy(addr, ptr, f.size);
        return Ok(Data {
            ty,
            place: Place::Owned(addr),
            is_const,
            persistent: false,
            held: Box::default(),
        });
    }
    Ok(Data::borrowed(ty, ptr, is_const))
}

/// `zend_ffi_bit_field_read`.
fn bit_field_read(ptr: usize, field: &Field) -> u64 {
    let bit = usize::from(field.first_bit);
    let last_bit = bit + usize::from(field.bits) - 1;
    let mut p = ptr + bit / 8;
    let last_p = ptr + last_bit / 8;
    let pos = bit % 8;
    if p == last_p {
        let mask = (1u32 << field.bits) - 1;
        return u64::from((u32::from(ffi::read::<u8>(p)) >> pos) & mask);
    }
    let mut val: u64 = 0;
    let mut insert = 0;
    if pos != 0 {
        let num_bits = 8 - pos;
        let mask = (1u32 << num_bits) - 1;
        val = u64::from((u32::from(ffi::read::<u8>(p)) >> pos) & mask);
        p += 1;
        insert += num_bits;
    }
    while p < last_p {
        val |= u64::from(ffi::read::<u8>(p)) << insert;
        p += 1;
        insert += 8;
    }
    if p == last_p {
        let num_bits = last_bit % 8 + 1;
        let mask = (1u32 << num_bits) - 1;
        val |= u64::from(u32::from(ffi::read::<u8>(p)) & mask) << insert;
    }
    val
}

/// `zend_ffi_bit_field_to_zval`: the field's bits, sign-extended for a
/// signed type.
pub(crate) fn bit_field_value(req: &mut Request, ptr: usize, field: &Field) -> Value {
    let mut val = bit_field_read(ptr, field);
    let signed = matches!(
        types(req).get(field.ty).kind,
        Kind::Char | Kind::Int(Int::I8 | Int::I16 | Int::I32 | Int::I64)
    );
    if signed {
        let shift = 64 - (u32::from(field.bits) % 64);
        if shift != 0 && shift != 64 {
            val = ((val << shift) as i64 >> shift) as u64;
        }
    }
    Value::int(val as i64)
}

/// `zend_ffi_zval_to_bit_field`.
fn write_bit_field(
    req: &mut Request,
    ptr: usize,
    field: &Field,
    value: &Value,
) -> Result<(), Thrown> {
    let mut val = to_long(req, value)? as u64;
    let bit = usize::from(field.first_bit);
    let last_bit = bit + usize::from(field.bits) - 1;
    let mut p = ptr + bit / 8;
    let last_p = ptr + last_bit / 8;
    let pos = bit % 8;
    let byte = |p: usize| u32::from(ffi::read::<u8>(p));
    if p == last_p {
        let mask = ((1u32 << field.bits) - 1) << pos;
        ffi::write(p, ((byte(p) & !mask) | ((val as u32) << pos & mask)) as u8);
        return Ok(());
    }
    if pos != 0 {
        let num_bits = 8 - pos;
        let mask = ((1u32 << num_bits) - 1) << pos;
        ffi::write(p, ((byte(p) & !mask) | ((val as u32) << pos & mask)) as u8);
        p += 1;
        val >>= num_bits;
    }
    while p < last_p {
        ffi::write(p, val as u8);
        p += 1;
        val >>= 8;
    }
    if p == last_p {
        let num_bits = last_bit % 8 + 1;
        let mask = (1u32 << num_bits) - 1;
        ffi::write(p, ((byte(p) & !mask) | (val as u32 & mask)) as u8);
    }
    Ok(())
}

/// The name a type is shown by, `zend_ffi_ctype_name`'s, if it fits.
pub(crate) fn type_name(req: &mut Request, ty: TypeId) -> Option<String> {
    types(req)
        .name(ty)
        .map(|name| String::from_utf8_lossy(&name).into_owned())
}

/// `zend_ffi_assign_incompatible`.
pub(crate) fn assign_incompatible(
    req: &mut Request,
    value: &Value,
    ty: TypeId,
) -> Result<Thrown, Thrown> {
    let Some(name) = type_name(req, ty) else {
        return Ok(ffi_error(req, "Incompatible types when assigning"));
    };
    let message = if let Some(object) = as_cdata(req, value)? {
        let from = view(req, object).ty;
        match type_name(req, from) {
            Some(from) => {
                format!("Incompatible types when assigning to type '{name}' from type '{from}'")
            }
            None => format!("Incompatible types when assigning to type '{name}'"),
        }
        .into_bytes()
    } else {
        let given = value_name(req, &value.deref());
        let head = format!("Incompatible types when assigning to type '{name}' from PHP '");
        [head.as_bytes(), &given, b"'"].concat()
    };
    Ok(ffi_error(req, &message))
}

/// `zend_ffi_zval_to_cdata`: `value` written at `ptr` as a `ty`.
pub(crate) fn write_value(
    req: &mut Request,
    ptr: usize,
    ty: TypeId,
    value: &Value,
) -> Result<(), Thrown> {
    let value = value.deref();
    let f = facts(req, ty);
    let cdata = as_cdata(req, &value)?;
    if f.pointee.is_none()
        && let Some(object) = cdata
    {
        let other = view(req, object);
        let compatible = types(req).is_compatible(ty, other.ty);
        if compatible && f.size == types(req).get(other.ty).size {
            ffi::copy(ptr, other.ptr, f.size);
            return Ok(());
        }
    }
    if let Some(scalar) = f.scalar {
        match scalar {
            Scalar::Float => ffi::write(ptr, to_double(req, &value)? as f32),
            Scalar::Double | Scalar::LongDouble => ffi::write(ptr, to_double(req, &value)?),
            Scalar::Int(int) => {
                let n = to_long(req, &value)?;
                match int {
                    Int::U8 | Int::I8 => ffi::write(ptr, n as u8),
                    Int::U16 | Int::I16 => ffi::write(ptr, n as u16),
                    Int::U32 | Int::I32 => ffi::write(ptr, n as u32),
                    Int::U64 | Int::I64 => ffi::write(ptr, n),
                }
            }
            Scalar::Bool => {
                let truth = truthy(req, &value)?;
                ffi::write(ptr, u8::from(truth));
            }
            Scalar::Char => {
                let byte = with_string(req, &value, |bytes| match bytes {
                    [byte] => Some(*byte),
                    _ => None,
                })?;
                if let Some(byte) = byte {
                    ffi::write(ptr, byte);
                } else {
                    return Err(assign_incompatible(req, &value, ty)?);
                }
            }
        }
        return Ok(());
    }
    if let Some(pointee) = f.pointee {
        if matches!(value.as_ref(), ValueRef::Null) {
            ffi::write(ptr, 0usize);
            return Ok(());
        }
        if let Some(object) = cdata {
            let other = view(req, object);
            let (compatible, other_pointer, to_compatible) = {
                let types = types(req);
                (
                    types.is_compatible(ty, other.ty),
                    types.get(other.ty).pointee().is_some(),
                    types.is_compatible(pointee, other.ty),
                )
            };
            if compatible {
                if other_pointer {
                    ffi::write(ptr, ffi::read::<usize>(other.ptr));
                } else {
                    if other.owned {
                        return Err(ffi_error(
                            req,
                            "Attempt to perform assign of owned C pointer",
                        ));
                    }
                    ffi::write(ptr, other.ptr);
                }
                return Ok(());
            }
            if !other_pointer && to_compatible {
                if other.owned {
                    return Err(ffi_error(
                        req,
                        "Attempt to perform assign pointer to owned C data",
                    ));
                }
                ffi::write(ptr, other.ptr);
                return Ok(());
            }
        } else if let Some(func) = func_pointee(req, ty) {
            let code = crate::call::create_callback(req, func, &value)?;
            ffi::write(ptr, code);
            return Ok(());
        }
    }
    Err(assign_incompatible(req, &value, ty)?)
}

/// `zend_bad_array_access`: `Error`, the class named.
fn bad_array_access(req: &mut Request) -> Thrown {
    let ce = match crate::throwables::Throwables::of(req) {
        Ok(ce) => ce,
        Err(fatal) => return fatal,
    };
    throw_error(
        req,
        ce.error,
        "Cannot use object of type FFI\\CData as array",
    )
}

/// `zend_ffi_use_after_free`.
fn use_after_free(req: &mut Request) -> Thrown {
    ffi_error(req, "Use after free()")
}

/// Which of php's three tables an object's handlers are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    Value,
    Data,
    Free,
}

fn table(req: &mut Request, v: &View) -> Table {
    if v.freed() {
        Table::Free
    } else if !v.uninit() && types(req).get(v.ty).is_scalar() {
        Table::Value
    } else {
        Table::Data
    }
}

/// `zend_ffi_cdata_read_field`'s and `zend_ffi_cdata_write_field`'s
/// lookup: the struct a value or a pointer names, and the field.
fn field_of(
    req: &mut Request,
    v: &View,
    name: &[u8],
    verb: &str,
) -> Result<(Field, usize), Thrown> {
    let (record_ty, through_pointer) = {
        let types = types(req);
        match types.get(v.ty).pointee() {
            Some(to) => (to, true),
            None => (v.ty, false),
        }
    };
    let shown = String::from_utf8_lossy(name).into_owned();
    let field = match types(req).get(record_ty).record() {
        None => {
            return Err(ffi_error(
                req,
                &format!("Attempt to {verb} field '{shown}' of non C struct/union"),
            ));
        }
        Some(record) => record.field(name).copied(),
    };
    let Some(field) = field else {
        let what = if verb == "read" {
            "read undefined"
        } else {
            "assign undefined"
        };
        return Err(ffi_error(
            req,
            &format!("Attempt to {what} field '{shown}' of C struct/union"),
        ));
    };
    let mut ptr = v.ptr;
    if through_pointer {
        if ptr == 0 {
            return Err(ffi_error(req, "NULL pointer dereference"));
        }
        ptr = ffi::read::<usize>(ptr);
        if ptr == 0 {
            return Err(ffi_error(req, "NULL pointer dereference"));
        }
    }
    Ok((field, ptr))
}

/// `zend_ffi_cdata_read_dim`'s and `zend_ffi_cdata_write_dim`'s
/// element: its address, its type and whether it is read-only.
fn element_of(
    req: &mut Request,
    v: &View,
    dim: i64,
    verb: &str,
) -> Result<(usize, TypeId, bool), Thrown> {
    let (array, pointer, type_const) = {
        let types = types(req);
        let t = types.get(v.ty);
        (
            t.element().map(|(of, len)| (of, len, types.get(of).size)),
            t.pointee().map(|to| (to, types.get(to).size)),
            t.attr & attr::CONST != 0,
        )
    };
    let is_const = v.is_const || type_const;
    if let Some((of, len, size)) = array {
        if (dim as u64) >= (len as u64) && (dim < 0 || len != 0) {
            return Err(ffi_error(req, "C array index out of bounds"));
        }
        return Ok((
            v.ptr.wrapping_add(size.wrapping_mul(dim as usize)),
            of,
            is_const,
        ));
    }
    if let Some((to, size)) = pointer {
        if v.ptr == 0 {
            return Err(ffi_error(req, "NULL pointer dereference"));
        }
        let base = ffi::read::<usize>(v.ptr);
        return Ok((
            base.wrapping_add(size.wrapping_mul(dim as usize)),
            to,
            is_const,
        ));
    }
    Err(ffi_error(
        req,
        &format!("Attempt to {verb} element of non C array"),
    ))
}

impl ObjectHandlers for Handlers {
    /// `zend_fake_get_constructor`.
    fn get_constructor(&self, req: &mut Request, _: PhpObject) -> Result<Option<Function>, Thrown> {
        let ce = crate::throwables::Throwables::of(req)?;
        Err(throw_error(
            req,
            ce.error,
            "Instantiation of FFI\\CData is not allowed",
        ))
    }

    /// `zend_ffi_cdata_clone_obj`: the value copied into memory the
    /// clone owns.
    ///
    /// ```php
    /// $a = FFI::cdef()->new("int[2]"); $a[0] = 5;
    /// $b = clone $a; $b[0] = 7;
    /// var_dump($a[0], $b[0]);
    /// ```
    /// ```expect
    /// int(5)
    /// int(7)
    /// ```
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_cdata)
    }

    /// `zend_ffi_cdata_free_obj`: the memory it owns and the types it
    /// owns given back, `pefree` and `zend_ffi_type_dtor`, so
    /// `memory_get_usage()` falls by what `FFI::new` raised it by.
    ///
    /// ```php
    /// $ffi = FFI::cdef();
    /// $a = memory_get_usage(); $x = $ffi->new("char[4096]"); $b = memory_get_usage(); unset($x); $c = memory_get_usage();
    /// $y = $ffi->new("int"); $d = memory_get_usage(); $y = null; $e = memory_get_usage();
    /// $p = $ffi->new("int[2]", false); $f = memory_get_usage(); FFI::free($p); $p = null; $g = memory_get_usage();
    /// $t = $ffi->type("char[16]"); $h = memory_get_usage(); $q = $ffi->new("char[16]", true, true); $i = memory_get_usage();
    /// var_dump($b - $a, $c - $b, $d - $c, $e - $d, $f - $e, $g - $f, $h - $g, $i - $h);
    /// ```
    /// ```expect
    /// int(4288)
    /// int(-4288)
    /// int(112)
    /// int(-112)
    /// int(208)
    /// int(-176)
    /// int(160)
    /// int(192)
    /// ```
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let Some(data) = object.native_mut::<Data>(req) else {
            return;
        };
        let place = std::mem::take(&mut data.place);
        let held = std::mem::take(&mut data.held);
        match place {
            Place::Owned(addr) => free(req, addr),
            Place::Holder {
                target: Some(addr), ..
            } => free(req, addr.get()),
            _ => {}
        }
        free_types(req, &held);
    }

    /// `zend_ffi_cdata_read_field`, `zend_ffi_cdata_get` for a scalar.
    ///
    /// ```php
    /// $ffi = FFI::cdef("struct point { int x; int y; };");
    /// $p = $ffi->new("struct point"); $p->x = 3; $p->y = -4;
    /// $i = $ffi->new("int"); $i->cdata = 42;
    /// var_dump($p->x, $p->y, $i->cdata);
    /// try { $p->z; } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// try { $i->x; } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(3)
    /// int(-4)
    /// int(42)
    /// Attempt to read undefined field 'z' of C struct/union
    /// Only 'cdata' property may be read
    /// ```
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        let v = view(req, object);
        match table(req, &v) {
            Table::Free => Err(use_after_free(req)),
            Table::Value => {
                if name != b"cdata" {
                    return Err(ffi_error(req, "Only 'cdata' property may be read"));
                }
                to_value(
                    req,
                    Some(object),
                    v.ptr,
                    v.ty,
                    Fetch::Read,
                    false,
                    false,
                    false,
                )
            }
            Table::Data => {
                let (field, ptr) = field_of(req, &v, name, "read")?;
                if field.bits == 0 {
                    let fetch = if read == PropertyRead::Warning {
                        Fetch::Read
                    } else {
                        Fetch::Place
                    };
                    to_value(
                        req,
                        None,
                        ptr.wrapping_add(field.offset),
                        field.ty,
                        fetch,
                        v.is_const || field.is_const,
                        false,
                        false,
                    )
                } else {
                    let value = bit_field_value(req, ptr, &field);
                    Ok(Owner::take(req, value))
                }
            }
        }
    }

    /// `zend_ffi_cdata_write_field`, `zend_ffi_cdata_set` for a scalar.
    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let written = write_property(req, object, name.as_bytes(), &value);
        value.give_up(req);
        name.give_up(req);
        written
    }

    /// `zend_fake_get_property_ptr_ptr`: every name through the read
    /// and write handlers.
    fn get_property_ptr_ptr(
        &self,
        _: &mut Request,
        _: PhpObject,
        _: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        Ok(PropertyPlace::Handlers)
    }

    /// `zend_fake_has_property`, whose refusal is the array one.
    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        _: &[u8],
        _: PropertyTest,
    ) -> Result<bool, Thrown> {
        let v = view(req, object);
        Err(if v.freed() {
            use_after_free(req)
        } else {
            bad_array_access(req)
        })
    }

    /// `zend_fake_unset_property`.
    fn unset_property(&self, req: &mut Request, object: PhpObject, _: &[u8]) -> Result<(), Thrown> {
        let v = view(req, object);
        Err(if v.freed() {
            use_after_free(req)
        } else {
            bad_array_access(req)
        })
    }

    /// `zend_ffi_cdata_read_dim`.
    ///
    /// ```php
    /// $a = FFI::cdef()->new("int[3]");
    /// $a[1] = 9;
    /// var_dump($a[1], count($a));
    /// try { $a[3]; } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(9)
    /// int(3)
    /// C array index out of bounds
    /// ```
    fn read_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        read: DimensionRead,
    ) -> Result<Owner<Value>, Thrown> {
        let v = view(req, object);
        match table(req, &v) {
            Table::Free => Err(use_after_free(req)),
            Table::Value => Err(bad_array_access(req)),
            Table::Data => {
                let dim = match offset {
                    Some(offset) => to_long(req, offset)?,
                    None => 0,
                };
                let (ptr, ty, is_const) = element_of(req, &v, dim, "read")?;
                let fetch = if read == DimensionRead::Warning {
                    Fetch::Read
                } else {
                    Fetch::Place
                };
                to_value(req, None, ptr, ty, fetch, is_const, false, false)
            }
        }
    }

    /// `zend_ffi_cdata_write_dim`.
    fn write_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let written = write_dimension(req, object, offset, &value);
        value.give_up(req);
        written
    }

    /// `zend_fake_has_dimension`.
    fn has_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        _: &Value,
        _: bool,
    ) -> Result<bool, Thrown> {
        let v = view(req, object);
        Err(if v.freed() {
            use_after_free(req)
        } else {
            bad_array_access(req)
        })
    }

    /// `zend_fake_unset_dimension`.
    fn unset_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        _: &Value,
    ) -> Result<(), Thrown> {
        let v = view(req, object);
        Err(if v.freed() {
            use_after_free(req)
        } else {
            bad_array_access(req)
        })
    }

    /// `zend_fake_get_properties`: none.
    fn get_properties(&self, _: &mut Request, _: PhpObject) -> Result<walk::Table, Thrown> {
        Ok(walk::Table::Array(PhpArray::empty()))
    }

    fn properties_overloaded(&self) -> bool {
        true
    }

    /// `zend_fake_get_method`.
    fn get_method(
        &self,
        req: &mut Request,
        _: PhpObject,
        _: &[u8],
    ) -> Result<Option<Function>, Thrown> {
        let ce = crate::throwables::Throwables::of(req)?;
        Err(throw_error(
            req,
            ce.error,
            "Object of type FFI\\CData does not support method calls",
        ))
    }

    /// `zend_ffi_cdata_get_class_name`: the class, then the C type.
    ///
    /// ```php
    /// var_dump(FFI::cdef()->new("uint8_t[2]"));
    /// ```
    /// ```expect
    /// object(FFI\CData:uint8_t[2])#2 (2) {
    ///   [0]=>
    ///   int(0)
    ///   [1]=>
    ///   int(0)
    /// }
    /// ```
    fn get_class_name(&self, req: &mut Request, object: PhpObject) -> Vec<u8> {
        let ty = view(req, object).ty;
        match types(req).name(ty) {
            Some(name) => [b"FFI\\CData:".as_slice(), &name].concat(),
            None => b"FFI\\CData".to_vec(),
        }
    }

    /// `zend_ffi_cdata_cast_object`: a scalar as a string, a `const char
    /// *` as the string it points at, and any value as `true`.
    ///
    /// ```php
    /// $ffi = FFI::cdef();
    /// $i = $ffi->new("int"); $i->cdata = 7;
    /// $f = $ffi->new("float"); $f->cdata = 1.5;
    /// var_dump((string) $i, (string) $f, (bool) $i);
    /// ```
    /// ```expect
    /// string(1) "7"
    /// string(3) "1.5"
    /// bool(true)
    /// ```
    fn cast_object(
        &self,
        req: &mut Request,
        object: PhpObject,
        to: TypeTag,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        let v = view(req, object);
        match to {
            TypeTag::Bool => Ok(Some(Owner::bool(true))),
            TypeTag::String if !v.freed() => {
                let f = facts(req, v.ty);
                let value = if let Some(scalar) = f.scalar {
                    let value = read_scalar(req, scalar, v.ptr)?;
                    if scalar == Scalar::Char {
                        return Ok(Some(value));
                    }
                    value
                } else if let Some(pointee) = f.pointee {
                    let value = ffi::read::<usize>(v.ptr);
                    if value == 0 {
                        Owner::null()
                    } else if f.attr & attr::CONST != 0 && is_char(req, pointee) {
                        let text = ffi::c_string(value);
                        let text = PhpString::new(req, &text)?;
                        return Ok(Some(text.into_value()));
                    } else {
                        return Ok(None);
                    }
                } else {
                    return Ok(None);
                };
                let text = to_string(req, &value);
                value.give_up(req);
                Ok(Some(text?.into_value()))
            }
            _ => Ok(None),
        }
    }

    /// `zend_ffi_cdata_count_elements`, on the value table none.
    fn count_elements(&self, req: &mut Request, object: PhpObject) -> Result<Option<i64>, Thrown> {
        let v = view(req, object);
        if table(req, &v) != Table::Data {
            return Ok(None);
        }
        match types(req).get(v.ty).element() {
            Some((_, len)) => Ok(Some(len)),
            None => Err(ffi_error(req, "Attempt to count() on non C array")),
        }
    }

    /// `zend_ffi_cdata_get_debug_info`.
    ///
    /// ```php
    /// $ffi = FFI::cdef("struct s { int a; char b; };");
    /// $s = $ffi->new("struct s"); $s->a = 1; $s->b = 'x';
    /// var_dump($s, $ffi->new("void*"));
    /// ```
    /// ```expect
    /// object(FFI\CData:struct s)#2 (2) {
    ///   ["a"]=>
    ///   int(1)
    ///   ["b"]=>
    ///   string(1) "x"
    /// }
    /// object(FFI\CData:void*)#3 (1) {
    ///   [0]=>
    ///   NULL
    /// }
    /// ```
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<DebugInfo>, Thrown> {
        let v = view(req, object);
        if v.freed() {
            let thrown = use_after_free(req);
            return Ok(Some(DebugInfo {
                table: PhpArray::new(req)?,
                thrown: Some(thrown),
            }));
        }
        if v.ptr == 0 {
            let thrown = ffi_error(req, "NULL pointer dereference");
            return Ok(Some(DebugInfo {
                table: PhpArray::new(req)?,
                thrown: Some(thrown),
            }));
        }
        debug_table(req, &v).map(|table| table.map(DebugInfo::from))
    }

    /// `zend_ffi_cdata_get_closure`: a pointer to a C function called as
    /// one, through the same trampoline `FFI`'s methods are, which
    /// `zend_is_callable` asks too; the trampoline has no scope, so
    /// `Closure::fromCallable` refuses it.
    ///
    /// ```php
    /// $ffi = FFI::cdef("int abs(int j);"); $f = $ffi->abs;
    /// var_dump(is_callable($f, false, $name), $name, call_user_func($f, -3), call_user_func_array($f, [-4]), array_map($f, [-1]));
    /// try { Closure::fromCallable($f); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { call_user_func($ffi->new("int")); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// string(19) "FFI\CData::__invoke"
    /// int(3)
    /// int(4)
    /// array(1) {
    ///   [0]=>
    ///   int(1)
    /// }
    /// Failed to create closure from callable
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, no array or string given
    /// ```
    fn get_closure(&self, req: &mut Request, object: PhpObject) -> Option<ClosureInfo> {
        let v = view(req, object);
        if v.freed() || v.ptr == 0 {
            return None;
        }
        func_pointee(req, v.ty)?;
        let function = req
            .symbols_mut()
            .native_trampoline(b"__invoke", crate::call::invoke)?;
        Some(ClosureInfo {
            function,
            this: Some(object),
            scope: None,
            called_scope: None,
            bound: None,
            fake: false,
        })
    }

    /// `zend_fake_get_gc`: nothing.
    fn get_gc(&self, _: &mut Request, _: PhpObject, _: &mut dyn FnMut(Value)) -> GcTable {
        GcTable::Properties
    }

    /// `zend_ffi_cdata_do_operation`: pointer arithmetic, `+` and `-` of
    /// an integer and `-` of two pointers to one type.
    ///
    /// ```php
    /// $a = FFI::cdef()->new("int[4]");
    /// $p = $a + 1; $q = $p + 2;
    /// var_dump($q - $p, FFI::typeof($p)->getName());
    /// ```
    /// ```expect
    /// int(2)
    /// string(8) "int32_t*"
    /// ```
    fn do_operation(
        &self,
        req: &mut Request,
        op: BinaryOp,
        left: &Value,
        right: Option<&Value>,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        let Some(right) = right else {
            return Ok(None);
        };
        let (left, right) = (left.deref(), right.deref());
        if let Some(base) = as_cdata(req, &left)? {
            let v = view(req, base);
            if v.freed() || types(req).get(v.ty).code() < 15 || !is_sequence(req, v.ty) {
                return Ok(None);
            }
            match op {
                BinaryOp::Add => {
                    let offset = to_long(req, &right)?;
                    return Ok(Some(add(req, base, &v, offset)?.into_value()));
                }
                BinaryOp::Sub => {
                    if let Some(other) = as_cdata(req, &right)? {
                        let w = view(req, other);
                        if is_sequence(req, w.ty) {
                            let (t1, p1) = element_base(req, &v);
                            let (t2, p2) = element_base(req, &w);
                            if types(req).is_same(t1, t2) {
                                let size = types(req).get(t1).size as i64;
                                let diff = (p1 as i64).wrapping_sub(p2 as i64);
                                let diff = Value::int(if size == 0 { 0 } else { diff / size });
                                return Ok(Some(Owner::take(req, diff)));
                            }
                        }
                    }
                    let offset = to_long(req, &right)?;
                    return Ok(Some(
                        add(req, base, &v, offset.wrapping_neg())?.into_value(),
                    ));
                }
                _ => return Ok(None),
            }
        }
        if let Some(base) = as_cdata(req, &right)? {
            let v = view(req, base);
            if !v.freed() && is_sequence(req, v.ty) && op == BinaryOp::Add {
                let offset = to_long(req, &left)?;
                return Ok(Some(add(req, base, &v, offset)?.into_value()));
            }
        }
        Ok(None)
    }

    /// `zend_ffi_cdata_compare_objects`: two pointers by their addresses;
    /// anything else is not comparable.
    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        if let (Some(a), Some(b)) = (as_cdata(req, left)?, as_cdata(req, right)?) {
            let (va, vb) = (view(req, a), view(req, b));
            let pointers = {
                let types = types(req);
                types.get(va.ty).pointee().is_some() && types.get(vb.ty).pointee().is_some()
            };
            if pointers {
                let p1 = ffi::read::<usize>(va.ptr);
                let p2 = ffi::read::<usize>(vb.ptr);
                if p1 == 0 || p2 == 0 {
                    return Err(ffi_error(req, "NULL pointer dereference"));
                }
                return Ok(Some(p1.cmp(&p2)));
            }
        }
        Err(ffi_error(req, "Comparison of incompatible C types"))
    }

    /// `zend_ffi_cdata_get_iterator`: a C array's elements in order.
    ///
    /// ```php
    /// $a = FFI::cdef()->new("int[3]");
    /// foreach ($a as $k => $v) { $a[$k] = $k * 10; }
    /// foreach ($a as $k => $v) { echo "[$k=$v]"; }
    /// echo "\n";
    /// try { foreach (FFI::cdef()->new("int") as $v) {} } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// [0=0][1=10][2=20]
    /// Attempt to iterate on non C array
    /// ```
    fn get_iterator(
        &self,
        req: &mut Request,
        object: PhpObject,
        by_ref: bool,
    ) -> Result<Option<Box<dyn ClassIterator>>, Thrown> {
        let v = view(req, object);
        if types(req).get(v.ty).element().is_none() {
            return Err(ffi_error(req, "Attempt to iterate on non C array"));
        }
        Ok(Some(Box::new(Iter { key: 0, by_ref })))
    }

    /// `zend_ffi_cdata_ce->get_iterator`, on a class that is no
    /// `Traversable`.
    fn own_iterator(&self) -> bool {
        true
    }

    /// `sizeof(zend_ffi_cdata_iterator)`.
    fn iterator_size(&self) -> usize {
        120
    }
}

/// Whether a type is a pointer or an array, what arithmetic takes.
fn is_sequence(req: &mut Request, ty: TypeId) -> bool {
    let t = types(req).get(ty);
    t.pointee().is_some() || t.element().is_some()
}

/// The element type and the first element's address of a pointer or an
/// array.
fn element_base(req: &mut Request, v: &View) -> (TypeId, usize) {
    let t = types(req).get(v.ty);
    match (t.pointee(), t.element()) {
        (Some(to), _) => (to, ffi::read::<usize>(v.ptr)),
        (None, Some((of, _))) => (of, v.ptr),
        _ => (v.ty, v.ptr),
    }
}

/// `zend_ffi_add`: a pointer `offset` elements past what `base` points
/// at or holds.
fn add(
    req: &mut Request,
    base: PhpObject,
    v: &View,
    offset: i64,
) -> Result<Owner<PhpObject>, Thrown> {
    let (ty, ptr, element) = {
        let t = types(req).get(v.ty).clone();
        match (t.pointee(), t.element()) {
            (Some(to), _) => (v.ty, ffi::read::<usize>(v.ptr), to),
            (None, Some((of, _))) => (types(req).pointer_to(of, 0), v.ptr, of),
            _ => (v.ty, v.ptr, v.ty),
        }
    };
    let _ = base;
    let size = types(req).get(element).size;
    let value = ptr.wrapping_add((offset as usize).wrapping_mul(size));
    make(req, Data::holding(ty, value, v.is_const))
}

/// The table `zend_ffi_cdata_get_debug_info` builds; none for `void`.
fn debug_table(req: &mut Request, v: &View) -> Result<Option<Owner<PhpArray>>, Thrown> {
    let t = types(req).get(v.ty).clone();
    let mut table = PhpArray::new(req)?;
    match &t.kind {
        Kind::Void => return Ok(None),
        _ if t.scalar().is_some() => {
            let value = to_value(req, None, v.ptr, v.ty, Fetch::Read, true, false, false)?;
            table.set_str(req, b"cdata", value)?;
        }
        Kind::Pointer(to) => {
            let value = ffi::read::<usize>(v.ptr);
            let shown = if value == 0 {
                Owner::null()
            } else if matches!(types(req).get(*to).kind, Kind::Void) {
                Owner::int(value as i64)
            } else {
                to_value(req, None, value, *to, Fetch::Read, true, false, false)?
            };
            table.set(req, Owner::index(0), shown)?;
        }
        Kind::Struct(record) => {
            let union = t.attr & attr::UNION != 0;
            for (key, field) in &record.fields {
                let Some(key) = key else {
                    continue;
                };
                let value = if field.bits == 0 {
                    to_value(
                        req,
                        None,
                        v.ptr.wrapping_add(field.offset),
                        field.ty,
                        Fetch::Read,
                        true,
                        false,
                        union,
                    )?
                } else {
                    let bits = bit_field_value(req, v.ptr, field);
                    Owner::take(req, bits)
                };
                table.set_str(req, key, value)?;
            }
        }
        Kind::Array { of, len } => {
            let size = types(req).get(*of).size;
            let mut ptr = v.ptr;
            for n in 0..*len {
                let value = to_value(req, None, ptr, *of, Fetch::Read, true, false, false)?;
                table.set(req, Owner::index(n), value)?;
                ptr = ptr.wrapping_add(size);
            }
        }
        _ => {}
    }
    Ok(Some(table))
}

/// `zend_ffi_cdata_clone_obj`, the free table's `Use after free()`.
fn clone_cdata(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let v = view(req, object);
    if v.freed() {
        return Err(use_after_free(req));
    }
    let size = types(req).get(v.ty).size;
    let addr = alloc(req, size, false)?;
    ffi::copy(addr, v.ptr, size);
    make(
        req,
        Data {
            ty: v.ty,
            place: Place::Owned(addr),
            is_const: false,
            persistent: false,
            held: Box::default(),
        },
    )
}

/// The write handler's body, its arguments still the caller's.
fn write_property(
    req: &mut Request,
    object: PhpObject,
    name: &[u8],
    value: &Value,
) -> Result<(), Thrown> {
    let v = view(req, object);
    match table(req, &v) {
        Table::Free => Err(use_after_free(req)),
        Table::Value => {
            if name != b"cdata" {
                return Err(ffi_error(req, "Only 'cdata' property may be set"));
            }
            write_value(req, v.ptr, v.ty, value)
        }
        Table::Data => {
            if v.uninit() {
                let shown = String::from_utf8_lossy(name);
                let message =
                    format!("Attempt to assign field '{shown}' to uninitialized FFI\\CData object");
                return Err(ffi_error(req, &message));
            }
            let (field, ptr) = field_of(req, &v, name, "assign")?;
            if v.is_const {
                return Err(ffi_error(req, "Attempt to assign read-only location"));
            }
            if field.is_const {
                let shown = String::from_utf8_lossy(name);
                return Err(ffi_error(
                    req,
                    &format!("Attempt to assign read-only field '{shown}'"),
                ));
            }
            if field.bits == 0 {
                write_value(req, ptr.wrapping_add(field.offset), field.ty, value)
            } else {
                write_bit_field(req, ptr, &field, value)
            }
        }
    }
}

/// The dimension write handler's body.
fn write_dimension(
    req: &mut Request,
    object: PhpObject,
    offset: Option<&Value>,
    value: &Value,
) -> Result<(), Thrown> {
    let v = view(req, object);
    match table(req, &v) {
        Table::Free => Err(use_after_free(req)),
        Table::Value => Err(bad_array_access(req)),
        Table::Data => {
            let Some(offset) = offset else {
                return Err(ffi_error(
                    req,
                    "Cannot add next element to object of type FFI\\CData",
                ));
            };
            let dim = to_long(req, offset)?;
            let (ptr, ty, is_const) = element_of(req, &v, dim, "assign")?;
            if is_const {
                return Err(ffi_error(req, "Attempt to assign read-only location"));
            }
            write_value(req, ptr, ty, value)
        }
    }
}

/// `zend_ffi_cdata_iterator`: the key it stands at.
struct Iter {
    key: i64,
    by_ref: bool,
}

impl ClassIterator for Iter {
    fn rewind(&mut self, _: &mut Request, _: PhpObject) -> Result<(), Thrown> {
        self.key = 0;
        Ok(())
    }

    fn valid(&mut self, req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
        let v = view(req, object);
        let len = types(req).get(v.ty).element().map_or(0, |(_, len)| len);
        Ok(self.key >= 0 && self.key < len)
    }

    fn current(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
        let v = view(req, object);
        if v.ptr == 0 {
            return Err(ffi_error(req, "NULL pointer dereference"));
        }
        let (of, is_const) = {
            let t = types(req).get(v.ty);
            (
                t.element().map_or(v.ty, |(of, _)| of),
                t.attr & attr::CONST != 0,
            )
        };
        let size = types(req).get(of).size;
        let ptr = v.ptr.wrapping_add(size.wrapping_mul(self.key as usize));
        let fetch = if self.by_ref {
            Fetch::Place
        } else {
            Fetch::Read
        };
        to_value(
            req,
            None,
            ptr,
            of,
            fetch,
            v.is_const || is_const,
            false,
            false,
        )
    }

    fn key(&mut self, _: &mut Request, _: PhpObject) -> Result<Owner<Value>, Thrown> {
        Ok(Owner::int(self.key))
    }

    fn move_forward(&mut self, _: &mut Request, _: PhpObject) -> Result<(), Thrown> {
        self.key += 1;
        Ok(())
    }
}
