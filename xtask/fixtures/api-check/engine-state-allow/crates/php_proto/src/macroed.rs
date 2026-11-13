//! The module an entry names, whose one hand impl a `macro_rules!` body
//! writes, so it stands wherever the macro expands: held.

macro_rules! sealed {
    ($t:ty) => {
        impl php_heap::EngineState for $t {}
    };
}
