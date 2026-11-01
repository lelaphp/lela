The fixture of a run with no host of ours beside the engine (#3763):
`get.phpt` is CGI-shaped and `debugger.phpt` a `--PHPDBG--` test, both in the
target, run on an engine with no php-cgi or phpdbg built beside it. Each is
`unbuilt`, a build the run is missing, and a target miss of that kind fails
the reading, so a runner that skips them as upstream's "CGI not available",
counting them among `missed_skipped` and reading pass, fails
`a_target_test_no_host_of_ours_stands_beside_the_engine_for_fails_the_reading`.
`plain.phpt` runs through the command line and is met either way.
