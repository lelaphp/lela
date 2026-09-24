//! `spl_observer.stub.php`: `SplObserver` and `SplSubject`, then
//! `SplObjectStorage` and `MultipleIterator`, as
//! `PHP_MINIT_FUNCTION(spl_observer)` registers them.

use crate::stub::{self, Declared};

const STUB: &str = "
interface SplObserver {
    /** @tentative-return-type */ public function update(SplSubject $subject): void;
}
interface SplSubject {
    /** @tentative-return-type */ public function attach(SplObserver $observer): void;
    /** @tentative-return-type */ public function detach(SplObserver $observer): void;
    /** @tentative-return-type */ public function notify(): void;
}
";

/// The interfaces and classes.
///
/// ```php
/// class Subject implements SplSubject {
///     private $observers = [];
///     function attach(SplObserver $o): void { $this->observers[] = $o; }
///     function detach(SplObserver $o): void {}
///     function notify(): void { foreach ($this->observers as $o) $o->update($this); }
/// }
/// class Printer implements SplObserver {
///     function update(SplSubject $s): void { echo "updated by ", get_class($s), "\n"; }
/// }
/// $s = new Subject; $s->attach(new Printer); $s->notify();
/// ```
/// ```expect
/// updated by Subject
/// ```
pub(crate) fn classes() -> Vec<Declared> {
    stub::classes(STUB, &[])
}
