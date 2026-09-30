--TEST--
A redirect whose extension the binary does not load skips, and runs no set
--EXTENSIONS--
not_an_extension
--REDIRECTTEST--
return array('ENV' => array(), 'TESTS' => __DIR__ . '/ext/shared/tests');
