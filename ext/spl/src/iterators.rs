//! `spl_iterators.stub.php`: the iterator interfaces and classes, as
//! `PHP_MINIT_FUNCTION(spl_iterators)` registers them, in its order.

use crate::stub::{self, Declared};

const STUB: &str = "
interface RecursiveIterator extends Iterator {
    /** @tentative-return-type */ public function hasChildren(): bool;
    /** @tentative-return-type */ public function getChildren(): ?RecursiveIterator;
}
interface OuterIterator extends Iterator {
    /** @tentative-return-type */ public function getInnerIterator(): ?Iterator;
}
interface SeekableIterator extends Iterator {
    /** @tentative-return-type */ public function seek(int $offset): void;
}
";

/// The interfaces and classes.
///
/// ```php
/// class Tree implements RecursiveIterator, SeekableIterator {
///     function current(): mixed { return 1; } function key(): mixed { return 0; }
///     function next(): void {} function rewind(): void {} function valid(): bool { return false; }
///     function hasChildren(): bool { return false; } function getChildren(): ?RecursiveIterator { return null; }
///     function seek(int $offset): void {}
/// }
/// var_dump(new Tree instanceof Traversable, interface_exists('OuterIterator'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
pub(crate) fn classes() -> Vec<Declared> {
    stub::classes(STUB, &[])
}
