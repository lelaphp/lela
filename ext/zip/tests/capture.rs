//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

fn serve(code: &str) -> Vec<u8> {
    let engine = Engine::builder()
        .extensions(&[php_ext_zip::extension()])
        .build();
    engine.serve_capture(code, Capture::default()).stdout
}

#[test]
fn the_extension_is_loaded() {
    assert_eq!(
        serve("<?php var_dump(extension_loaded('zip'));"),
        b"bool(true)\n"
    );
}

/// An archive written, read back by name and by index, its methods
/// round-tripped through the file the close wrote.
#[test]
fn an_archive_round_trips() {
    let dir = std::env::temp_dir().join(format!("zip-capture-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap_or_default();
    let path = dir.join("t.zip");
    let path = path.to_string_lossy();
    let code = format!(
        r#"<?php
$z = new ZipArchive;
$z->open('{path}', ZipArchive::CREATE);
$z->addFromString('a.txt', 'abcabcabcabcabcabcabcabcabcabc');
$z->addFromString('b.bin', 'xy');
$z->setCompressionName('b.bin', ZipArchive::CM_BZIP2);
$z->setEncryptionName('a.txt', ZipArchive::EM_AES_256, 'pw');
$z->setArchiveComment('note');
echo (int) $z->close(), "\n";
$z->open('{path}');
echo $z->numFiles, ' ', $z->comment, ' ', $z->statIndex(1)['comp_method'], ' ', $z->statName('a.txt')['encryption_method'], "\n";
echo $z->getFromIndex(1), ' ', (int) ($z->getFromName('a.txt') === false), "\n";
$z->setPassword('pw');
echo strlen($z->getFromName('a.txt')), "\n";
"#
    );
    let out = serve(&code);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        String::from_utf8_lossy(&out),
        "1\n2 note 12 259\nxy 1\n30\n"
    );
}
