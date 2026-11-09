//! The placement `build.rs` reads for each variant (`build/arms.rs`),
//! asked of one variant of each placement and held to the arm the
//! lists give it today, and the lists held to the variants served.

#[path = "../build/arms.rs"]
mod arms;

use arms::{Answer, InlinePath, Placement, Reads};

/// A variant no list names is called out of line.
#[test]
fn a_variant_no_list_names_is_cold() {
    let placement = arms::place("ConcatCvConst", "concat", false);
    assert_eq!(placement, Placement::Cold);
    assert!(!placement.in_the_loop());
}

/// `PATH_IN_THE_LOOP`'s variant has its inline path alone in the loop's
/// body, and its handler out of line.
#[test]
fn a_path_in_the_loop_is_the_path_alone() {
    let placement = arms::place("VerifyReturnType", "verify_return_type", false);
    assert_eq!(
        placement,
        Placement::PathInTheLoop(InlinePath {
            name: "verify_return_type",
            reads: Reads::Slots,
        })
    );
    assert!(!placement.in_the_loop());
}

/// A variant in the loop's body whose handler has no inline path is
/// called there, its answer read where the arms meet.
#[test]
fn in_the_loop_without_a_path() {
    let placement = arms::place("Echo", "echo", false);
    assert_eq!(
        placement,
        Placement::InTheLoop {
            path: None,
            answer: Answer::Cold,
        }
    );
    assert!(placement.in_the_loop());
}

/// A variant in the loop's body whose handler has an inline path over
/// the slots alone serves it first.
#[test]
fn in_the_loop_with_a_path() {
    assert_eq!(
        arms::place("AddCvCv", "add", false),
        Placement::InTheLoop {
            path: Some(InlinePath {
                name: "add",
                reads: Reads::Slots,
            }),
            answer: Answer::Cold,
        }
    );
}

/// The fused variant of an arithmetic `ASSIGNED` names serves the
/// handler's `_assign_cv` path, which serves the `ASSIGN` too.
#[test]
fn a_fused_variant_assigns() {
    assert!(arms::fuses("ADD"));
    assert!(!arms::fuses("CONCAT"));
    assert_eq!(
        arms::place("AddCvCvAssignCv", "add", true),
        Placement::InTheLoop {
            path: Some(InlinePath {
                name: "add_assign_cv",
                reads: Reads::Assign,
            }),
            answer: Answer::Cold,
        }
    );
}

/// A path `CALL_PATHS` names reads the call being built too.
#[test]
fn a_call_path_reads_the_call() {
    assert_eq!(
        arms::place("SendValTmpUnused", "send_val", false),
        Placement::InTheLoop {
            path: Some(InlinePath {
                name: "send_val",
                reads: Reads::Call,
            }),
            answer: Answer::Cold,
        }
    );
}

/// A return, a call edge of `WAY_IN_THE_ARM`, reads its answer as a way
/// in the arm.
#[test]
fn a_return_has_a_way_in_the_arm() {
    assert_eq!(
        arms::place("ReturnCv", "r#return", false),
        Placement::InTheLoop {
            path: None,
            answer: Answer::Way,
        }
    );
}

/// `DO_UCALL`, which `ENTERS` names, serves the push first.
#[test]
fn a_call_enters() {
    assert_eq!(
        arms::place("DoUcall", "do_ucall", false),
        Placement::InTheLoop {
            path: None,
            answer: Answer::Pushed {
                flagged: false,
                opcode: "ZEND_DO_UCALL",
            },
        }
    );
}

/// A name of `IN_THE_LOOP` that no handler serves is a build error.
#[test]
fn a_name_no_handler_serves_is_refused() {
    let held = arms::held(&[], std::path::Path::new("src/handlers.rs"));
    assert_eq!(
        held,
        Err(
            "`AddArrayElement` is in the loop's body by `IN_THE_LOOP` and is no variant a handler of src/handlers.rs serves"
                .to_owned()
        )
    );
}
