//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine, Files};

fn serve(code: &str) -> Vec<u8> {
    let engine = Engine::builder()
        .extensions(&[php_ext_pcre::extension(), php_ext_zip::extension()])
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
    let files = Files::new("zip-capture", &[]);
    let dir = files.path();
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
    assert_eq!(
        String::from_utf8_lossy(&out),
        "1\n2 note 12 259\nxy 1\n30\n"
    );
}

/// An entry added after `AFL_WANT_TORRENTZIP` is set is written,
/// as libzip 1.11.4 and 1.12.1 write it. `_zip_file_replace` clears the
/// entry's extra fields through `zip_file_extra_field_delete(za, idx,
/// ZIP_EXTRA_FIELD_ALL, ZIP_FL_CENTRAL | ZIP_FL_LOCAL)`
/// (`lib/zip_file_replace.c:89`), which libzip 1.12 alone refuses with
/// `ZIP_ER_NOT_ALLOWED` while torrentzip is wanted
/// (`lib/zip_extra_field_api.c:61`). 1.12 leaves the named entry without
/// data, and its close fails with `ZIP_ER_INTERNAL` where it stats the
/// entry's unchanged data (`lib/zip_close.c:236`,
/// `lib/zip_source_zip_new.c:90`). 1.12.1 lets that call through (libzip
/// e3f6db9, "Fix adding and replacing files in torrentzip archives"), so
/// the pinned oracle, which loads 1.12, misses `oo_torrentzip.phpt`, and
/// this is held through `Capture` and not a witness.
#[test]
fn an_entry_added_once_torrentzip_is_wanted_is_written() {
    let files = Files::new("zip-torrentzip", &[("a.txt", "xyz")]);
    let dir = files.path();
    let source = dir.join("a.txt");
    let source = source.to_string_lossy();
    let path = dir.join("t.zip");
    let path = path.to_string_lossy();
    let code = format!(
        r#"<?php
$z = new ZipArchive;
$z->open('{path}', ZipArchive::CREATE);
$z->setArchiveFlag(ZipArchive::AFL_WANT_TORRENTZIP, 1);
echo (int) $z->addFromString('b', 'abc'), (int) $z->addFile('{source}', 'a'), ' ', $z->status, "\n";
echo (int) $z->close(), "\n";
$z->open('{path}', ZipArchive::RDONLY);
echo $z->getArchiveFlag(ZipArchive::AFL_IS_TORRENTZIP), ' ', $z->getNameIndex(0), $z->getNameIndex(1), ' ', $z->getFromName('a'), $z->getFromName('b'), "\n";
"#
    );
    let out = serve(&code);
    assert_eq!(String::from_utf8_lossy(&out), "11 0\n1\n1 ab xyzabc\n");
}
