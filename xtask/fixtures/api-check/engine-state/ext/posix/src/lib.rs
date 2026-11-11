//! The trait handed to a macro another crate defines, whose body this
//! crate's sources do not hold: the sensor fails the crate naming the call.

use php_ext::abi::EngineState;
use php_ext::values::Value;

pub(crate) struct Kept {
    last: Value,
}

php_ext::seal!(EngineState, Kept);
