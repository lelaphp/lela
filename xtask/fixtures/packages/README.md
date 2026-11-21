# The `packages` lane's fixture

`xtask packages` (#64) runs the test suites of the pinned Composer packages
of a list. This is its fixture list and the packages it pins, read by
`xtask/tests/packages.rs`, which lays each package of `sources/` down as a
git repository at one commit, made with a fixed author, committer and time,
so the commit the lists pin is the same on any machine and nothing is
fetched from the network. Each package's runner is `run-tests.php`, the
same in each: its `cases.txt` says what each test comes to under the oracle
and under Lela, and the test's stand-in for Lela's binary runs the pinned
php saying it is Lela.

`list.toml` must fail:

- `fixture/met` passes: one test met, one the oracle fails and one it skips,
  both outside whatever Lela does.
- `fixture/missed` fails: one test met, one Lela fails, one Lela's run does
  not report, both missed, and one the oracle errors, outside.
- `fixture/unrun` is left out: its runner ends before it writes its log, so
  its suite did not run under the oracle.
- `fixture/cache` is left out unfetched: the list marks it as needing
  memcached, which the lane does not provide.

So the whole fails, every listed package judged or left out, and names the
two left out. `refused.toml` pins `fixture/met` by a digest its source does
not have: the package is refused, the rest is judged, the whole is a skip
with `whole = 0` and the run exits 4. `empty.lock` is the lock every row
pins, of no development dependency.
