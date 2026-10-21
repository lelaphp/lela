//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let engine = Engine::builder()
        .extensions(&[php_ext_json::extension(), php_ext_spl::extension()])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('spl'));", sink);
    assert_eq!(served.stdout_text(), "bool(true)\n");
}

/// `spl_directory.c`, `spl_array.c` and the other serializable
/// containers call ext/standard's `PHPAPI` functions directly, so a
/// method stands with its native named in `disable_functions`
/// (`SplFileObject_fstat_with_basic_fstat_disabled.phpt`). The expected
/// bytes are the pinned oracle's under `-n -d disable_functions=<LIST>`.
#[test]
fn a_method_stands_with_the_native_it_mirrors_disabled() {
    const LIST: &str = "fstat,fputcsv,fgetcsv,str_getcsv,sscanf,serialize,unserialize,\
                        is_readable,is_writable,is_executable,glob";
    const PROBE: &str = r#"<?php
$t = new SplTempFileObject();
var_dump($t->fputcsv([7, 'apples'], ',', '"', ''));
$t->fwrite("\"x\ny\",z\n");
var_dump($t->fstat()['size']);
$t->rewind();
var_dump($t->fscanf('%d,%s'));
$t->setFlags(SplFileObject::READ_CSV);
$t->setCsvControl(',', '"', '');
foreach ($t as $row) { var_dump($row); }
$a = new ArrayObject([1, 'two']);
$s = $a->serialize();
echo $s, "\n";
$b = new ArrayObject();
$b->unserialize($s);
var_dump($b->getArrayCopy() === [1, 'two']);
var_dump((new SplFileInfo('/'))->isReadable());
var_dump((new GlobIterator('/*'))->count() > 0);
"#;
    const EXPECTED: &str = r#"int(9)
int(17)
array(2) {
  [0]=>
  int(7)
  [1]=>
  string(6) "apples"
}
array(2) {
  [0]=>
  string(1) "7"
  [1]=>
  string(6) "apples"
}
array(2) {
  [0]=>
  string(3) "x
y"
  [1]=>
  string(1) "z"
}
x:i:0;a:2:{i:0;i:1;i:1;s:3:"two";};m:a:0:{}
bool(true)
bool(true)
bool(true)
"#;
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_json::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
            php_ext_spl::extension(),
        ])
        .ini("disable_functions", LIST)
        .build();
    let served = engine.serve_capture(PROBE, Capture::default());
    assert_eq!(served.stdout_text(), EXPECTED);
}
