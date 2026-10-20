//! A `Generator`'s or a `Fiber`'s execution read from outside it, what
//! `ReflectionGenerator` and `ReflectionFiber` answer from
//! (`Z/ext/reflection/php_reflection.c` at 8.5.9): php's
//! `generator->execute_data` and `fiber->execute_data`, and the trace
//! `zend_fetch_debug_backtrace` makes over that execution alone.

use super::Frames;
use super::traces::Rows;
use super::{Expand, Link, Request, Slots, links_of};
use crate::api::abi::Callable;
use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::values::{ClassId, Owner, PhpArray, PhpObject, adopted_some};
use crate::exec::{FrameAt, Vm};
use crate::fiber::Standing;
use crate::generator;

/// A generator's or a fiber's execution where it stands, one view for
/// both: what the object was made to serve, and the frame it stands at.
/// A view: it takes no owner of anything it names.
#[derive(Debug, Clone)]
pub struct Execution {
    /// `generator->func`, which outlives the generator's frame; none for
    /// a fiber, which serves `callable`.
    pub function: Option<Function>,
    /// The class `function` is a method of, `func->op_array.scope`.
    pub scope: Option<ClassId>,
    /// The closure `function` is, `ZEND_CLOSURE_OBJECT(func)`, which the
    /// generator keeps until it is freed.
    pub closure: Option<PhpObject>,
    /// A fiber's `fci.function_name`, from its construction until it is
    /// dead; none for a generator.
    pub callable: Option<Callable>,
    /// The object whose frame runs when the execution goes on: for a
    /// generator the root of its delegation, `zend_generator_get_current`,
    /// and for a fiber the fiber. None where there is no frame: a
    /// generator closed, a fiber not started or dead.
    pub executing: Option<PhpObject>,
    /// The file and line of the frame the execution stands at: a
    /// generator's own, and on a fiber's stack the innermost frame of
    /// user code, which a fiber whose stack holds only natives has not.
    pub site: Option<(Vec<u8>, u32)>,
    /// `$this` of that frame.
    pub this: Option<PhpObject>,
}

impl Execution {
    /// What a view says of one frame's engine half.
    fn stand(&mut self, state: FrameAt<'_>) {
        self.site = state.file().map(|file| (file.to_vec(), state.line()));
        self.this = state.this();
    }

    /// What a view says of the innermost frame of user code on `vm`, if
    /// one stands there.
    fn stand_on(&mut self, req: &Request, vm: &Vm) {
        if let Some(state) = vm.innermost_at().and_then(|at| req.frame_on(vm, at)) {
            self.stand(state);
        }
    }
}

impl Request {
    /// What [`execution_trace`](Self::execution_trace) makes, its one owner not yet named.
    pub(crate) fn execution_trace_made(
        &mut self,
        object: PhpObject,
        options: i64,
    ) -> Result<Option<PhpArray>, Thrown> {
        if generator::is_generator(self, object) {
            return self.generator_trace(object, options);
        }
        let mut rows = {
            let Some((_, Some(standing))) = crate::fiber::view(self, object) else {
                return Ok(None);
            };
            // From a bottom frame php cuts the chain under, no row.
            let links: Vec<Link> = match standing {
                Standing::Bottom { .. } => Vec::new(),
                standing => {
                    let vm = standing.stack(self);
                    links_of(self, vm, 0..vm.depth(), Expand::Generators).collect()
                }
            };
            self.trace_rows(&links)
        };
        self.render_trace(&mut rows, options).map(Some)
    }

    fn generator_execution(&mut self, object: PhpObject) -> Execution {
        let (function, closure) = generator::served(self, object);
        let mut execution = Execution {
            function,
            scope: function.and_then(|function| self.method_class(function)),
            closure,
            callable: None,
            executing: None,
            site: None,
            this: None,
        };
        if !generator::has_frame(self, object) {
            return execution;
        }
        execution.executing = Some(generator::current(self, object));
        let state = match generator::suspended_state(self, object) {
            Some(state) => Some(state),
            None => self
                .running_frame(object)
                .and_then(|(vm, at)| self.frame_on(vm, at)),
        };
        if let Some(state) = state {
            execution.stand(state);
        }
        execution
    }

    /// `ReflectionGenerator::getTrace`'s chain: the root's frame, then
    /// each generator's below it down to `object`'s, its site the frame
    /// below it and the last one with none, as php cuts
    /// `prev_execute_data` under `object`'s frame.
    fn generator_trace(
        &mut self,
        object: PhpObject,
        options: i64,
    ) -> Result<Option<PhpArray>, Thrown> {
        if !generator::has_frame(self, object) {
            return Ok(None);
        }
        let root = generator::current(self, object);
        if generator::is_running(self, object) {
            let mut rows = match self.running_frame(object) {
                Some((vm, at)) => {
                    let top = self
                        .running_frame(root)
                        .filter(|(on, top)| std::ptr::eq(*on, vm) && *top >= at)
                        .map_or(at, |(_, top)| top);
                    let links = frames_between(self, vm, at, top);
                    self.trace_rows(&links)
                }
                None => Rows::default(),
            };
            return self.render_trace(&mut rows, options).map(Some);
        }
        // The root runs, and `object` delegates to it: php links the
        // root's frame to `object`'s `execute_fake`, which the walk
        // expands into the frames between, cut under `object`'s.
        if let Some((vm, at)) = self.running_frame(root)
            && let Some(state) = self.frame_on(vm, at)
        {
            let mut links = vec![Link::Frame(Slots::Stack(vm, at), state)];
            links.extend(
                generator::kept_frames(self, object, root)
                    .into_iter()
                    .map(|state| Link::Frame(Slots::Kept, state)),
            );
            let mut rows = self.trace_rows(&links);
            return self.render_trace(&mut rows, options).map(Some);
        }
        generator::with_frames(self, object, |req, floor| {
            let top = req.vm.depth().saturating_sub(1);
            let mut rows = if top < floor {
                Rows::default()
            } else {
                let links = frames_between(req, &req.vm, floor, top);
                req.trace_rows(&links)
            };
            req.render_trace(&mut rows, options)
        })
        .map(Some)
    }

    /// Where the frame of a running `generator` stands: the stack, its
    /// frames' engine half and the frame's place, on the stack the
    /// request runs on, on one below it, or on the stack of the
    /// suspended fiber the resumption runs on.
    fn running_frame(&self, generator: PhpObject) -> Option<(&Vm, usize)> {
        let place = |vm: &Vm| {
            (0..vm.depth()).rev().find(|&at| {
                self.frame_on(vm, at)
                    .and_then(FrameAt::generator)
                    .is_some_and(|running| running.index() == generator.index())
            })
        };
        if let Some(found) = self
            .executions()
            .find_map(|vm| place(vm).map(|at| (vm, at)))
        {
            return Some(found);
        }
        let fiber = generator::in_fiber(self, generator)?;
        let vm = match crate::fiber::view(self, fiber) {
            Some((_, Some(standing @ (Standing::Held(_) | Standing::Bottom { .. })))) => {
                standing.stack(self)
            }
            _ => return None,
        };
        place(vm).map(|at| (vm, at))
    }
}

impl Frames<&mut Request> {
    /// The execution of `object` where it stands, for a `Generator` or a
    /// `Fiber`; none for any other object.
    ///
    /// ```php
    /// function g() { yield 1; yield 2; }
    /// $g = g();
    /// $r = new ReflectionGenerator($g);
    /// var_dump($r->getExecutingLine(), $r->isClosed());
    /// $g->next();
    /// var_dump($r->getExecutingLine(), $r->getFunction()->name);
    /// $g->next(); $g->next();
    /// var_dump($r->isClosed(), $r->getFunction()->name);
    /// $f = new Fiber(function () { Fiber::suspend(); });
    /// $rf = new ReflectionFiber($f);
    /// $f->start();
    /// var_dump($rf->getExecutingLine());
    /// ```
    /// ```expect
    /// int(2)
    /// bool(false)
    /// int(2)
    /// string(1) "g"
    /// bool(true)
    /// string(1) "g"
    /// int(10)
    /// ```
    pub fn execution(self, object: PhpObject) -> Option<Execution> {
        if generator::is_generator(self.0, object) {
            return Some(self.0.generator_execution(object));
        }
        let (callable, standing) = crate::fiber::view(self.0, object)?;
        let mut execution = Execution {
            function: None,
            scope: None,
            closure: None,
            callable,
            executing: standing.is_some().then_some(object),
            site: None,
            this: None,
        };
        // Where php's `fiber->execute_data` reads: the innermost frame of
        // user code on the fiber's stack, or, from its bottom frame, the
        // resumer's below it.
        let resumer = match standing {
            None => return Some(execution),
            Some(Standing::Bottom { resumer, .. }) => resumer,
            Some(standing) => {
                execution.stand_on(self.0, standing.stack(self.0));
                return Some(execution);
            }
        };
        if let Some(resumer) = resumer {
            crate::fiber::with_held(self.0, resumer, |req, held| {
                execution.stand_on(req, &held.vm);
            });
        }
        Some(execution)
    }

    /// `zend_fetch_debug_backtrace` over the execution of `object` alone
    /// (`ReflectionGenerator::getTrace`, `ReflectionFiber::getTrace`):
    /// for a generator, the frame of the generator its delegation runs
    /// on top, down to its own, and for a fiber every link on the
    /// fiber's stack, `options` as `debug_backtrace()` reads them. None
    /// where there is no frame, and for an object that is neither.
    ///
    /// ```php
    /// function inner() { yield 1; }
    /// function outer() { yield from inner(); }
    /// $g = outer(); $g->current();
    /// var_dump(count((new ReflectionGenerator($g))->getTrace()));
    /// $f = new Fiber(function ($x) { Fiber::suspend(); });
    /// $f->start(1);
    /// var_dump((new ReflectionFiber($f))->getTrace(DEBUG_BACKTRACE_IGNORE_ARGS));
    /// ```
    /// ```expect
    /// int(2)
    /// array(2) {
    ///   [0]=>
    ///   array(5) {
    ///     ["file"]=>
    ///     string(%d) "%s"
    ///     ["line"]=>
    ///     int(6)
    ///     ["function"]=>
    ///     string(7) "suspend"
    ///     ["class"]=>
    ///     string(5) "Fiber"
    ///     ["type"]=>
    ///     string(2) "::"
    ///   }
    ///   [1]=>
    ///   array(1) {
    ///     ["function"]=>
    ///     string(%d) "{closure:%s:6}"
    ///   }
    /// }
    /// ```
    ///
    /// A generator running on the stack of a fiber that is suspended is
    /// read where it stands there:
    ///
    /// ```php
    /// function g() { Fiber::suspend(); yield 1; }
    /// $g = g();
    /// $f = new Fiber(function () use ($g) { $g->current(); });
    /// $f->start();
    /// $r = new ReflectionGenerator($g);
    /// var_dump($r->getExecutingLine(), $r->getTrace());
    /// ```
    /// ```expect
    /// int(2)
    /// array(1) {
    ///   [0]=>
    ///   array(2) {
    ///     ["function"]=>
    ///     string(1) "g"
    ///     ["args"]=>
    ///     array(0) {
    ///     }
    ///   }
    /// }
    /// ```
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn execution_trace(
        self,
        object: PhpObject,
        options: i64,
    ) -> Result<Option<Owner<PhpArray>>, Thrown> {
        let made = self.0.execution_trace_made(object, options);
        adopted_some(made)
    }
}

/// The links of the records `from` to `to` of one stack, the frames and
/// the natives' calls served between them: a trace that starts at a
/// frame names no native that frame is calling.
fn frames_between<'a>(req: &'a Request, vm: &'a Vm, from: usize, to: usize) -> Vec<Link<'a>> {
    // The generators a delegation runs through stand on the stack here,
    // put there by the reader ([`generator::with_frames`]), so no
    // placeholder is expanded a second time.
    links_of(req, vm, from..to.saturating_add(1), Expand::Nothing).collect()
}
