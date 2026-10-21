//! Seam 9, the observer, over a native trait's method a class takes in:
//! php's `zend_add_trait_method` lays a copy of the trait's
//! `zend_internal_function` in the using class's function table with `scope` set
//! to that class, so `EX(func)->common.scope` names the using class, and
//! the copy shares the trait's runtime cache, so the method is asked of
//! once, `Z/ext/opcache/tests/gh13712.phpt`.

use php_macros::php_class;
use php_unit::decl::ClassKind;

use super::observer::{observed, serve_main};
use super::test_ext::{Event, Phase, Recording, TestExt};
use crate::api::abi::{Extension, ExtensionBuilder, This};
use crate::engine::Engine;
use crate::test::Files;

/// `trait Greets { function greet(): bool }`, a native trait as
/// `_ZendTestTrait` is one.
#[php_class]
struct Greets;

#[php_class]
impl Greets {
    /// `greet(): bool`, true.
    fn greet(this: This<Self>) -> bool {
        let _ = this;
        true
    }
}

fn greets() -> Extension {
    ExtensionBuilder::default()
        .class_reshaped::<Greets>(|class| class.kind = ClassKind::Trait)
        .build("observer_traits")
}

fn event(phase: Phase) -> Event {
    Event::Ext(0, phase)
}

/// Each class that uses the trait is the scope of its own call, a child
/// calls it under the class that took it in, and the method is asked of
/// once, under the first class to call it.
#[test]
fn a_native_trait_method_is_observed_under_the_class_that_uses_it() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[greets(), TestExt::<0>::observing()])
        .build();
    let files = Files::new("observer-traits-native-trait", &[]);
    rec.mark("built");
    let served = serve_main(
        &engine,
        &files,
        "<?php\nclass Foo { use Greets; }\nclass Bar { use Greets; }\nclass Baz extends Foo {}\n\
         var_dump((new Foo)->greet(), (new Bar)->greet(), (new Baz)->greet());\n",
    );
    rec.mark("served");
    assert_eq!(served.stdout_text(), "bool(true)\nbool(true)\nbool(true)\n");
    let calls: Vec<Event> = observed(&rec.events().between("built", "served"))
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                Event::Ext(0, Phase::ObserverInit(name) | Phase::Begin(name))
                    if name.ends_with("::greet")
            )
        })
        .collect();
    assert_eq!(
        calls,
        [
            event(Phase::ObserverInit("Foo::greet".to_owned())),
            event(Phase::Begin("Foo::greet".to_owned())),
            event(Phase::Begin("Bar::greet".to_owned())),
            event(Phase::Begin("Foo::greet".to_owned())),
        ]
    );
}

/// A native trait's method taken in under an alias is the class's own
/// row: the observer names it by the alias, as php's copy carries the
/// alias as its `function_name` (`zend_add_trait_method`), and an alias
/// that makes it protected is refused from the global scope, as
/// `zend_traits_copy_functions` gives the copy the alias's visibility.
#[test]
fn a_native_trait_method_taken_under_an_alias_carries_its_name_and_visibility() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[greets(), TestExt::<0>::observing()])
        .build();
    let files = Files::new("observer-traits-native-trait-alias", &[]);
    rec.mark("built");
    let served = serve_main(
        &engine,
        &files,
        "<?php\nclass Foo { use Greets { greet as hello; greet as protected prot; } }\n\
         var_dump((new Foo)->hello());\n\
         try { (new Foo)->prot(); } catch (Error $e) { echo $e->getMessage(), \"\\n\"; }\n",
    );
    rec.mark("served");
    assert_eq!(
        served.stdout_text(),
        "bool(true)\nCall to protected method Foo::prot() from global scope\n"
    );
    let calls: Vec<Event> = observed(&rec.events().between("built", "served"))
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                Event::Ext(0, Phase::ObserverInit(name) | Phase::Begin(name))
                    if name.starts_with("Foo::")
            )
        })
        .collect();
    assert_eq!(
        calls,
        [
            event(Phase::ObserverInit("Foo::hello".to_owned())),
            event(Phase::Begin("Foo::hello".to_owned())),
        ]
    );
}
