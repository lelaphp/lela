The `--REDIRECTTEST--` fixture of `xtask phpt` (#814), laid out as php-src
lays the PDO drivers out: `ext/driver/tests/common.phpt` redirects to
`ext/shared/tests` under `FIXTURE_DSN`, as `ext/pdo_sqlite/tests/common.phpt`
redirects to `ext/pdo/tests` under `PDOTEST_DSN`, and needs `json` beside the
set's own extensions. `ext/shared/tests/dsn.phpt` passes only where it runs
with the section's environment and `REDIR_TEST_DIR`, its program written in
the redirecting test's directory, and skips "no driver" when its own
directory is run as the subset; `ext/shared/tests/second.phpt` does the
same, so the driver's run judges three tests over the two files its
directory holds, and its reading says both (#1589). `ext/refused/tests` holds the two
redirects that run no set: one whose extension the binary does not load,
a skip, and one whose section names no directory, a bork in upstream's
words. `a_redirecttest_runs_its_set_under_the_environment_its_section_returns`
holds all three, and
`a_reading_counts_the_files_its_subset_holds_beside_the_tests_a_redirect_ran`
the count.
