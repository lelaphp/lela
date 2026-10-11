One subset's run as `xtask phpt` keeps it, at
`target/harness/phpt-runs/<tree>/<scope>.json`, with what each missed test keeps
under `misses` (#1185): its first differing lines and its signature. Every
kind of signature stands in it once, two tests share the `Warning` and the
unread construct, and three share `undefined function array_any`, so the
census of it is widest first with ties between two-test groups. The pass,
the skip, the xfail and the diverged test missed nothing and keep nothing.
`xtask/src/phpt/tests.rs` builds the census from this run and holds it with
`Census::accounts_for`, then drops one test from a group, and then one miss
from the run, and requires each of the two to fail.
