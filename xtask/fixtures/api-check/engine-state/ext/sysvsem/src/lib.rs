//! A macro whose trait path a repetition builds, `$($p)::+`: what it
//! names is known only where the macro is called, so the sensor fails the
//! crate naming the macro's impl.

use php_ext::values::Value;

pub(crate) struct Kept {
    last: Value,
}

macro_rules! m {
    ($($p:ident)::+ ; $x:ty) => {
        impl $($p)::+ for $x {}
    };
}

m!(php_ext::abi::EngineState; Kept);
