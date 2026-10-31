//! The classes this extension raises, each resolved once as the engine
//! starts, as php's `MINIT` keeps `zend_ce_error`, `zend_ce_value_error` and the rest
//! (ADR-0005: engine state, immutable): a raise names its class by the
//! identity, never by a name looked up as it throws (#3403).

use std::mem;

use php_ext::abi::{Refusal, Start};
use php_ext::attr::php_class;
use php_ext::raising::{self, Cause, Thrown, leave_pending};
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
///
/// One owner holds what stands at every moment ([`Standing`]), as php's
/// `EG(exception)` holds it throughout, so what PHP code a step serves,
/// a destructor or a stream's close among it, chains onto it or gives it
/// up as it would in php, and every `Thrown` a step answers is an owner.
#[derive(Default)]
pub(crate) struct Pending(Standing);

/// Where what stands is held.
enum Standing {
    /// Between the body's steps: here, or nothing.
    Here(Option<Thrown>),
    /// During a step ([`Pending::step`]): the request's slot, where what
    /// the step's PHP code raises and what it parks is chained onto it.
    Slot,
}

impl Default for Standing {
    fn default() -> Standing {
        Standing::Here(None)
    }
}

impl Pending {
    /// Whether anything stands, php's `if (EG(exception))`: here, or in
    /// the request's slot.
    pub(crate) fn stands(&self, req: &Request) -> bool {
        matches!(self.0, Standing::Here(Some(_)))
            || req.diagnostics().raised().is_some()
            || req.diagnostics().bailing_out()
    }

    /// Whether what stands is a fatal, which leaves by `zend_bailout`
    /// past every line of php's body after it, where an `exit` stands as
    /// the unwind exit php's body goes on past ([`Pending::step`]). A
    /// fatal raised after an `exit` leaves past it as well, so a fatal
    /// leaving is one whatever the slot holds.
    pub(crate) fn fatal(&self, req: &Request) -> bool {
        req.diagnostics().bailing_out()
            || matches!(&self.0, Standing::Here(Some(thrown)) if matches!(thrown.cause(req), Cause::Fatal))
    }

    /// `thrown` left pending ([`raised`]): chained onto what stands here,
    /// or, during a step, onto what stands in the request's slot.
    pub(crate) fn park(&mut self, req: &mut Request, thrown: Thrown) {
        match &mut self.0 {
            Standing::Here(standing) => *standing = Some(raised(req, standing.take(), thrown)),
            Standing::Slot => {
                let earlier = raising::pending(req).err();
                let standing = raised(req, earlier, thrown);
                leave_pending(req, standing);
            }
        }
    }

    /// `step` taken as php takes it, with what stands in `EG(exception)`,
    /// so a call it makes returns at once and a diagnostic it raises
    /// reaches no handler; `step` may leave more pending in turn. What
    /// stands is the request's slot's for the step ([`leave_pending`]),
    /// with what the step parks and what its PHP code raises, a release's
    /// destructor or a stream's close, chained onto it there, and all of
    /// it is taken back as the step returns ([`raising::pending`]). An
    /// `exit` stands in the slot already, as php's unwind exit stands in
    /// `EG(exception)` while the C body goes on past it, and so does what
    /// an enclosing step put there, so the step is taken with either
    /// there. `None`, with nothing taken, where what stands is a fatal.
    ///
    /// A user wrapper's method a stream op of the step serves, a
    /// `stream_stat` or a `stream_close`, may throw, and the stream layer
    /// keeps that throw ([`Diagnostics::stream_thrown`]) where php's
    /// stream layer leaves it in `EG(exception)`. What the step's stream
    /// ops kept is left pending here as the step returns, so it stands
    /// over the steps after it and what they raise is chained onto it;
    /// what was kept before the step began is no op of its.
    ///
    /// A destructor a step serves, a callable the option replaces, chains
    /// what it throws onto what stands, as `zend_objects_destroy_object`
    /// chains it onto the `EG(exception)` it put aside:
    ///
    /// ```php
    /// class D { function __invoke() {} function __destruct() { echo "dtor\n"; throw new Exception("dtor"); } }
    /// $h = curl_init();
    /// curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $t, $d) { echo $d; throw new Exception("debug"); });
    /// curl_setopt($h, CURLOPT_WRITEFUNCTION, new D);
    /// try { curl_setopt_array($h, [CURLOPT_VERBOSE => 2, CURLOPT_WRITEFUNCTION => null]); } catch (Exception $e) { echo $e->getMessage(), " <- ", $e->getPrevious()->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// boolean setopt(41) got unsupported argument 2, treated as 1
    /// dtor
    /// dtor <- debug
    /// ```
    ///
    /// A destructor's throwable replaces an `exit` standing, as
    /// `zend_exception_set_previous` gives the unwind exit up for it, and
    /// the steps after it go on under that throwable, not under a fatal:
    ///
    /// ```php
    /// class D { function __invoke() {} function __destruct() { echo "dtor\n"; throw new Exception("dtor"); } }
    /// $h = curl_init();
    /// curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $t, $d) { exit("exit\n"); });
    /// curl_setopt($h, CURLOPT_WRITEFUNCTION, new D);
    /// register_shutdown_function(function () use ($h) { var_dump(curl_getinfo($h, CURLINFO_PRIVATE)); });
    /// try { curl_setopt_array($h, [CURLOPT_VERBOSE => 2, CURLOPT_WRITEFUNCTION => null, CURLOPT_PRIVATE => 'p']); } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// exit
    /// dtor
    /// caught dtor
    /// string(1) "p"
    /// ```
    ///
    /// [`Diagnostics::stream_thrown`]: php_ext::request::Diagnostics::stream_thrown
    pub(crate) fn step<R>(
        &mut self,
        req: &mut Request,
        step: impl FnOnce(&mut Request, &mut Pending) -> R,
    ) -> Option<R> {
        if self.fatal(req) {
            return None;
        }
        let kept = req.diagnostics_mut().stream_thrown().is_some();
        let answer = if matches!(self.0, Standing::Slot) || req.diagnostics().raised().is_some() {
            let exit =
                matches!(&self.0, Standing::Here(Some(thrown)) if thrown.exception().is_none());
            let answer = step(req, self);
            if exit {
                // A destructor the step served may have thrown over the
                // exit, which `zend_exception_set_previous` gives up for
                // it: what stands is what the slot holds now.
                self.0 = Standing::Here(raising::pending(req).err());
            }
            answer
        } else {
            if let Standing::Here(Some(standing)) = mem::replace(&mut self.0, Standing::Slot) {
                leave_pending(req, standing);
            }
            let answer = step(req, self);
            self.0 = Standing::Here(raising::pending(req).err());
            answer
        };
        if !kept && let Some(thrown) = req.diagnostics_mut().stream_thrown().take() {
            self.park(req, thrown);
        }
        Some(answer)
    }

    /// `release`, which gives owners up, taken as [`Pending::step`] takes
    /// a step, and taken all the same where a fatal stands:
    /// what php's `zend_bailout` leaves to the request's end is given
    /// back here, so no owner outlives the request's heap.
    ///
    /// A stream the release closes, a form's file part through its
    /// `free_cb`, may serve a user wrapper's `stream_close` that throws,
    /// which stands over the steps after it as the step says:
    ///
    /// ```php
    /// class W { public $context; function stream_open($p,$m,$o,&$op){return true;} function stream_read($n){return '';} function stream_eof(){return true;} function stream_stat(){return ['size'=>0];} function stream_close(){ echo "close\n"; throw new Exception("close"); } }
    /// stream_wrapper_register('ww', 'W');
    /// class U { function __toString(): string { echo "U called\n"; return "u"; } }
    /// $h = curl_init('file:///nonexistent');
    /// curl_setopt($h, CURLOPT_POSTFIELDS, ['f' => new CURLFile('ww://x')]);
    /// try { curl_setopt_array($h, [CURLOPT_POSTFIELDS => ['a' => 'b'], CURLOPT_URL => new U]); } catch (Exception $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
    /// curl_setopt($h, CURLOPT_POSTFIELDS, ['f' => new CURLFile('ww://x')]);
    /// try { curl_setopt_array($h, [CURLOPT_POSTFIELDS => ['a' => 'b'], CURLOPT_URL => "a\0b"]); } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), " prev ", $e->getPrevious() ? $e->getPrevious()->getMessage() : "none", "\n"; }
    /// ```
    /// ```expect
    /// close
    /// Exception: close
    /// close
    /// ValueError: curl_setopt_array(): cURL option must not contain any null bytes prev close
    /// ```
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
        if let Standing::Here(Some(thrown)) = self.0 {
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
        let standing = match self.0 {
            Standing::Here(standing) => standing,
            // A step lends Pending by reference, so one taken by value
            // is between steps.
            Standing::Slot => None,
        };
        match (answer, standing) {
            (Ok(answered), None) => Ok(answered),
            (Ok(_), Some(standing)) => Err(standing),
            (Err(thrown), standing) => Err(raised(req, standing, thrown)),
        }
    }
}

/// `later` raised while `earlier` stands pending: chained onto it, as
/// `zend_throw_exception_internal` chains one raised while another is
/// pending. A step that answers the throwable standing in the slot,
/// having served nothing, answers an owner of it, which the chain gives
/// back, as `zend_exception_set_previous` chains no throwable onto
/// itself.
fn raised(req: &mut Request, earlier: Option<Thrown>, later: Thrown) -> Thrown {
    match earlier {
        Some(earlier) => earlier.chain(req, later),
        None => later,
    }
}
