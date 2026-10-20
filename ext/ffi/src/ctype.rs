//! `FFI\CType`: a C type PHP code holds, `zend_ffi_ctype`, and the
//! reflection over it its methods are.

use std::cmp::Ordering;

use php_ext::abi::{
    DimensionRead, GcTable, Instance, ObjectHandlers, PropertyPlace, PropertyRead, PropertyTest,
    Refusals, This,
};
use php_ext::attr::php_class;
use php_ext::call::{Function, new_object};
use php_ext::raising::{Thrown, throw_error};
use php_ext::request::Request;
use php_ext::values::walk;
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, TypeTag, Value};

use crate::cdata::free_types;
use crate::types::{Kind, TypeId};
use crate::{classes, ffi_error, types};

/// `zend_ffi_ctype` past its object header.
#[derive(Debug, Default)]
pub(crate) struct TypeRef {
    pub(crate) ty: TypeId,
    /// The types it owns ([`crate::cdata::hold_types`]).
    pub(crate) held: Box<[usize]>,
}

/// `final class FFI\CType`, `@not-serializable`.
///
/// ```php
/// $t = FFI::cdef()->type("unsigned short");
/// var_dump($t, FFI\CType::TYPE_UINT16 === $t->getKind());
/// ```
/// ```expect
/// object(FFI\CType:uint16_t)#2 (0) {
/// }
/// bool(true)
/// ```
// `size = 80`: php's `zend_ffi_ctype_new` asks `emalloc` for
// `sizeof(zend_ffi_ctype)`, 64, whole, not `zend_object_alloc`.
#[php_class(namespace(FFI), final, not_serializable, payload = TypeRef, size = 80, handlers = crate::ctype::HANDLERS)]
pub(crate) struct CType;

#[php_class]
impl CType {
    const TYPE_VOID: i64 = 0;
    const TYPE_FLOAT: i64 = 1;
    const TYPE_DOUBLE: i64 = 2;
    const TYPE_LONGDOUBLE: i64 = 3;
    const TYPE_UINT8: i64 = 4;
    const TYPE_SINT8: i64 = 5;
    const TYPE_UINT16: i64 = 6;
    const TYPE_SINT16: i64 = 7;
    const TYPE_UINT32: i64 = 8;
    const TYPE_SINT32: i64 = 9;
    const TYPE_UINT64: i64 = 10;
    const TYPE_SINT64: i64 = 11;
    const TYPE_ENUM: i64 = 12;
    const TYPE_BOOL: i64 = 13;
    const TYPE_CHAR: i64 = 14;
    const TYPE_POINTER: i64 = 15;
    const TYPE_FUNC: i64 = 16;
    const TYPE_ARRAY: i64 = 17;
    const TYPE_STRUCT: i64 = 18;
    const ATTR_CONST: i64 = 1;
    const ATTR_INCOMPLETE_TAG: i64 = 2;
    const ATTR_VARIADIC: i64 = 4;
    const ATTR_INCOMPLETE_ARRAY: i64 = 8;
    const ATTR_VLA: i64 = 16;
    const ATTR_UNION: i64 = 32;
    const ATTR_PACKED: i64 = 64;
    const ATTR_MS_STRUCT: i64 = 128;
    const ATTR_GCC_STRUCT: i64 = 256;
    const ABI_DEFAULT: i64 = 0;
    const ABI_CDECL: i64 = 1;
    const ABI_FASTCALL: i64 = 2;
    const ABI_THISCALL: i64 = 3;
    const ABI_STDCALL: i64 = 4;
    const ABI_PASCAL: i64 = 5;
    const ABI_REGISTER: i64 = 6;
    const ABI_MS: i64 = 7;
    const ABI_SYSV: i64 = 8;
    const ABI_VECTORCALL: i64 = 9;

    /// `getName(): string`: the type's C name, or the class's where it
    /// is too long to print.
    ///
    /// ```php
    /// $ffi = FFI::cdef("struct s; typedef int (*fn)(char, double);");
    /// var_dump($ffi->type("struct s*")->getName(), $ffi->type("fn")->getName(), $ffi->type("const char*[2]")->getName());
    /// ```
    /// ```expect
    /// string(9) "struct s*"
    /// string(12) "int32_t(*)()"
    /// string(8) "char*[2]"
    /// ```
    fn getName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let ty = ctype_of(req, this.object());
        match types(req).name(ty) {
            Some(name) => PhpString::new(req, &name),
            None => PhpString::new(req, b"FFI\\CType"),
        }
    }

    /// `getKind(): int`.
    fn getKind(req: &mut Request, this: This<Self>) -> i64 {
        let ty = ctype_of(req, this.object());
        types(req).get(ty).code()
    }

    /// `getSize(): int`.
    ///
    /// ```php
    /// $ffi = FFI::cdef("struct s { char c; int i; };");
    /// var_dump($ffi->type("struct s")->getSize(), $ffi->type("long double")->getSize());
    /// ```
    /// ```expect
    /// int(8)
    /// int(8)
    /// ```
    fn getSize(req: &mut Request, this: This<Self>) -> i64 {
        let ty = ctype_of(req, this.object());
        types(req).get(ty).size as i64
    }

    /// `getAlignment(): int`.
    ///
    /// ```php
    /// $ffi = FFI::cdef("struct s { char c; short h; };");
    /// var_dump($ffi->type("struct s")->getAlignment(), $ffi->type("char")->getAlignment());
    /// ```
    /// ```expect
    /// int(2)
    /// int(1)
    /// ```
    fn getAlignment(req: &mut Request, this: This<Self>) -> i64 {
        let ty = ctype_of(req, this.object());
        i64::from(types(req).get(ty).align)
    }

    /// `getAttributes(): int`.
    ///
    /// ```php
    /// $ffi = FFI::cdef("typedef struct __attribute__((packed)) { char c; int i; } p;");
    /// var_dump($ffi->type("p")->getAttributes() & FFI\CType::ATTR_PACKED, FFI::sizeof($ffi->type("p")));
    /// ```
    /// ```expect
    /// int(64)
    /// int(5)
    /// ```
    fn getAttributes(req: &mut Request, this: This<Self>) -> i64 {
        let ty = ctype_of(req, this.object());
        i64::from(types(req).get(ty).attr)
    }

    /// `getEnumKind(): int`: the integer an enumeration is stored as,
    /// the narrowest its values fit.
    ///
    /// ```php
    /// $ffi = FFI::cdef("enum small { A = 1 }; enum big { B = 0x100000000 }; enum neg { C = -1 };");
    /// var_dump($ffi->type("enum small")->getEnumKind() === FFI\CType::TYPE_UINT32, $ffi->type("enum big")->getEnumKind() === FFI\CType::TYPE_UINT64, $ffi->type("enum neg")->getEnumKind() === FFI\CType::TYPE_SINT32);
    /// try { $ffi->type("int")->getEnumKind(); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// FFI\CType is not an enumeration
    /// ```
    fn getEnumKind(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let ty = ctype_of(req, this.object());
        match types(req).get(ty).kind {
            Kind::Enum { of, .. } => Ok(crate::types::int_code(of)),
            _ => Err(ffi_error(req, "FFI\\CType is not an enumeration")),
        }
    }

    /// `getArrayElementType(): CType`.
    ///
    /// ```php
    /// $ffi = FFI::cdef();
    /// $t = $ffi->type("char[2][3]");
    /// var_dump($t->getArrayElementType()->getName());
    /// try { $ffi->type("int")->getArrayElementType(); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(7) "char[3]"
    /// FFI\CType is not an array
    /// ```
    fn getArrayElementType(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<CType>>, Thrown> {
        let ty = ctype_of(req, this.object());
        match types(req).get(ty).element() {
            Some((of, _)) => make_ctype(req, of),
            None => Err(ffi_error(req, "FFI\\CType is not an array")),
        }
    }

    /// `getArrayLength(): int`.
    ///
    /// ```php
    /// $t = FFI::cdef()->type("int[5]");
    /// var_dump($t->getArrayLength(), $t->getArrayElementType()->getName());
    /// try { $t->getPointerType(); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(5)
    /// string(7) "int32_t"
    /// FFI\CType is not a pointer
    /// ```
    fn getArrayLength(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let ty = ctype_of(req, this.object());
        match types(req).get(ty).element() {
            Some((_, len)) => Ok(len),
            None => Err(ffi_error(req, "FFI\\CType is not an array")),
        }
    }

    /// `getPointerType(): CType`.
    ///
    /// ```php
    /// $ffi = FFI::cdef();
    /// $t = $ffi->type("double**");
    /// var_dump($t->getPointerType()->getName());
    /// ```
    /// ```expect
    /// string(7) "double*"
    /// ```
    fn getPointerType(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<CType>>, Thrown> {
        let ty = ctype_of(req, this.object());
        match types(req).get(ty).pointee() {
            Some(to) => make_ctype(req, to),
            None => Err(ffi_error(req, "FFI\\CType is not a pointer")),
        }
    }

    /// `getStructFieldNames(): array`.
    ///
    /// ```php
    /// $ffi = FFI::cdef("struct s { int a; char b; };");
    /// $t = $ffi->type("struct s");
    /// var_dump($t->getStructFieldNames(), $t->getStructFieldOffset("b"), $t->getStructFieldType("b")->getName());
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   string(1) "a"
    ///   [1]=>
    ///   string(1) "b"
    /// }
    /// int(4)
    /// string(4) "char"
    /// ```
    fn getStructFieldNames(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let ty = ctype_of(req, this.object());
        let names: Option<Vec<Vec<u8>>> = types(req).get(ty).record().map(|record| {
            record
                .fields
                .iter()
                .filter_map(|(name, _)| name.clone())
                .collect()
        });
        let Some(names) = names else {
            return Err(ffi_error(req, "FFI\\CType is not a structure"));
        };
        let mut array = PhpArray::new(req)?;
        for name in names {
            let name = PhpString::new(req, &name)?;
            array.append(req, name.into_value())?;
        }
        Ok(array)
    }

    /// `getStructFieldOffset(string $name): int`.
    ///
    /// ```php
    /// $ffi = FFI::cdef("struct s { char c; double d; };");
    /// $t = $ffi->type("struct s");
    /// var_dump($t->getStructFieldOffset("d"));
    /// try { $t->getStructFieldOffset("x"); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(8)
    /// Wrong field name
    /// ```
    fn getStructFieldOffset(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
    ) -> Result<i64, Thrown> {
        let ty = ctype_of(req, this.object());
        let field = match types(req).get(ty).record() {
            Some(record) => record.field(name.as_bytes()).copied(),
            None => return Err(ffi_error(req, "FFI\\CType is not a structure")),
        };
        match field {
            Some(field) => Ok(field.offset as i64),
            None => Err(ffi_error(req, "Wrong field name")),
        }
    }

    /// `getStructFieldType(string $name): CType`.
    ///
    /// ```php
    /// $ffi = FFI::cdef("struct s { unsigned short u[4]; };");
    /// var_dump($ffi->type("struct s")->getStructFieldType("u")->getName());
    /// try { $ffi->type("int")->getStructFieldType("u"); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(11) "uint16_t[4]"
    /// FFI\CType is not a structure
    /// ```
    fn getStructFieldType(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
    ) -> Result<Owner<Instance<CType>>, Thrown> {
        let ty = ctype_of(req, this.object());
        let field = match types(req).get(ty).record() {
            Some(record) => record.field(name.as_bytes()).copied(),
            None => return Err(ffi_error(req, "FFI\\CType is not a structure")),
        };
        match field {
            Some(field) => make_ctype(req, field.ty),
            None => Err(ffi_error(req, "Wrong field name")),
        }
    }

    /// `getFuncABI(): int`, libffi's number for the convention.
    ///
    /// ```php
    /// $ffi = FFI::cdef("typedef int (*fn)(void);");
    /// var_dump($ffi->type("fn")->getPointerType()->getFuncABI());
    /// try { $ffi->type("int")->getFuncABI(); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(1)
    /// FFI\CType is not a function
    /// ```
    fn getFuncABI(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let ty = ctype_of(req, this.object());
        match types(req).get(ty).func() {
            Some(func) => Ok(i64::from(func.abi)),
            None => Err(ffi_error(req, "FFI\\CType is not a function")),
        }
    }

    /// `getFuncReturnType(): CType`.
    ///
    /// ```php
    /// $ffi = FFI::cdef("typedef double (*f)(int, char*);");
    /// $t = $ffi->type("f")->getPointerType();
    /// var_dump($t->getFuncReturnType()->getName(), $t->getFuncParameterCount(), $t->getFuncParameterType(1)->getName());
    /// try { $t->getFuncParameterType(2); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(6) "double"
    /// int(2)
    /// string(5) "char*"
    /// Wrong argument number
    /// ```
    fn getFuncReturnType(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<CType>>, Thrown> {
        let ty = ctype_of(req, this.object());
        match types(req).get(ty).func().map(|func| func.ret) {
            Some(ret) => make_ctype(req, ret),
            None => Err(ffi_error(req, "FFI\\CType is not a function")),
        }
    }

    /// `getFuncParameterCount(): int`, none for `(void)`.
    ///
    /// ```php
    /// $ffi = FFI::cdef("typedef void (*a)(void); typedef void (*b)(int, ...);");
    /// var_dump($ffi->type("a")->getPointerType()->getFuncParameterCount(), $ffi->type("b")->getPointerType()->getFuncParameterCount());
    /// ```
    /// ```expect
    /// int(0)
    /// int(1)
    /// ```
    fn getFuncParameterCount(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let ty = ctype_of(req, this.object());
        match types(req).get(ty).func() {
            Some(func) => Ok(func.args.as_ref().map_or(0, Vec::len) as i64),
            None => Err(ffi_error(req, "FFI\\CType is not a function")),
        }
    }

    /// `getFuncParameterType(int $index): CType`.
    ///
    /// ```php
    /// $ffi = FFI::cdef("typedef void (*f)(float, char[3]);");
    /// $t = $ffi->type("f")->getPointerType();
    /// var_dump($t->getFuncParameterType(0)->getName(), $t->getFuncParameterType(1)->getName());
    /// try { $t->getFuncParameterType(-1); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(5) "float"
    /// string(5) "char*"
    /// Wrong argument number
    /// ```
    fn getFuncParameterType(
        req: &mut Request,
        this: This<Self>,
        index: i64,
    ) -> Result<Owner<Instance<CType>>, Thrown> {
        let ty = ctype_of(req, this.object());
        let arg = match types(req).get(ty).func() {
            Some(func) => func.args.as_ref().and_then(|args| {
                usize::try_from(index)
                    .ok()
                    .and_then(|at| args.get(at).copied())
            }),
            None => return Err(ffi_error(req, "FFI\\CType is not a function")),
        };
        match arg {
            Some(arg) => make_ctype(req, arg),
            None => Err(ffi_error(req, "Wrong argument number")),
        }
    }
}

/// A `FFI\CType`'s type.
pub(crate) fn ctype_of(req: &Request, object: PhpObject) -> TypeId {
    object
        .native::<TypeRef>(req)
        .map_or(TypeId::VOID, |held| held.ty)
}

/// A new `FFI\CType` over `ty`.
pub(crate) fn make_ctype(req: &mut Request, ty: TypeId) -> Result<Owner<Instance<CType>>, Thrown> {
    make_owning(req, ty, Box::default())
}

/// A `FFI\CType` of `ty` that owns the types `held`.
pub(crate) fn make_owning(
    req: &mut Request,
    ty: TypeId,
    held: Box<[usize]>,
) -> Result<Owner<Instance<CType>>, Thrown> {
    let class = classes(req)?.ctype;
    let object = new_object(req, class)?;
    if let Some(slot) = object.native_mut::<TypeRef>(req) {
        slot.ty = ty;
        slot.held = held;
    }
    Ok(object.into_instance())
}

/// The handlers of `FFI\CType`.
pub(crate) static HANDLERS: Handlers = Handlers;

/// The handlers of `FFI\CType`.
pub(crate) struct Handlers;

/// `zend_bad_array_access` over `FFI\CType`.
fn bad_array_access(req: &mut Request) -> Thrown {
    throw_error(
        req,
        "Error",
        "Cannot use object of type FFI\\CType as array",
    )
}

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    fn get_constructor(&self, req: &mut Request, _: PhpObject) -> Result<Option<Function>, Thrown> {
        Err(throw_error(
            req,
            "Error",
            "Instantiation of FFI\\CType is not allowed",
        ))
    }

    /// `zend_ffi_ctype_free_obj`: the types it owns given back.
    ///
    /// ```php
    /// $ffi = FFI::cdef();
    /// $a = memory_get_usage(); $t = $ffi->type("char[4]"); $b = memory_get_usage(); $t = null; $c = memory_get_usage();
    /// var_dump($b - $a, $c - $b);
    /// ```
    /// ```expect
    /// int(160)
    /// int(-160)
    /// ```
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let Some(slot) = object.native_mut::<TypeRef>(req) else {
            return;
        };
        let held = std::mem::take(&mut slot.held);
        free_types(req, &held);
    }

    /// `zend_fake_read_property`.
    fn read_property(
        &self,
        req: &mut Request,
        _: PhpObject,
        _: &[u8],
        _: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        Err(throw_error(
            req,
            "Error",
            "Cannot access property of object of type FFI\\CType",
        ))
    }

    /// `zend_fake_write_property`, whose refusal is the array one.
    fn write_property(
        &self,
        req: &mut Request,
        _: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        value.give_up(req);
        name.give_up(req);
        Err(bad_array_access(req))
    }

    fn read_dimension(
        &self,
        req: &mut Request,
        _: PhpObject,
        _: Option<&Value>,
        _: DimensionRead,
    ) -> Result<Owner<Value>, Thrown> {
        Err(bad_array_access(req))
    }

    fn write_dimension(
        &self,
        req: &mut Request,
        _: PhpObject,
        _: Option<&Value>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        value.give_up(req);
        Err(bad_array_access(req))
    }

    fn get_property_ptr_ptr(
        &self,
        _: &mut Request,
        _: PhpObject,
        _: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        Ok(PropertyPlace::Handlers)
    }

    fn has_property(
        &self,
        req: &mut Request,
        _: PhpObject,
        _: &[u8],
        _: PropertyTest,
    ) -> Result<bool, Thrown> {
        Err(bad_array_access(req))
    }

    fn unset_property(&self, req: &mut Request, _: PhpObject, _: &[u8]) -> Result<(), Thrown> {
        Err(bad_array_access(req))
    }

    fn has_dimension(
        &self,
        req: &mut Request,
        _: PhpObject,
        _: &Value,
        _: bool,
    ) -> Result<bool, Thrown> {
        Err(bad_array_access(req))
    }

    fn unset_dimension(&self, req: &mut Request, _: PhpObject, _: &Value) -> Result<(), Thrown> {
        Err(bad_array_access(req))
    }

    fn get_properties(&self, _: &mut Request, _: PhpObject) -> Result<walk::Table, Thrown> {
        Ok(walk::Table::Array(PhpArray::empty()))
    }

    fn properties_overloaded(&self) -> bool {
        true
    }

    /// `zend_ffi_ctype_get_class_name`.
    fn get_class_name(&self, req: &mut Request, object: PhpObject) -> Vec<u8> {
        let ty = ctype_of(req, object);
        match types(req).name(ty) {
            Some(name) => [b"FFI\\CType:".as_slice(), &name].concat(),
            None => b"FFI\\CType".to_vec(),
        }
    }

    /// `zend_ffi_ctype_compare_objects`: equal for one type, else not.
    ///
    /// ```php
    /// $ffi = FFI::cdef();
    /// var_dump($ffi->type("int") == $ffi->type("int32_t"), $ffi->type("int") == $ffi->type("char"));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        let class = classes(req)?.ctype;
        let of = |value: &Value| {
            value
                .deref()
                .as_object()
                .filter(|object| object.class() == class)
        };
        if let (Some(a), Some(b)) = (of(left), of(right)) {
            let (a, b) = (ctype_of(req, a), ctype_of(req, b));
            return Ok(Some(if types(req).is_same(a, b) {
                Ordering::Equal
            } else {
                Ordering::Greater
            }));
        }
        Err(ffi_error(req, "Comparison of incompatible C types"))
    }

    /// `zend_fake_cast_object`.
    fn cast_object(
        &self,
        _: &mut Request,
        _: PhpObject,
        to: TypeTag,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        Ok((to == TypeTag::Bool).then(|| Owner::bool(true)))
    }

    /// `zend_ffi_ctype_get_debug_info`: nothing to show.
    fn get_debug_info(
        &self,
        _: &mut Request,
        _: PhpObject,
    ) -> Result<Option<php_ext::abi::DebugInfo>, Thrown> {
        Ok(None)
    }

    fn get_gc(&self, _: &mut Request, _: PhpObject, _: &mut dyn FnMut(Value)) -> GcTable {
        GcTable::Properties
    }
}
