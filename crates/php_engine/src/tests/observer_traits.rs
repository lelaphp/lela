//! Seam 9, the observer, over a native trait's method a class takes in:
//! php's `zend_add_trait_method` lays a copy of the trait's
//! `zend_internal_function` in the using class's function table with `scope` set
//! to that class, so `EX(func)->common.scope` names the using class, and
//! the copy shares the trait's runtime cache, so the method is asked of
//! once, `Z/ext/opcache/tests/gh13712.phpt`.

use php_macros::php_class;
use php_unit::decl::ClassKind;

use super::observer::{observed, scratch, serve_main};
use super::test_ext::{Event, Phase, Recording, TestExt};
use crate::api::abi::{Extension, ExtensionBuilder, This};
use crate::engine::Engine;

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
    let dir = scratch("native_trait");
    rec.mark("built");
    let served = serve_main(
        &engine,
        &dir,
        "<?php\nclass Foo { use Greets; }\nclass Bar { use Greets; }\nclass Baz extends Foo {}\n\
         var_dump((new Foo)->greet(), (new Bar)->greet(), (new Baz)->greet());\n",
    );
    rec.mark("served");
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "bool(true)\nbool(true)\nbool(true)\n"
    );
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
