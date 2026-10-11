The `makala:` fixture of `xtask fpm-compare` and the served phpt lane
(#2629, #2644): `divergence.toml` holds one accepted row naming
`ext/filter/tests/004.phpt` and covering `007.phpt`, and none for
`011.phpt`. A difference on 004 is accepted, a 007 that agrees names the
row stale under `fpm-compare`, and a difference on 011 fails the reading,
so a comparison that reads no row, or reads one for a test it does not
name, fails
`a_difference_no_makala_row_names_fails_and_one_a_row_names_is_accepted`
and `the_served_lane_reads_makala_rows_as_fpm_compare_does`.
