The FPM fixture of `xtask phpt` (#1291): `found.phpt` stands where a test
of `sapi/fpm/tests` stands and asks for the php-fpm `FPM\Tester` finds last,
`dirname(__DIR__)/php-fpm`, which it runs for its version; it passes only
where the runner put a php-fpm there, so a runner that hands the subset
none skips it and fails `a_test_of_sapi_fpm_finds_the_process_manager`, and
the same test standing anywhere else skips, since the runner puts php-fpm
beside `sapi/fpm/tests` alone.
