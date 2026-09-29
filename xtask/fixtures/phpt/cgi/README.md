The CGI-shaped fixture of `xtask phpt` (#1198): `post.phpt` is a `--POST--`
test with a query string, a cookie and a header the run must write, which
passes only where the runner hands it to a CGI host with its request
described, so a runner that skips it, or runs it through the command line,
fails `a_post_test_runs_through_the_cgi_host`; `headers.phpt` is its negative
control, whose body is met and whose `--EXPECTHEADERS--` is not.
