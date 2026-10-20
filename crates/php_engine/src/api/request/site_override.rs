//! `EG(filename_override)` and `EG(lineno_override)`
//! (`Z/Zend/zend_globals.h:305`): the site a constant expression with a
//! class for its scope is evaluated at. `zend_ast_evaluate_ex`
//! (`Z/Zend/zend_ast.c:569`) stands it at the scope's file and at the
//! line of each node it evaluates, and puts the outer one back after the
//! node; `zend_get_executed_filename_ex` and `zend_get_executed_lineno`
//! answer it before any frame, so what the evaluation raises, a user
//! method it reaches included, names the expression's own file and
//! line, and `zend_fetch_debug_backtrace` puts a `[constant
//! expression]` row with the executing site on top of a trace taken
//! under it. `zend_lookup_class_ex` clears it around the autoloaders.
//!
//! ```php
//! class Foo {
//!     public int $bar =
//!         BAR::BAZ * 2;
//! }
//! function make() { return new Foo; }
//! try { make(); } catch (Error $e) { echo $e->getMessage(), " on ", $e->getLine(), "\n", $e->getTraceAsString(), "\n"; }
//! ```
//! ```expect
//! Class "BAR" not found on 4
//! #0 %s(6): [constant expression]()
//! #1 %s(7): make()
//! #2 {main}
//! ```
//!
//! A user method the evaluation reaches raises under the expression's
//! site, and an autoloader runs under its own:
//!
//! ```php
//! class S { function __toString(): string { throw new Exception('in S'); } }
//! const S = new S;
//! class B { public $p = 'a' . S; }
//! spl_autoload_register(function ($c) { echo "load $c on ", (new Exception)->getLine(), "\n"; });
//! class C { const X = [
//!     Missing::X,
//! ]; }
//! try { new B; } catch (Exception $e) { echo $e->getMessage(), " on ", $e->getLine(), "\n", $e->getTraceAsString(), "\n"; }
//! try { C::X; } catch (Error $e) { echo $e->getMessage(), " on ", $e->getLine(), "\n"; }
//! ```
//! ```expect
//! in S on 4
//! #0 %s(2): [constant expression]()
//! #1 %s(9): S->__toString()
//! #2 {main}
//! load Missing on 5
//! Class "Missing" not found on 7
//! ```

use super::Request;
use crate::api::raising::Thrown;
use crate::api::values::ClassId;

/// The scope whose file, and the line of the node being evaluated, a
/// constant expression stands the executed site at.
#[derive(Debug, Clone, Copy)]
pub(super) struct SiteOverride {
    scope: ClassId,
    line: u32,
}

impl Request {
    /// `zend_ast_evaluate_ex`'s override around one node: where the
    /// expression has a program's class for its scope and the node a
    /// line, the executed site is the class's file at that line while
    /// `evaluate` runs, and the outer one after it.
    pub(crate) fn evaluate_at<T>(
        &mut self,
        scope: Option<ClassId>,
        line: Option<u32>,
        evaluate: impl FnOnce(&mut Request) -> Result<T, Thrown>,
    ) -> Result<T, Thrown> {
        let (Some(scope), Some(line)) = (scope, line) else {
            return evaluate(self);
        };
        if self.native_class(scope) {
            return evaluate(self);
        }
        let outer = self.site_override.replace(SiteOverride { scope, line });
        let evaluated = evaluate(self);
        self.site_override = outer;
        evaluated
    }

    /// `zend_lookup_class_ex`'s clearing of the override around
    /// `zend_autoload`: an autoloader runs at its own site.
    pub(crate) fn without_site_override<T>(&mut self, run: impl FnOnce(&mut Request) -> T) -> T {
        let outer = self.site_override.take();
        let ran = run(self);
        self.site_override = outer;
        ran
    }

    /// The file and line the override stands the executed site at,
    /// none where no constant expression is being evaluated.
    pub(super) fn overridden_site(&self) -> Option<(&[u8], u32)> {
        let site = self.site_override?;
        let file = &self.symbols().class_decl(site.scope)?.file.0;
        Some((file, site.line))
    }
}
