//! `FFI`: a set of C declarations and the library they resolve
//! against, `zend_ffi`, with php's static API over C values and types.
//! An object reads and writes the C variables it declares as
//! properties and calls the C functions it declares as methods, each a
//! trampoline its `get_method` makes under the function's name.

use php_ext::abi::{
    GcTable, Instance, ObjectHandlers, PropertyPlace, PropertyRead, PropertyTest, Raw, Ref,
    Refusals, Union, Untyped,
};
use php_ext::attr::php_class;
use php_ext::call::{Function, new_object};
use php_ext::coerce::{to_long, value_name};
use php_ext::raising::{
    Thrown, arg_type_error, deprecated, pending, throw_error, throw_error_chained,
};
use php_ext::request::Request;
use php_ext::values::walk;
use php_ext::values::{Emalloc, Owner, PhpArray, PhpObject, PhpString, TypeTag, Value, ValueRef};

use crate::cdata::{
    CData, Data, Fetch, Place, alloc, as_cdata, free, free_types, hold, hold_types, make, remember,
    to_value, view, write_value,
};
use crate::ctype::{CType, TypeRef, ctype_of, make_ctype, make_owning};
use crate::ffi::{self, Library};
use crate::parse::{BIGGEST_ALIGNMENT, Dcl, Parser, Scope, SymbolKind};
use crate::preload::Fail;
use crate::types::{Kind, Type, TypeId, attr};
use crate::{State, classes, ffi_error, parser_error, types, validate_api_restriction};

/// `zend_ffi` past its object header: the library its declarations
/// resolve against, and the declarations.
#[derive(Debug, Default)]
pub(crate) struct Decls {
    /// Held for its drop, `DL_UNLOAD` with the object's last owner.
    #[allow(dead_code)]
    pub(crate) lib: Option<Library>,
    pub(crate) scope: Box<Scope>,
    /// What php `emalloc`s for the declarations, their tables and their
    /// types ([`hold_decls`]), given back with the object.
    pub(crate) held: Box<[usize]>,
}

/// `final class FFI`, `@not-serializable`.
///
/// ```php
/// var_dump(FFI::__BIGGEST_ALIGNMENT__, (new ReflectionClass('FFI'))->isFinal());
/// ```
/// ```expect
/// int(8)
/// bool(true)
/// ```
// `size = 104`: php's `zend_ffi_new` asks `emalloc` for
// `sizeof(zend_ffi)`, 88, whole, not `zend_object_alloc`.
#[php_class(final, not_serializable, payload = Decls, size = 104, handlers = crate::decls::HANDLERS)]
#[allow(clippy::upper_case_acronyms)]
pub(crate) struct FFI;

#[php_class]
impl FFI {
    const __BIGGEST_ALIGNMENT__: i64 = BIGGEST_ALIGNMENT as i64;

    /// `FFI::cdef(string $code = "", ?string $lib = null): FFI`: the
    /// declarations parsed, each variable and function resolved in the
    /// library, or in the process where none is named.
    ///
    /// ```php
    /// $ffi = FFI::cdef("int abs(int);");
    /// var_dump($ffi->abs(-2));
    /// try { FFI::cdef("void nowhere_at_all(void);"); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// try { FFI::cdef("int x"); } catch (FFI\ParserException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(2)
    /// Failed resolving C function 'nowhere_at_all'
    /// ';' expected, got '<EOF>' at line 1
    /// ```
    ///
    /// A relative `$lib`, and a header's relative `FFI_LIB` for
    /// [`FFI::load`], are opened against the request's working directory,
    /// which `chdir()` moved, as php's `dlopen` opens them against its
    /// process's ([`crate::preload::library_path`]):
    ///
    /// ```php
    /// $d = sys_get_temp_dir() . '/witness_ffi_lib_' . getmypid();
    /// @mkdir("$d/lib", 0777, true);
    /// @symlink('/opt/homebrew/opt/libsodium/lib/libsodium.dylib', "$d/lib/sodium.dylib");
    /// file_put_contents("$d/sodium.h", "#define FFI_LIB \"./lib/sodium.dylib\"\nint sodium_init(void);");
    /// chdir($d);
    /// var_dump(FFI::cdef('int sodium_init(void);', './lib/sodium.dylib')->sodium_init() >= 0);
    /// var_dump(FFI::load('./sodium.h')->sodium_init() >= 0);
    /// unlink("$d/sodium.h"); unlink("$d/lib/sodium.dylib"); rmdir("$d/lib"); rmdir($d);
    /// ```
    /// ```expect exit=0
    /// bool(true)
    /// bool(true)
    /// ```
    fn cdef(
        req: &mut Request,
        #[php(default = "")] code: PhpString,
        #[php(default = null)] lib: Option<PhpString>,
    ) -> Result<Owner<Instance<FFI>>, Thrown> {
        validate_api_restriction(req)?;
        let lib = match lib {
            Some(path) => match Library::open(&crate::preload::library_path(
                Some(&req.environment().cwd()),
                path.as_bytes(),
            )) {
                Ok(lib) => Some(lib),
                Err(error) => {
                    let shown = String::from_utf8_lossy(path.as_bytes()).into_owned();
                    let message = format!("Failed loading '{shown}' ({error})");
                    return Err(ffi_error(req, &message));
                }
            },
            None => None,
        };
        let mut scope = Scope::default();
        let from = types(req).len();
        if !code.as_bytes().is_empty() {
            parse_decl(req, code.as_bytes(), &mut scope, attr::STORED)
                .map_err(|bail| parser_error(req, &bail))?;
            resolve(req, &mut scope, lib.as_ref())?;
        }
        let held = hold_decls(req, &scope, from)?;
        made(
            req,
            Decls {
                lib,
                scope: Box::new(scope),
                held,
            },
        )
    }

    /// `FFI::load(string $filename): ?FFI`: a header file's
    /// declarations, with the `FFI_LIB` its directives name.
    ///
    /// ```php
    /// try { FFI::load(__DIR__ . "/nowhere.h"); } catch (FFI\Exception $e) { echo basename($e->getMessage()), "\n"; }
    /// ```
    /// ```expect
    /// nowhere.h', file doesn't exist
    /// ```
    ///
    /// A relative path names a file in the request's working directory,
    /// where `chdir()` moved it, as php's `stat` and `open` find it in
    /// its process's: the file is found, and its parse fails.
    ///
    /// ```php
    /// $d = sys_get_temp_dir() . '/ffi-load-' . getmypid(); mkdir($d); chdir($d);
    /// file_put_contents('bad.h', "int x(;\n");
    /// try { FFI::load('bad.h'); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// unlink('bad.h'); chdir('/'); rmdir($d);
    /// ```
    /// ```expect
    /// Failed loading 'bad.h'
    /// ```
    fn load(
        req: &mut Request,
        filename: PhpString,
    ) -> Result<Option<Owner<Instance<FFI>>>, Thrown> {
        validate_api_restriction(req)?;
        // `ZEND_COMPILE_PRELOAD_IN_CHILD`: the preload's request served
        // as `opcache.preload_user`, php's child, publishes no scope.
        if req.code().preloading_in_child() {
            return Err(ffi_error(
                req,
                "FFI::load() doesn't work in conjunction with \"opcache.preload_user\". Use \"ffi.preload\" instead.",
            ));
        }
        if req.code().preloading() {
            let Some(scope) = crate::preload::register(req, filename.as_bytes()) else {
                return Ok(None);
            };
            return made(
                req,
                Decls {
                    lib: None,
                    scope: Box::new(scope),
                    held: Box::default(),
                },
            )
            .map(Some);
        }
        load(req, filename.as_bytes()).map(Some)
    }

    /// `FFI::scope(string $name): FFI`: the declarations `ffi.preload`
    /// loaded under the scope's name.
    ///
    /// ```php
    /// try { FFI::scope("NONE"); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Failed loading scope 'NONE'
    /// ```
    fn scope(req: &mut Request, name: PhpString) -> Result<Owner<Instance<FFI>>, Thrown> {
        validate_api_restriction(req)?;
        let Some(scope) = crate::preload::scope(req, name.as_bytes()) else {
            let shown = String::from_utf8_lossy(name.as_bytes()).into_owned();
            return Err(ffi_error(req, &format!("Failed loading scope '{shown}'")));
        };
        made(
            req,
            Decls {
                lib: None,
                scope: Box::new(scope),
                held: Box::default(),
            },
        )
    }

    /// `FFI::new(FFI\CType|string $type, bool $owned = true, bool
    /// $persistent = false): FFI\CData`, statically deprecated.
    ///
    /// ```php
    /// $i = FFI::new("int");
    /// var_dump($i->cdata);
    /// ```
    /// ```expect
    /// Deprecated: Calling FFI::new() statically is deprecated in %s on line 2
    /// int(0)
    /// ```
    #[expect(
        clippy::new_ret_no_self,
        reason = "php's FFI::new, a value of a C type"
    )]
    fn new(
        req: &mut Request,
        r#type: Union<Instance<CType>, PhpString>,
        #[php(default = true)] owned: bool,
        #[php(default = false)] persistent: bool,
    ) -> Result<Owner<Instance<CData>>, Thrown> {
        ffi_new(req, None, r#type, owned, persistent)
    }

    /// `FFI::free(FFI\CData &$ptr): void`: the memory an unowned value
    /// or a pointer names given back.
    ///
    /// ```php
    /// $p = FFI::cdef()->new("int", false);
    /// FFI::free($p);
    /// try { $p->cdata; } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// try { FFI::free(FFI::cdef()->new("int")); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Use after free()
    /// free() non a C pointer
    /// ```
    fn free(req: &mut Request, #[php(prefer_ref)] ptr: Ref<Instance<CData>>) -> Result<(), Thrown> {
        validate_api_restriction(req)?;
        let Some(object) = ptr.get().map(Instance::object) else {
            return Ok(());
        };
        let v = view(req, object);
        if types(req).get(v.ty).pointee().is_some() {
            if v.ptr == 0 {
                return Err(ffi_error(req, "NULL pointer dereference"));
            }
            free(req, ffi::read::<usize>(v.ptr));
            ffi::write(v.ptr, 0usize);
            if let Some(data) = object.native_mut::<Data>(req)
                && let Place::Holder { target, .. } = &mut data.place
            {
                *target = None;
            }
            return Ok(());
        }
        if v.owned {
            return Err(ffi_error(req, "free() non a C pointer"));
        }
        free(req, v.ptr);
        if let Some(data) = object.native_mut::<Data>(req) {
            data.place = Place::Freed;
            data.persistent = false;
        }
        Ok(())
    }

    /// `FFI::cast(FFI\CType|string $type, &$ptr): FFI\CData`, statically
    /// deprecated: the same memory read as another type, or a number
    /// made a value or a pointer.
    ///
    /// ```php
    /// $ffi = FFI::cdef();
    /// $i = $ffi->new("int32_t"); $i->cdata = -1;
    /// var_dump($ffi->cast("uint8_t", $i)->cdata, $ffi->cast("int", 2.5)->cdata);
    /// var_dump(FFI::cast("void*", 0));
    /// try { $ffi->cast("int", "1"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { $ffi->cast("int64_t", $i); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(255)
    /// int(2)
    ///
    /// Deprecated: Calling FFI::cast() statically is deprecated in %s on line 5
    /// object(FFI\CData:void*)#%d (1) {
    ///   [0]=>
    ///   NULL
    /// }
    /// FFI::cast(): Argument #2 ($ptr) must be of type FFI\CData, string given
    /// attempt to cast to larger type
    /// ```
    fn cast(
        req: &mut Request,
        r#type: Union<Instance<CType>, PhpString>,
        #[php(prefer_ref)] ptr: Ref<Untyped<Value>>,
    ) -> Result<Owner<Instance<CData>>, Thrown> {
        let value = ptr.get().map_or(Value::null(), |held| held.0);
        ffi_cast(req, None, r#type, value)
    }

    /// `FFI::type(string $type): FFI\CType`, statically deprecated.
    ///
    /// ```php
    /// var_dump(FFI::type("uint32_t*")->getName());
    /// try { FFI::cdef()->type("int x"); } catch (FFI\ParserException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Deprecated: Calling FFI::type() statically is deprecated in %s on line 2
    /// string(9) "uint32_t*"
    /// <EOF> expected, got '<ID>' at line 1
    /// ```
    fn r#type(req: &mut Request, r#type: PhpString) -> Result<Owner<Instance<CType>>, Thrown> {
        ffi_type(req, None, r#type)
    }

    /// `FFI::typeof(FFI\CData &$ptr): FFI\CType`.
    ///
    /// ```php
    /// var_dump(FFI::typeof(FFI::cdef()->new("uint16_t[3]"))->getName());
    /// ```
    /// ```expect
    /// string(11) "uint16_t[3]"
    /// ```
    fn r#typeof(
        req: &mut Request,
        #[php(prefer_ref)] ptr: Ref<Instance<CData>>,
    ) -> Result<Owner<Instance<CType>>, Thrown> {
        validate_api_restriction(req)?;
        let ty = match ptr.get() {
            Some(object) => view(req, object.object()).ty,
            None => TypeId::VOID,
        };
        make_ctype(req, ty)
    }

    /// `FFI::arrayType(FFI\CType $type, array $dimensions): FFI\CType`.
    ///
    /// ```php
    /// $t = FFI::arrayType(FFI::cdef()->type("int"), [2, 3]);
    /// var_dump($t->getName(), FFI::sizeof($t));
    /// ```
    /// ```expect
    /// string(13) "int32_t[2][3]"
    /// int(24)
    /// ```
    fn arrayType(
        req: &mut Request,
        r#type: Instance<CType>,
        dimensions: PhpArray,
    ) -> Result<Owner<Instance<CType>>, Thrown> {
        validate_api_restriction(req)?;
        let ty = ctype_of(req, r#type.object());
        let (kind_func, incomplete_array, is_void, incomplete_tag) = {
            let t = types(req).get(ty);
            (
                matches!(t.kind, Kind::Func(_)),
                matches!(t.kind, Kind::Array { .. }) && t.attr & attr::INCOMPLETE_ARRAY != 0,
                matches!(t.kind, Kind::Void),
                t.attr & attr::INCOMPLETE_TAG != 0,
            )
        };
        if kind_func {
            return Err(ffi_error(req, "Array of functions is not allowed"));
        } else if incomplete_array {
            return Err(ffi_error(
                req,
                "Only the leftmost array can be undimensioned",
            ));
        } else if is_void {
            return Err(ffi_error(req, "Array of void type is not allowed"));
        } else if incomplete_tag {
            return Err(ffi_error(req, "Array of incomplete type is not allowed"));
        }
        // `ZEND_HASH_REVERSE_FOREACH_VAL(dims, val)` (`Z/ext/ffi/ffi.c:4331`):
        // the rows last to first. The walks read first to last, so the
        // rows are read under the borrowed request, each kept by an owner
        // while `zval_get_long` reaches a diagnostic's handler, and given
        // back after.
        let dims: Vec<Owner<Value>> = walk::rows(req, dimensions)
            .map(|(_, value)| Owner::take(req, value))
            .collect();
        let sized = dimensioned(req, ty, &dims);
        for dim in dims {
            dim.give_up(req);
        }
        make_ctype(req, sized?)
    }

    /// `FFI::addr(FFI\CData &$ptr): FFI\CData`: a pointer to the value.
    ///
    /// ```php
    /// $i = FFI::cdef()->new("int"); $i->cdata = 5;
    /// $p = FFI::addr($i);
    /// $p[0] = 6;
    /// var_dump($i->cdata, FFI::typeof($p)->getName());
    /// try { FFI::addr(FFI::addr($i)); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(6)
    /// string(8) "int32_t*"
    /// FFI::addr() cannot create a reference to a temporary pointer
    /// ```
    fn addr(
        req: &mut Request,
        #[php(prefer_ref)] ptr: Ref<Instance<CData>>,
    ) -> Result<Owner<Instance<CData>>, Thrown> {
        validate_api_restriction(req)?;
        let Some(object) = ptr.get().map(Instance::object) else {
            return Err(ffi_error(req, "NULL pointer dereference"));
        };
        let v = view(req, object);
        let temporary = temporary(req, 1, object);
        if temporary && v.holder && types(req).get(v.ty).pointee().is_some() {
            return Err(ffi_error(
                req,
                "FFI::addr() cannot create a reference to a temporary pointer",
            ));
        }
        let from = types(req).len();
        let ty = types(req).pointer_to(v.ty, 0);
        let mut data = Data::holding(ty, v.ptr, false);
        data.held = hold_types(req, from)?;
        if temporary
            && let Some(old) = object.native_mut::<Data>(req)
            && let Place::Owned(addr) = old.place
        {
            old.place = Place::Borrowed(addr);
            if let Place::Holder { target, .. } = &mut data.place {
                *target = std::num::NonZeroUsize::new(addr);
            }
        }
        made_cdata(req, data)
    }

    /// `FFI::sizeof(FFI\CData|FFI\CType &$ptr): int`.
    ///
    /// ```php
    /// $ffi = FFI::cdef("struct s { char c; double d; };");
    /// var_dump(FFI::sizeof($ffi->type("struct s")), FFI::alignof($ffi->new("struct s")));
    /// try { FFI::sizeof(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(16)
    /// int(8)
    /// FFI::sizeof(): Argument #1 ($ptr) must be of type FFI\CData or FFI\CType, int given
    /// ```
    fn sizeof(
        req: &mut Request,
        #[php(prefer_ref)] ptr: Ref<Raw<Union<Instance<CData>, Instance<CType>>>>,
    ) -> Result<i64, Thrown> {
        validate_api_restriction(req)?;
        let ty = data_or_type(req, ptr.get().map(Raw::value))?;
        Ok(types(req).get(ty).size as i64)
    }

    /// `FFI::alignof(FFI\CData|FFI\CType &$ptr): int`.
    ///
    /// ```php
    /// $ffi = FFI::cdef();
    /// var_dump(FFI::alignof($ffi->type("char")), FFI::alignof($ffi->new("double[2]")));
    /// try { FFI::alignof("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(1)
    /// int(8)
    /// FFI::alignof(): Argument #1 ($ptr) must be of type FFI\CData or FFI\CType, string given
    /// ```
    fn alignof(
        req: &mut Request,
        #[php(prefer_ref)] ptr: Ref<Raw<Union<Instance<CData>, Instance<CType>>>>,
    ) -> Result<i64, Thrown> {
        validate_api_restriction(req)?;
        let ty = data_or_type(req, ptr.get().map(Raw::value))?;
        Ok(i64::from(types(req).get(ty).align))
    }

    /// `FFI::memcpy(FFI\CData &$to, &$from, int $size): void`.
    ///
    /// ```php
    /// $a = FFI::cdef()->new("char[6]");
    /// FFI::memcpy($a, "hello", 5);
    /// var_dump(FFI::string($a));
    /// try { FFI::memcpy($a, "x", 2); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(5) "hello"
    /// Attempt to read over string boundary
    /// ```
    fn memcpy(
        req: &mut Request,
        #[php(prefer_ref)] to: Ref<Instance<CData>>,
        #[php(prefer_ref)] from: Ref<Untyped<Value>>,
        size: i64,
    ) -> Result<(), Thrown> {
        validate_api_restriction(req)?;
        let Some(to) = to.get().map(Instance::object) else {
            return Ok(());
        };
        let dst = bounded(req, to, size, "Attempt to write over data boundary")?;
        let from = from.get().map_or(Value::null(), |held| held.0).deref();
        let mut text = Vec::new();
        let src = source(
            req,
            &from,
            size,
            2,
            &mut text,
            [
                "Attempt to read over string boundary",
                "Attempt to read over data boundary",
            ],
        )?;
        ffi::copy(dst, src, size as usize);
        Ok(())
    }

    /// `FFI::memcmp(&$ptr1, &$ptr2, int $size): int`.
    ///
    /// ```php
    /// $a = FFI::cdef()->new("char[3]");
    /// FFI::memcpy($a, "abc", 3);
    /// var_dump(FFI::memcmp($a, "abd", 3), FFI::memcmp("abc", $a, 3));
    /// ```
    /// ```expect
    /// int(-1)
    /// int(0)
    /// ```
    fn memcmp(
        req: &mut Request,
        #[php(prefer_ref)] ptr1: Ref<Untyped<Value>>,
        #[php(prefer_ref)] ptr2: Ref<Untyped<Value>>,
        size: i64,
    ) -> Result<i64, Thrown> {
        validate_api_restriction(req)?;
        let one = ptr1.get().map_or(Value::null(), |held| held.0).deref();
        let two = ptr2.get().map_or(Value::null(), |held| held.0).deref();
        let (mut text1, mut text2) = (Vec::new(), Vec::new());
        let a = source(
            req,
            &one,
            size,
            1,
            &mut text1,
            [
                "attempt to read over string boundary",
                "attempt to read over data boundary",
            ],
        )?;
        let b = source(
            req,
            &two,
            size,
            2,
            &mut text2,
            [
                "Attempt to read over string boundary",
                "Attempt to read over data boundary",
            ],
        )?;
        let ret = ffi::compare(a, b, size as usize);
        Ok(i64::from(ret.signum()))
    }

    /// `FFI::memset(FFI\CData &$ptr, int $value, int $size): void`.
    ///
    /// ```php
    /// $a = FFI::cdef()->new("uint8_t[3]");
    /// FFI::memset($a, 7, 2);
    /// var_dump($a[0], $a[1], $a[2]);
    /// try { FFI::memset($a, 0, 4); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(7)
    /// int(7)
    /// int(0)
    /// attempt to write over data boundary
    /// ```
    fn memset(
        req: &mut Request,
        #[php(prefer_ref)] ptr: Ref<Instance<CData>>,
        value: i64,
        size: i64,
    ) -> Result<(), Thrown> {
        validate_api_restriction(req)?;
        let Some(object) = ptr.get().map(Instance::object) else {
            return Ok(());
        };
        let dst = bounded(req, object, size, "attempt to write over data boundary")?;
        ffi::fill(dst, value as u8, size as usize);
        Ok(())
    }

    /// `FFI::string(FFI\CData &$ptr, ?int $size = null): string`.
    ///
    /// ```php
    /// $a = FFI::cdef()->new("char[4]");
    /// FFI::memcpy($a, "ab", 2);
    /// var_dump(FFI::string($a), bin2hex(FFI::string($a, 3)));
    /// try { FFI::string(FFI::cdef()->new("int")); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(2) "ab"
    /// string(6) "616200"
    /// FFI\Cdata is not a C string
    /// ```
    fn string(
        req: &mut Request,
        #[php(prefer_ref)] ptr: Ref<Instance<CData>>,
        #[php(default = null)] size: Option<i64>,
    ) -> Result<Owner<PhpString>, Thrown> {
        validate_api_restriction(req)?;
        let Some(object) = ptr.get().map(Instance::object) else {
            return PhpString::new(req, b"");
        };
        if let Some(size) = size {
            let src = bounded(req, object, size, "attempt to read over data boundary")?;
            let bytes = ffi::bytes(src, size as usize);
            return PhpString::new(req, &bytes);
        }
        let v = view(req, object);
        let addr = {
            let types = types(req);
            let t = types.get(v.ty);
            match (t.pointee(), t.element()) {
                (Some(to), _) if matches!(types.get(to).kind, Kind::Char) => {
                    Some(ffi::read::<usize>(v.ptr))
                }
                (None, Some((of, _))) if matches!(types.get(of).kind, Kind::Char) => Some(v.ptr),
                _ => None,
            }
        };
        let Some(addr) = addr else {
            return Err(ffi_error(req, "FFI\\Cdata is not a C string"));
        };
        let bytes = ffi::c_string(addr);
        PhpString::new(req, &bytes)
    }

    /// `FFI::isNull(FFI\CData &$ptr): bool`.
    ///
    /// ```php
    /// var_dump(FFI::isNull(FFI::cdef()->new("void*")));
    /// try { FFI::isNull(FFI::cdef()->new("int")); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// FFI\Cdata is not a pointer
    /// ```
    fn isNull(
        req: &mut Request,
        #[php(prefer_ref)] ptr: Ref<Instance<CData>>,
    ) -> Result<bool, Thrown> {
        validate_api_restriction(req)?;
        let Some(object) = ptr.get().map(Instance::object) else {
            return Ok(false);
        };
        let v = view(req, object);
        if types(req).get(v.ty).pointee().is_none() {
            return Err(ffi_error(req, "FFI\\Cdata is not a pointer"));
        }
        Ok(ffi::read::<usize>(v.ptr) == 0)
    }
}

/// The non-static `new`, `cast` and `type` an `FFI` object's
/// `get_method` answers, `zend_ffi_new_fn` and its kin: the static
/// methods' bodies with the object's declarations in scope.
pub(crate) mod instance {
    use php_ext::abi::{Instance, Ref, This, Union, Untyped};
    use php_ext::attr::php_class;
    use php_ext::raising::Thrown;
    use php_ext::request::Request;
    use php_ext::values::{Owner, PhpString, Value};

    use crate::cdata::CData;
    use crate::ctype::CType;

    /// `FFI`, whose instance methods these are.
    #[php_class(extern)]
    #[allow(clippy::upper_case_acronyms)]
    pub(crate) struct FFI;

    #[php_class(trampolines)]
    impl FFI {
        /// `$ffi->new(...)`: a value of a type the object declares.
        ///
        /// ```php
        /// $ffi = FFI::cdef("typedef struct { int x; } pt;");
        /// $p = $ffi->new("pt"); $p->x = 2;
        /// var_dump($p->x);
        /// ```
        /// ```expect
        /// int(2)
        /// ```
        #[expect(
            clippy::new_ret_no_self,
            reason = "php's FFI::new, a value of a C type"
        )]
        fn new(
            req: &mut Request,
            this: This<Self>,
            r#type: Union<Instance<CType>, PhpString>,
            #[php(default = true)] owned: bool,
            #[php(default = false)] persistent: bool,
        ) -> Result<Owner<Instance<CData>>, Thrown> {
            super::ffi_new(req, Some(this.object()), r#type, owned, persistent)
        }

        /// `$ffi->cast(...)`.
        ///
        /// ```php
        /// $ffi = FFI::cdef("typedef unsigned char byte;");
        /// $i = $ffi->new("int"); $i->cdata = 0x1234;
        /// var_dump($ffi->cast("byte", $i)->cdata);
        /// ```
        /// ```expect
        /// int(52)
        /// ```
        fn cast(
            req: &mut Request,
            this: This<Self>,
            r#type: Union<Instance<CType>, PhpString>,
            #[php(prefer_ref)] ptr: Ref<Untyped<Value>>,
        ) -> Result<Owner<Instance<CData>>, Thrown> {
            let value = ptr.get().map_or(Value::null(), |held| held.0);
            super::ffi_cast(req, Some(this.object()), r#type, value)
        }

        /// `$ffi->type(...)`.
        ///
        /// ```php
        /// $ffi = FFI::cdef("typedef long word;");
        /// var_dump($ffi->type("word*")->getName());
        /// ```
        /// ```expect
        /// string(8) "int64_t*"
        /// ```
        fn r#type(
            req: &mut Request,
            this: This<Self>,
            r#type: PhpString,
        ) -> Result<Owner<Instance<CType>>, Thrown> {
            super::ffi_type(req, Some(this.object()), r#type)
        }
    }
}

/// `zend_ffi_parse_decl` into `scope`, the message of php's error.
pub(crate) fn parse_decl(
    req: &mut Request,
    code: &[u8],
    scope: &mut Scope,
    default_attr: u32,
) -> Result<(), String> {
    let state = req.state::<State>();
    let mut parser = Parser::new(code, &mut state.types, scope, default_attr);
    parser.parse_decl().map_err(|bail| bail.0)
}

/// `zend_ffi_parse_type` with the object's declarations in scope, or
/// none for a static call: the type and whether it is `const`.
fn parse_type(
    req: &mut Request,
    scope_of: Option<PhpObject>,
    text: &[u8],
) -> Result<(TypeId, bool), Thrown> {
    let mut scope = match scope_of {
        Some(object) => object
            .native_mut::<Decls>(req)
            .map(|decls| std::mem::take(&mut *decls.scope))
            .unwrap_or_default(),
        None => Scope::default(),
    };
    // `clean_symbols` and `clean_tags`: what a parse declares into a
    // table the object did not have yet goes with the parse.
    let (had_symbols, had_tags) = (scope.symbols.is_some(), scope.tags.is_some());
    let mut dcl = Dcl::default();
    let parsed = {
        let state = req.state::<State>();
        let mut parser = Parser::new(text, &mut state.types, &mut scope, 0);
        parser.parse_type(&mut dcl)
    };
    if !had_symbols {
        scope.symbols = None;
    }
    if !had_tags {
        scope.tags = None;
    }
    if let Some(object) = scope_of
        && let Some(decls) = object.native_mut::<Decls>(req)
    {
        *decls.scope = scope;
    }
    if let Err(bail) = parsed {
        return Err(parser_error(req, &bail.0));
    }
    let ty = dcl.ty.unwrap_or(TypeId::VOID);
    Ok((ty, dcl.attr & attr::CONST != 0))
}

/// Each variable and function of `scope` resolved in `lib`, or in the
/// process, `DL_FETCH_SYMBOL`, or php's error naming the first missing.
fn resolve(req: &mut Request, scope: &mut Scope, lib: Option<&Library>) -> Result<(), Thrown> {
    crate::preload::resolve_symbols(scope, lib, None).map_err(|fail| {
        let message = match fail {
            Fail::Symbol(what, name) => {
                format!(
                    "Failed resolving C {what} '{}'",
                    String::from_utf8_lossy(&name)
                )
            }
            _ => String::from("Failed resolving C symbols"),
        };
        ffi_error(req, &message)
    })
}

/// What php `emalloc`s for the declarations parsed into `scope`, the
/// types made from the `from`th on and the tables
/// ([`Scope::emallocs`]), held by the `FFI` object made of them.
fn hold_decls(req: &mut Request, scope: &Scope, from: usize) -> Result<Box<[usize]>, Thrown> {
    let mut sizes = types(req).emallocs(from);
    sizes.extend(scope.emallocs());
    hold(req, &sizes)
}

/// An `FFI` object holding `decls`.
fn made(req: &mut Request, decls: Decls) -> Result<Owner<Instance<FFI>>, Thrown> {
    let class = classes(req)?.ffi;
    let object = new_object(req, class)?;
    if let Some(slot) = object.native_mut::<Decls>(req) {
        *slot = decls;
    }
    Ok(object.into_instance())
}

/// A `FFI\CData` holding `data`, returned.
fn made_cdata(req: &mut Request, data: Data) -> Result<Owner<Instance<CData>>, Thrown> {
    make(req, data).map(Owner::into_instance)
}

/// `zend_ffi_load(filename, 0)`: a header's declarations, each failure
/// thrown as php throws it. php reads a regular file into `code`,
/// `emalloc(code_size + 1)`, and frees it at its `cleanup`, so every throw
/// past the file's checks stands over it; the block here is that buffer,
/// held across [`load_read`].
///
/// ```php
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, "\n"; }
/// $h = sprintf('/tmp/ffi-%07d.h', getmypid()); file_put_contents($h, "int x(;\n");
/// pk(function () use ($h) { try { FFI::load($h); } catch (FFI\Exception $e) {} });
/// try { FFI::load($h); } catch (FFI\Exception $e) { echo $e->getMessage(), " / ", $e->getPrevious()->getMessage(), "\n"; }
/// unlink($h);
/// ```
/// ```expect
/// 4560
/// Failed loading '/tmp/ffi-%d.h' / ')' expected, got ';' at line 1
/// ```
fn load(req: &mut Request, filename: &[u8]) -> Result<Owner<Instance<FFI>>, Thrown> {
    let shown = String::from_utf8_lossy(filename).into_owned();
    let file = crate::preload::header_file(req, filename);
    let code = match std::fs::metadata(&file) {
        Ok(meta) if meta.is_file() => {
            let len = usize::try_from(meta.len()).unwrap_or(usize::MAX);
            Some(Emalloc::new(req, len.saturating_add(1))?)
        }
        _ => None,
    };
    let loaded = load_read(req, &file, &shown);
    if let Some(code) = code {
        code.free(req);
    }
    loaded
}

/// [`load`] once its `code` is held.
fn load_read(
    req: &mut Request,
    file: &std::path::Path,
    shown: &str,
) -> Result<Owner<Instance<FFI>>, Thrown> {
    let from = types(req).len();
    let cwd = req.environment().cwd();
    let loaded = match crate::preload::load_header(file, Some(&cwd), types(req), attr::STORED, None)
    {
        Ok(loaded) => loaded,
        Err(fail) => {
            let message = match fail {
                Fail::Missing => format!("Failed loading '{shown}', file doesn't exist"),
                Fail::NotFile => format!("Failed loading '{shown}', not a regular file"),
                Fail::Unreadable => format!("Failed loading '{shown}', cannot read_file"),
                Fail::BadDefine(what) => format!("Failed loading '{shown}', bad {what} define"),
                Fail::Twice(what) => format!("Failed loading '{shown}', {what} defined twice"),
                // `zend_throw_error(zend_ffi_exception_ce, "Failed loading
                // '%s'", ...)` over the parser's pending throw, its
                // `zend_vspprintf` buffer standing across the throw.
                Fail::Parse(message) => {
                    let ce = crate::throwables::Throwables::of(req)?;
                    let thrown = parser_error(req, &message);
                    return Err(throw_error_chained(
                        req,
                        thrown,
                        ce.exception,
                        &format!("Failed loading '{shown}'"),
                    ));
                }
                Fail::Lib(lib, error) => format!(
                    "Failed loading '{}' ({error})",
                    String::from_utf8_lossy(&lib)
                ),
                Fail::Symbol(what, name) => format!(
                    "Failed resolving C {what} '{}'",
                    String::from_utf8_lossy(&name)
                ),
                Fail::Redefined(what) => format!("redefinition of '{what}'"),
            };
            return Err(ffi_error(req, &message));
        }
    };
    let held = hold_decls(req, &loaded.scope, from)?;
    made(
        req,
        Decls {
            lib: loaded.lib,
            scope: Box::new(loaded.scope),
            held,
        },
    )
}

/// `is_static_call`'s deprecation: raised, and a throwable an error
/// handler made of it handed on.
fn static_call(req: &mut Request, scope_of: Option<PhpObject>, method: &str) -> Result<(), Thrown> {
    if scope_of.is_none() {
        deprecated(
            req,
            &format!("Calling FFI::{method}() statically is deprecated"),
        );
        pending(req)?;
    }
    Ok(())
}

/// A type argument, `FFI\CType|string`: the parsed string's type or the
/// object's, and whether it is `const`.
fn type_argument(
    req: &mut Request,
    scope_of: Option<PhpObject>,
    ty: Union<Instance<CType>, PhpString>,
) -> Result<(TypeId, bool), Thrown> {
    match ty {
        Union::A(ctype) => Ok((ctype_of(req, ctype.object()), false)),
        Union::B(text) => parse_type(req, scope_of, text.as_bytes()),
    }
}

/// `ZEND_METHOD(FFI, new)`.
fn ffi_new(
    req: &mut Request,
    scope_of: Option<PhpObject>,
    ty: Union<Instance<CType>, PhpString>,
    owned: bool,
    persistent: bool,
) -> Result<Owner<Instance<CData>>, Thrown> {
    validate_api_restriction(req)?;
    static_call(req, scope_of, "new")?;
    let from = types(req).len();
    let ctype = match &ty {
        Union::A(ctype) => Some(ctype.object()),
        Union::B(_) => None,
    };
    let (ty, is_const) = type_argument(req, scope_of, ty)?;
    let mut held = hold_types(req, from)?;
    if let Some(object) = ctype {
        held = take_ctype_types(req, object, held)?;
    }
    let size = types(req).get(ty).size;
    if size == 0 {
        free_types(req, &held);
        return Err(ffi_error(req, "Cannot instantiate FFI\\CData of zero size"));
    }
    let addr = alloc(req, size, persistent)?;
    let place = if owned {
        Place::Owned(addr)
    } else {
        Place::Borrowed(addr)
    };
    made_cdata(
        req,
        Data {
            ty,
            place,
            is_const,
            persistent,
            held,
        },
    )
}

/// `FFI::new` over a `FFI\\CType` that owns its type: the new value
/// takes the type where the object is its one owner's, `transfer type
/// ownership`; otherwise `zend_ffi_remember_type` keeps it until the
/// request ends. Either way the object owns it no longer. `held` is
/// what the value holds otherwise.
///
/// ```php
/// $ffi = FFI::cdef();
/// $m = memory_get_usage(); $x = $ffi->new($ffi->type("int[2]")); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); unset($x); echo memory_get_usage() - $m, "\n";
/// $t = $ffi->type("int[3]");
/// $m = memory_get_usage(); $x = $ffi->new($t); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $y = $ffi->new($t); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); unset($x, $y, $t); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 208
/// -208
/// 328
/// 112
/// -288
/// ```
fn take_ctype_types(
    req: &mut Request,
    object: PhpObject,
    held: Box<[usize]>,
) -> Result<Box<[usize]>, Thrown> {
    let sole = object.as_value().refcount() == 1;
    let Some(slot) = object.native_mut::<TypeRef>(req) else {
        return Ok(held);
    };
    if slot.held.is_empty() {
        return Ok(held);
    }
    let owned = std::mem::take(&mut slot.held);
    if sole {
        let mut all = held.into_vec();
        all.extend(owned);
        return Ok(all.into_boxed_slice());
    }
    remember(req, owned)?;
    Ok(held)
}

/// `ZEND_METHOD(FFI, cast)`.
fn ffi_cast(
    req: &mut Request,
    scope_of: Option<PhpObject>,
    ty: Union<Instance<CType>, PhpString>,
    given: Value,
) -> Result<Owner<Instance<CData>>, Thrown> {
    validate_api_restriction(req)?;
    static_call(req, scope_of, "cast")?;
    let from = types(req).len();
    let (ty, is_const) = type_argument(req, scope_of, ty)?;
    let value = given.deref();
    let Some(old_object) = as_cdata(req, &value)? else {
        let (scalar, pointer, size) = {
            let t = types(req).get(ty);
            (t.is_scalar(), t.pointee().is_some(), t.size)
        };
        let simple = matches!(
            value.as_ref(),
            ValueRef::Null | ValueRef::Bool(_) | ValueRef::Int(_) | ValueRef::Float(_)
        );
        if scalar && simple {
            let addr = alloc(req, size, false)?;
            write_value(req, addr, ty, &value)?;
            let held = hold_types(req, from)?;
            return made_cdata(
                req,
                Data {
                    ty,
                    place: Place::Owned(addr),
                    is_const,
                    persistent: false,
                    held,
                },
            );
        }
        if pointer && let ValueRef::Int(n) = value.as_ref() {
            return made_cdata(req, Data::holding(ty, n as usize, is_const));
        }
        if pointer && matches!(value.as_ref(), ValueRef::Null) {
            return made_cdata(req, Data::holding(ty, 0, is_const));
        }
        let given = value_name(req, &value);
        return Err(arg_type_error(
            req,
            2,
            &[b"be of type FFI\\CData, ".as_slice(), &given, b" given"].concat(),
        ));
    };
    let old = view(req, old_object);
    let place = {
        let types = types(req);
        let (t, o) = (types.get(ty), types.get(old.ty));
        match (o.pointee(), t.pointee(), o.element(), t.element()) {
            (Some(op), None, _, _) if matches!(types.get(op).kind, Kind::Void) => {
                Place::Borrowed(ffi::read::<usize>(old.ptr))
            }
            (None, Some(tp), Some((oe, _)), _) if types.is_compatible(oe, tp) => {
                Place::holder(old.ptr)
            }
            (Some(op), None, _, Some((te, _))) if types.is_compatible(op, te) => {
                Place::Borrowed(ffi::read::<usize>(old.ptr))
            }
            _ if t.size > o.size => {
                return Err(ffi_error(req, "attempt to cast to larger type"));
            }
            _ if !old.holder => Place::Borrowed(old.ptr),
            _ => Place::holder(ffi::read::<usize>(old.ptr)),
        }
    };
    let mut data = Data {
        ty,
        place,
        is_const,
        persistent: false,
        held: hold_types(req, from)?,
    };
    if old.owned && temporary(req, 2, old_object) {
        transfer(req, old_object, &mut data);
    }
    made_cdata(req, data)
}

/// The owner moved from `old` to the new value, as `FFI::cast` moves
/// php's owned bit from a temporary to its cast.
fn transfer(req: &mut Request, old: PhpObject, data: &mut Data) {
    let Some(old) = old.native_mut::<Data>(req) else {
        return;
    };
    match std::mem::take(&mut old.place) {
        Place::Owned(addr) => old.place = Place::Borrowed(addr),
        Place::Holder { cell, .. } => old.place = Place::Holder { cell, target: None },
        other => old.place = other,
    }
    match std::mem::take(&mut data.place) {
        Place::Borrowed(addr) if addr != 0 => data.place = Place::Owned(addr),
        Place::Holder { cell, .. } => {
            let target = std::num::NonZeroUsize::new(ffi::read::<usize>(cell.addr()));
            data.place = Place::Holder { cell, target };
        }
        other => data.place = other,
    }
}

/// `ZEND_METHOD(FFI, type)`.
fn ffi_type(
    req: &mut Request,
    scope_of: Option<PhpObject>,
    text: PhpString,
) -> Result<Owner<Instance<CType>>, Thrown> {
    validate_api_restriction(req)?;
    static_call(req, scope_of, "type")?;
    let from = types(req).len();
    let (ty, _) = parse_type(req, scope_of, text.as_bytes())?;
    let held = hold_types(req, from)?;
    make_owning(req, ty, held)
}

/// `GC_REFCOUNT(obj) == 1 && Z_REFCOUNT_P(arg) == 1`: the argument at
/// `position` is the value's one owner, a temporary php may take the
/// ownership of.
fn temporary(req: &mut Request, position: u32, object: PhpObject) -> bool {
    let Some(arg) = req.frames().argument(position) else {
        return false;
    };
    arg.refcount() == 1 && object.as_value().refcount() == 1
}

/// A `FFI\CData`'s or a `FFI\CType`'s type, or php's
/// `zend_wrong_parameter_class_error`.
fn data_or_type(req: &mut Request, value: Option<Value>) -> Result<TypeId, Thrown> {
    let value = value.unwrap_or(Value::null()).deref();
    if let Some(object) = as_cdata(req, &value)? {
        return Ok(view(req, object).ty);
    }
    let class = classes(req)?.ctype;
    if let Some(object) = value.as_object().filter(|object| object.class() == class) {
        return Ok(ctype_of(req, object));
    }
    let given = value_name(req, &value);
    Err(arg_type_error(
        req,
        1,
        &[
            b"be of type FFI\\CData or FFI\\CType, ".as_slice(),
            &given,
            b" given",
        ]
        .concat(),
    ))
}

/// The address `FFI::memcpy`'s, `FFI::memset`'s and `FFI::string`'s
/// value is read or written at: what a pointer points at, or the value
/// itself held to its size.
fn bounded(
    req: &mut Request,
    object: PhpObject,
    size: i64,
    refusal: &str,
) -> Result<usize, Thrown> {
    let v = view(req, object);
    let t = types(req).get(v.ty);
    if t.pointee().is_some() {
        return Ok(ffi::read::<usize>(v.ptr));
    }
    if size as u64 > t.size as u64 {
        return Err(ffi_error(req, refusal));
    }
    Ok(v.ptr)
}

/// A memory argument that is a string or a `FFI\CData`: its address,
/// held to `size`; the string's bytes kept in `text` while it stands.
fn source(
    req: &mut Request,
    value: &Value,
    size: i64,
    position: u32,
    text: &mut Vec<u8>,
    refusals: [&str; 2],
) -> Result<usize, Thrown> {
    if let ValueRef::Str(s) = value.as_ref() {
        if size as u64 > s.as_bytes().len() as u64 {
            return Err(ffi_error(req, refusals[0]));
        }
        *text = s.as_bytes().to_vec();
        return Ok(text.as_ptr() as usize);
    }
    if let Some(object) = as_cdata(req, value)? {
        return bounded(req, object, size, refusals[1]);
    }
    let given = value_name(req, value);
    Err(arg_type_error(
        req,
        position,
        &[
            b"be of type FFI\\CData or string, ".as_slice(),
            &given,
            b" given",
        ]
        .concat(),
    ))
}

/// The C function an `FFI` object declares under `name`, and its
/// address.
pub(crate) fn function_symbol(
    req: &mut Request,
    object: PhpObject,
    name: &[u8],
) -> Option<(TypeId, usize)> {
    let decls = object.native::<Decls>(req)?;
    let sym = decls.scope.symbols.as_ref()?.get(name)?;
    (sym.kind == SymbolKind::Func).then_some((sym.ty, sym.addr))
}

/// The handlers of `FFI`.
pub(crate) static HANDLERS: Handlers = Handlers;

/// The handlers of `FFI`.
pub(crate) struct Handlers;

/// `zend_bad_array_access` over `FFI`.
fn bad_array_access(req: &mut Request) -> Thrown {
    let ce = match crate::throwables::Throwables::of(req) {
        Ok(ce) => ce,
        Err(fatal) => return fatal,
    };
    throw_error(req, ce.error, "Cannot use object of type FFI as array")
}

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    /// `zend_fake_get_constructor`.
    fn get_constructor(&self, req: &mut Request, _: PhpObject) -> Result<Option<Function>, Thrown> {
        let ce = crate::throwables::Throwables::of(req)?;
        Err(throw_error(
            req,
            ce.error,
            "Instantiation of FFI is not allowed",
        ))
    }

    /// `zend_ffi_free_obj`: the declarations' tables, their rows and
    /// their types given back.
    ///
    /// ```php
    /// $m = memory_get_usage(); $f = FFI::cdef("int abs(int);"); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); unset($f); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $f = FFI::cdef("typedef struct { int a; long b; } pt; struct tag { int x; }; int abs(int); long labs(long); enum e { A, B = 3 };"); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); unset($f); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 840
    /// -840
    /// 2944
    /// -2944
    /// ```
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let Some(decls) = object.native_mut::<Decls>(req) else {
            return;
        };
        let held = std::mem::take(&mut decls.held);
        free_types(req, &held);
    }

    /// `zend_ffi_read_var`: a C variable's value, a function as a
    /// pointer to it, and an enumerator's integer.
    ///
    /// ```php
    /// $ffi = FFI::cdef("enum { RED = 3, GREEN }; int abs(int);");
    /// var_dump($ffi->GREEN, FFI::typeof($ffi->abs)->getName());
    /// try { $ffi->nope; } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(4)
    /// string(12) "int32_t(*)()"
    /// Attempt to read undefined C variable 'nope'
    /// ```
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        let sym = object
            .native::<Decls>(req)
            .and_then(|decls| decls.scope.symbols.as_ref()?.get(name).cloned())
            .filter(|sym| sym.kind != SymbolKind::Type);
        let Some(sym) = sym else {
            let shown = String::from_utf8_lossy(name).into_owned();
            return Err(ffi_error(
                req,
                &format!("Attempt to read undefined C variable '{shown}'"),
            ));
        };
        match sym.kind {
            SymbolKind::Var => {
                let fetch = if read == PropertyRead::Warning {
                    Fetch::Read
                } else {
                    Fetch::Place
                };
                to_value(
                    req,
                    None,
                    sym.addr,
                    sym.ty,
                    fetch,
                    sym.is_const,
                    false,
                    false,
                )
            }
            SymbolKind::Func => {
                let ty = types(req).pointer_to(sym.ty, 0);
                let made = make(req, Data::holding(ty, sym.addr, true))?;
                Ok(made.into_value())
            }
            _ => Ok(Owner::int(sym.value)),
        }
    }

    /// `zend_ffi_write_var`.
    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let sym = object
            .native::<Decls>(req)
            .and_then(|decls| decls.scope.symbols.as_ref()?.get(name.as_bytes()).cloned())
            .filter(|sym| sym.kind == SymbolKind::Var);
        let shown = String::from_utf8_lossy(name.as_bytes()).into_owned();
        let written = match sym {
            None => Err(ffi_error(
                req,
                &format!("Attempt to assign undefined C variable '{shown}'"),
            )),
            Some(sym) if sym.is_const => Err(ffi_error(
                req,
                &format!("Attempt to assign read-only C variable '{shown}'"),
            )),
            Some(sym) => write_value(req, sym.addr, sym.ty, &value),
        };
        value.give_up(req);
        name.give_up(req);
        written
    }

    fn read_dimension(
        &self,
        req: &mut Request,
        _: PhpObject,
        _: Option<&Value>,
        _: php_ext::abi::DimensionRead,
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

    fn get_method(
        &self,
        req: &mut Request,
        object: PhpObject,
        lowered: &[u8],
    ) -> Result<Option<Function>, Thrown> {
        self.get_method_as(req, object, lowered, lowered)
    }

    /// `zend_ffi_get_func`: `new`, `cast` and `type` by any case, then
    /// the C function of the name as written.
    fn get_method_as(
        &self,
        req: &mut Request,
        object: PhpObject,
        lowered: &[u8],
        written: &[u8],
    ) -> Result<Option<Function>, Thrown> {
        if matches!(lowered, b"new" | b"cast" | b"type") {
            let class = classes(req)?.ffi;
            return Ok(req.symbols().trampoline(class, lowered));
        }
        if function_symbol(req, object, written).is_none() {
            let shown = String::from_utf8_lossy(written).into_owned();
            return Err(ffi_error(
                req,
                &format!("Attempt to call undefined C function '{shown}'"),
            ));
        }
        Ok(req
            .symbols_mut()
            .native_trampoline(written, crate::call::trampoline))
    }

    /// `zend_fake_compare_objects`.
    fn compare(
        &self,
        req: &mut Request,
        _: &Value,
        _: &Value,
    ) -> Result<Option<std::cmp::Ordering>, Thrown> {
        Err(ffi_error(req, "Cannot compare FFI objects"))
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

    /// `get_debug_info = NULL` over `zend_fake_get_properties`: nothing.
    fn get_debug_info(
        &self,
        _: &mut Request,
        _: PhpObject,
    ) -> Result<Option<php_ext::abi::DebugInfo>, Thrown> {
        Ok(None)
    }

    /// `zend_fake_get_gc`.
    fn get_gc(&self, _: &mut Request, _: PhpObject, _: &mut dyn FnMut(Value)) -> GcTable {
        GcTable::Properties
    }

    /// `get_closure = NULL`.
    fn get_closure(&self, _: &mut Request, _: PhpObject) -> Option<php_ext::values::ClosureInfo> {
        None
    }
}

/// `ty` dimensioned by `dims`, the last one innermost.
fn dimensioned(req: &mut Request, mut ty: TypeId, dims: &[Owner<Value>]) -> Result<TypeId, Thrown> {
    for value in dims.iter().rev() {
        let value = value.deref();
        let n = to_long(req, &value)?;
        if n < 0 {
            return Err(ffi_error(req, "negative array index"));
        }
        let (size, align, undimensioned) = {
            let t = types(req).get(ty);
            (
                t.size,
                t.align,
                matches!(t.kind, Kind::Array { .. }) && t.attr & attr::INCOMPLETE_ARRAY != 0,
            )
        };
        if undimensioned {
            return Err(ffi_error(
                req,
                "only the leftmost array can be undimensioned",
            ));
        }
        ty = types(req).add(Type {
            kind: Kind::Array { of: ty, len: n },
            size: (n as usize).wrapping_mul(size),
            align,
            attr: if n == 0 { attr::INCOMPLETE_ARRAY } else { 0 },
        });
    }
    Ok(ty)
}
