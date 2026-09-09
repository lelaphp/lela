//! The fixture's `xtask`: the call `scripts/worktree-add.sh` makes of the
//! binary it built, answered the way the harness answers it, and nothing
//! else; `track`, which the command asks of the clone's harness before the
//! worktree exists, is the real one's, placed by the test. `status --session` lays `.claude/harness.local.md` down
//! when the clone has none (harness-design.md §5), which is what the test
//! reads back; any other call is a usage error, so a command that grew a
//! third call fails here rather than passing quietly.

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["status", "--session"] => {
            std::fs::create_dir_all(".claude").expect("creating .claude");
            let local = std::path::Path::new(".claude/harness.local.md");
            if !local.exists() {
                std::fs::write(local, "# Local facts\n\n<!-- computed exec cmd=\"echo fixture\" volatile name=budget | do not edit; run computed -->\n<!-- /computed -->\n")
                    .expect("writing the local file");
            }
            print!("{}", std::fs::read_to_string(local).expect("reading it"));
        }
        _ => {
            writeln!(std::io::stderr(), "fixture xtask: no {args:?}").expect("stderr");
            std::process::exit(2);
        }
    }
}
