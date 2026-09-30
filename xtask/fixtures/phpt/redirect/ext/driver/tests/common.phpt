--TEST--
Fixture driver
--EXTENSIONS--
json
--REDIRECTTEST--
return array(
	'ENV' => array(
			'FIXTURE_DSN' => 'fixture::memory:'
		),
	'TESTS' => __DIR__ . '/ext/shared/tests'
	);
