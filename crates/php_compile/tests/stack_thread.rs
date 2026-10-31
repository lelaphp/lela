//! The compiler's stack limit on the thread it is built on (design
//! §5.6): the chain the witness beside `StackLimit::check` refuses, two
//! hundred thousand `+1` links, refused with php's `E_COMPILE_ERROR` on
//! a thread spawned at a size of this test's choosing and on the main
//! thread under an environment 64 KB larger than this test's own.
//! `zend.max_allowed_stack_size=0` detects, so the limit is the
//! thread's bounds as the platform describes them, and what is held is
//! that detect reads the calling thread, not an assumed size: the
//! spawned thread is far smaller than the assumption, and the main
//! thread's environment block, laid at the top of the stack by
//! `execve`, is what pushed the assumption over the real stack under
//! `git hook run pre-commit` (#297). libtest runs every `#[test]` on a
//! thread of its own, so this binary is `harness = false`: `main` is the
//! main thread, and the larger environment is this binary started again
//! with one variable more.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

use std::process::{Command, exit};

use php_compile::{Options, StackLimit, compile_source};
use php_unit::Level;

/// The variable the second process carries: 64 KB of padding, and the sign
/// that this process is the padded one.
const PAD: &str = "PHP_COMPILE_STACK_PAD";

fn main() {
    if std::env::var_os(PAD).is_some() {
        refused_on_this_thread("the main thread under the larger environment");
        return;
    }
    let spawned = std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| refused_on_this_thread("a 1 MiB thread"));
    match spawned.map(std::thread::JoinHandle::join) {
        Ok(Ok(())) => {}
        Ok(Err(_)) => fail("the 1 MiB thread did not finish"),
        Err(err) => fail(&format!("spawning the 1 MiB thread: {err}")),
    }
    let Ok(exe) = std::env::current_exe() else {
        fail("this binary's own path could not be read");
    };
    let padded = Command::new(exe).env(PAD, "x".repeat(64 * 1024)).status();
    match padded {
        Ok(status) if status.success() => {}
        Ok(status) => fail(&format!("the padded run ended with {status}")),
        Err(err) => fail(&format!("running the padded run: {err}")),
    }
    println!("test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out");
}

/// The witness's chain, compiled under detect on the calling thread:
/// php's sentence at line 2, or the failure named after `on`.
fn refused_on_this_thread(on: &str) {
    let source = format!("<?php\n$y = 1{};\necho 'never';", "+1".repeat(200_000));
    let options = Options {
        stack: StackLimit::init(0, 0),
        ..Options::default()
    };
    let mut strings = php_heap::Strings::new();
    match compile_source(&mut php_compile::syntax::Arena::default(), source.as_bytes(), &options, &mut strings) {
        Err(err)
            if err.level == Level::CompileError
                && err.line == 2
                && err.message.starts_with("Maximum call stack size of ")
                && err.message.ends_with(
                    " bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached during compilation. Try splitting expression",
                ) => {}
        Err(err) => fail(&format!("on {on}, the chain was refused with {:?} {}", err.level, err.message)),
        Ok(_) => fail(&format!("on {on}, the chain compiled")),
    }
}

fn fail(what: &str) -> ! {
    eprintln!("stack_thread: {what}");
    exit(1)
}
