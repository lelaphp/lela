//! `register_tick_function()` and `unregister_tick_function()` of a
//! request, over `BG(user_tick_functions)`, and the round `ZEND_TICKS`
//! serves under `declare(ticks=N)`, extensions' hooks first. The list
//! and `EG(ticks_count)` are [`Ticks`]'s, whose fields no file but this
//! one reads.

use std::sync::Arc;

use crate::api::abi::Callable;
use crate::api::raising::Thrown;
use crate::api::values::{Owner, Value};

use super::Code;
use super::Request;

/// One entry of `BG(user_tick_functions)`, php's
/// `user_tick_function_entry`: the callable and the arguments it is
/// served with, each held by its owners, and whether a round is serving
/// it now (`calling`), which keeps a `ZEND_TICKS` inside it from serving
/// it again and `unregister_tick_function()` from dropping it.
struct TickFunction {
    callback: Owner<Callable>,
    arguments: Vec<Value>,
    calling: bool,
}

/// `BG(user_tick_functions)` and `EG(ticks_count)`, whose fields no
/// file but this one reads.
#[derive(Default)]
pub(super) struct Ticks {
    /// `BG(user_tick_functions)`, in registration order.
    tick_functions: Vec<TickFunction>,
    /// `EG(ticks_count)`: the `ZEND_TICKS` passed since the functions
    /// they serve were last served.
    ticks_count: u32,
}

impl Request {
    /// `PHP_RSHUTDOWN(basic)`'s `zend_llist_destroy` of
    /// `BG(user_tick_functions)`: each function and its arguments given
    /// up, in registration order.
    pub(super) fn free_tick_functions(&mut self) {
        for TickFunction {
            callback,
            arguments,
            ..
        } in std::mem::take(&mut self.ticks.tick_functions)
        {
            callback.give_up(self);
            for value in arguments {
                self.release(value);
            }
        }
    }

    /// `ZEND_TICKS`' count (`Z/Zend/zend_vm_def.h:8024`): `EG(ticks_count)`
    /// stepped, and where it reaches `every` it starts again and the
    /// round is served, `zend_ticks_function` as `php_run_ticks`
    /// runs `PG(tick_functions)`: each extension's hook in bundle order,
    /// then each function `register_tick_function()` kept, in
    /// registration order, one already running passed over
    /// (`user_tick_function_call`), all with fiber switching blocked
    /// (`zend_fiber_switch_block`). The first throw ends the round, as
    /// php's `zend_call_function` calls nothing while one is pending.
    pub(crate) fn tick(&mut self, every: u32) -> Result<(), Thrown> {
        self.ticks.ticks_count = self.ticks.ticks_count.saturating_add(1);
        if self.ticks.ticks_count < every {
            return Ok(());
        }
        self.ticks.ticks_count = 0;
        self.code_mut()
            .with_fiber_switch_blocked(Request::serve_ticks)
    }

    /// [`Request::tick`]'s round, while fiber switching is blocked.
    fn serve_ticks(&mut self) -> Result<(), Thrown> {
        let shared = Arc::clone(&self.shared);
        for (by, hook) in shared.supplied.ticks.iter() {
            self.serve_for(by, hook)?;
        }
        let mut at = 0;
        while let Some(entry) = self.ticks.tick_functions.get_mut(at) {
            at += 1;
            if entry.calling {
                continue;
            }
            entry.calling = true;
            let (callback, arguments) = ((*entry.callback).clone(), entry.arguments.clone());
            let called = crate::api::call::call_made(self, &callback, &arguments);
            if let Some(entry) = self
                .ticks
                .tick_functions
                .iter_mut()
                .find(|entry| entry.calling && *entry.callback == callback)
            {
                entry.calling = false;
            }
            self.release(called?);
        }
        Ok(())
    }
}

impl Code<&mut Request> {
    /// `register_tick_function()`: the callable and its arguments held,
    /// with the owners `zend_fcc_addref` and `ZVAL_COPY` take, in
    /// `BG(user_tick_functions)`.
    pub fn register_tick_function(self, callback: Callable, arguments: Vec<Value>) -> bool {
        let callback = Owner::take(self.0, callback);
        for value in &arguments {
            self.0.addref(value);
        }
        self.0.ticks.tick_functions.push(TickFunction {
            callback,
            arguments,
            calling: false,
        });
        true
    }

    /// `unregister_tick_function()`: `zend_llist_del_element`, the first
    /// entry for the same callee, scope and object dropped and its
    /// owners given up; php's `Error` where that entry is the one being
    /// served (`user_tick_function_compare`).
    pub fn unregister_tick_function(self, callback: &Callable) -> Result<bool, Thrown> {
        let Some(at) = self
            .0
            .ticks
            .tick_functions
            .iter()
            .position(|entry| *entry.callback == *callback)
        else {
            return Ok(false);
        };
        if self.0.ticks.tick_functions[at].calling {
            return Err(crate::api::raising::throw_error(
                self.0,
                "Error",
                "Registered tick function cannot be unregistered while it is being executed",
            ));
        }
        let TickFunction {
            callback: held,
            arguments,
            ..
        } = self.0.ticks.tick_functions.remove(at);
        held.give_up(self.0);
        for value in arguments {
            self.0.release(value);
        }
        Ok(true)
    }
}
