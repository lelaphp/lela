--TEST--
A redirect that names no set borks in upstream's words
--REDIRECTTEST--
return array('ENV' => array('FIXTURE_DSN' => 'fixture::memory:'));
