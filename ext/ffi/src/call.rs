//! The call into C and back: `zend_ffi_trampoline`, which serves every
//! C function an `FFI` object declares and every function pointer a
//! `FFI\CData` holds, each argument passed as `zend_ffi_pass_arg` and
//! `zend_ffi_pass_var_arg` pass it; and the callbacks
//! `zend_ffi_create_callback` makes of a PHP callable a C function is
//! handed, which C calls back through `zend_ffi_callback_trampoline`.

use php_ext::abi::{Beneath, Callable, chain};
use php_ext::coerce::{callable_checked, to_double, to_long, truthy, value_name, with_string};
use php_ext::raising::{Thrown, fatal, while_pending};
use php_ext::request::Request;
use php_ext::values::{Owner, Value, ValueRef};

use crate::cdata::{Fetch, as_cdata, func_pointee, to_value, type_name, view, write_value};
use crate::decls::function_symbol;
use crate::ffi::{self, ARG_SIZE, Closure, ClosureError, Layout, Signature};
use crate::types::{Int, Kind, Scalar, TypeId, attr};
use crate::{State, ffi_error, types};

/// A callback C may call: libffi's closure over the PHP callable, kept
/// until the request ends as php keeps `FFI_G(callbacks)`.
///
/// It owns the callable whole, as php-8.5.12's `zend_ffi_create_callback`
/// holds its `zend_fcall_info_cache` with `zend_fcc_addref` and gives it
/// up with `zend_fcc_dtor` (`Z/ext/ffi/ffi.c:1104`, `:923`): a closure's
/// count, and the receiver of an `[$obj, 'm']` callback, so `$obj`
/// stands until the request's end however its PHP owners let go.
pub(crate) struct Callback {
    closure: Closure,
    callable: Owner<Callable>,
    ty: TypeId,
}

/// `zend_ffi_trampoline` for a method of an `FFI` object: the C
/// function its declarations name under the method's name.
///
/// ```php
/// $ffi = FFI::cdef("size_t strlen(const char *s); int abs(int j);");
/// var_dump($ffi->strlen("hello"), $ffi->abs(-7));
/// try { $ffi->abs(); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
/// try { $ffi->nope(); } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(5)
/// int(7)
/// Incorrect number of arguments for C function 'abs', expecting exactly 1 parameter
/// Attempt to call undefined C function 'nope'
/// ```
pub(crate) fn trampoline(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    let name = req.frames().active_function_name();
    let Some(this) = req.frames().this() else {
        return Ok(Owner::null());
    };
    let Some((ty, addr)) = function_symbol(req, this, name.as_bytes()) else {
        let message = format!("Attempt to call undefined C function '{name}'");
        return Err(ffi_error(req, &message));
    };
    call(req, &name, ty, addr)
}

/// `zend_ffi_trampoline` for a `FFI\CData` called as a function, which
/// `zend_ffi_cdata_get_closure` names `__invoke`.
///
/// ```php
/// $ffi = FFI::cdef("int abs(int j);");
/// $f = $ffi->abs;
/// var_dump($f(-3));
/// ```
/// ```expect
/// int(3)
/// ```
///
/// The call holds no receiver: the `FFI\CData` a struct's field read
/// for it is given up before the arguments are sent, so the callback's
/// argument and the struct returned take its number, and the frame
/// names no class (gh16013.phpt):
///
/// ```php
/// $ffi = FFI::cdef("typedef struct { int field; } S; typedef struct { S (*f)(S); } CB;");
/// $cb = $ffi->new("CB");
/// $cb->f = function ($v) { debug_print_backtrace(); return $v; };
/// $s = $ffi->new("S"); $s->field = 10;
/// var_dump(($cb->f)($s));
/// ```
/// ```expect
/// #0 %s(6): {closure:%s:4}(Object(FFI\CData:struct <anonymous>))
/// #1 %s(6): __invoke(Object(FFI\CData:struct <anonymous>))
/// object(FFI\CData:struct <anonymous>)#5 (1) {
///   ["field"]=>
///   int(10)
/// }
/// ```
pub(crate) fn invoke(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    let Some(CFunction { func, addr }) = req.frames().reserved::<CFunction>() else {
        return Err(fatal(
            req,
            "InternalError: a C function pointer's trampoline carries no function",
        ));
    };
    call(req, "__invoke", func, addr)
}

/// What `zend_ffi_cdata_get_closure` keeps in the trampoline's
/// `reserved[]` (`Z/ext/ffi/ffi.c:2186-2187`): the function type the
/// pointer points to and the address it held when the call was made.
/// The call holds no receiver, so the `FFI\CData` read for it is given
/// up as php gives it up, before the arguments are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CFunction {
    pub(crate) func: TypeId,
    pub(crate) addr: usize,
}

/// The function's parameters, its return, its calling convention and
/// whether it takes more after them.
struct Shape {
    args: Vec<TypeId>,
    ret: TypeId,
    abi: i32,
    variadic: bool,
}

fn shape(req: &mut Request, func: TypeId) -> Option<Shape> {
    let t = types(req).get(func);
    let f = t.func()?;
    Some(Shape {
        args: f.args.clone().unwrap_or_default(),
        ret: f.ret,
        abi: f.abi,
        variadic: t.attr & attr::VARIADIC != 0,
    })
}

/// What a call's arguments are laid out in: each value's room, the
/// strings it passes, and each argument's address and libffi type.
struct Frame {
    slots: Vec<[u64; 2]>,
    strings: Vec<Vec<u8>>,
    addrs: Vec<usize>,
    layouts: Vec<Layout>,
    /// A throwable an argument raised that php leaves pending while the
    /// call still goes ahead, `zend_ffi_pass_incompatible` over a `char`.
    pending: Option<Thrown>,
}

impl Frame {
    fn slot(&self, n: usize) -> usize {
        self.slots.as_ptr() as usize + n * ARG_SIZE
    }

    /// A string's bytes and a NUL, standing until the call returns, as
    /// php passes `Z_STRVAL`.
    fn string(&mut self, bytes: &[u8]) -> usize {
        let mut owned = bytes.to_vec();
        owned.push(0);
        let addr = owned.as_ptr() as usize;
        self.strings.push(owned);
        addr
    }
}

/// `zend_ffi_trampoline`: the arity held to the declaration, each
/// argument passed, the call, and the return read back.
fn call(req: &mut Request, name: &str, func: TypeId, addr: usize) -> Result<Owner<Value>, Thrown> {
    let Some(shape) = shape(req, func) else {
        return Err(fatal(
            req,
            "InternalError: a C function of no function type",
        ));
    };
    let given = req.frames().argument_count();
    let count = shape.args.len();
    let plural = if count == 1 { "" } else { "s" };
    if shape.variadic && count > given {
        let message = format!(
            "Incorrect number of arguments for C function '{name}', expecting at least {count} parameter{plural}"
        );
        return Err(ffi_error(req, &message));
    }
    if !shape.variadic && count != given {
        let message = format!(
            "Incorrect number of arguments for C function '{name}', expecting exactly {count} parameter{plural}"
        );
        return Err(ffi_error(req, &message));
    }
    let mut frame = Frame {
        slots: vec![[0; 2]; given],
        strings: Vec::new(),
        addrs: Vec::with_capacity(given),
        layouts: Vec::with_capacity(given),
        pending: None,
    };
    for n in 0..given {
        let slot = frame.slot(n);
        frame.addrs.push(slot);
        let position = u32::try_from(n + 1).unwrap_or(u32::MAX);
        let arg = req
            .frames()
            .argument(position)
            .unwrap_or(Value::null())
            .deref();
        match shape.args.get(n) {
            Some(ty) => pass_arg(req, name, n, &arg, *ty, &mut frame)?,
            None => pass_var_arg(req, name, n, &arg, &mut frame)?,
        }
    }
    let Some(ret) = types(req).layout(shape.ret) else {
        return Err(return_unsupported(req, shape.ret));
    };
    let fixed = shape.variadic.then_some(count);
    let Some(mut sig) = Signature::prepare(shape.abi, &ret, &frame.layouts, fixed) else {
        return Err(ffi_error(req, "Cannot prepare callback CIF"));
    };
    let ret_size = types(req).get(shape.ret).size;
    let mut buffer = vec![0u64; ret_size.max(8).div_ceil(8)];
    ffi::call(req, &mut sig, addr, &frame.addrs, &mut buffer);
    drop(frame.strings);
    if let Some(thrown) = frame.pending {
        return Err(thrown);
    }
    if matches!(types(req).get(shape.ret).kind, Kind::Void) {
        return Ok(Owner::null());
    }
    to_value(
        req,
        None,
        buffer.as_ptr() as usize,
        shape.ret,
        Fetch::Read,
        false,
        true,
        false,
    )
}

/// `zend_ffi_return_unsupported`.
fn return_unsupported(req: &mut Request, ty: TypeId) -> Thrown {
    let message = match types(req).get(ty).kind {
        Kind::Struct(_) => "FFI return struct/union is not implemented",
        Kind::Array { .. } => "FFI return array is not implemented",
        _ => "FFI internal error. Unsupported return type",
    };
    ffi_error(req, message)
}

/// `zend_ffi_pass_unsupported`.
fn pass_unsupported(req: &mut Request, ty: TypeId) -> Thrown {
    let message = match types(req).get(ty).kind {
        Kind::Struct(_) => "FFI passing struct/union is not implemented",
        Kind::Array { .. } => "FFI passing array is not implemented",
        _ => "FFI internal error. Unsupported parameter type",
    };
    ffi_error(req, message)
}

/// `zend_ffi_pass_incompatible`.
fn pass_incompatible(
    req: &mut Request,
    name: &str,
    n: usize,
    arg: &Value,
    ty: TypeId,
) -> Result<Thrown, Thrown> {
    let at = n + 1;
    let Some(expecting) = type_name(req, ty) else {
        let message = format!("Passing incompatible argument {at} of C function '{name}'");
        return Ok(ffi_error(req, &message));
    };
    let message = if let Some(object) = as_cdata(req, arg)? {
        let found = view(req, object).ty;
        match type_name(req, found) {
            Some(found) => format!(
                "Passing incompatible argument {at} of C function '{name}', expecting '{expecting}', found '{found}'"
            ),
            None => format!(
                "Passing incompatible argument {at} of C function '{name}', expecting '{expecting}'"
            ),
        }
        .into_bytes()
    } else {
        let given = value_name(req, arg);
        let head = format!(
            "Passing incompatible argument {at} of C function '{name}', expecting '{expecting}', found PHP '"
        );
        [head.as_bytes(), &given, b"'"].concat()
    };
    Ok(ffi_error(req, &message))
}

/// `zend_ffi_pass_arg`: one argument of a declared parameter.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per kind of C type, as zend_ffi_pass_arg's switch is"
)]
fn pass_arg(
    req: &mut Request,
    name: &str,
    n: usize,
    arg: &Value,
    ty: TypeId,
    frame: &mut Frame,
) -> Result<(), Thrown> {
    let slot = frame.slot(n);
    let (scalar, pointee, is_struct) = {
        let t = types(req).get(ty);
        (t.scalar(), t.pointee(), t.record().is_some())
    };
    if let Some(scalar) = scalar {
        let layout = match scalar {
            Scalar::Float => {
                ffi::write(slot, to_double(req, arg)? as f32);
                Layout::Float
            }
            Scalar::Double | Scalar::LongDouble => {
                ffi::write(slot, to_double(req, arg)?);
                Layout::Double
            }
            Scalar::Int(int) => {
                let value = to_long(req, arg)?;
                match int {
                    Int::U8 | Int::I8 => ffi::write(slot, value as u8),
                    Int::U16 | Int::I16 => ffi::write(slot, value as u16),
                    Int::U32 | Int::I32 => ffi::write(slot, value as u32),
                    Int::U64 | Int::I64 => ffi::write(slot, value),
                }
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
            Scalar::Bool => {
                let truth = truthy(req, arg)?;
                ffi::write(slot, u8::from(truth));
                Layout::U8
            }
            Scalar::Char => {
                let (first, len) = with_string(req, arg, |bytes| {
                    (bytes.first().copied().unwrap_or(0), bytes.len())
                })?;
                ffi::write(slot, first);
                if len != 1 && frame.pending.is_none() {
                    frame.pending = Some(pass_incompatible(req, name, n, arg, ty)?);
                }
                Layout::I8
            }
        };
        frame.layouts.push(layout);
        return Ok(());
    }
    if let Some(pointee) = pointee {
        frame.layouts.push(Layout::Pointer);
        if matches!(arg.as_ref(), ValueRef::Null) {
            ffi::write(slot, 0usize);
            return Ok(());
        }
        if let ValueRef::Str(text) = arg.as_ref()
            && matches!(types(req).get(pointee).kind, Kind::Char | Kind::Void)
        {
            let bytes = text.as_bytes().to_vec();
            let addr = frame.string(&bytes);
            ffi::write(slot, addr);
            return Ok(());
        }
        if let Some(object) = as_cdata(req, arg)? {
            let v = view(req, object);
            let (compatible, is_pointer) = {
                let types = types(req);
                (
                    types.is_compatible(ty, v.ty),
                    types.get(v.ty).pointee().is_some(),
                )
            };
            if compatible {
                if is_pointer {
                    if v.ptr == 0 {
                        return Err(ffi_error(req, "NULL pointer dereference"));
                    }
                    ffi::write(slot, ffi::read::<usize>(v.ptr));
                } else {
                    ffi::write(slot, v.ptr);
                }
                return Ok(());
            }
        } else if let Some(func) = func_pointee(req, ty) {
            let code = create_callback(req, func, arg)?;
            ffi::write(slot, code);
            return Ok(());
        }
        return Err(pass_incompatible(req, name, n, arg, ty)?);
    }
    if is_struct {
        if let Some(object) = as_cdata(req, arg)? {
            let v = view(req, object);
            if types(req).is_compatible(ty, v.ty) {
                let Some(layout) = types(req).layout(ty) else {
                    return Err(pass_unsupported(req, ty));
                };
                frame.layouts.push(layout);
                if let Some(addr) = frame.addrs.get_mut(n) {
                    *addr = v.ptr;
                }
                return Ok(());
            }
        }
        return Err(pass_incompatible(req, name, n, arg, ty)?);
    }
    Err(pass_unsupported(req, ty))
}

/// `zend_ffi_pass_var_arg`: one argument past the declared ones, its C
/// type read off its PHP type.
fn pass_var_arg(
    req: &mut Request,
    name: &str,
    n: usize,
    arg: &Value,
    frame: &mut Frame,
) -> Result<(), Thrown> {
    let slot = frame.slot(n);
    let layout = match arg.as_ref() {
        ValueRef::Null => {
            ffi::write(slot, 0usize);
            Layout::Pointer
        }
        ValueRef::Bool(b) => {
            ffi::write(slot, u8::from(b));
            Layout::U8
        }
        ValueRef::Int(value) => {
            ffi::write(slot, value);
            Layout::I64
        }
        ValueRef::Float(value) => {
            ffi::write(slot, value);
            Layout::Double
        }
        ValueRef::Str(text) => {
            let bytes = text.as_bytes().to_vec();
            let addr = frame.string(&bytes);
            ffi::write(slot, addr);
            Layout::Pointer
        }
        ValueRef::Object(_) => {
            if let Some(object) = as_cdata(req, arg)? {
                let ty = view(req, object).ty;
                return pass_arg(req, name, n, arg, ty, frame);
            }
            return Err(ffi_error(req, "Unsupported argument type"));
        }
        _ => return Err(ffi_error(req, "Unsupported argument type")),
    };
    frame.layouts.push(layout);
    Ok(())
}

/// `zend_ffi_create_callback`: the C function pointer a callable is
/// made into for a parameter or a field of the function type `func`.
///
/// ```php
/// $ffi = FFI::cdef("void qsort(void *base, size_t n, size_t size, int (*cmp)(const void *, const void *));");
/// $a = $ffi->new("int[4]");
/// foreach ([3, 1, 4, 2] as $i => $v) { $a[$i] = $v; }
/// $ffi->qsort($a, 4, 4, function ($x, $y) use ($ffi) {
///     return $ffi->cast("int*", $x)[0] <=> $ffi->cast("int*", $y)[0];
/// });
/// foreach ($a as $v) { echo $v; }
/// echo "\n";
/// ```
/// ```expect
/// 1234
/// ```
///
/// The callback owns its callable whole ([`Callback`]): an
/// `[$obj, 'm']` callback's object and a closure made of one are both
/// destructed at the request's end, after the program's last line.
///
/// ```php
/// class A { function m($x) { return $x + 1; } function __destruct() { echo "dtor\n"; } }
/// $ffi = FFI::cdef("typedef int (*cb)(int); typedef struct { cb f; } S;");
/// $s = $ffi->new("S");
/// $a = new A;
/// $s->f = [$a, 'm'];
/// var_dump(($s->f)(1));
/// unset($a);
/// echo "unset\n";
/// $c = new A;
/// $s->f = Closure::fromCallable([$c, 'm']);
/// unset($c);
/// var_dump(($s->f)(5));
/// echo "end\n";
/// ```
/// ```expect
/// int(2)
/// unset
/// int(6)
/// end
/// dtor
/// dtor
/// ```
pub(crate) fn create_callback(
    req: &mut Request,
    func: TypeId,
    value: &Value,
) -> Result<usize, Thrown> {
    let Some(shape) = shape(req, func) else {
        return Err(ffi_error(req, "Cannot prepare callback CIF"));
    };
    if shape.variadic {
        return Err(ffi_error(
            req,
            "Variadic function closures are not supported",
        ));
    }
    let callable = match callable_checked(req, value) {
        Ok(callable) => callable,
        Err(error) => {
            let message = format!("Attempt to assign an invalid callback, {error}");
            return Err(ffi_error(req, &message));
        }
    };
    if shape.args.len() < callable.required(req) {
        return Err(ffi_error(
            req,
            "Attempt to assign an invalid callback, insufficient number of arguments",
        ));
    }
    let mut layouts = Vec::with_capacity(shape.args.len());
    for arg in &shape.args {
        match types(req).layout(*arg) {
            Some(layout) => layouts.push(layout),
            None => return Err(pass_unsupported(req, *arg)),
        }
    }
    let Some(ret) = types(req).layout(shape.ret) else {
        return Err(return_unsupported(req, shape.ret));
    };
    let Some(sig) = Signature::prepare(shape.abi, &ret, &layouts, None) else {
        return Err(ffi_error(req, "Cannot prepare callback CIF"));
    };
    let reentry = req.limits_mut().reentry();
    let index = req.state::<State>().callbacks.len();
    let closure = match Closure::new(sig, reentry, callback, index) {
        Ok(closure) => closure,
        Err(ClosureError::Alloc) => return Err(ffi_error(req, "Cannot allocate callback")),
        Err(ClosureError::Prepare) => return Err(ffi_error(req, "Cannot prepare callback")),
    };
    let code = closure.code();
    let callable = Owner::take(req, callable);
    req.state::<State>().callbacks.push(Callback {
        closure,
        callable,
        ty: func,
    });
    Ok(code)
}

/// `zend_ffi_callback_trampoline`: C called a callback; its arguments
/// are read into PHP values, the callable called, and its return
/// written back where C reads it, unless `ret` is zero because C read
/// its zeroed return already. A throwable the callable leaves is
/// php's fatal, since it cannot unwind through C, and the fatal reports
/// it first as a warning, as `zend_error_noreturn` does with
/// `EG(exception)` set, whether C called the callback inside a call
/// into C or from a signal handler served at the interrupt check:
///
/// ```php
/// $ffi = FFI::cdef("typedef void (*handler)(int); handler signal(int sig, handler h); int raise(int sig);");
/// $ffi->signal(SIGUSR1, function ($sig) { echo "caught $sig\n"; throw new Exception("x"); });
/// try { echo "a" . $ffi->raise(SIGUSR1) . "b\n"; } catch (Throwable $e) { echo get_class($e), "\n"; }
/// echo "done\n";
/// ```
/// ```expect process exit=255
/// caught %d
///
/// Warning: Uncaught Exception: x in %s:3
/// Stack trace:
/// #0 %s(4): {closure:%s:3}(%d)
/// #1 %s(4): FFI->raise(%d)
/// #2 {main}
///   thrown in %s on line 3
///
/// Fatal error: Throwing from FFI callbacks is not allowed in %s on line 4
/// ```
///
/// ```php
/// $ffi = FFI::cdef("typedef void (*handler)(int); handler signal(int sig, handler h);");
/// $ffi->signal(SIGUSR1, function ($sig) { echo "caught $sig\n"; throw new Exception("x"); });
/// try { posix_kill(posix_getpid(), SIGUSR1); echo "after\n"; } catch (Throwable $e) { echo get_class($e), "\n"; }
/// echo "done\n";
/// ```
/// ```expect process exit=255
/// caught %d
///
/// Warning: Uncaught Exception: x in %s:3
/// Stack trace:
/// #0 [internal function]: {closure:%s:3}(%d)
/// #1 %s(4): posix_kill(%d, %d)
/// #2 {main}
///   thrown in %s on line 3
///
/// Fatal error: Throwing from FFI callbacks is not allowed in %s on line 4
/// ```
///
/// Once the request is ending on a fatal, a call C makes serves nothing,
/// and C reads the zeroed return it was given on entry. php's
/// `zend_bailout` jumps out of the C function, so `qsort` calls the
/// callback no more; here C runs on to its own return, and each later
/// call it makes returns at once without running PHP code, until the
/// call into C returns to the request that leaves:
///
/// ```php
/// $ffi = FFI::cdef('void qsort(void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *));', 'libSystem.B.dylib');
/// $a = $ffi->new('int[3]'); $a[0] = 3; $a[1] = 1; $a[2] = 2;
/// register_shutdown_function(function () { echo "shutdown\n"; });
/// try { $ffi->qsort(FFI::addr($a), 3, 4, function ($x, $y) { echo "cmp\n"; throw new Exception("cmp"); }); } catch (Throwable $e) { echo "caught\n"; }
/// echo "after\n";
/// ```
/// ```expect exit=255
/// cmp
///
/// Warning: Uncaught Exception: cmp in %s:5
/// Stack trace:
/// #0 %s(5): {closure:%s:5}(Object(FFI\CData:void*), Object(FFI\CData:void*))
/// #1 %s(5): FFI->qsort(Object(FFI\CData:int32_t(*)[3]), 3, 4, Object(Closure))
/// #2 {main}
///   thrown in %s on line 5
///
/// Fatal error: Throwing from FFI callbacks is not allowed in %s on line 5
/// shutdown
/// ```
fn callback(req: &mut Request, index: usize, ret: usize, args: &[usize]) {
    if req.diagnostics().bailing_out() {
        return;
    }
    let Some((callable, ty)) = req
        .state::<State>()
        .callbacks
        .get(index)
        .map(|callback| (Callable::clone(&callback.callable), callback.ty))
    else {
        return;
    };
    let Some(shape) = shape(req, ty) else {
        return;
    };
    let mut values = Vec::with_capacity(args.len());
    for (arg, arg_ty) in args.iter().zip(&shape.args) {
        let is_const = types(req).get(*arg_ty).attr & attr::CONST != 0;
        match to_value(
            req,
            None,
            *arg,
            *arg_ty,
            Fetch::Read,
            is_const,
            false,
            false,
        ) {
            Ok(value) => values.push(value),
            Err(_) => break,
        }
    }
    let arguments: Vec<Value> = values.iter().map(|value| **value).collect();
    let called = php_ext::call::call(req, &callable, &arguments);
    for value in values {
        value.give_up(req);
    }
    let returned = match called {
        Ok(returned) => returned,
        Err(thrown) => {
            // `zend_error_noreturn` with `EG(exception)` set, which
            // reports the throwable as a warning before the fatal.
            let refuse = |req: &mut Request| {
                let _ = fatal(req, "Throwing from FFI callbacks is not allowed");
            };
            if while_pending(req, &thrown, refuse).is_none() {
                refuse(req);
            }
            return;
        }
    };
    if ret != 0 && !matches!(types(req).get(shape.ret).kind, Kind::Void) {
        write_value(req, ret, shape.ret, &returned).ok();
    }
    returned.give_up(req);
}

/// The calls C made of this request's callbacks while it ran PHP code,
/// served at the request's interrupt check as pcntl's dispatch serves
/// the signals delivered, before the link of `zend_interrupt_function`
/// beneath this one. A call from a signal handler the kernel
/// entered in another extension's native here: php's handler called
/// PHP code inside `posix_kill()`, and this one when it returns, which
/// is the same place in the program; C read a zeroed return and the
/// return PHP code makes is dropped, which a signal handler's caller
/// never reads either.
///
/// ```php
/// $ffi = FFI::cdef("typedef void (*handler)(int); handler signal(int sig, handler h); int raise(int sig);");
/// $ffi->signal(SIGUSR1, function ($sig) { echo "caught $sig\n"; });
/// $ffi->raise(SIGUSR1);
/// posix_kill(posix_getpid(), SIGUSR1);
/// echo "done\n";
/// ```
/// ```expect process
/// caught 30
/// caught 30
/// done
/// ```
///
/// A call from another thread, after the call into C that made it
/// returned (#1640, #1614): php calls the callable on that thread at
/// once, concurrently with the request; here the thread waits, the
/// request serves the call at its next check and hands back the return,
/// which is one interleaving of php's race. A thread still waiting when
/// the request blocks in a call into C, `pthread_join` here, or in a
/// native such as `usleep`, takes the lent request and is served on its
/// own thread.
///
/// ```php
/// $ffi = FFI::cdef("typedef void *(*start)(void *); int pthread_create(void **t, void *attr, start s, void *a); int pthread_join(void *t, void **r);");
/// $t = $ffi->new("void *");
/// $ffi->pthread_create(FFI::addr($t), null, function ($a) { echo "thread\n"; return null; }, null);
/// $x = 0; for ($i = 0; $i < 3000000; $i++) { $x += $i; }
/// echo "loop $x\n";
/// $ffi->pthread_join($t, null);
/// echo "done\n";
/// ```
/// ```expect
/// thread
/// loop 4499998500000
/// done
/// ```
pub(crate) fn serve_deferred(
    req: &mut Request,
    beneath: Beneath<'_, chain::Interrupt>,
) -> Result<(), Thrown> {
    if req.state::<State>().callbacks.is_empty() {
        return beneath.serve(req);
    }
    let reentry = req.limits_mut().reentry();
    reentry.serve_waiting(|call| ffi::serve_waiting(req, call));
    for mut deferred in reentry.take_deferred() {
        let Some(addrs) = req
            .state::<State>()
            .callbacks
            .get(deferred.callback)
            .map(|callback| callback.closure.args_in(&mut deferred))
        else {
            continue;
        };
        callback(req, deferred.callback, 0, &addrs);
    }
    beneath.serve(req)
}

/// `ZEND_RSHUTDOWN_FUNCTION(ffi)`: the callbacks given up
/// ([`release_callbacks`]), then the types `FFI_G(weak_types)`
/// remembered ([`crate::cdata::forget_types`]).
pub(crate) fn rshutdown(req: &mut Request) {
    release_callbacks(req);
    crate::cdata::forget_types(req);
}

/// The callbacks given up at the end of the request, each one's code
/// freed and the callable it owned given up, as php's `RSHUTDOWN`
/// destroys `FFI_G(callbacks)` (`zend_ffi_callback_hash_dtor`).
fn release_callbacks(req: &mut Request) {
    let callbacks = std::mem::take(&mut req.state::<State>().callbacks);
    for callback in callbacks {
        drop(callback.closure);
        callback.callable.give_up(req);
    }
}
