//! Encoder recursion is refused at the fiber's floor, inside a child
//! process so a regression cannot end the test runner.

mod support;

use php_ext::test::{Capture, Engine};

const CHILD: &str = "SOAP_ENCODER_RECURSION_CHILD";
const SOURCE: &str = include_str!("encoder_recursion.php");

#[test]
fn encoder_recursion_asks_the_floor() {
    if let Ok(shape) = std::env::var(CHILD) {
        let engine = Engine::builder()
            .extensions(support::extensions())
            .ini("fiber.stack_size", "1M")
            .ini("zend.max_allowed_stack_size", "256K")
            .build();
        for (depth, expected) in [(8, "ok\ncalls:1\n"), (6000, "floor\ncalls:0\n")] {
            let source = SOURCE.replacen(
                "<?php",
                &format!("<?php $shape = '{shape}'; $depth = {depth};"),
                1,
            );
            let served = engine.serve_capture(&source, Capture::default());
            assert_eq!(served.exit_code, 0, "{shape}, {depth}: {served:?}");
            assert!(served.stderr.is_empty(), "{shape}, {depth}: {served:?}");
            assert_eq!(
                served.stdout_text(),
                format!("{expected}ok\ntrace\nclean\n"),
                "{shape}, {depth}"
            );
        }
        let served = engine.serve_capture("<?php echo 'next request';", Capture::default());
        assert_eq!(served.stdout_text(), "next request");
    } else {
        let mut failures = Vec::new();
        for shape in ["array", "object", "soapvar"] {
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args(["--exact", "encoder_recursion_asks_the_floor", "--nocapture"])
                .env(CHILD, shape)
                .output()
                .expect("encoder child");
            if !output.status.success() {
                failures.push(format!(
                    "{shape}: child {}\n{}\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
