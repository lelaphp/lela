//! `Generator` and `ClosedGeneratorException` as
//! `zend_generators.stub.php` declares them, and the resumption of a
//! generator's frame (`Z/Zend/zend_generators.c` at 8.5.9; design
//! §5.11).
//!
//! A generator's frame lives on the heap between its runs, as php's
//! `zend_generator` keeps its own `execute_data`: `GENERATOR_CREATE`
//! moves the frame off the VM stack into the object the call returns,
//! and every resumption puts it back on top of whichever stack the
//! resumer stands on, a fiber's included, and runs the engine's one
//! loop over it again ([`crate::tier::resume`]) until `YIELD` moves it
//! off once more, `GENERATOR_RETURN` closes it, or a throwable leaves
//! it. The frame is the heap's `Frame` and the engine's `FrameState`
//! together, with the calls its body was building when it yielded, so
//! no slot is copied on the way in or out.
//!
//! `yield from` a generator follows php's delegation tree: each
//! generator that delegates is a child of the one it delegates to, the
//! root of the tree is the one whose body runs, and a leaf caches its
//! root ([`current`]), so resuming the outermost resumes the innermost
//! and the order of resumption is php's.
//!
//! A generator destroyed while suspended inside a `try` with a
//! `finally` runs the block, as `zend_generator_dtor_storage` resumes
//! it there under `ZEND_GENERATOR_FORCED_CLOSE`.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_heap::{Fatal, FrameRef};
use php_unit::decl::{
    ClassDecl, ClassFlags, ClassKind, DynamicProperties, EnumBacking, FunctionDecl, MethodDecl,
    Name, Parameter, ParameterDefault, Type, Visibility,
};
use php_unit::{OpArray, Opcode};

use crate::api::abi::{CloneObj, Entry, NativeClassDecl, ObjectHandlers};
use crate::api::call::Function;
use crate::api::raising::{Thrown, arg_count_error, arg_type_error, throw};
use crate::api::request::Request;
use crate::api::values::{Key, PhpArray, PhpObject, PhpString, TypeTag, Value};
use crate::exec::{self, CallState, FrameState};

/// Where a generator's frame is: php's `execute_data`, and
/// `ZEND_GENERATOR_CURRENTLY_RUNNING` as the one state its frame is on
/// a stack in, which is also the only one a resumption holds it in.
#[derive(Default)]
enum Execution {
    /// Off the stack, between two runs or before the first.
    Suspended(Box<Suspended>),
    /// On a stack, a resumption running it.
    Running,
    /// Returned, left by a throwable, or destroyed: php's
    /// `execute_data` of `NULL`.
    #[default]
    Closed,
}

/// A frame off the stack: its slots, its engine half, and the calls its
/// body was building (`frozen_call_stack`).
pub(crate) struct Suspended {
    slots: php_heap::Frame,
    state: FrameState,
    calls: Vec<CallState>,
}

/// php's `generator->values`: what a `yield from` over something that
/// is no generator hands out, one element per resumption.
#[derive(Default)]
enum Delegated {
    #[default]
    Nothing,
    /// An array and the position `Z_FE_POS` holds.
    Array { array: Value, pos: u32 },
    /// The iterator a `Traversable`'s `get_iterator` made, the wrapper
    /// of a [`crate::interfaces::ObjectIterator`], which keeps its own
    /// `iter->index`; one owner.
    Iterator(PhpObject),
}

// `forced_close` to `destroying` are php's `ZEND_GENERATOR_*` flags and
// `IS_OBJ_DESTRUCTOR_CALLED`, each one fact.
#[allow(clippy::struct_excessive_bools)]
/// `zend_generator`'s payload: the frame, the current value and key,
/// what the body returned, where `send()` writes, the auto-key, the
/// delegated values, the delegation tree's links and the flags.
pub(crate) struct Generator {
    execution: Execution,
    /// `generator->func`: what `__debugInfo` and `ReflectionGenerator`
    /// name.
    function: Option<Function>,
    value: Value,
    key: Value,
    /// `UNDEF` until the body returned.
    retval: Value,
    /// The slot of the `VAR` the last `YIELD` wrote `null` into.
    send_target: Option<u32>,
    largest_used_integer_key: i64,
    values: Delegated,
    /// `node.parent`, one owner: the generator this one delegates to.
    parent: Option<PhpObject>,
    /// `node.child`: the generators delegating to this one, no owner.
    children: Vec<PhpObject>,
    /// `node.ptr`: the leaf a root caches, or the root a leaf does; no
    /// owner, and which of the two it is is whether `parent` is set.
    link: Option<PhpObject>,
    forced_close: bool,
    at_first_yield: bool,
    do_init: bool,
    /// `ZEND_GENERATOR_IN_FIBER`: a resumption made while a fiber runs
    /// is under way, on the resumed generator and on the one that runs,
    /// so a destruction while the fiber is suspended leaves the
    /// generators on the fiber's stack to the fiber's teardown.
    in_fiber: bool,
    /// `IS_OBJ_DESTRUCTOR_CALLED` while [`dtor_storage`] runs, which
    /// `zend_generator_update_current` reads off the object.
    destroying: bool,
}

impl Default for Generator {
    /// `zend_generator_create`: the key "will be incremented on first
    /// use, so it'll start at 0", and `retval` and `values` undefined.
    fn default() -> Generator {
        Generator {
            execution: Execution::Closed,
            function: None,
            value: Value::undef(),
            key: Value::undef(),
            retval: Value::undef(),
            send_target: None,
            largest_used_integer_key: -1,
            values: Delegated::Nothing,
            parent: None,
            children: Vec::new(),
            link: None,
            forced_close: false,
            at_first_yield: false,
            do_init: false,
            in_fiber: false,
            destroying: false,
        }
    }
}

/// `zend_generator_handlers`: the slots `zend_register_generator_ce`
/// overrides over `std_object_handlers`.
struct GeneratorHandlers;

impl ObjectHandlers for GeneratorHandlers {
    /// `zend_generator_dtor_storage`.
    fn dtor_obj(&self, req: &mut Request, object: PhpObject) {
        dtor_storage(req, object);
    }

    /// `zend_generator_free_storage`: the frame closed, and the value,
    /// the key and what the body returned given up.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        close(req, object, false);
        let held = with(req, object, |generator| {
            let values = std::mem::take(&mut generator.values);
            [
                std::mem::replace(&mut generator.value, Value::undef()),
                std::mem::replace(&mut generator.key, Value::undef()),
                std::mem::replace(&mut generator.retval, Value::undef()),
                match values {
                    Delegated::Nothing => Value::undef(),
                    Delegated::Array { array, .. } => array,
                    Delegated::Iterator(iterator) => iterator.as_value(),
                },
            ]
        });
        for value in held.into_iter().flatten() {
            req.release(value);
        }
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }

    /// `zend_generator_get_constructor`.
    fn get_constructor(&self, req: &mut Request, _: PhpObject) -> Result<Option<Function>, Thrown> {
        Err(throw(
            req,
            "Error",
            "The \"Generator\" class is reserved for internal use and cannot be manually instantiated",
        ))
    }

    /// `zend_generator_get_gc`: the value, the key, what the body
    /// returned, the delegated values and the generator delegated to.
    /// A suspended frame's slots are not walked yet (#687's neighbour),
    /// which keeps what they hold alive through a collection and frees
    /// nothing early.
    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        let Some(generator) = object.native::<Generator>(req) else {
            return;
        };
        visit(generator.value);
        visit(generator.key);
        visit(generator.retval);
        match &generator.values {
            Delegated::Nothing => {}
            Delegated::Array { array, .. } => visit(*array),
            Delegated::Iterator(iterator) => visit(iterator.as_value()),
        }
        if let Some(parent) = generator.parent {
            visit(parent.as_value());
        }
    }
}

static GENERATOR: GeneratorHandlers = GeneratorHandlers;

/// Whether `object` is a `Generator`, `ce == zend_ce_generator`.
pub(crate) fn is_generator(req: &Request, object: PhpObject) -> bool {
    std::ptr::addr_eq(req.handlers_of(object.class()), &raw const GENERATOR)
}

fn new_generator(
    heap: &mut php_heap::Heap,
    class: u32,
    slots: u32,
) -> Result<php_heap::Obj, Fatal> {
    heap.object_with(class, slots, Generator::default())
}

/// `final class Generator implements Iterator` and
/// `class ClosedGeneratorException extends Exception`, in the stub's
/// order; the second is what a generator waiting on one that was torn
/// down raises.
///
/// ```php
/// function g() { yield 1; }
/// $g = g();
/// var_dump($g instanceof Iterator, $g instanceof Traversable, get_class($g));
/// try { clone $g; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump(new ClosedGeneratorException("x") instanceof Exception);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(9) "Generator"
/// Trying to clone an uncloneable object of class Generator
/// bool(true)
/// ```
pub(crate) fn classes() -> Vec<NativeClassDecl> {
    vec![
        (
            generator_class(),
            generator_methods(),
            Some(&GENERATOR),
            Some(new_generator as crate::api::abi::NativeNew),
        ),
        (
            ClassDecl {
                parent: Some(Name::from("Exception")),
                ..class(b"ClosedGeneratorException")
            },
            Vec::new(),
            None,
            None,
        ),
    ]
}

fn class(name: &[u8]) -> ClassDecl {
    ClassDecl {
        name: Name(name.into()),
        kind: ClassKind::Class,
        flags: ClassFlags::default(),
        parent: None,
        interfaces: Vec::new(),
        traits: Vec::new(),
        backing: EnumBacking::Pure,
        constants: Vec::new(),
        properties: Vec::new(),
        methods: Vec::new(),
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
    }
}

/// `@strict-properties` is `ZEND_ACC_NO_DYNAMIC_PROPERTIES`.
fn generator_class() -> ClassDecl {
    ClassDecl {
        flags: ClassFlags {
            is_final: true,
            dynamic_properties: DynamicProperties::Forbidden,
            ..ClassFlags::default()
        },
        interfaces: vec![Name::from("Iterator")],
        ..class(b"Generator")
    }
}

fn generator_methods() -> Vec<(MethodDecl, Entry)> {
    vec![
        (method(b"rewind", Vec::new(), Type::Void), rewind as Entry),
        (method(b"valid", Vec::new(), Type::Bool), valid as Entry),
        (
            method(b"current", Vec::new(), Type::Mixed),
            current_value as Entry,
        ),
        (method(b"key", Vec::new(), Type::Mixed), key as Entry),
        (method(b"next", Vec::new(), Type::Void), next as Entry),
        (
            method(b"send", vec![parameter(b"value", Type::Mixed)], Type::Mixed),
            send as Entry,
        ),
        (
            method(
                b"throw",
                vec![parameter(
                    b"exception",
                    Type::Named(Name::from("Throwable")),
                )],
                Type::Mixed,
            ),
            throw_into as Entry,
        ),
        (
            method(b"getReturn", Vec::new(), Type::Mixed),
            get_return as Entry,
        ),
        (
            method(b"__debugInfo", Vec::new(), Type::Array),
            debug_info as Entry,
        ),
    ]
}

fn method(name: &[u8], parameters: Vec<Parameter>, returns: Type) -> MethodDecl {
    MethodDecl {
        function: FunctionDecl {
            name: Name(name.into()),
            parameters,
            return_type: Some(returns),
            returns_by_ref: false,
            is_generator: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            end_line: 0,
            frameless: Vec::new(),
        },
        visibility: Visibility::Public,
        is_instance: true,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    }
}

fn parameter(name: &[u8], ty: Type) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty: Some(ty),
        by_ref: false,
        prefer_ref: false,
        variadic: false,
        default: ParameterDefault::None,
        attributes: Vec::new(),
    }
}

/// `f` over the payload of `object`; none for an object that has none.
fn with<R>(req: &mut Request, object: PhpObject, f: impl FnOnce(&mut Generator) -> R) -> Option<R> {
    object.native_mut::<Generator>(req).map(f)
}

/// A read of the payload.
fn read<R>(req: &Request, object: PhpObject, f: impl FnOnce(&Generator) -> R) -> Option<R> {
    object.native::<Generator>(req).map(f)
}

/// `ZEND_GENERATOR_CURRENTLY_RUNNING`: a resumption holds the frame on
/// a stack.
fn is_running(req: &Request, object: PhpObject) -> bool {
    read(req, object, |generator| {
        matches!(generator.execution, Execution::Running)
    })
    .unwrap_or(false)
}

/// `generator->execute_data != NULL`: suspended or running.
fn has_frame(req: &Request, object: PhpObject) -> bool {
    read(req, object, |generator| {
        !matches!(generator.execution, Execution::Closed)
    })
    .unwrap_or(false)
}

fn parent(req: &Request, object: PhpObject) -> Option<PhpObject> {
    read(req, object, |generator| generator.parent).flatten()
}

fn link(req: &Request, object: PhpObject) -> Option<PhpObject> {
    read(req, object, |generator| generator.link).flatten()
}

fn set_link(req: &mut Request, object: PhpObject, to: Option<PhpObject>) {
    with(req, object, |generator| generator.link = to);
}

fn same(a: PhpObject, b: PhpObject) -> bool {
    a.index() == b.index()
}

/// The opline a suspended frame stands at, and its op array's opcode
/// there.
fn suspended_at(req: &Request, object: PhpObject) -> Option<Opcode> {
    read(req, object, |generator| match &generator.execution {
        Execution::Suspended(suspended) => {
            let code = exec::body_of(&suspended.state.unit, suspended.state.body)?;
            exec::opline_at(code, suspended.state.opline).map(|op| op.opcode)
        }
        _ => None,
    })
    .flatten()
}

// ----- the delegation tree (`zend_generators.h`, `zend_generators.c`) -----

/// `zend_generator_get_current`: the generator whose body a resumption
/// of `generator` runs, the root of its delegation tree.
pub(crate) fn current(req: &mut Request, generator: PhpObject) -> PhpObject {
    if parent(req, generator).is_none() {
        return generator;
    }
    let root = match link(req, generator) {
        Some(root) => root,
        None => update_root(req, generator),
    };
    if has_frame(req, root) {
        return root;
    }
    update_current(req, generator)
}

/// `clear_link_to_leaf`.
fn clear_link_to_leaf(req: &mut Request, generator: PhpObject) -> Option<PhpObject> {
    let leaf = link(req, generator)?;
    set_link(req, leaf, None);
    set_link(req, generator, None);
    Some(leaf)
}

/// `clear_link_to_root`.
fn clear_link_to_root(req: &mut Request, generator: PhpObject) {
    if let Some(root) = link(req, generator) {
        set_link(req, root, None);
        set_link(req, generator, None);
    }
}

/// `zend_generator_update_root`.
fn update_root(req: &mut Request, generator: PhpObject) -> PhpObject {
    let mut root = parent(req, generator).unwrap_or(generator);
    let mut depth = 0u32;
    while let Some(above) = parent(req, root) {
        root = above;
        depth += 1;
        if depth > 1 << 20 {
            break;
        }
    }
    clear_link_to_leaf(req, root);
    set_link(req, root, Some(generator));
    set_link(req, generator, Some(root));
    root
}

/// `get_new_root`.
fn new_root(req: &mut Request, generator: PhpObject, root: PhpObject) -> PhpObject {
    let mut root = root;
    loop {
        if has_frame(req, root) {
            return root;
        }
        let only = read(req, root, |g| {
            (g.children.len() == 1)
                .then(|| g.children.first().copied())
                .flatten()
        })
        .flatten();
        match only {
            Some(child) => root = child,
            None => break,
        }
    }
    let mut generator = generator;
    while let Some(above) = parent(req, generator) {
        if !has_frame(req, above) {
            break;
        }
        generator = above;
    }
    generator
}

/// `zend_generator_remove_child`.
fn remove_child(req: &mut Request, node: PhpObject, child: PhpObject) {
    with(req, node, |generator| {
        generator.children.retain(|held| !same(*held, child));
    });
}

/// `zend_generator_update_current`: the root has finished, so the
/// delegation tree hands the running over to the generator that
/// delegated to it, which takes what the finished one returned as the
/// value of its `yield from`; one that finished with no return value,
/// torn down by a throwable or a destructor, is php's
/// `ClosedGeneratorException` in the generator that waited on it.
fn update_current(req: &mut Request, generator: PhpObject) -> PhpObject {
    let Some(old_root) = link(req, generator) else {
        return generator;
    };
    let root = new_root(req, generator, old_root);
    set_link(req, generator, Some(root));
    set_link(req, root, Some(generator));
    set_link(req, old_root, None);
    let Some(root_parent) = parent(req, root) else {
        return root;
    };
    remove_child(req, root_parent, root);
    if !req.is_pending()
        && !read(req, generator, |g| g.destroying).unwrap_or(false)
        && suspended_at(req, root) == Some(Opcode::YieldFrom)
    {
        let retval = read(req, root_parent, |g| g.retval).unwrap_or_else(Value::undef);
        if retval.tag() == TypeTag::Undef {
            let thrown = in_context(req, root, |req| {
                throw(
                    req,
                    "ClosedGeneratorException",
                    "Generator yielded from aborted, no return value available",
                )
            });
            if let Some(thrown) = thrown {
                req.leave_pending(&thrown);
            }
            let old_running = is_running(req, old_root);
            if !old_running {
                with(req, root, |g| g.parent = None);
                req.release(root_parent.as_value());
                // A resumption that bails out leaves the request ending,
                // which the caller of `current` reads there.
                let _ = resume(req, generator);
                return current(req, generator);
            }
        } else {
            let value = read(req, root_parent, |g| g.value).unwrap_or_else(Value::null);
            req.addref(&value);
            let old = with(req, root, |g| std::mem::replace(&mut g.value, value));
            if let Some(old) = old {
                req.release(old);
            }
            req.addref(&retval);
            let written = with(req, root, |g| {
                let Execution::Suspended(suspended) = &mut g.execution else {
                    return Some(retval);
                };
                let code = exec::body_of(&suspended.state.unit, suspended.state.body)?;
                let op = exec::opline_at(code, suspended.state.opline)?;
                let slot = exec::temporary_slot(code, op.out);
                let old = suspended.slots.slot(php_heap::Slot(slot));
                suspended.slots.set(php_heap::Slot(slot), retval.to_zval());
                Some(Value::of_zval(old))
            })
            .flatten();
            if let Some(old) = written {
                req.release(old);
            }
        }
    }
    with(req, root, |g| g.parent = None);
    req.release(root_parent.as_value());
    root
}

/// `zend_generator_yield_from`: `generator` delegates to `from`, the
/// owner of `from` the caller holds becoming `generator`'s link to it.
fn yield_from_generator(req: &mut Request, generator: PhpObject, from: PhpObject) {
    let leaf = clear_link_to_leaf(req, generator);
    if let Some(leaf) = leaf
        && parent(req, from).is_none()
        && link(req, from).is_none()
    {
        set_link(req, from, Some(leaf));
        set_link(req, leaf, Some(from));
    }
    with(req, generator, |g| {
        g.parent = Some(from);
        g.do_init = true;
    });
    with(req, from, |g| g.children.push(generator));
}

// ----- the frame on and off the stack -----

/// The suspended frame of `generator` back on top of the stack the
/// request runs on, `Running` from here; none where it has none. Inside
/// a fiber that is the fiber's stack, so a generator a fiber walks is
/// suspended with it, by a `Fiber::suspend()` in its body as in the
/// loop's.
///
/// ```php
/// function gen() { $x = Fiber::suspend('s1'); echo "got $x\n"; yield 1; Fiber::suspend('s2'); yield 2; }
/// $f = new Fiber(function () { foreach (gen() as $k => $v) echo "$k=$v\n"; return 'done'; });
/// echo $f->start(), "\n";
/// echo $f->resume('r1'), "\n";
/// var_dump($f->resume(), $f->getReturn());
/// ```
/// ```expect
/// s1
/// got r1
/// 0=1
/// s2
/// 1=2
/// NULL
/// string(4) "done"
/// ```
fn push(req: &mut Request, generator: PhpObject) -> Option<FrameRef> {
    let suspended = with(req, generator, |g| {
        match std::mem::replace(&mut g.execution, Execution::Running) {
            Execution::Suspended(suspended) => Some(suspended),
            other => {
                g.execution = other;
                None
            }
        }
    })
    .flatten()?;
    let Suspended {
        slots,
        mut state,
        calls,
    } = *suspended;
    state.caller = None;
    let frame = req.stack_mut().resume_frame(slots);
    req.vm_mut().resume(state, calls);
    Some(frame)
}

/// The frame on top of the stack, which is `generator`'s, off it again
/// and suspended: what `YIELD` and `YIELD_FROM` leave.
pub(crate) fn suspend(req: &mut Request, generator: PhpObject) -> Result<(), Fatal> {
    let Some(slots) = req.stack_mut().move_to_heap() else {
        return Err(Fatal::Error(String::from(
            "InternalError: a generator suspended with no frame on the stack",
        )));
    };
    let Some((state, calls)) = req.vm_mut().suspend() else {
        return Err(Fatal::Error(String::from(
            "InternalError: a generator suspended with no frame state on the stack",
        )));
    };
    with(req, generator, |g| {
        g.execution = Execution::Suspended(Box::new(Suspended {
            slots,
            state,
            calls,
        }));
    });
    Ok(())
}

/// `f` with the suspended frame of `generator` on the stack, so what it
/// raises is raised with the generator's frame standing, php's
/// `EG(current_execute_data) = generator->execute_data`; the frame is
/// suspended again after it. None where the generator has no suspended
/// frame, and `f` runs where the resumer stands then.
fn in_context<R>(
    req: &mut Request,
    generator: PhpObject,
    f: impl FnOnce(&mut Request) -> R,
) -> Option<R> {
    let Some(_frame) = push(req, generator) else {
        return Some(f(req));
    };
    let out = f(req);
    suspend(req, generator).ok()?;
    Some(out)
}

/// `GENERATOR_CREATE` (`Z/Zend/zend_vm_def.h:4652`): the frame that is
/// running, `frame`, moved into a new `Generator` and the object handed
/// to the caller as what the call returned; the caller goes on after its
/// call opline. A call whose value nothing reads makes no generator at
/// all, and its frame leaves as a `return` would, so the body never
/// runs:
///
/// ```php
/// function g() { echo "body\n"; yield 1; }
/// g();
/// $g = g();
/// echo "made\n";
/// $g->current();
/// ```
/// ```expect
/// made
/// body
/// ```
pub(crate) fn create(req: &mut Request, frame: FrameRef) -> Result<crate::tier::Stepped, Fatal> {
    let entry = req.stack_mut().entry() == frame;
    let returns_to = req.vm().returns_to(frame);
    if !entry && returns_to.is_none() {
        req.leave_frame(frame);
        let caller = req.stack_mut().current();
        req.vm_mut().advance(caller);
        return Ok(crate::tier::Stepped::Popped(caller));
    }
    let Some(class) = req.class_named(b"generator") else {
        return Err(Fatal::Error(String::from(
            "InternalError: no Generator class is linked",
        )));
    };
    let object = match req.new_object(class) {
        Ok(object) => object,
        Err(thrown) => {
            req.leave_pending(&thrown);
            return Ok(crate::tier::Stepped::Threw(frame));
        }
    };
    let function = req.vm().at(frame).and_then(|state| state.function);
    if let Some(state) = req.vm_mut().at_mut(frame) {
        state.generator = Some(object);
        state.returns_to = None;
    }
    with(req, object, |g| g.function = function);
    suspend(req, object)?;
    if entry {
        req.set_returned(object.as_value());
        return Ok(crate::tier::Stepped::Done);
    }
    let caller = req.stack_mut().current();
    if let Some((at, slot)) = returns_to {
        let previous = req.set_slot(at, slot, object.as_value());
        req.release(previous);
    }
    req.vm_mut().advance(caller);
    Ok(crate::tier::Stepped::Moved(caller))
}

/// The generator whose frame `frame` is, `zend_get_running_generator`.
pub(crate) fn running(req: &Request, frame: FrameRef) -> Result<PhpObject, Fatal> {
    req.vm()
        .at(frame)
        .and_then(|state| state.generator)
        .ok_or_else(|| {
            Fatal::Error(String::from(
                "InternalError: a generator opcode on a frame that is no generator's",
            ))
        })
}

/// Whether `generator` is in `ZEND_GENERATOR_FORCED_CLOSE`.
pub(crate) fn forced_close(req: &Request, generator: PhpObject) -> bool {
    read(req, generator, |g| g.forced_close).unwrap_or(false)
}

/// The value and the key a `YIELD` is about to replace, given up
/// first as the handler's `zval_ptr_dtor`s give them up.
pub(crate) fn drop_current(req: &mut Request, generator: PhpObject) {
    let old = with(req, generator, |g| {
        (
            std::mem::replace(&mut g.value, Value::undef()),
            std::mem::replace(&mut g.key, Value::undef()),
        )
    });
    if let Some((value, key)) = old {
        req.release(value);
        req.release(key);
    }
}

/// What `YIELD` hands out: the value and the key, the previous ones
/// given back, the auto-key moved past an integer key, and the slot
/// `send()` writes into.
pub(crate) fn yielded(
    req: &mut Request,
    generator: PhpObject,
    value: Value,
    key: Option<Value>,
    send_target: Option<u32>,
) {
    let old = with(req, generator, |g| {
        let key = if let Some(key) = key {
            if let Some(n) = key.as_int()
                && n > g.largest_used_integer_key
            {
                g.largest_used_integer_key = n;
            }
            key
        } else {
            g.largest_used_integer_key = g.largest_used_integer_key.wrapping_add(1);
            Value::int(g.largest_used_integer_key)
        };
        g.send_target = send_target;
        (
            std::mem::replace(&mut g.value, value),
            std::mem::replace(&mut g.key, key),
        )
    });
    if let Some((value, key)) = old {
        req.release(value);
        req.release(key);
    }
}

/// `GENERATOR_RETURN`'s half that is the generator's: what the body
/// returned kept, and the frame closed as a finished one.
pub(crate) fn returned(req: &mut Request, generator: PhpObject, value: Value) {
    let old = with(req, generator, |g| std::mem::replace(&mut g.retval, value));
    if let Some(old) = old {
        req.release(old);
    }
    close(req, generator, true);
}

/// What `YIELD_FROM` found over its operand.
pub(crate) enum From {
    /// It delegates: the values are set, or the tree is linked.
    Delegating,
    /// A generator that has already returned: its return value, one
    /// owner, and the opline goes on.
    Finished(Value),
}

/// `YIELD_FROM`'s arms over an operand (`Z/Zend/zend_vm_def.h:8518`):
/// an array or a `Traversable` becomes the generator's delegated values,
/// a generator becomes the one it delegates to. `value` is one owner the
/// caller hands over.
pub(crate) fn yield_from(
    req: &mut Request,
    generator: PhpObject,
    value: Value,
    constant: bool,
) -> Result<From, Thrown> {
    let value = value.deref();
    if value.as_array().is_some() {
        with(req, generator, |g| {
            g.values = Delegated::Array {
                array: value,
                pos: 0,
            };
            g.send_target = None;
        });
        return Ok(From::Delegating);
    }
    let object = value.as_object().copied().filter(|_| !constant);
    let Some(object) = object else {
        req.release(value);
        return Err(throw(
            req,
            "Error",
            "Can use \"yield from\" only with arrays and Traversables",
        ));
    };
    if is_generator(req, object) {
        if !has_frame(req, object) {
            req.release(value);
            return Err(throw(
                req,
                "Error",
                "Generator passed to yield from was aborted without proper return and is unable to continue",
            ));
        }
        let retval = read(req, object, |g| g.retval).unwrap_or_else(Value::undef);
        if retval.tag() != TypeTag::Undef {
            req.addref(&retval);
            req.release(value);
            return Ok(From::Finished(retval));
        }
        if same(current(req, object), generator) {
            req.release(value);
            return Err(throw(
                req,
                "Error",
                "Impossible to yield from the Generator being currently run",
            ));
        }
        yield_from_generator(req, generator, object);
        with(req, generator, |g| g.send_target = None);
        return Ok(From::Delegating);
    }
    if !crate::interfaces::is_traversable(req, object.class()) {
        req.release(value);
        return Err(throw(
            req,
            "Error",
            "Can use \"yield from\" only with arrays and Traversables",
        ));
    }
    let iterator = crate::interfaces::get_iterator(req, object, false);
    req.release(value);
    let iterator = iterator?;
    if let Err(thrown) = crate::interfaces::rewind(req, iterator) {
        req.release(iterator.as_value());
        return Err(thrown);
    }
    with(req, generator, |g| {
        g.values = Delegated::Iterator(iterator);
        g.send_target = None;
    });
    Ok(From::Delegating)
}

/// `zend_generator_get_next_delegated_value`: the next element of an
/// array or a `Traversable` the generator delegated to, as its value and
/// key; `false` where they are spent, the values given up.
fn next_delegated(req: &mut Request, generator: PhpObject) -> bool {
    let values = with(req, generator, |g| std::mem::take(&mut g.values)).unwrap_or_default();
    match values {
        Delegated::Nothing => false,
        Delegated::Array { array, pos } => {
            let found = array.to_zval().as_arr().and_then(|arr| arr.live_from(pos));
            let Some((at, key, value)) = found else {
                req.release(array);
                return false;
            };
            let value = exec::value_of(value);
            req.addref(&value);
            let key = match key {
                php_heap::Key::Long(n) => Value::int(n),
                php_heap::Key::Str(s) => {
                    let s = Value::string(PhpString::of(s));
                    req.addref(&s);
                    s
                }
            };
            let old = with(req, generator, |g| {
                g.values = Delegated::Array {
                    array,
                    pos: at.saturating_add(1),
                };
                (
                    std::mem::replace(&mut g.value, value),
                    std::mem::replace(&mut g.key, key),
                )
            });
            if let Some((value, key)) = old {
                req.release(value);
                req.release(key);
            }
            true
        }
        Delegated::Iterator(iterator) => {
            let (value, key) = match crate::interfaces::delegated(req, iterator) {
                Ok(Some(found)) => found,
                Ok(None) => {
                    req.release(iterator.as_value());
                    return false;
                }
                Err(thrown) => {
                    req.release(iterator.as_value());
                    req.leave_pending(&thrown);
                    return false;
                }
            };
            let old = with(req, generator, |g| {
                g.values = Delegated::Iterator(iterator);
                (
                    std::mem::replace(&mut g.value, value),
                    std::mem::replace(&mut g.key, key),
                )
            });
            if let Some((value, key)) = old {
                req.release(value);
                req.release(key);
            }
            true
        }
    }
}

/// The delegated values given up, `zval_ptr_dtor(&generator->values)`.
fn drop_values(req: &mut Request, generator: PhpObject) {
    let values = with(req, generator, |g| std::mem::take(&mut g.values)).unwrap_or_default();
    match values {
        Delegated::Nothing => {}
        Delegated::Array { array, .. } => req.release(array),
        Delegated::Iterator(iterator) => req.release(iterator.as_value()),
    }
}

// ----- the resumption (`zend_generator_resume`) -----

/// `zend_generator_resume` (`Z/Zend/zend_generators.c:762`): serve the
/// generator whose body a resumption of `orig` runs until it yields,
/// returns or throws, following the delegation tree. A throwable the
/// body left is left pending, as php leaves `EG(exception)` set for the
/// frame that resumed it; the `Err` is a fatal.
pub(crate) fn resume(req: &mut Request, orig: PhpObject) -> Result<(), Fatal> {
    let mut generator = current(req, orig);
    if !has_frame(req, generator) {
        return Ok(());
    }
    loop {
        if is_running(req, generator) {
            let thrown = throw(req, "Error", "Cannot resume an already running generator");
            req.leave_pending(&thrown);
            return Ok(());
        }
        let do_init = read(req, orig, |g| g.do_init).unwrap_or(false);
        let started = read(req, generator, |g| g.value.tag() != TypeTag::Undef).unwrap_or(false);
        if do_init && started {
            with(req, orig, |g| g.do_init = false);
            return Ok(());
        }
        if req.fibers().active().is_some() {
            with(req, orig, |g| g.in_fiber = true);
            with(req, generator, |g| g.in_fiber = true);
        }
        with(req, orig, |g| g.at_first_yield = false);
        // The generators between the one resumed and the one that runs
        // stand below it on the stack while it runs, so a trace reads the
        // whole delegation, which php's `execute_fake` placeholder
        // expands into (`zend_generator_check_placeholder_frame`).
        let chain = delegation_chain(req, orig, generator);
        for link in &chain {
            let _ = push(req, *link);
        }
        let standing = req.vm().frames().len();
        let Some(frame) = push(req, generator) else {
            unwind_chain(req, &chain);
            return Ok(());
        };
        let delegating =
            read(req, generator, |g| !matches!(g.values, Delegated::Nothing)).unwrap_or(false);
        if delegating && next_delegated(req, generator) {
            suspend(req, generator)?;
            unwind_chain(req, &chain);
            with(req, orig, |g| {
                g.do_init = false;
                g.in_fiber = false;
            });
            with(req, generator, |g| g.in_fiber = false);
            return Ok(());
        }
        if !req.is_pending() {
            req.vm_mut().advance(frame);
        }
        let run = req.resume_loop();
        let served = run(req, frame);
        if req.vm().frames().len() > standing {
            // The body left the frame standing: a throwable it did not
            // catch, which the unwinder has already cleaned the frame
            // for (`zend_generator_close(generator, 1)` in
            // `zend_dispatch_try_catch_finally_helper`), or a fatal.
            while req.vm().frames().len() > standing.saturating_add(1) {
                req.pop_frame();
            }
            finish_on_stack(req, generator, true);
        }
        with(req, generator, |g| g.in_fiber = false);
        unwind_chain(req, &chain);
        served?;
        if req.is_pending() {
            if same(generator, orig) {
                close(req, generator, false);
                return Ok(());
            }
            generator = current(req, orig);
            throw_into_generator(req, generator);
            with(req, orig, |g| g.do_init = false);
            continue;
        }
        let returned = read(req, generator, |g| g.retval.tag() != TypeTag::Undef).unwrap_or(false);
        if (!same(generator, orig) && returned)
            || suspended_at(req, generator) == Some(Opcode::YieldFrom)
        {
            generator = current(req, orig);
            continue;
        }
        with(req, orig, |g| {
            g.do_init = false;
            g.in_fiber = false;
        });
        return Ok(());
    }
}

/// `check_node_running_in_fiber` (`Z/Zend/zend_generators.c:216`):
/// whether `generator`, or a generator delegating to it, is marked
/// [`Generator::in_fiber`]. A collection that destroys a generator whose
/// delegation runs in a suspended fiber leaves it to the fiber's
/// teardown, which gives the frames on the fiber's stack up.
///
/// ```php
/// class D { public $fiber; function __destruct() { echo "D destructed\n"; } }
/// function inner() { yield 'a'; Fiber::suspend(); echo "never\n"; }
/// function outer($d) { yield from inner(); }
/// $d = new D;
/// $g = outer($d);
/// $f = new Fiber(function () use ($g, $d) { echo $g->current(), "\n"; $g->next(); });
/// $d->fiber = $f;
/// $f->start();
/// $f = $g = $d = null;
/// echo "collect\n";
/// gc_collect_cycles();
/// echo "done\n";
/// ```
/// ```expect
/// a
/// collect
/// D destructed
/// done
/// ```
fn running_in_fiber(req: &Request, generator: PhpObject) -> bool {
    let Some((in_fiber, children)) = read(req, generator, |g| (g.in_fiber, g.children.clone()))
    else {
        return false;
    };
    in_fiber
        || children
            .into_iter()
            .any(|child| running_in_fiber(req, child))
}

/// The generators from `orig` towards `root` that delegate, `orig`
/// first and `root` left out, each with a suspended frame.
fn delegation_chain(req: &Request, orig: PhpObject, root: PhpObject) -> Vec<PhpObject> {
    let mut chain = Vec::new();
    let mut at = orig;
    while !same(at, root) {
        let suspended =
            read(req, at, |g| matches!(g.execution, Execution::Suspended(_))).unwrap_or(false);
        if suspended {
            chain.push(at);
        }
        let Some(above) = parent(req, at) else {
            break;
        };
        at = above;
        if chain.len() > 1 << 16 {
            break;
        }
    }
    chain
}

/// The frames [`delegation_chain`] put on the stack suspended again,
/// the last one pushed first.
fn unwind_chain(req: &mut Request, chain: &[PhpObject]) {
    for link in chain.iter().rev() {
        let _ = suspend(req, *link);
    }
}

/// `zend_generator_throw_exception(generator, NULL)`: the pending
/// throwable is the generator's to raise at the opline it stands at,
/// and a `yield from` over values stops.
fn throw_into_generator(req: &mut Request, generator: PhpObject) {
    drop_values(req, generator);
}

/// [`resume`] for a native: a fatal ends the request, and what the body
/// left pending is the native's `Err`, as `zend_call_function` hands
/// `EG(exception)` back.
fn resumed(req: &mut Request, generator: PhpObject) -> Result<(), Thrown> {
    if let Err(fatal) = resume(req, generator) {
        req.end_in(&fatal);
        return Err(Thrown::of(None));
    }
    pending_as_err(req)
}

fn pending_as_err(req: &mut Request) -> Result<(), Thrown> {
    if let Some(pending) = req.take_pending() {
        return Err(Thrown::of(Some(pending)));
    }
    if req.unwind_exit() {
        return Err(Thrown::of(None));
    }
    Ok(())
}

/// `zend_generator_ensure_initialized`: a generator never resumed runs
/// to its first `yield` before anything reads it.
fn ensure_initialized(req: &mut Request, generator: PhpObject) -> Result<(), Thrown> {
    let fresh = read(req, generator, |g| {
        g.value.tag() == TypeTag::Undef && g.parent.is_none()
    })
    .unwrap_or(false);
    if fresh && has_frame(req, generator) {
        resumed(req, generator)?;
        with(req, generator, |g| g.at_first_yield = true);
    }
    Ok(())
}

// ----- closing (`zend_generator_close`) -----

/// `zend_generator_close`: the frame given up and the generator
/// closed. `finished` is a frame that ran to its end; one closed before
/// it has its live temporaries and unfinished calls given up too
/// (`zend_generator_cleanup_unfinished_execution`).
fn close(req: &mut Request, generator: PhpObject, finished: bool) {
    let taken = with(req, generator, |g| {
        match std::mem::replace(&mut g.execution, Execution::Closed) {
            Execution::Suspended(suspended) => Some(Some(suspended)),
            Execution::Running => {
                g.execution = Execution::Running;
                Some(None)
            }
            Execution::Closed => None,
        }
    })
    .flatten();
    match taken {
        Some(Some(suspended)) => release_frame(req, *suspended, finished),
        Some(None) => finish_on_stack(req, generator, finished),
        None => {}
    }
}

/// [`close`] over the frame on top of the stack, which is
/// `generator`'s: the frame off the stack first, as `GENERATOR_RETURN`
/// and the unwinder move `EG(current_execute_data)` back to the resumer
/// before `zend_generator_close`, so a destructor what the frame held
/// runs sees none of it on its trace.
///
/// ```php
/// class Foo { function __destruct() { foreach ((new Exception)->getTrace() as $f) echo $f["function"], " "; echo "|\n"; } }
/// function bar() { yield from foo(); }
/// function foo() { $foo = new Foo; yield; }
/// foreach (bar() as $v);
/// ```
/// ```expect
/// __destruct bar |
/// ```
fn finish_on_stack(req: &mut Request, generator: PhpObject, finished: bool) {
    with(req, generator, |g| g.execution = Execution::Closed);
    // A generator running inside a suspended fiber has its frame on the
    // fiber's stack and not on the one the request runs on; the fiber's
    // teardown gives that frame up ([`crate::fiber`]).
    let on_top = req
        .vm()
        .frames()
        .last()
        .and_then(|state| state.generator)
        .is_some_and(|running| same(running, generator));
    if !on_top {
        return;
    }
    let Some(slots) = req.stack_mut().move_to_heap() else {
        return;
    };
    let Some((state, calls)) = req.vm_mut().suspend() else {
        return;
    };
    release_frame(
        req,
        Suspended {
            slots,
            state,
            calls,
        },
        finished,
    );
}

/// A generator's frame on top of a stack a fiber's teardown unwinds,
/// given up with the generator closed, as the unwinding of that frame
/// closes it in `zend_dispatch_try_catch_finally_helper`: its unfinished
/// calls and its live temporaries with it.
pub(crate) fn close_unwound(req: &mut Request, generator: PhpObject) {
    finish_on_stack(req, generator, false);
}

/// What a closed frame held, given up in `zend_generator_close`'s
/// order: the CVs, the extra named arguments, the receiver, the extra
/// arguments, then for an unfinished frame the calls it was building and
/// the temporaries live where it stood, and last whatever else a slot
/// still holds and the closure it ran.
fn release_frame(req: &mut Request, suspended: Suspended, finished: bool) {
    let Suspended {
        mut slots,
        state,
        calls,
    } = suspended;
    let unit = std::sync::Arc::clone(&state.unit);
    let code = exec::body_of(&unit, state.body);
    let num_cvs = code.map_or(0, |code| code.num_cvs);
    let take = |slots: &mut php_heap::Frame, at: u32| {
        let z = slots.slot(php_heap::Slot(at));
        slots.set(php_heap::Slot(at), php_heap::Zval::UNDEF);
        Value::of_zval(z)
    };
    for at in 0..num_cvs {
        let value = take(&mut slots, at);
        req.release(value);
    }
    for (name, value) in state.extra_named {
        req.release(Value::string(name));
        req.release(value);
    }
    if let Some(this) = state.this {
        req.release(this.as_value());
    }
    for extra in state.extra_args {
        req.release(extra);
    }
    if !finished && state.opline != 0 {
        for call in calls.into_iter().rev() {
            req.vm_mut().push_call(call);
            req.pop_call();
        }
        if let Some(code) = code {
            for range in &code.live_ranges {
                if range.start.0 > state.opline {
                    break;
                }
                if state.opline >= range.end.0 {
                    continue;
                }
                let slot = exec::temporary_slot(code, range.var);
                if range.kind == php_unit::LiveKind::Loop {
                    crate::handlers::ForeachVar::iterator_del(
                        req,
                        slots.slot(php_heap::Slot(slot)),
                    );
                }
                let value = take(&mut slots, slot);
                if range.kind == php_unit::LiveKind::Silence {
                    exec::restore_silenced(req, &value);
                }
                req.release(value);
            }
        }
    } else {
        for call in calls.into_iter().rev() {
            req.vm_mut().push_call(call);
            req.pop_call();
        }
    }
    let len = u32::try_from(slots.len()).unwrap_or(u32::MAX);
    for at in 0..len {
        let value = take(&mut slots, at);
        req.release(value);
    }
    if let Some(closure) = state.closure {
        req.release(closure.as_value());
    }
}

/// `zend_generator_dtor_storage` (`Z/Zend/zend_generators.c:249`): a
/// generator destroyed while suspended inside a `try` with a `finally`
/// runs the block, the delegation links cut first; one whose frame is
/// anywhere else is closed.
///
/// ```php
/// function g() { try { echo "a\n"; yield 1; echo "no\n"; } finally { echo "finally\n"; } }
/// $g = g();
/// $g->current();
/// unset($g);
/// echo "after\n";
/// ```
/// ```expect
/// a
/// finally
/// after
/// ```
///
/// A `yield` inside that block is php's `Error`:
///
/// ```php
/// function g() { try { yield 1; } finally { yield 2; } }
/// $g = g();
/// $g->current();
/// try { unset($g); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Cannot yield from finally in a force-closed generator
/// ```
fn dtor_storage(req: &mut Request, object: PhpObject) {
    with(req, object, |g| g.destroying = true);
    let root = current(req, object);
    // The path from the generator that runs to this one runs in a
    // suspended fiber: its frames are the fiber's to give up when the
    // fiber is torn down, and a `finally` this one runs then may not
    // yield.
    if read(req, root, |g| g.in_fiber).unwrap_or(false) && running_in_fiber(req, object) {
        with(req, object, |g| g.forced_close = true);
        return;
    }
    drop_values(req, object);
    match parent(req, object) {
        Some(above) => {
            remove_child(req, above, object);
            clear_link_to_root(req, object);
            with(req, object, |g| g.parent = None);
            req.release(above.as_value());
        }
        None => {
            clear_link_to_leaf(req, object);
        }
    }
    let Some(frame) = push(req, object) else {
        close(req, object, false);
        return;
    };
    let Some(serving) = req.vm().serving(frame) else {
        finish_on_stack(req, object, false);
        return;
    };
    let Some(code) = serving.code() else {
        finish_on_stack(req, object, false);
        return;
    };
    let op_num = req.vm().opline(frame);
    let mut at = innermost_try(code, op_num);
    let mut finally = None;
    while let Some(offset) = at {
        let Some(element) = code.try_catch.get(offset as usize).copied() else {
            break;
        };
        if op_num < element.finally_op.0 {
            finally = Some(element);
            break;
        }
        if op_num < element.finally_end.0 {
            let var = exec::opline_at(code, element.finally_end.0).map(|op| op.op1);
            if let Some(var) = var {
                let carried = req
                    .vm_mut()
                    .fast_call(frame, var)
                    .and_then(|cell| cell.from);
                if let Some(from) = carried {
                    exec::free_carried_value(req, frame, code, from);
                }
                let delayed = req
                    .vm_mut()
                    .fast_call(frame, var)
                    .and_then(|cell| cell.delayed.take());
                if let Some(delayed) = delayed {
                    req.release(delayed.as_value());
                }
            }
        }
        at = offset.checked_sub(1);
    }
    let Some(element) = finally else {
        finish_on_stack(req, object, false);
        return;
    };
    exec::cleanup_unfinished_calls(req, frame);
    exec::cleanup_live_vars(req, frame, code, op_num, element.finally_op.0);
    let var = exec::opline_at(code, element.finally_end.0).map(|op| op.op1);
    if let Some(var) = var
        && let Some(cell) = req.vm_mut().fast_call(frame, var)
    {
        cell.delayed = None;
        cell.from = None;
    }
    // `-1 because zend_generator_resume() will increment it`.
    req.vm_mut()
        .jump(frame, element.finally_op.0.saturating_sub(1));
    if suspend(req, object).is_err() {
        return;
    }
    let old_exception = req.take_pending();
    with(req, object, |g| g.forced_close = true);
    if let Err(fatal) = resume(req, object) {
        req.end_in(&fatal);
    }
    if let Some(old) = old_exception {
        match req.pending() {
            Some(pending) => req.set_previous(pending, old),
            None => req.leave_pending(&Thrown::of(Some(old))),
        }
    }
    close(req, object, false);
}

/// The innermost `try` element `op_num` stands in.
fn innermost_try(code: &OpArray, op_num: u32) -> Option<u32> {
    let mut current = None;
    for (at, element) in code.try_catch.iter().enumerate() {
        if op_num < element.try_op.0 {
            break;
        }
        if op_num < element.catch_op.0 || op_num < element.finally_end.0 {
            current = u32::try_from(at).ok();
        }
    }
    current
}

// ----- the iterator `get_iterator` hands out (`zend_generator_iterator_*`) -----

/// `zend_generator_get_iterator`: a closed generator cannot be walked,
/// and one walked by reference must yield by reference.
pub(crate) fn get_iterator(
    req: &mut Request,
    generator: PhpObject,
    by_ref: bool,
) -> Result<(), Thrown> {
    if !has_frame(req, generator) {
        return Err(throw(
            req,
            "Exception",
            "Cannot traverse an already closed generator",
        ));
    }
    let yields_by_ref = read(req, generator, |g| g.function)
        .flatten()
        .and_then(|function| req.function_decl(function))
        .is_some_and(|decl| decl.returns_by_ref);
    if by_ref && !yields_by_ref {
        return Err(throw(
            req,
            "Exception",
            "You can only iterate a generator by-reference if it declared that it yields by-reference",
        ));
    }
    Ok(())
}

/// `&root->value` under `FE_FETCH_RW`: the current value made a
/// reference where it is none (`ZVAL_NEW_EMPTY_REF` over it), one more
/// owner of the cell for the variable bound to it.
pub(crate) fn iterator_current_ref(
    req: &mut Request,
    generator: PhpObject,
) -> Result<crate::api::values::PhpRef, Thrown> {
    ensure_initialized(req, generator)?;
    let root = current(req, generator);
    let value = read(req, root, |g| g.value).unwrap_or_else(Value::undef);
    let cell = if let Some(cell) = value.as_php_ref() {
        *cell
    } else {
        let cell = req.reference(value.undef_as_null())?;
        with(req, root, |g| g.value = cell.as_value());
        cell
    };
    req.addref(&cell.as_value());
    Ok(cell)
}

/// `zend_generator_iterator_valid`.
pub(crate) fn iterator_valid(req: &mut Request, generator: PhpObject) -> Result<bool, Thrown> {
    ensure_initialized(req, generator)?;
    let _ = current(req, generator);
    Ok(has_frame(req, generator))
}

/// `zend_generator_iterator_get_data`: the current value, a view.
pub(crate) fn iterator_current(req: &mut Request, generator: PhpObject) -> Result<Value, Thrown> {
    ensure_initialized(req, generator)?;
    let root = current(req, generator);
    Ok(read(req, root, |g| g.value).unwrap_or_else(Value::null))
}

/// `zend_generator_iterator_get_key`: the current key, one owner, `null`
/// where there is none.
pub(crate) fn iterator_key(req: &mut Request, generator: PhpObject) -> Result<Value, Thrown> {
    ensure_initialized(req, generator)?;
    let root = current(req, generator);
    let key = read(req, root, |g| g.key).unwrap_or_else(Value::undef);
    if key.tag() == TypeTag::Undef {
        return Ok(Value::null());
    }
    let key = key.deref();
    req.addref(&key);
    Ok(key)
}

/// `zend_generator_iterator_move_forward`.
pub(crate) fn iterator_move_forward(req: &mut Request, generator: PhpObject) -> Result<(), Thrown> {
    ensure_initialized(req, generator)?;
    resumed(req, generator)
}

/// `zend_generator_rewind`: a generator is resumed to its first `yield` and
/// may be rewound only while it stands there.
///
/// ```php
/// function g() { yield 1; yield 2; }
/// $g = g();
/// foreach ($g as $v) echo $v;
/// try { foreach ($g as $v) echo $v; } catch (Exception $e) { echo "\n", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 12
/// Cannot traverse an already closed generator
/// ```
///
/// ```php
/// function g() { yield 1; yield 2; }
/// $g = g();
/// $g->rewind(); $g->rewind();
/// $g->next();
/// try { $g->rewind(); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Cannot rewind a generator that was already run
/// ```
pub(crate) fn iterator_rewind(req: &mut Request, generator: PhpObject) -> Result<(), Thrown> {
    ensure_initialized(req, generator)?;
    if !read(req, generator, |g| g.at_first_yield).unwrap_or(false) {
        return Err(throw(
            req,
            "Exception",
            "Cannot rewind a generator that was already run",
        ));
    }
    Ok(())
}

// ----- the methods -----

fn this_generator(req: &mut Request) -> Result<PhpObject, Thrown> {
    req.this()
        .filter(|this| this.native::<Generator>(req).is_some())
        .ok_or_else(|| {
            throw(
                req,
                "Error",
                "Generator method called with no generator receiver",
            )
        })
}

fn no_arguments(req: &mut Request) -> Result<(), Thrown> {
    let count = req.argument_count();
    if count == 0 {
        return Ok(());
    }
    Err(arg_count_error(
        req,
        &format!("exactly 0 arguments, {count} given"),
    ))
}

/// `RETURN_COPY_DEREF` of `value` where it is set, `null` where it is
/// not.
fn copy_deref(req: &mut Request, value: Value) -> Value {
    if value.tag() == TypeTag::Undef {
        return Value::null();
    }
    let value = value.deref();
    req.addref(&value);
    value
}

/// `Generator::rewind()`.
fn rewind(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let this = this_generator(req)?;
    iterator_rewind(req, this)?;
    Ok(Value::null())
}

/// `Generator::valid()`: whether the generator has not finished.
fn valid(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let this = this_generator(req)?;
    ensure_initialized(req, this)?;
    let _ = current(req, this);
    Ok(Value::bool(has_frame(req, this)))
}

/// `Generator::current()`: the value the innermost generator it
/// delegates to yielded last, `null` once it has finished.
fn current_value(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let this = this_generator(req)?;
    ensure_initialized(req, this)?;
    let root = current(req, this);
    if !has_frame(req, this) {
        return Ok(Value::null());
    }
    let value = read(req, root, |g| g.value).unwrap_or_else(Value::undef);
    Ok(copy_deref(req, value))
}

/// `Generator::key()`: the key beside [`current_value`]'s value; an
/// integer counted up from zero where the `yield` wrote none, past the
/// largest integer key written.
///
/// ```php
/// function g() { yield 'a'; yield 5 => 'b'; yield 'c'; yield 'k' => 'd'; yield 'e'; }
/// foreach (g() as $k => $v) echo "$k=$v ";
/// ```
/// ```expect
/// 0=a 5=b 6=c k=d 7=e
/// ```
fn key(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let this = this_generator(req)?;
    ensure_initialized(req, this)?;
    let root = current(req, this);
    if !has_frame(req, this) {
        return Ok(Value::null());
    }
    let key = read(req, root, |g| g.key).unwrap_or_else(Value::undef);
    Ok(copy_deref(req, key))
}

/// `Generator::next()`.
fn next(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let this = this_generator(req)?;
    ensure_initialized(req, this)?;
    resumed(req, this)?;
    Ok(Value::null())
}

/// `Generator::send()`: a generator never resumed runs to its first
/// `yield` first, which is then the `yield` the value is sent into;
/// what comes back is the value yielded next.
///
/// ```php
/// function g() { echo "start\n"; while (true) { $x = yield; if ($x === null) return; echo "got $x\n"; } }
/// $g = g();
/// $g->send('a');
/// $g->send('b');
/// var_dump($g->send(null), $g->valid());
/// ```
/// ```expect
/// start
/// got a
/// got b
/// NULL
/// bool(false)
/// ```
fn send(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(arg_count_error(
            req,
            &format!("exactly 1 argument, {count} given"),
        ));
    }
    let value = req.argument(1).unwrap_or_default();
    let this = this_generator(req)?;
    ensure_initialized(req, this)?;
    if !has_frame(req, this) {
        return Ok(Value::null());
    }
    let root = current(req, this);
    let target = read(req, root, |g| match g.execution {
        Execution::Running => None,
        _ => g.send_target,
    })
    .flatten();
    if let Some(slot) = target {
        let sent = value.deref();
        req.addref(&sent);
        let old = with(req, root, |g| match &mut g.execution {
            Execution::Suspended(suspended) => {
                let old = suspended.slots.slot(php_heap::Slot(slot));
                suspended.slots.set(php_heap::Slot(slot), sent.to_zval());
                Some(Value::of_zval(old))
            }
            _ => None,
        })
        .flatten();
        match old {
            Some(old) => req.release(old),
            None => req.release(sent),
        }
    }
    resumed(req, this)?;
    let root = current(req, this);
    if !has_frame(req, this) {
        return Ok(Value::null());
    }
    let value = read(req, root, |g| g.value).unwrap_or_else(Value::undef);
    Ok(copy_deref(req, value))
}

/// `Generator::throw()`: the throwable raised at the `yield` the
/// innermost generator stands at, and the value yielded next handed
/// back; over a closed generator it is raised where the call stands.
///
/// ```php
/// function g() { try { yield 1; } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; yield 2; } }
/// $g = g();
/// var_dump($g->current(), $g->throw(new Exception("e")));
/// function h() { yield 1; }
/// try { h()->throw(new Exception("x")); } catch (Exception $e) { echo "out ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// caught e
/// int(1)
/// int(2)
/// out x
/// ```
fn throw_into(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(arg_count_error(
            req,
            &format!("exactly 1 argument, {count} given"),
        ));
    }
    let given = req.argument(1).unwrap_or_default().deref();
    let throwable = given.as_object().copied().filter(|object| {
        req.class_named(b"Throwable")
            .is_some_and(|throwable| req.instance_of(object.class(), throwable))
    });
    let Some(throwable) = throwable else {
        let named = given.as_object().map_or_else(
            || String::from(given.type_name()),
            |object| req.class_name(object.class()),
        );
        return Err(arg_type_error(
            req,
            1,
            &format!("be of type Throwable, {named} given"),
        ));
    };
    req.addref(&throwable.as_value());
    let this = this_generator(req)?;
    if let Err(thrown) = ensure_initialized(req, this) {
        req.release(throwable.as_value());
        return Err(thrown);
    }
    if !has_frame(req, this) {
        return Err(Thrown::of(Some(throwable)));
    }
    let root = current(req, this);
    if is_running(req, root) {
        req.release(throwable.as_value());
        return Err(throw(
            req,
            "Error",
            "Cannot resume an already running generator",
        ));
    }
    req.leave_pending(&Thrown::of(Some(throwable)));
    throw_into_generator(req, root);
    resumed(req, this)?;
    let root = current(req, this);
    if !has_frame(req, this) {
        return Ok(Value::null());
    }
    let value = read(req, root, |g| g.value).unwrap_or_else(Value::undef);
    Ok(copy_deref(req, value))
}

/// `Generator::getReturn()`.
///
/// ```php
/// function g() { yield 1; return 3; }
/// $g = g();
/// foreach ($g as $v) {}
/// var_dump($g->getReturn());
/// try { g()->getReturn(); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// Cannot get return value of a generator that hasn't returned
/// ```
fn get_return(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let this = this_generator(req)?;
    ensure_initialized(req, this)?;
    let retval = read(req, this, |g| g.retval).unwrap_or_else(Value::undef);
    if retval.tag() == TypeTag::Undef {
        return Err(throw(
            req,
            "Exception",
            "Cannot get return value of a generator that hasn't returned",
        ));
    }
    req.addref(&retval);
    Ok(retval)
}

/// `Generator::__debugInfo()`: the function the generator runs, as
/// `Class::method` for a method.
///
/// ```php
/// function g() { yield; }
/// class C { function m() { yield; } }
/// var_dump(g(), (new C)->m());
/// ```
/// ```expect
/// object(Generator)#1 (1) {
///   ["function"]=>
///   string(1) "g"
/// }
/// object(Generator)#3 (1) {
///   ["function"]=>
///   string(4) "C::m"
/// }
/// ```
fn debug_info(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    let this = this_generator(req)?;
    let mut out = PhpArray::new(req)?;
    let function = read(req, this, |g| g.function).flatten();
    let name = function.map(|function| {
        let name = req
            .function_decl(function)
            .map(|decl| decl.name.0.to_vec())
            .unwrap_or_default();
        match req.method_class(function) {
            Some(class) => {
                let mut full = req.class_name(class).into_bytes();
                full.extend_from_slice(b"::");
                full.extend_from_slice(&name);
                full
            }
            None => name,
        }
    });
    if let Some(name) = name {
        let key = Key::from_string(PhpString::new(req, b"function")?);
        let value = Value::string(PhpString::new(req, &name)?);
        out.set(req, key, value)?;
    }
    Ok(Value::array(out))
}

/// Whether `op` is a `YIELD` or a `YIELD_FROM` of a by-reference
/// generator's op array: what the handler reads off the frame's
/// function.
pub(crate) fn yields_by_ref(req: &Request, frame: FrameRef) -> bool {
    req.vm()
        .at(frame)
        .and_then(|state| state.function)
        .and_then(|function| req.function_decl(function))
        .is_some_and(|decl| decl.returns_by_ref)
}
