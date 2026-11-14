//! The module an entry names, whose one hand impl stands in an inline
//! `mod`, a child module: held.

mod inner {
    use php_heap::EngineState;

    pub(crate) struct Inner(u8);

    impl EngineState for Inner {}
}
