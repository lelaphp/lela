--TEST--
A test of a MySQL suite, which skips where the server does not answer
--SKIPIF--
<?php
try {
    new PDO(getenv('PDO_MYSQL_TEST_DSN'), 'root', '');
} catch (PDOException $e) {
    die('skip ' . $e->getMessage());
}
?>
--FILE--
<?php
$db = new PDO(getenv('PDO_MYSQL_TEST_DSN'), 'root', '');
var_dump($db->query('SELECT 1')->fetchColumn());
?>
--EXPECT--
int(1)
