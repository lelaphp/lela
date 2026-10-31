//! The classes this extension raises, each resolved once as the engine
//! starts, as php's `MINIT` keeps `zend_ce_error`, `zend_ce_value_error` and the rest
//! (ADR-0005: engine state, immutable): a raise names its class by the
//! identity, never by a name looked up as it throws (#3403).

use php_ext::abi::{Refusal, Start};
use php_ext::attr::php_class;
use php_ext::raising::{self, Cause, Thrown, leave_pending, while_pending};
use php_ext::request::Request;
use php_ext::values::ClassId;

/// Core's `Error`.
#[php_class(extern)]
pub(crate) struct Error;

/// Core's `ValueError`.
#[php_class(extern)]
pub(crate) struct ValueError;

/// Core's `TypeError`.
#[php_class(extern)]
pub(crate) struct TypeError;

/// Core's `Exception`.
#[php_class(extern)]
pub(crate) struct Exception;

/// The classes the natives raise, by their identity in this engine.
#[derive(Clone, Copy)]
pub(crate) struct Throwables {
    pub(crate) error: ClassId,
    pub(crate) value_error: ClassId,
    pub(crate) type_error: ClassId,
    pub(crate) exception: ClassId,
}

impl Throwables {
    /// Each class looked up once, at the engine start.
    pub(crate) fn start(start: &Start) -> Result<Throwables, Refusal> {
        Ok(Throwables {
            error: start.class::<Error>()?,
            value_error: start.class::<ValueError>()?,
            type_error: start.class::<TypeError>()?,
            exception: start.class::<Exception>()?,
        })
    }

    /// What the engine start resolved, or the `InternalError` fatal
    /// where this engine started no `curl`.
    pub(crate) fn of(req: &mut Request) -> Result<Throwables, Thrown> {
        match req
            .engine::<crate::share::Pooled>()
            .map(|state| state.throwables)
        {
            Some(throwables) => Ok(throwables),
            None => Err(raising::fatal(
                req,
                "InternalError: curl's classes were not resolved at its engine start",
            )),
        }
    }
}

/// What a native's body left in php's `EG(exception)` and went on past:
/// the throwable that stands, with what was raised after it chained onto
/// it; the unwind exit an `exit` throws (`zend_throw_unwind_exit`), which
/// stands as a throwable does and gives up what is raised after it; or
/// the fatal that leaves at once. A callable the library called back
/// leaves what it threw here as `zend_call_known_fcc` leaves it, and so
/// does a `__toString` a walk converts with.
#[derive(Default)]
pub(crate) struct Pending(Option<Thrown>);

impl Pending {
    /// Whether anything stands, php's `if (EG(exception))`.
    pub(crate) fn stands(&self) -> bool {
        self.0.is_some()
    }

    /// Whether what stands is a fatal, which leaves by `zend_bailout`
    /// past every line of php's body after it, where an `exit` stands as
    /// the unwind exit php's body goes on past ([`Pending::step`]).
    pub(crate) fn fatal(&self, req: &Request) -> bool {
        self.0
            .as_ref()
            .is_some_and(|thrown| matches!(thrown.cause(req), Cause::Fatal))
    }

    /// `thrown` left pending ([`raised`]).
    pub(crate) fn park(&mut self, req: &mut Request, thrown: Thrown) {
        self.0 = Some(raised(req, self.0.take(), thrown));
    }

    /// `step` taken as php takes it, with what stands in `EG(exception)`
    /// ([`while_pending`]), so a call it makes returns at once and a
    /// diagnostic it raises reaches no handler; `step` may leave more
    /// pending in turn. An `exit` stands in the request's slot already,
    /// as php's unwind exit stands in `EG(exception)` while the C body
    /// goes on past it, so the step is taken with it there. `None`, with
    /// nothing taken, where what stands is a fatal.
    pub(crate) fn step<R>(
        &mut self,
        req: &mut Request,
        step: impl FnOnce(&mut Request, &mut Pending) -> R,
    ) -> Option<R> {
        // A copy naming the throwable, for the step to stand it in; the
        // owner stays this one.
        let Some(standing) = self.0.clone() else {
            return Some(step(req, self));
        };
        match standing.cause(req) {
            Cause::Throwable(_) => while_pending(req, &standing, |req| step(req, self)),
            Cause::Exit => Some(step(req, self)),
            Cause::Fatal => None,
        }
    }

    /// `release`, which gives owners up, taken as [`Pending::step`] takes
    /// a step, and taken all the same where a fatal stands:
    /// what php's `zend_bailout` leaves to the request's end is given
    /// back here, so no owner outlives the request's heap.
    pub(crate) fn release(&mut self, req: &mut Request, mut release: impl FnMut(&mut Request)) {
        if self.step(req, |req, _| release(req)).is_none() {
            release(req);
        }
    }

    /// What stands left in php's `EG(exception)` where the step that
    /// served it answers nothing, a `free_obj`, for the release that freed
    /// the object to raise ([`leave_pending`]); a fatal or an `exit` is
    /// the request's already.
    pub(crate) fn leave(self, req: &mut Request) {
        if let Some(thrown) = self.0 {
            leave_pending(req, thrown);
        }
    }

    /// The native's answer: `answer` where nothing stands, and what
    /// stands otherwise, a throwable `answer` raised chained onto it, as
    /// php's return value is dropped with `EG(exception)` set. `T` holds
    /// no owner.
    pub(crate) fn answer<T>(
        self,
        req: &mut Request,
        answer: Result<T, Thrown>,
    ) -> Result<T, Thrown> {
        match (answer, self.0) {
            (Ok(answered), None) => Ok(answered),
            (Ok(_), Some(standing)) => Err(standing),
            (Err(thrown), standing) => Err(raised(req, standing, thrown)),
        }
    }
}

/// `later` raised while `earlier` stands pending: chained onto it, as
/// `zend_throw_exception_internal` chains one raised while another is
/// pending. A call made while a throwable stands answers that same
/// throwable, having served nothing, which is no second throwable.
fn raised(req: &mut Request, earlier: Option<Thrown>, later: Thrown) -> Thrown {
    match earlier {
        Some(earlier) if earlier.exception() == later.exception() => earlier,
        Some(earlier) => earlier.chain(req, later),
        None => later,
    }
}
