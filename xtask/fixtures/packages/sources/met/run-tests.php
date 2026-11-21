<?php
// The fixture package's own runner, the same in each package: each line
// of cases.txt is a test, `<class>::<name> <under the oracle> <under Lela>`,
// each side `pass`, `fail`, `error`, `skip`, `-` for a test that side does
// not report, or `nolog` for a run that ends before it writes its log.
// Lela's side is the run whose environment says LELA_FIXTURE_ENGINE=1, as
// the test's stand-in for Lela's binary sets it. It writes the log
// `--log-junit` names, in the shape phpunit's is.
$log = $argv[array_search('--log-junit', $argv, true) + 1];
$side = getenv('LELA_FIXTURE_ENGINE') === '1' ? 2 : 1;
$children = [
    'pass' => '',
    'fail' => '<failure type="Fixture">it failed</failure>',
    'error' => '<error type="Fixture">it broke</error>',
    'skip' => '<skipped/>',
];
$xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>\n<testsuite name=\"fixture\">\n";
foreach (file(__DIR__ . '/cases.txt', FILE_IGNORE_NEW_LINES | FILE_SKIP_EMPTY_LINES) as $line) {
    $words = explode(' ', $line);
    [$class, $name] = explode('::', $words[0]);
    $outcome = $words[$side];
    if ($outcome === 'nolog') {
        exit(255);
    }
    if ($outcome !== '-') {
        $xml .= "<testcase name=\"$name\" class=\"$class\">{$children[$outcome]}</testcase>\n";
    }
}
file_put_contents($log, $xml . "</testsuite>\n</testsuites>\n");
