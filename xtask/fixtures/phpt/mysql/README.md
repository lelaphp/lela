The MySQL fixture of `xtask phpt` (#1366): `answered.phpt` stands where a
test of `ext/pdo_mysql/tests` stands and, as that suite's tests do, skips
where the server `PDO_MYSQL_TEST_DSN` names does not answer. With no server
a runner that reads it as skipped hands back a subset of skips as a verdict,
and fails `a_mysql_subset_with_no_server_is_refused_and_not_skipped`, which
holds that the probe and the runner both refuse it; where the pinned
recipe's server answers, it passes and the reading names the version.
