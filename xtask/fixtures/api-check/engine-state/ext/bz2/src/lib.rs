//! A macro that takes the trait as a metavariable, in a crate that names
//! the trait: what `$t` stands for is known only where the macro is
//! called, so the sensor fails the crate naming the macro's impl.

use php_ext::abi::EngineState;
use php_ext::values::Value;

pub(crate) struct Kept {
    last: Value,
}

macro_rules! m {
    ($t:path, $x:ty) => {
        impl $t for $x {}
    };
}

m!(EngineState, Kept);
