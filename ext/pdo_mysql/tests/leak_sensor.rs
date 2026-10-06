//! The request leak sensor (#1992) over PDO through this connector: each
//! request below gives up everything the natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
//! Against the server of `ci/mysql/compose.yaml`; with none listening
//! each test says so and passes.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// What one request of an armed engine with PDO, this connector and
/// standard wrote for `body`, served after `$db` is connected with
/// exceptions on; `None` where no server answers.
fn armed(body: &str) -> Option<String> {
    if std::net::TcpStream::connect("127.0.0.1:3306").is_err() {
        eprintln!("no MySQL server on 127.0.0.1:3306; ci/mysql/compose.yaml starts one");
        return None;
    }
    let engine = Engine::builder()
        .extensions(&[
            php_ext_standard::extension(),
            php_ext_spl::extension(),
            php_ext_pdo::extension(),
            php_ext_mysqlnd::extension(),
            php_ext_pdo_mysql::extension(),
        ])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let source = format!(
        "<?php $db = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '', [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION]);\n{body}"
    );
    let served = engine.serve_capture(&source, Capture::default());
    Some(String::from_utf8_lossy(&served.stdout).into_owned())
}

/// An emulated prepare quotes each bound value into the query as its
/// string, `zval_try_get_string`, and gives that string up once quoted:
/// a number bound as a string, converted in place where it is bound,
/// and a string bound as one (#2117, #2116).
#[test]
fn an_emulated_prepare_gives_up_the_strings_it_quotes() {
    let Some(stdout) = armed(
        r#"$s = $db->prepare('SELECT ?, ?'); $s->execute([15, 'a' . 'bc']); echo implode(',', $s->fetch(PDO::FETCH_NUM)), "\n";
$s->execute([16, str_repeat('x', 3)]); echo implode(',', $s->fetch(PDO::FETCH_NUM)), "\n";
"#,
    ) else {
        return;
    };
    assert_eq!(stdout, "15,abc\n16,xxx\n");
}

/// A fetch into a class whose constructor takes no arguments calls it
/// with none, and the empty array that stood for them is given up
/// (#2117).
#[test]
fn a_constructor_called_without_arguments_keeps_no_array() {
    let Some(stdout) = armed(
        r#"class R { public $a; function __construct() { echo "made\n"; } }
$o = $db->query('SELECT 1 AS a')->fetchObject('R'); echo $o->a, "\n";
"#,
    ) else {
        return;
    };
    assert_eq!(stdout, "made\n1\n");
}
