//! What a host that serves its program as php's phpdbg does reads off a
//! request ([`crate::engine::Primary::Bare`]): the throwable the
//! program left uncaught, kept for the host to render.

use super::Request;
use crate::api::values::PhpObject;

impl Request {
    /// How the program is served.
    pub(crate) fn primary(&self) -> crate::engine::Primary {
        self.description.primary
    }

    /// A property of a throwable as its bytes, `zval_get_string` over
    /// `zend_read_property_ex(..., silent)`: the empty string where it
    /// holds no string.
    pub(crate) fn throwable_prop_bytes(&self, object: PhpObject, name: &[u8]) -> Vec<u8> {
        object
            .prop(self, name)
            .and_then(|value| value.as_str_bytes().map(<[u8]>::to_vec))
            .unwrap_or_default()
    }

    /// A property of a throwable as a number, `zval_get_long`: zero where
    /// it holds no int.
    pub(crate) fn throwable_prop_int(&self, object: PhpObject, name: &[u8]) -> i64 {
        object
            .prop(self, name)
            .and_then(|value| value.as_int())
            .unwrap_or(0)
    }

    /// `zend_try_exception_handler`, then what is left kept for the host
    /// as phpdbg's `phpdbg_handle_exception` reads it
    /// (`Z/sapi/phpdbg/phpdbg_prompt.c:688`): its class, `file` and
    /// `line`, and what `__toString` answered, the status 255.
    pub(crate) fn keep_uncaught(&mut self) {
        self.user_exception_handler();
        let Some(throwable) = self.take_pending() else {
            self.raised = None;
            return;
        };
        let class = self.class_name(throwable.class()).into_bytes();
        let text = match crate::api::call::call_method(self, &throwable, b"__tostring", &[]) {
            Ok(returned) => {
                let text = returned
                    .deref()
                    .as_str_bytes()
                    .map(<[u8]>::to_vec)
                    .unwrap_or_default();
                self.release(returned);
                text
            }
            Err(thrown) => {
                if let Some(inner) = thrown.exception() {
                    self.release(inner.as_value());
                }
                self.raised = None;
                Vec::new()
            }
        };
        self.kept_uncaught = Some(crate::engine::Uncaught {
            file: self.throwable_prop_bytes(throwable, b"file"),
            line: self.throwable_prop_int(throwable, b"line"),
            class,
            text,
        });
        self.release(throwable.as_value());
        self.exit_status = 255;
    }

    /// What [`Request::keep_uncaught`] kept, taken.
    pub(crate) fn take_kept_uncaught(&mut self) -> Option<crate::engine::Uncaught> {
        self.kept_uncaught.take()
    }
}
