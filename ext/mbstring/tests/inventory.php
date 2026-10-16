<?php
// Independent PHP-visible inventory; numeric identities come from pinned C, not list indices.
$rows = [];
$warnings = [];
set_error_handler(function ($severity, $message) use (&$warnings) { $warnings[] = $message; return true; });
foreach (mb_list_encodings() as $name) {
    try { $mime = mb_preferred_mime_name($name); } catch (ValueError $e) { $mime = null; }
    if ($mime === false) $mime = null;
    $rows[] = ['name' => $name, 'mime' => $mime, 'aliases' => mb_encoding_aliases($name)];
}
restore_error_handler();
echo json_encode(['encodings' => $rows, 'warnings' => $warnings], JSON_PRETTY_PRINT), "\n";
