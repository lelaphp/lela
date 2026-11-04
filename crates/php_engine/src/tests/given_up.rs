//! A handler that gives up what an opline is reading or writing: each
//! program here is served with the leak sensor armed and ends the
//! request with status 0, what it wrote held, and with nothing leaked.

use crate::engine::{Engine, LeakSensor};
use crate::test::Capture;

/// `source` served with the leak sensor armed: what it wrote.
fn served(source: &str) -> String {
    let engine = Engine::cli_builder().sense_leaks(LeakSensor::Armed).build();
    let served = engine.serve_capture(source, Capture::default());
    let stdout = served.stdout_text();
    assert_eq!(served.exit_code, 0, "{stdout}");
    stdout
}

#[test]
fn a_string_a_float_offset_gave_up_answers_as_no_string() {
    let stdout = served(
        "<?php\n\
         set_error_handler(function () { $GLOBALS['s'] = 1; return true; });\n\
         $p = 'ab'; $s = $p . 'cd'; echo isset($s[1.5]) ? 'set' : 'not set', \"\\n\";\n\
         $s = $p . 'cd'; echo empty($s[1.5]) ? 'empty' : 'not empty', \"\\n\";\n",
    );
    assert_eq!(stdout, "not set\nempty\n");
}
