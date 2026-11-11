//! The lane's pure part, held without a process: what a mutation does to
//! a line.

use crate::generate::{Rng, trapped};

/// A trap never lands on a line that names a loop, so no mutation takes
/// a loop's bound or step away; a line that names none takes one.
#[test]
fn a_trap_leaves_a_loop_s_bound_and_step_alone() {
    let loops = [
        "$v0 = 0; while ($v0 < 4) { echo \"w\", $v0; $v0 = $v0 + 1; }",
        "for ($i = 0; $i < 4; $i++) { echo $i; }",
        "foreach ([1, 2] as $k => $v) { echo $k; }",
        "do { $n = $n + 1; } while ($n < 3);",
    ];
    for seed in 0..64 {
        let mut rng = Rng::new(seed);
        for line in loops {
            assert_eq!(trapped(line, &mut rng), line, "seed {seed}");
        }
        assert_ne!(trapped("echo 4, 7;", &mut rng), "echo 4, 7;");
    }
}
