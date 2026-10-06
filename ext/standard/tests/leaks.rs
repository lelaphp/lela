//! The leaks the request leak sensor found in this extension's natives
//! (#2057), each a program served with the sensor armed: what it wrote,
//! and no `InternalError` naming a block the heap still held after it.

#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// `source` served by an engine of this extension alone with the sensor
/// armed: what it wrote, and the fatal that ended it, if any.
fn armed(source: &str) -> (String, Option<String>) {
    let engine = Engine::builder()
        .extensions(&[php_ext_standard::extension()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    (
        String::from_utf8_lossy(&served.stdout).into_owned(),
        served.fatal,
    )
}

/// `proc_open` reads each element of its command array and each value
/// of its environment as a string and gives the string up once it has
/// copied its bytes, as `get_command_from_array` and `php_array_to_envp`
/// release what `zval_get_string` answered (#2064).
#[test]
fn proc_open_gives_up_the_strings_of_its_command_and_environment() {
    let (stdout, fatal) = armed(
        r#"<?php
$s = str_repeat('a', 3);
$p = proc_open(['/bin/echo', $s, "-$s"], [1 => ['pipe', 'w']], $pipes, null, ['X' => $s . 'b']);
echo stream_get_contents($pipes[1]);
proc_close($p);
"#,
    );
    assert_eq!(fatal, None);
    assert_eq!(stdout, "aaa -aaa\n");
}

/// The ini parser gives up the string it converted a constant's value
/// to once it has copied the bytes, as `zend_ini_get_constant` releases
/// its `convert_to_string` copy, whether the constant is the value, an
/// operand, a `${…}` fallback, an offset or a concatenation's right
/// side (#2065).
#[test]
fn the_ini_parser_gives_up_the_strings_of_the_constants_it_expands() {
    let (stdout, fatal) = armed(
        r#"<?php
echo serialize(parse_ini_string("a = E_ALL\nb = E_ALL & ~E_NOTICE\nc = \${nope:-E_ALL}\nd[E_ALL] = x\ne = \"v\" E_ALL")), "\n";
"#,
    );
    assert_eq!(fatal, None);
    assert_eq!(
        stdout,
        "a:5:{s:1:\"a\";s:5:\"30719\";s:1:\"b\";s:5:\"30711\";s:1:\"c\";s:5:\"30719\";\
         s:1:\"d\";a:1:{i:30719;s:1:\"x\";}s:1:\"e\";s:6:\"v30719\";}\n"
    );
}
