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
//! it. The frame is the heap's `Frame`, its header with it,
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
use php_macros::php_class;
use php_unit::Opcode;

use crate::api::abi::{DtorObj, GcTable, Instance, ObjectHandlers, Refusals, This};
use crate::api::call::Function;
use crate::api::raising::{Thrown, throw, throw_error};
use crate::api::request::Request;
use crate::api::values::{Key, Owner, PhpArray, PhpObject, PhpString, TypeTag, Value};
use crate::classes::{Exception, Throwable};
use crate::exec::{self, FrameAt, HeapFrame, TableId};
use crate::interfaces::Iterator;

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
    frame: crate::exec::HeapFrame,
    calls: Vec<HeapFrame>,
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
pub(crate) struct ZendGenerator {
    execution: Execution,
    /// `generator->func`: what `__debugInfo` and `ReflectionGenerator`
    /// name.
    function: Option<Function>,
    /// `ZEND_CLOSURE_OBJECT(generator->func)` where the function is a
    /// closure's: one owner, the one the call's frame took, which the
    /// frame never gives up and the generator keeps until it is freed,
    /// as `zend_generator_free_storage` releases it, so the function
    /// stays readable after the frame closed.
    closure: Option<PhpObject>,
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
    /// generators on the fiber's stack to the fiber's teardown. The
    /// fiber is the one whose stack the frames stand on, no owner.
    in_fiber: Option<PhpObject>,
    /// `IS_OBJ_DESTRUCTOR_CALLED` while [`dtor_storage`] runs, which
    /// `zend_generator_update_current` reads off the object.
    destroying: bool,
    /// php's `execute_fake` placeholder, on a generator a resumption runs
    /// that was made through another: the generator resumed, whose
    /// delegation to this one a backtrace expands between this frame
    /// and the resumer's ([`placeholder_frames`]); no owner.
    resumed_through: Option<PhpObject>,
}

impl ZendGenerator {
    /// The value and the key it stands at replaced, the old pair handed
    /// back for the caller to give up.
    fn replace_current(&mut self, value: Value, key: Value) -> (Value, Value) {
        (
            std::mem::replace(&mut self.value, value),
            std::mem::replace(&mut self.key, key),
        )
    }
}

impl Default for ZendGenerator {
    /// `zend_generator_create`: the key "will be incremented on first
    /// use, so it'll start at 0", and `retval` and `values` undefined.
    fn default() -> ZendGenerator {
        ZendGenerator {
            execution: Execution::Closed,
            function: None,
            closure: None,
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
            in_fiber: None,
            destroying: false,
            resumed_through: None,
        }
    }
}

/// `zend_generator_handlers`: the slots `zend_register_generator_ce`
/// overrides over `std_object_handlers`.
struct GeneratorHandlers;

impl ObjectHandlers for GeneratorHandlers {
    /// `zend_generator_dtor_storage`.
    fn dtor_obj(&self) -> DtorObj {
        DtorObj::Own(dtor_storage)
    }

    /// `zend_generator_free_storage`: the frame closed, and the
    /// closure, the value, the key and what the body returned given up.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        close(req, object, false);
        let held = with(req, object, |generator| {
            let values = std::mem::take(&mut generator.values);
            [
                generator
                    .closure
                    .take()
                    .map_or(Value::undef(), |closure| closure.as_value()),
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

    /// `zend_generator_handlers.clone_obj = NULL`.
    fn refusals(&self) -> Refusals {
        Refusals::NONE.cloning()
    }

    /// `zend_generator_get_constructor`.
    fn get_constructor(&self, req: &mut Request, _: PhpObject) -> Result<Option<Function>, Thrown> {
        Err(throw_error(
            req,
            "Error",
            "The \"Generator\" class is reserved for internal use and cannot be manually instantiated",
        ))
    }

    /// `zend_generator_get_gc` (`Z/Zend/zend_generators.c:440`): a
    /// closed generator's value, key, what the body returned and its
    /// closure; nothing for one that is running, whose frame the stack
    /// it runs on holds; and a suspended one's frame as
    /// [`frame_gc`] names it, so a cycle through a suspended
    /// generator's variables is found.
    ///
    /// ```php
    /// class C { public $g; function __destruct() { echo "C::__destruct\n"; } }
    /// function gen() { $c = new C; $c->g = yield; yield; }
    /// $g = gen();
    /// $g->send($g);
    /// unset($g);
    /// var_dump(gc_collect_cycles() > 0);
    /// echo "done\n";
    /// ```
    /// ```expect
    /// C::__destruct
    /// bool(true)
    /// done
    /// ```
    ///
    /// A frame that made a variable by name has a symbol table, which is
    /// returned as the generator's table and walked after its values, the
    /// CVs among its rows, so what the variables reach is found before
    /// what the generator yielded:
    ///
    /// ```php
    /// class D { public $self; function __construct(public $n) { $this->self = $this; } function __destruct() { echo $this->n, "\n"; } }
    /// function g() { $c = new D('cv'); $y = 'x'; $$y = new D('by name'); $h = yield; yield new D('yielded'); }
    /// $g = g();
    /// $g->send($g);
    /// gc_collect_cycles();
    /// unset($g);
    /// gc_collect_cycles();
    /// echo "done\n";
    /// ```
    /// ```expect
    /// cv
    /// by name
    /// yielded
    /// done
    /// ```
    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        let Some(generator) = object.native::<ZendGenerator>(req) else {
            return GcTable::Properties;
        };
        match &generator.execution {
            Execution::Closed => {
                visit(generator.value);
                visit(generator.key);
                visit(generator.retval);
                if let Some(closure) = generator.closure {
                    visit(closure.as_value());
                }
                GcTable::Properties
            }
            Execution::Running => GcTable::Properties,
            Execution::Suspended(frame) => {
                let calls: Vec<FrameAt<'_>> = frame
                    .calls
                    .iter()
                    .map(|call| req.kept_frame(call))
                    .collect();
                let state = req.kept_frame(&frame.frame);
                match frame_gc(generator, state, &calls, visit) {
                    Some(table) => {
                        let mut rows = Vec::new();
                        // The frame is the table's only one: a generator
                        // suspends at its own `yield`, never inside a
                        // file it included.
                        req.gc_symbol_rows(
                            table,
                            &|_, at| state.frame.slot(php_heap::Slot(at)),
                            &mut |value| rows.push(value),
                        );
                        GcTable::Rows(rows)
                    }
                    None => GcTable::Properties,
                }
            }
        }
    }
}

static GENERATOR: GeneratorHandlers = GeneratorHandlers;

/// Whether `object` is a `Generator`, `ce == zend_ce_generator`.
pub(crate) fn is_generator(req: &Request, object: PhpObject) -> bool {
    std::ptr::addr_eq(req.handlers_of(object.class()), &raw const GENERATOR)
}

/// `f` over the payload of `object`; none for an object that has none.
fn with<R>(
    req: &mut Request,
    object: PhpObject,
    f: impl FnOnce(&mut ZendGenerator) -> R,
) -> Option<R> {
    object.native_mut::<ZendGenerator>(req).map(f)
}

/// A read of the payload.
fn read<R>(req: &Request, object: PhpObject, f: impl FnOnce(&ZendGenerator) -> R) -> Option<R> {
    object.native::<ZendGenerator>(req).map(f)
}

/// `generator->value`, the value it yielded last, borrowed: no owner
/// taken, `UNDEF` before its first `yield`.
pub(crate) fn value(req: &Request, generator: PhpObject) -> Value {
    read(req, generator, |g| g.value).unwrap_or_else(Value::undef)
}

/// `zend_generator_frame_gc` (`Z/Zend/zend_generators.c:399`): what a
/// generator with a frame holds, its value, key, return value and
/// delegated values, its frame by [`exec::unfinished_execution_gc`]
/// with the calls it was building, and the generator it delegates to;
/// and the frame's symbol table, which php returns. The frame is the one
/// the generator keeps while it is suspended, or the one on the stack of
/// a suspended fiber it runs on.
pub(crate) fn frame_gc(
    generator: &ZendGenerator,
    state: FrameAt<'_>,
    calls: &[FrameAt<'_>],
    visit: &mut dyn FnMut(Value),
) -> Option<TableId> {
    visit(generator.value);
    visit(generator.key);
    visit(generator.retval);
    match &generator.values {
        Delegated::Nothing => {}
        Delegated::Array { array, .. } => visit(*array),
        Delegated::Iterator(iterator) => visit(iterator.as_value()),
    }
    let table = exec::unfinished_execution_gc(state, calls, visit);
    if let Some(parent) = generator.parent {
        visit(parent.as_value());
    }
    table
}

/// [`frame_gc`] over a running generator's frame on a suspended
/// fiber's stack, which the generator's own `get_gc` leaves to the
/// fiber; none for an object that is no running generator.
pub(crate) fn running_frame_gc(
    req: &Request,
    object: PhpObject,
    state: FrameAt<'_>,
    visit: &mut dyn FnMut(Value),
) -> Option<TableId> {
    let generator = object.native::<ZendGenerator>(req)?;
    matches!(generator.execution, Execution::Running)
        .then(|| frame_gc(generator, state, &[], visit))
        .flatten()
}

/// `ZEND_GENERATOR_CURRENTLY_RUNNING`: a resumption holds the frame on
/// a stack.
pub(crate) fn is_running(req: &Request, object: PhpObject) -> bool {
    read(req, object, |generator| {
        matches!(generator.execution, Execution::Running)
    })
    .unwrap_or(false)
}

/// `generator->execute_data != NULL`: suspended or running.
pub(crate) fn has_frame(req: &Request, object: PhpObject) -> bool {
    read(req, object, |generator| {
        !matches!(generator.execution, Execution::Closed)
    })
    .unwrap_or(false)
}

/// `generator->func` and the closure it is, which outlive the frame.
pub(crate) fn served(req: &Request, object: PhpObject) -> (Option<Function>, Option<PhpObject>) {
    read(req, object, |generator| {
        (generator.function, generator.closure)
    })
    .unwrap_or((None, None))
}

/// The fiber whose stack a resumption of `object` runs on, while one
/// is under way.
pub(crate) fn in_fiber(req: &Request, object: PhpObject) -> Option<PhpObject> {
    read(req, object, |generator| generator.in_fiber).flatten()
}

/// The engine half of a frame off the stack, where the generator's is.
pub(crate) fn suspended_state(req: &Request, object: PhpObject) -> Option<FrameAt<'_>> {
    object
        .native::<ZendGenerator>(req)
        .and_then(|generator| match &generator.execution {
            Execution::Suspended(suspended) => Some(req.kept_frame(&suspended.frame)),
            Execution::Running | Execution::Closed => None,
        })
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

/// The temporary the opline a suspended frame stands at writes, the
/// one its `result` operand names.
fn suspended_out(req: &Request, object: PhpObject) -> Option<u32> {
    read(req, object, |generator| match &generator.execution {
        Execution::Suspended(suspended) => {
            let state = req.kept_frame(&suspended.frame);
            let code = state.code()?;
            let op = exec::opline_at(code, state.opline())?;
            Some(exec::temporary_slot(code, op.out))
        }
        _ => None,
    })
    .flatten()
}

/// The opline a suspended frame stands at, and its op array's opcode
/// there.
fn suspended_at(req: &Request, object: PhpObject) -> Option<Opcode> {
    read(req, object, |generator| match &generator.execution {
        Execution::Suspended(suspended) => {
            let state = req.kept_frame(&suspended.frame);
            exec::opline_at(state.code()?, state.opline()).map(|op| op.opcode)
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
                resume(req, generator).ok();
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
            let out = suspended_out(req, root);
            let written = with(req, root, |g| {
                let Execution::Suspended(suspended) = &mut g.execution else {
                    return Some(retval);
                };
                let slot = out?;
                let old = suspended.frame.slot(php_heap::Slot(slot));
                suspended.frame.set(php_heap::Slot(slot), retval.to_zval());
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
    let Suspended { mut frame, calls } = *suspended;
    frame.header_mut().caller = crate::exec::Caller::Outside;
    let table = req.kept_frame(&frame).symbol_table();
    let frame = req.resume_frame(frame, calls).ok()?;
    if let Some(table) = table {
        req.resume_generator_table(table, frame);
    }
    Some(frame)
}

/// The frame on top of the stack, which is `generator`'s, off it again
/// and suspended: what `YIELD` and `YIELD_FROM` leave.
pub(crate) fn suspend(req: &mut Request, generator: PhpObject) -> Result<(), Fatal> {
    let Some((frame, calls)) = req.suspend_frame()? else {
        return Err(Fatal::Error(String::from(
            "InternalError: a generator suspended with no frame on the stack",
        )));
    };
    with(req, generator, |g| {
        g.execution = Execution::Suspended(Box::new(Suspended { frame, calls }));
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

/// `f` with the suspended frame of `generator` on top of the running
/// stack, where a resumption stands it, and its place there; suspended
/// again after. None where the generator is not suspended or its frame
/// is not the one `identity` names, so a frame kept past the
/// generator's end is refused and never read as another's. The
/// generator is running meanwhile, so what `f` serves cannot resume it.
pub(crate) fn with_kept_frame<R>(
    req: &mut Request,
    generator: PhpObject,
    identity: crate::api::request::Identity,
    f: impl FnOnce(&mut Request, FrameRef) -> R,
) -> Option<R> {
    let kept = read(req, generator, |g| match &g.execution {
        Execution::Suspended(kept) => kept.frame.header().identity.get() == Some(identity),
        Execution::Running | Execution::Closed => false,
    })?;
    if !kept {
        return None;
    }
    let frame = push(req, generator)?;
    let out = f(req, frame);
    suspend(req, generator).ok()?;
    Some(out)
}

/// `f` with the frames of a suspended `generator` and of the generators
/// it delegates to on the stack, the root's on top, as a resumption
/// stands them ([`resume`]), and the stack's height below them; each is
/// suspended again after it. php's `ReflectionGenerator::getTrace`
/// links the same frames into one `prev_execute_data` chain for as
/// long as it reads it. A root a resumption is running is left where
/// it stands, and the frames below it are pushed alone.
pub(crate) fn with_frames<R>(
    req: &mut Request,
    generator: PhpObject,
    f: impl FnOnce(&mut Request, usize) -> R,
) -> R {
    let root = current(req, generator);
    let floor = req.vm().depth();
    let chain = delegation_chain(req, generator, root);
    for link in &chain {
        let _ = push(req, *link);
    }
    let root_pushed = push(req, root).is_some();
    let out = f(req, floor);
    if root_pushed {
        suspend(req, root).ok();
    }
    unwind_chain(req, &chain);
    out
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
///
/// The frame is copied into an allocation of its own, `emalloc(used_stack)`,
/// beside the object's `zend_generator`, so what `memory_get_usage()`
/// counts is php's, and a call made while the generator is suspended
/// lands on the page it left and not on its variables:
///
/// ```php
/// function gen($a, $b) { $c = [$a + $b]; yield $c[0]; $d = [$a, $b]; yield $d[1]; yield $c[0]; }
/// function other($p) { $z = str_repeat('q', 10); $y = [$z, $z]; $w = 5; return count($y) + $p; }
/// function deeper($p) { $q1 = 1; $q2 = 2; return other($p) + other($p); }
/// $keep = []; $m = memory_get_usage();
/// for ($i = 0; $i < 100; $i++) $keep[] = gen(1, 2);
/// echo (memory_get_usage() - $m) / 100, "\n";
/// $g = gen(1, 2);
/// echo $g->current(), deeper(1), deeper(2);
/// $g->next();
/// echo $g->current(), deeper(3);
/// $g->next();
/// echo $g->current(), "\n";
/// ```
/// ```expect
/// 666.16
/// 3682103
/// ```
///
/// ```php
/// function gen() { $a = 'xyz'; $b = [1, 2]; yield $a; yield $b[1]; yield $a; }
/// function mk() { return gen(); }
/// function other($p) { $z = str_repeat('q', 10); $y = [$z, $z]; $w = 5; return count($y) + $p; }
/// function deeper($p) { $q1 = 1; $q2 = 2; return other($p) + other($p); }
/// $g = mk();
/// echo $g->current(), deeper(1), deeper(2);
/// $g->next();
/// echo $g->current(), deeper(3);
/// $g->next();
/// echo $g->current(), "\n";
/// ```
/// ```expect
/// xyz68210xyz
/// ```
pub(crate) fn create(req: &mut Request, frame: FrameRef) -> Result<crate::tier::Stepped, Fatal> {
    let entry = req.stack_mut().entry() == frame;
    let returns_to = req.vm().returns_to(frame);
    let caller = req.returns_into(frame);
    if !entry && returns_to.is_none() {
        req.leave_frame(frame);
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
    let (function, closure) = req
        .vm()
        .at(frame)
        .map_or((None, None), |state| (state.function, state.closure));
    if let Some(extra) = req.annex_mut(frame) {
        extra.generator = Some(object);
    }
    // `ZEND_CALL_RELEASE_THIS` and `Z_ADDREF` on a receiver the frame
    // was lent: the generator's frame outlives the call that lent it.
    let lent = req.vm_mut().at_mut(frame).and_then(|state| {
        state.returns_to = None;
        match state.receiver() {
            Some(crate::exec::Receiver::Lent(this)) => {
                state.set_receiver(Some(crate::exec::Receiver::Owned(this)));
                Some(this)
            }
            _ => None,
        }
    });
    if let Some(this) = lent {
        req.own_receiver(this);
    }
    with(req, object, |g| {
        g.function = function;
        g.closure = closure;
    });
    suspend(req, object)?;
    if entry {
        req.set_returned(object.as_value());
        return Ok(crate::tier::Stepped::Done);
    }
    if let Some((at, slot)) = returns_to {
        req.replace_slot(at, slot, object.as_value());
    }
    req.vm_mut().advance(caller);
    Ok(crate::tier::Stepped::Moved(caller))
}

/// The generator whose frame `frame` is, `zend_get_running_generator`.
pub(crate) fn running(req: &Request, frame: FrameRef) -> Result<PhpObject, Fatal> {
    req.frame_at(frame)
        .and_then(FrameAt::generator)
        .ok_or_else(|| {
            Fatal::Error(String::from(
                "InternalError: a generator opcode on a frame that is no generator's",
            ))
        })
}

/// The value and the key a generator stood at, given up value first as
/// php's `zval_ptr_dtor`s over them go; nothing for no generator.
fn release_current(req: &mut Request, old: Option<(Value, Value)>) {
    if let Some((value, key)) = old {
        req.release(value);
        req.release(key);
    }
}

/// Whether `generator` is in `ZEND_GENERATOR_FORCED_CLOSE`.
pub(crate) fn forced_close(req: &Request, generator: PhpObject) -> bool {
    read(req, generator, |g| g.forced_close).unwrap_or(false)
}

/// The value and the key a `YIELD` is about to replace, given up
/// first as the handler's `zval_ptr_dtor`s give them up.
pub(crate) fn drop_current(req: &mut Request, generator: PhpObject) {
    let old = with(req, generator, |g| {
        g.replace_current(Value::undef(), Value::undef())
    });
    release_current(req, old);
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
        g.replace_current(value, key)
    });
    release_current(req, old);
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
        return Err(throw_error(
            req,
            "Error",
            "Can use \"yield from\" only with arrays and Traversables",
        ));
    };
    if is_generator(req, object) {
        if !has_frame(req, object) {
            req.release(value);
            return Err(throw_error(
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
            return Err(throw_error(
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
        return Err(throw_error(
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
                g.replace_current(value, key)
            });
            release_current(req, old);
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
                g.replace_current(value, key)
            });
            release_current(req, old);
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
            let thrown = throw_error(req, "Error", "Cannot resume an already running generator");
            req.leave_pending(&thrown);
            return Ok(());
        }
        let do_init = read(req, orig, |g| g.do_init).unwrap_or(false);
        let started = read(req, generator, |g| g.value.tag() != TypeTag::Undef).unwrap_or(false);
        if do_init && started {
            with(req, orig, |g| g.do_init = false);
            return Ok(());
        }
        if let Some(fiber) = req.fibers().active() {
            with(req, orig, |g| g.in_fiber = Some(fiber));
            with(req, generator, |g| g.in_fiber = Some(fiber));
        }
        with(req, orig, |g| g.at_first_yield = false);
        // The generators between the one resumed and the one that runs
        // stay suspended, and a trace reads them through the placeholder
        // php links the running frame to (`execute_fake`), which
        // `zend_generator_check_placeholder_frame` expands.
        let through = (!same(generator, orig)).then_some(orig);
        let standing = req.vm().depth();
        let Some(frame) = push(req, generator) else {
            return Ok(());
        };
        with(req, generator, |g| g.resumed_through = through);
        let delegating =
            read(req, generator, |g| !matches!(g.values, Delegated::Nothing)).unwrap_or(false);
        if delegating && req.is_pending() {
            // `zend_generator_throw_exception`: a throwable raised into a
            // generator stops the `yield from` over values it stands in,
            // and their release is served with the generator's frame
            // current, so a destructor it serves is called from there.
            drop_values(req, generator);
        } else if delegating && next_delegated(req, generator) {
            with(req, generator, |g| g.resumed_through = None);
            suspend(req, generator)?;
            with(req, orig, |g| {
                g.do_init = false;
                g.in_fiber = None;
            });
            with(req, generator, |g| g.in_fiber = None);
            return Ok(());
        }
        if !req.is_pending() {
            req.vm_mut().advance(frame);
        }
        crate::observer::generator_resumed(req, frame);
        let run = req.resume_loop();
        let served = run(req, frame);
        if req.vm().depth() > standing {
            // The body left the frame standing: a throwable it did not
            // catch, which the unwinder has already cleaned the frame
            // for (`zend_generator_close(generator, 1)` in
            // `zend_dispatch_try_catch_finally_helper`), or a fatal.
            while req.vm().depth() > standing.saturating_add(1) {
                req.pop_frame();
            }
            finish_on_stack(req, generator, true);
        }
        with(req, generator, |g| {
            g.in_fiber = None;
            g.resumed_through = None;
        });
        served?;
        if req.is_pending() {
            if same(generator, orig) {
                close(req, generator, false);
                return Ok(());
            }
            generator = current(req, orig);
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
            g.in_fiber = None;
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
    let Some((in_fiber, children)) = read(req, generator, |g| {
        (g.in_fiber.is_some(), g.children.clone())
    }) else {
        return false;
    };
    in_fiber
        || children
            .into_iter()
            .any(|child| running_in_fiber(req, child))
}

/// `zend_generator_check_placeholder_frame` (`Z/Zend/zend_generators.c`):
/// the kept frames of the generators a running `root` was resumed
/// through, the one it runs for first and the one resumed last, which a
/// backtrace reads between `root`'s frame and the resumer's; none where
/// `root` was resumed itself.
///
/// ```php
/// function inner() { yield 1; debug_print_backtrace(); yield 2; }
/// function middle() { yield from inner(); }
/// function outer() { yield from middle(); }
/// $g = outer();
/// $g->current();
/// $g->next();
/// ```
/// ```expect
/// #0 %s(3): inner()
/// #1 %s(4): middle()
/// #2 [internal function]: outer()
/// #3 %s(7): Generator->next()
/// ```
pub(crate) fn placeholder_frames(req: &Request, root: PhpObject) -> Vec<FrameAt<'_>> {
    let Some(orig) = read(req, root, |g| g.resumed_through).flatten() else {
        return Vec::new();
    };
    kept_frames(req, orig, root)
}

/// The kept frames of the generators from `orig` towards `root`, the one
/// `root` runs for first and `orig`'s last: what php links under
/// `root`'s frame through `orig`'s `execute_fake`.
pub(crate) fn kept_frames(req: &Request, orig: PhpObject, root: PhpObject) -> Vec<FrameAt<'_>> {
    delegation_chain(req, orig, root)
        .into_iter()
        .rev()
        .filter_map(|link| match &link.native::<ZendGenerator>(req)?.execution {
            Execution::Suspended(kept) => Some(req.kept_frame(&kept.frame)),
            Execution::Running | Execution::Closed => None,
        })
        .collect()
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
        suspend(req, *link).ok();
    }
}

/// [`resume`] for a native: a fatal ends the request, and what the body
/// left pending is the native's `Err`, as `zend_call_function` hands
/// `EG(exception)` back.
fn resumed(req: &mut Request, generator: PhpObject) -> Result<(), Thrown> {
    if let Err(fatal) = resume(req, generator) {
        req.end_in(&fatal);
        return Err(Thrown::of(None));
    }
    crate::api::raising::pending(req)
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
        .innermost_user_frame()
        .and_then(|top| req.frame_at(top))
        .and_then(FrameAt::generator)
        .is_some_and(|running| same(running, generator));
    if !on_top {
        return;
    }
    let Ok(Some((frame, calls))) = req.suspend_frame() else {
        return;
    };
    // `EG(current_execute_data)` goes back to the placeholder the frame
    // stood under, which stands while what the frame held is released.
    let through = read(req, generator, |g| g.resumed_through.is_some()).unwrap_or(false);
    if through {
        req.vm_mut().push_placeholder(generator);
    }
    release_frame(req, Suspended { frame, calls }, finished);
    if through {
        req.vm_mut().pop_placeholder();
    }
}

/// What a closed frame held, given up in `zend_generator_close`'s
/// order: the symbol table the frame was given, the CVs, the extra
/// named arguments, the receiver, the extra arguments, then for an
/// unfinished frame the calls it was building and the temporaries live
/// where it stood, and last whatever else a slot still holds. The
/// closure it ran is the generator's to give up
/// ([`ZendGenerator::closure`]).
///
/// A variable named by a variable lives in the table and not in a CV,
/// and is the table's to give up, so a generator that holds itself
/// there is freed by the collector, the table given up as the
/// collector's destructor phase closes it; a variable named by a
/// variable reads the frame's CVs wherever the generator is resumed:
///
/// ```php
/// function gen() { $n = 'a'; $$n = yield; $m = 'n'; yield; echo $a === null ? 'gone' : 'held', ' ', $$m, "\n"; yield; }
/// $g = gen(); $g->send($g); unset($g);
/// var_dump(gc_collect_cycles());
/// function deep($x) { $y = 'y'; $GLOBALS['h']->next(); }
/// $h = gen(); $h->send(null); deep(1);
/// function other() { $q = 'q'; $$q = 1; }
/// $k = gen(); $k->send(null); other(); $k->next();
/// ```
/// ```expect
/// int(1)
/// gone a
/// gone a
/// ```
fn release_frame(req: &mut Request, suspended: Suspended, finished: bool) {
    let Suspended {
        frame: mut slots,
        calls,
    } = suspended;
    let header = slots.header().clone();
    let extra = req.take_annex(&header).unwrap_or_default();
    if let Some(table) = extra.symbol_table {
        req.free_generator_table(table);
    }
    let unit = header
        .function
        .and_then(|function| req.user_function(function))
        .map(|user| (std::sync::Arc::clone(&user.unit), user.body));
    let code = unit
        .as_ref()
        .and_then(|(unit, body)| exec::body_of(unit, *body));
    let num_cvs = code.map_or(0, |code| code.num_cvs);
    let take = |slots: &mut crate::exec::HeapFrame, at: u32| {
        let z = slots.slot(php_heap::Slot(at));
        slots.set(php_heap::Slot(at), php_heap::Zval::UNDEF);
        Value::of_zval(z)
    };
    for at in 0..num_cvs {
        let value = take(&mut slots, at);
        req.release(value);
    }
    req.release_named(extra.extra_named);
    if let Some(this) = header.receiver().and_then(crate::exec::Receiver::owned) {
        req.release(this.as_value());
    }
    // `zend_vm_stack_free_extra_args`: the arguments past the declared
    // ones, past the CVs and the temporaries.
    let own = code.map_or(0, |code| code.num_cvs.saturating_add(code.num_tmps));
    for at in own..u32::try_from(slots.len()).unwrap_or(u32::MAX) {
        let value = take(&mut slots, at);
        req.release(value);
    }
    let opline = header.opline;
    if !finished && opline != 0 {
        for call in calls.into_iter().rev() {
            req.release_frozen_call(call);
        }
        if let Some(code) = code {
            for range in &code.live_ranges {
                if range.start.0 > opline {
                    break;
                }
                if opline >= range.end.0 {
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
                exec::release_live(req, range.kind, value);
            }
        }
    } else {
        for call in calls.into_iter().rev() {
            req.release_frozen_call(call);
        }
    }
    let len = u32::try_from(slots.len()).unwrap_or(u32::MAX);
    for at in 0..len {
        let value = take(&mut slots, at);
        req.release(value);
    }
    slots.free(req.heap_mut());
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
    if read(req, root, |g| g.in_fiber.is_some()).unwrap_or(false) && running_in_fiber(req, object) {
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
    let Some(serving) = req.serving(frame) else {
        finish_on_stack(req, object, false);
        return;
    };
    let Some(code) = serving.code() else {
        finish_on_stack(req, object, false);
        return;
    };
    let op_num = req.vm().opline(frame);
    let mut at = exec::innermost_try(code, op_num);
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
                let carried = req.fast_call(frame, var).and_then(|cell| cell.from);
                if let Some(from) = carried {
                    exec::free_carried_value(req, frame, code, from);
                }
                let delayed = req
                    .fast_call(frame, var)
                    .and_then(|cell| cell.delayed.take());
                if let Some(delayed) = delayed {
                    req.release_raised(delayed);
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
        && let Some(cell) = req.fast_call(frame, var)
    {
        cell.delayed = None;
        cell.from = None;
    }
    // `-1 because zend_generator_resume() will increment it`.
    req.vm_mut()
        .stand(frame, element.finally_op.0.saturating_sub(1));
    if suspend(req, object).is_err() {
        return;
    }
    let old_exception = req.take_raised();
    with(req, object, |g| g.forced_close = true);
    if let Err(fatal) = resume(req, object) {
        req.end_in(&fatal);
    }
    if let Some(old) = old_exception {
        req.raise_under(old);
    }
    close(req, object, false);
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

/// `RETURN_COPY_DEREF` of `value` where it is set, `null` where it is
/// not: the view, which the engine takes its owner of as the return.
fn copy_deref(value: Value) -> Value {
    if value.tag() == TypeTag::Undef {
        return Value::null();
    }
    value.deref()
}

/// The receiver's value past `root`'s: `null` once the generator has
/// finished, and otherwise what `pick` reads off the innermost generator
/// it delegates to, `RETURN_COPY_DEREF` where it is set.
fn yielded_now(req: &mut Request, this: PhpObject, pick: fn(&ZendGenerator) -> Value) -> Value {
    let root = current(req, this);
    if !has_frame(req, this) {
        return Value::null();
    }
    let value = read(req, root, pick).unwrap_or_else(Value::undef);
    copy_deref(value)
}

/// `final class Generator implements Iterator`, `@strict-properties`
/// and `@not-serializable`, its methods in the stub's order over the
/// [`ZendGenerator`] its objects carry and [`GeneratorHandlers`].
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
///
/// Its methods walk it by hand as `foreach` walks it through its
/// iterator functions:
///
/// ```php
/// function g() { yield 'a' => 1; yield 'b' => 2; }
/// $g = g();
/// $g->rewind();
/// var_dump($g->valid(), $g->key(), $g->current());
/// $g->next();
/// var_dump($g->key(), $g->current());
/// $g->next();
/// var_dump($g->valid(), $g->current(), $g->key());
/// ```
/// ```expect
/// bool(true)
/// string(1) "a"
/// int(1)
/// string(1) "b"
/// int(2)
/// bool(false)
/// NULL
/// NULL
/// ```
#[php_class(
    final,
    not_serializable,
    dynamic_properties = forbidden,
    implements(Iterator),
    payload = ZendGenerator,
    // `emalloc(sizeof(zend_generator))`, 280 bytes whole.
    size = 296,
    handlers = GENERATOR
)]
pub(crate) struct Generator;

#[php_class]
impl Generator {
    /// `Generator::rewind()`.
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        iterator_rewind(req, this.object())
    }

    /// `Generator::valid()`: whether the generator has not finished.
    fn valid(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        iterator_valid(req, this.object())
    }

    /// `Generator::current()`: the value the innermost generator it
    /// delegates to yielded last, `null` once it has finished.
    fn current(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let this = this.object();
        ensure_initialized(req, this)?;
        Ok(yielded_now(req, this, |g| g.value))
    }

    /// `Generator::key()`: the key beside [`Generator::current`]'s value; an
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
    fn key(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let this = this.object();
        ensure_initialized(req, this)?;
        Ok(yielded_now(req, this, |g| g.key))
    }

    /// `Generator::next()`.
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        iterator_move_forward(req, this.object())
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
    fn send(req: &mut Request, this: This<Self>, value: Value) -> Result<Value, Thrown> {
        let this = this.object();
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
            let sent = value;
            req.addref(&sent);
            let old = with(req, root, |g| match &mut g.execution {
                Execution::Suspended(suspended) => {
                    let old = suspended.frame.slot(php_heap::Slot(slot));
                    suspended.frame.set(php_heap::Slot(slot), sent.to_zval());
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
        Ok(yielded_now(req, this, |g| g.value))
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
    ///
    /// A `yield from` over values stops where the throwable arrives, and
    /// what it still held is released with the generator's frame current
    /// (`zend_generator_throw_exception`):
    ///
    /// ```php
    /// function gen() { yield from [1, new class { function __destruct() { debug_print_backtrace(); } }]; }
    /// $g = gen(); $g->current();
    /// try { $g->throw(new Exception("x")); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// #0 %s(2): class@anonymous->__destruct()
    /// #1 [internal function]: gen()
    /// #2 %s(4): Generator->throw(Object(Exception))
    /// x
    /// ```
    fn throw(
        req: &mut Request,
        this: This<Self>,
        exception: Instance<Throwable>,
    ) -> Result<Value, Thrown> {
        // The pending slot's owner, taken first as php's `Z_ADDREF_P`
        // takes it, and handed to the throw or given up on every path.
        let throwable = Owner::take(req, exception.object());
        let this = this.object();
        if let Err(thrown) = ensure_initialized(req, this) {
            throwable.give_up(req);
            return Err(thrown);
        }
        if !has_frame(req, this) {
            return Err(crate::api::raising::throw_object(req, throwable.hand_on()));
        }
        let root = current(req, this);
        if is_running(req, root) {
            throwable.give_up(req);
            return Err(throw_error(
                req,
                "Error",
                "Cannot resume an already running generator",
            ));
        }
        // Told with the generator's frame current, its
        // `prev_execute_data` the caller's, as php switches to it before
        // it throws (`Z/Zend/zend_generators.c:524-531`), so a link
        // reading the trace sees the generator on top at its `yield`.
        let thrown = in_context(req, root, |req| {
            crate::api::raising::throw_object(req, throwable.hand_on())
        });
        let Some(thrown) = thrown else {
            return Err(crate::api::raising::fatal(
                req,
                "InternalError: a generator's frame not suspended again after Generator::throw() told its links",
            ));
        };
        req.leave_pending(&thrown);
        resumed(req, this)?;
        Ok(yielded_now(req, this, |g| g.value))
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
    fn getReturn(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let this = this.object();
        ensure_initialized(req, this)?;
        let retval = read(req, this, |g| g.retval).unwrap_or_else(Value::undef);
        if retval.tag() == TypeTag::Undef {
            return Err(throw(
                req,
                "Exception",
                "Cannot get return value of a generator that hasn't returned",
            ));
        }
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
    fn __debugInfo(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let this = this.object();
        let mut out = PhpArray::new(req)?;
        let function = read(req, this, |g| g.function).flatten();
        let name = function.map(|function| {
            let name = req
                .function_decl(function)
                .map(|decl| decl.name.0.to_vec())
                .unwrap_or_default();
            match req.method_class(function) {
                Some(class) => {
                    let mut full = req.class_name_bytes(class);
                    full.extend_from_slice(b"::");
                    full.extend_from_slice(&name);
                    full
                }
                None => name,
            }
        });
        if let Some(name) = name {
            let key = PhpString::new(req, b"function")?;
            let key = Key::from_string(req, key);
            let value = PhpString::new(req, &name)?;
            out.set(req, key, value.into_value())?;
        }
        Ok(out)
    }
}

/// `class ClosedGeneratorException extends Exception`, what a generator
/// waiting on one that was torn down raises.
#[php_class(extends(Exception))]
pub(crate) struct ClosedGeneratorException;

#[php_class]
impl ClosedGeneratorException {}

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
