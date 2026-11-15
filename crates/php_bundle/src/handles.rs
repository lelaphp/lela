//! The family witness of the classes that refuse direct construction
//! (design §5.4): every such class the bundle carries,
//! held to the oracle on what php's handler table and class flags decide
//! about it, which is whether it can be constructed, cloned, compared
//! with another of its class and given a dynamic property. Here, in the
//! bundle, because the family spans the enabled set: its classes come
//! from twenty-three extensions and the engine, and this is the one
//! crate below the hosts that links every one of them. And here because
//! nothing the witness holds is written here: when the refusals move
//! from hand-written handler tables to the engine's default handlers,
//! this text stays as it is, and a handler pasted,
//! dropped or defaulted wrongly is a line of it that moved. `done` asks
//! for this crate's witness reading only when the crate changes, so a
//! change to those handlers is held to it by `xtask witness --crate
//! php_bundle`, by name. Compiled under `cfg(doc)` alone, a file of
//! rustdoc and no item.
//!
//! The construction matrix: each class constructed by `new`, by
//! `ReflectionClass::newInstance` and by `newInstanceWithoutConstructor`.
//! The list is every class whose `new` with no argument throws other than
//! for its arguments, under the oracle, less those of the three
//! extensions the bundle does not carry (ldap, odbc, pgsql). Half refuse
//! in a `get_constructor` handler or a constructor body: `Closure`,
//! `Generator`, `WeakReference`, `FiberError`, FFI's three, `PDORow`,
//! and the classes that took the place of a resource or of gd's integer
//! font. The other half refuse by a private constructor: `Dom\Node` and
//! its children, `Dom\NamespaceInfo` and `Dom\TokenList`,
//! `HashContext`, `InternalIterator`, intl's factory-made classes,
//! `mysqli_warning`, `tidyNode`, and two each of Reflection's and
//! SQLite3's.
//!
//! ```php
//! $classes = [
//!     'Closure', 'FiberError', 'Generator', 'InternalIterator', 'WeakReference',
//!     'CurlHandle', 'CurlMultiHandle', 'CurlShareHandle', 'CurlSharePersistentHandle',
//!     'Dba\Connection',
//!     'Dom\Attr', 'Dom\CDATASection', 'Dom\CharacterData', 'Dom\Comment', 'Dom\DocumentFragment', 'Dom\DocumentType', 'Dom\Element', 'Dom\Entity', 'Dom\EntityReference',
//!     'Dom\HTMLDocument', 'Dom\HTMLElement', 'Dom\NamespaceInfo', 'Dom\Node', 'Dom\Notation', 'Dom\ProcessingInstruction', 'Dom\Text', 'Dom\TokenList', 'Dom\XMLDocument',
//!     'FFI', 'FFI\CData', 'FFI\CType',
//!     'FTP\Connection',
//!     'GdFont', 'GdImage',
//!     'HashContext',
//!     'IntlBreakIterator', 'IntlCalendar', 'IntlCodePointBreakIterator', 'IntlTimeZone', 'Transliterator',
//!     'mysqli_warning',
//!     'OpenSSLAsymmetricKey', 'OpenSSLCertificate', 'OpenSSLCertificateSigningRequest',
//!     'PDORow',
//!     'ReflectionAttribute', 'ReflectionReference',
//!     'Shmop',
//!     'Soap\Sdl', 'Soap\Url',
//!     'AddressInfo', 'Socket',
//!     'SQLite3Result', 'SQLite3Stmt',
//!     'Directory',
//!     'SysvMessageQueue', 'SysvSemaphore', 'SysvSharedMemory',
//!     'tidyNode',
//!     'XMLParser',
//!     'DeflateContext', 'InflateContext',
//! ];
//! $build = function ($f) { try { $o = $f(); return 'made ' . get_class($o); } catch (Throwable $e) { return get_class($e) . ': ' . $e->getMessage(); } };
//! foreach ($classes as $c) {
//!     $r = new ReflectionClass($c);
//!     echo $c, "\n  new: ", $build(fn() => new $c), "\n  newInstance: ", $build(fn() => $r->newInstance()), "\n  withoutConstructor: ", $build(fn() => $r->newInstanceWithoutConstructor()), "\n";
//! }
//! ```
//! ```expect
//! Closure
//!   new: Error: Instantiation of class Closure is not allowed
//!   newInstance: Error: Instantiation of class Closure is not allowed
//!   withoutConstructor: ReflectionException: Class Closure is an internal class marked as final that cannot be instantiated without invoking its constructor
//! FiberError
//!   new: Error: The "FiberError" class is reserved for internal use and cannot be manually instantiated
//!   newInstance: Error: The "FiberError" class is reserved for internal use and cannot be manually instantiated
//!   withoutConstructor: ReflectionException: Class FiberError is an internal class marked as final that cannot be instantiated without invoking its constructor
//! Generator
//!   new: Error: The "Generator" class is reserved for internal use and cannot be manually instantiated
//!   newInstance: Error: The "Generator" class is reserved for internal use and cannot be manually instantiated
//!   withoutConstructor: ReflectionException: Class Generator is an internal class marked as final that cannot be instantiated without invoking its constructor
//! InternalIterator
//!   new: Error: Call to private InternalIterator::__construct() from global scope
//!   newInstance: ReflectionException: Access to non-public constructor of class InternalIterator
//!   withoutConstructor: ReflectionException: Class InternalIterator is an internal class marked as final that cannot be instantiated without invoking its constructor
//! WeakReference
//!   new: Error: Direct instantiation of WeakReference is not allowed, use WeakReference::create instead
//!   newInstance: Error: Direct instantiation of WeakReference is not allowed, use WeakReference::create instead
//!   withoutConstructor: ReflectionException: Class WeakReference is an internal class marked as final that cannot be instantiated without invoking its constructor
//! CurlHandle
//!   new: Error: Cannot directly construct CurlHandle, use curl_init() instead
//!   newInstance: Error: Cannot directly construct CurlHandle, use curl_init() instead
//!   withoutConstructor: ReflectionException: Class CurlHandle is an internal class marked as final that cannot be instantiated without invoking its constructor
//! CurlMultiHandle
//!   new: Error: Cannot directly construct CurlMultiHandle, use curl_multi_init() instead
//!   newInstance: Error: Cannot directly construct CurlMultiHandle, use curl_multi_init() instead
//!   withoutConstructor: ReflectionException: Class CurlMultiHandle is an internal class marked as final that cannot be instantiated without invoking its constructor
//! CurlShareHandle
//!   new: Error: Cannot directly construct CurlShareHandle, use curl_share_init() instead
//!   newInstance: Error: Cannot directly construct CurlShareHandle, use curl_share_init() instead
//!   withoutConstructor: ReflectionException: Class CurlShareHandle is an internal class marked as final that cannot be instantiated without invoking its constructor
//! CurlSharePersistentHandle
//!   new: Error: Cannot directly construct CurlSharePersistentHandle, use curl_share_init_persistent() instead
//!   newInstance: Error: Cannot directly construct CurlSharePersistentHandle, use curl_share_init_persistent() instead
//!   withoutConstructor: ReflectionException: Class CurlSharePersistentHandle is an internal class marked as final that cannot be instantiated without invoking its constructor
//! Dba\Connection
//!   new: Error: Cannot directly construct Dba\Connection, use dba_open() or dba_popen() instead
//!   newInstance: Error: Cannot directly construct Dba\Connection, use dba_open() or dba_popen() instead
//!   withoutConstructor: ReflectionException: Class Dba\Connection is an internal class marked as final that cannot be instantiated without invoking its constructor
//! Dom\Attr
//!   new: Error: Call to private Dom\Node::__construct() from global scope
//!   newInstance: Error: Call to private Dom\Node::__construct() from scope Dom\Attr
//!   withoutConstructor: made Dom\Attr
//! Dom\CDATASection
//!   new: Error: Call to private Dom\Node::__construct() from global scope
//!   newInstance: Error: Call to private Dom\Node::__construct() from scope Dom\CDATASection
//!   withoutConstructor: made Dom\CDATASection
//! Dom\CharacterData
//!   new: Error: Call to private Dom\Node::__construct() from global scope
//!   newInstance: Error: Call to private Dom\Node::__construct() from scope Dom\CharacterData
//!   withoutConstructor: made Dom\CharacterData
//! Dom\Comment
//!   new: Error: Call to private Dom\Node::__construct() from global scope
//!   newInstance: Error: Call to private Dom\Node::__construct() from scope Dom\Comment
//!   withoutConstructor: made Dom\Comment
//! Dom\DocumentFragment
//!   new: Error: Call to private Dom\Node::__construct() from global scope
//!   newInstance: Error: Call to private Dom\Node::__construct() from scope Dom\DocumentFragment
//!   withoutConstructor: made Dom\DocumentFragment
//! Dom\DocumentType
//!   new: Error: Call to private Dom\Node::__construct() from global scope
//!   newInstance: Error: Call to private Dom\Node::__construct() from scope Dom\DocumentType
//!   withoutConstructor: made Dom\DocumentType
//! Dom\Element
//!   new: Error: Call to private Dom\Node::__construct() from global scope
//!   newInstance: Error: Call to private Dom\Node::__construct() from scope Dom\Element
//!   withoutConstructor: made Dom\Element
//! Dom\Entity
//!   new: Error: Call to private Dom\Node::__construct() from global scope
//!   newInstance: Error: Call to private Dom\Node::__construct() from scope Dom\Entity
//!   withoutConstructor: made Dom\Entity
//! Dom\EntityReference
//!   new: Error: Call to private Dom\Node::__construct() from global scope
//!   newInstance: Error: Call to private Dom\Node::__construct() from scope Dom\EntityReference
//!   withoutConstructor: made Dom\EntityReference
//! Dom\HTMLDocument
//!   new: Error: Call to private Dom\Node::__construct() from global scope
//!   newInstance: Error: Call to private Dom\Node::__construct() from scope Dom\HTMLDocument
//!   withoutConstructor: ReflectionException: Class Dom\HTMLDocument is an internal class marked as final that cannot be instantiated without invoking its constructor
//! Dom\HTMLElement
//!   new: Error: Call to private Dom\Node::__construct() from global scope
//!   newInstance: Error: Call to private Dom\Node::__construct() from scope Dom\HTMLElement
//!   withoutConstructor: made Dom\HTMLElement
//! Dom\NamespaceInfo
//!   new: Error: Call to private Dom\NamespaceInfo::__construct() from global scope
//!   newInstance: ReflectionException: Access to non-public constructor of class Dom\NamespaceInfo
//!   withoutConstructor: made Dom\NamespaceInfo
//! Dom\Node
//!   new: Error: Call to private Dom\Node::__construct() from global scope
//!   newInstance: ReflectionException: Access to non-public constructor of class Dom\Node
//!   withoutConstructor: made Dom\Node
//! Dom\Notation
//!   new: Error: Call to private Dom\Node::__construct() from global scope
//!   newInstance: Error: Call to private Dom\Node::__construct() from scope Dom\Notation
//!   withoutConstructor: made Dom\Notation
//! Dom\ProcessingInstruction
//!   new: Error: Call to private Dom\Node::__construct() from global scope
//!   newInstance: Error: Call to private Dom\Node::__construct() from scope Dom\ProcessingInstruction
//!   withoutConstructor: made Dom\ProcessingInstruction
//! Dom\Text
//!   new: Error: Call to private Dom\Node::__construct() from global scope
//!   newInstance: Error: Call to private Dom\Node::__construct() from scope Dom\Text
//!   withoutConstructor: made Dom\Text
//! Dom\TokenList
//!   new: Error: Call to private Dom\TokenList::__construct() from global scope
//!   newInstance: ReflectionException: Access to non-public constructor of class Dom\TokenList
//!   withoutConstructor: ReflectionException: Class Dom\TokenList is an internal class marked as final that cannot be instantiated without invoking its constructor
//! Dom\XMLDocument
//!   new: Error: Call to private Dom\Node::__construct() from global scope
//!   newInstance: Error: Call to private Dom\Node::__construct() from scope Dom\XMLDocument
//!   withoutConstructor: ReflectionException: Class Dom\XMLDocument is an internal class marked as final that cannot be instantiated without invoking its constructor
//! FFI
//!   new: Error: Instantiation of FFI is not allowed
//!   newInstance: Error: Instantiation of FFI is not allowed
//!   withoutConstructor: ReflectionException: Class FFI is an internal class marked as final that cannot be instantiated without invoking its constructor
//! FFI\CData
//!   new: Error: Instantiation of FFI\CData is not allowed
//!   newInstance: Error: Instantiation of FFI\CData is not allowed
//!   withoutConstructor: ReflectionException: Class FFI\CData is an internal class marked as final that cannot be instantiated without invoking its constructor
//! FFI\CType
//!   new: Error: Instantiation of FFI\CType is not allowed
//!   newInstance: Error: Instantiation of FFI\CType is not allowed
//!   withoutConstructor: ReflectionException: Class FFI\CType is an internal class marked as final that cannot be instantiated without invoking its constructor
//! FTP\Connection
//!   new: Error: Cannot directly construct FTP\Connection, use ftp_connect() or ftp_ssl_connect() instead
//!   newInstance: Error: Cannot directly construct FTP\Connection, use ftp_connect() or ftp_ssl_connect() instead
//!   withoutConstructor: ReflectionException: Class FTP\Connection is an internal class marked as final that cannot be instantiated without invoking its constructor
//! GdFont
//!   new: Error: You cannot initialize a GdFont object except through helper functions
//!   newInstance: Error: You cannot initialize a GdFont object except through helper functions
//!   withoutConstructor: ReflectionException: Class GdFont is an internal class marked as final that cannot be instantiated without invoking its constructor
//! GdImage
//!   new: Error: You cannot initialize a GdImage object except through helper functions
//!   newInstance: Error: You cannot initialize a GdImage object except through helper functions
//!   withoutConstructor: ReflectionException: Class GdImage is an internal class marked as final that cannot be instantiated without invoking its constructor
//! HashContext
//!   new: Error: Call to private HashContext::__construct() from global scope
//!   newInstance: ReflectionException: Access to non-public constructor of class HashContext
//!   withoutConstructor: ReflectionException: Class HashContext is an internal class marked as final that cannot be instantiated without invoking its constructor
//! IntlBreakIterator
//!   new: Error: Call to private IntlBreakIterator::__construct() from global scope
//!   newInstance: ReflectionException: Access to non-public constructor of class IntlBreakIterator
//!   withoutConstructor: made IntlBreakIterator
//! IntlCalendar
//!   new: Error: Call to private IntlCalendar::__construct() from global scope
//!   newInstance: ReflectionException: Access to non-public constructor of class IntlCalendar
//!   withoutConstructor: made IntlCalendar
//! IntlCodePointBreakIterator
//!   new: Error: Call to private IntlBreakIterator::__construct() from global scope
//!   newInstance: Error: Call to private IntlBreakIterator::__construct() from scope IntlCodePointBreakIterator
//!   withoutConstructor: made IntlCodePointBreakIterator
//! IntlTimeZone
//!   new: Error: Call to private IntlTimeZone::__construct() from global scope
//!   newInstance: ReflectionException: Access to non-public constructor of class IntlTimeZone
//!   withoutConstructor: made IntlTimeZone
//! Transliterator
//!   new: Error: Call to private Transliterator::__construct() from global scope
//!   newInstance: ReflectionException: Access to non-public constructor of class Transliterator
//!   withoutConstructor: made Transliterator
//! mysqli_warning
//!   new: Error: Call to private mysqli_warning::__construct() from global scope
//!   newInstance: ReflectionException: Access to non-public constructor of class mysqli_warning
//!   withoutConstructor: ReflectionException: Class mysqli_warning is an internal class marked as final that cannot be instantiated without invoking its constructor
//! OpenSSLAsymmetricKey
//!   new: Error: Cannot directly construct OpenSSLAsymmetricKey, use openssl_pkey_new() instead
//!   newInstance: Error: Cannot directly construct OpenSSLAsymmetricKey, use openssl_pkey_new() instead
//!   withoutConstructor: ReflectionException: Class OpenSSLAsymmetricKey is an internal class marked as final that cannot be instantiated without invoking its constructor
//! OpenSSLCertificate
//!   new: Error: Cannot directly construct OpenSSLCertificate, use openssl_x509_read() instead
//!   newInstance: Error: Cannot directly construct OpenSSLCertificate, use openssl_x509_read() instead
//!   withoutConstructor: ReflectionException: Class OpenSSLCertificate is an internal class marked as final that cannot be instantiated without invoking its constructor
//! OpenSSLCertificateSigningRequest
//!   new: Error: Cannot directly construct OpenSSLCertificateSigningRequest, use openssl_csr_new() instead
//!   newInstance: Error: Cannot directly construct OpenSSLCertificateSigningRequest, use openssl_csr_new() instead
//!   withoutConstructor: ReflectionException: Class OpenSSLCertificateSigningRequest is an internal class marked as final that cannot be instantiated without invoking its constructor
//! PDORow
//!   new: PDOException: You may not create a PDORow manually
//!   newInstance: PDOException: You may not create a PDORow manually
//!   withoutConstructor: ReflectionException: Class PDORow is an internal class marked as final that cannot be instantiated without invoking its constructor
//! ReflectionAttribute
//!   new: Error: Call to private ReflectionAttribute::__construct() from global scope
//!   newInstance: ReflectionException: Access to non-public constructor of class ReflectionAttribute
//!   withoutConstructor: made ReflectionAttribute
//! ReflectionReference
//!   new: Error: Call to private ReflectionReference::__construct() from global scope
//!   newInstance: ReflectionException: Access to non-public constructor of class ReflectionReference
//!   withoutConstructor: ReflectionException: Class ReflectionReference is an internal class marked as final that cannot be instantiated without invoking its constructor
//! Shmop
//!   new: Error: Cannot directly construct Shmop, use shmop_open() instead
//!   newInstance: Error: Cannot directly construct Shmop, use shmop_open() instead
//!   withoutConstructor: ReflectionException: Class Shmop is an internal class marked as final that cannot be instantiated without invoking its constructor
//! Soap\Sdl
//!   new: Error: Cannot directly construct Soap\Sdl
//!   newInstance: Error: Cannot directly construct Soap\Sdl
//!   withoutConstructor: ReflectionException: Class Soap\Sdl is an internal class marked as final that cannot be instantiated without invoking its constructor
//! Soap\Url
//!   new: Error: Cannot directly construct Soap\Url
//!   newInstance: Error: Cannot directly construct Soap\Url
//!   withoutConstructor: ReflectionException: Class Soap\Url is an internal class marked as final that cannot be instantiated without invoking its constructor
//! AddressInfo
//!   new: Error: Cannot directly construct AddressInfo, use socket_addrinfo_lookup() instead
//!   newInstance: Error: Cannot directly construct AddressInfo, use socket_addrinfo_lookup() instead
//!   withoutConstructor: ReflectionException: Class AddressInfo is an internal class marked as final that cannot be instantiated without invoking its constructor
//! Socket
//!   new: Error: Cannot directly construct Socket, use socket_create() instead
//!   newInstance: Error: Cannot directly construct Socket, use socket_create() instead
//!   withoutConstructor: ReflectionException: Class Socket is an internal class marked as final that cannot be instantiated without invoking its constructor
//! SQLite3Result
//!   new: Error: Call to private SQLite3Result::__construct() from global scope
//!   newInstance: ReflectionException: Access to non-public constructor of class SQLite3Result
//!   withoutConstructor: made SQLite3Result
//! SQLite3Stmt
//!   new: Error: Call to private SQLite3Stmt::__construct() from global scope
//!   newInstance: ReflectionException: Access to non-public constructor of class SQLite3Stmt
//!   withoutConstructor: made SQLite3Stmt
//! Directory
//!   new: Error: Cannot directly construct Directory, use dir() instead
//!   newInstance: Error: Cannot directly construct Directory, use dir() instead
//!   withoutConstructor: made Directory
//! SysvMessageQueue
//!   new: Error: Cannot directly construct SysvMessageQueue, use msg_get_queue() instead
//!   newInstance: Error: Cannot directly construct SysvMessageQueue, use msg_get_queue() instead
//!   withoutConstructor: ReflectionException: Class SysvMessageQueue is an internal class marked as final that cannot be instantiated without invoking its constructor
//! SysvSemaphore
//!   new: Error: Cannot directly construct SysvSemaphore, use sem_get() instead
//!   newInstance: Error: Cannot directly construct SysvSemaphore, use sem_get() instead
//!   withoutConstructor: ReflectionException: Class SysvSemaphore is an internal class marked as final that cannot be instantiated without invoking its constructor
//! SysvSharedMemory
//!   new: Error: Cannot directly construct SysvSharedMemory, use shm_attach() instead
//!   newInstance: Error: Cannot directly construct SysvSharedMemory, use shm_attach() instead
//!   withoutConstructor: ReflectionException: Class SysvSharedMemory is an internal class marked as final that cannot be instantiated without invoking its constructor
//! tidyNode
//!   new: Error: Call to private tidyNode::__construct() from global scope
//!   newInstance: ReflectionException: Access to non-public constructor of class tidyNode
//!   withoutConstructor: ReflectionException: Class tidyNode is an internal class marked as final that cannot be instantiated without invoking its constructor
//! XMLParser
//!   new: Error: Cannot directly construct XMLParser, use xml_parser_create() or xml_parser_create_ns() instead
//!   newInstance: Error: Cannot directly construct XMLParser, use xml_parser_create() or xml_parser_create_ns() instead
//!   withoutConstructor: ReflectionException: Class XMLParser is an internal class marked as final that cannot be instantiated without invoking its constructor
//! DeflateContext
//!   new: Error: Cannot directly construct DeflateContext, use deflate_init() instead
//!   newInstance: Error: Cannot directly construct DeflateContext, use deflate_init() instead
//!   withoutConstructor: ReflectionException: Class DeflateContext is an internal class marked as final that cannot be instantiated without invoking its constructor
//! InflateContext
//!   new: Error: Cannot directly construct InflateContext, use inflate_init() instead
//!   newInstance: Error: Cannot directly construct InflateContext, use inflate_init() instead
//!   withoutConstructor: ReflectionException: Class InflateContext is an internal class marked as final that cannot be instantiated without invoking its constructor
//! ```
//!
//! The clone-and-compare matrix: two of each class that one process can
//! make with no server, 57 of the 62, the first cloned, compared with the
//! second by `==`, `<` and `<=>` both ways, with itself and with a
//! `stdClass`, and given a dynamic property, each answer printed as its
//! value or as the message it threw; a deprecation prints in brackets
//! from the error handler. No document makes an object of exactly
//! `Dom\Node` or `Dom\CharacterData`, so neither has a pair; cloning a
//! `Dom\Entity` crashes the oracle itself; and only a `MySQL` server
//! makes a `mysqli_warning`, where a `mysql` fence would leave the whole
//! crate unjudged when none answers. The System V and shmop objects are
//! made under `IPC_PRIVATE` and removed at the end, so nothing outlives
//! the snippet.
//!
//! ```php
//! $font = tempnam(sys_get_temp_dir(), 'font');
//! $x = Dom\XMLDocument::createFromString('<!DOCTYPE r [<!ENTITY e "v"><!NOTATION n SYSTEM "s">]><r xmlns:p="urn:p">&e;&e;</r>');
//! $h = Dom\HTMLDocument::createEmpty();
//! $db = new PDO('sqlite::memory:');
//! $lite = new SQLite3(':memory:');
//! $ffi = FFI::cdef();
//! $key = openssl_pkey_new(['private_key_type' => OPENSSL_KEYTYPE_EC, 'curve_name' => 'prime256v1']);
//! $csr = openssl_csr_new(['commonName' => 'a'], $key);
//! file_put_contents($font, pack('V4', 1, 65, 1, 1) . "\1");
//! $srv = stream_socket_server('tcp://127.0.0.1:0');
//! ini_set('default_socket_timeout', 0);
//! $soap = fn() => new SoapClient(null, ['location' => 'http://' . stream_socket_get_name($srv, false) . '/', 'uri' => 'urn:t', 'connection_timeout' => 5, 'exceptions' => false]);
//! $wsdl = 'data://text/plain,' . urlencode("<definitions xmlns='http://schemas.xmlsoap.org/wsdl/' xmlns:soap='http://schemas.xmlsoap.org/wsdl/soap/' targetNamespace='urn:t'><portType name='P'/><binding name='B' type='P'><soap:binding transport='http://schemas.xmlsoap.org/soap/http'/></binding><service name='S'><port name='Q' binding='B'><soap:address location='http://localhost/'/></port></service></definitions>");
//! #[Attribute] class A {}
//! #[A] function attributed() {}
//! $keep = $files = [];
//! $make = [
//!     'Closure' => fn() => fn() => 1,
//!     'FiberError' => function () { try { (new Fiber(fn() => 1))->resume(); } catch (FiberError $e) { return $e; } },
//!     'Generator' => fn() => (fn() => yield 1)(),
//!     'InternalIterator' => fn() => (new DatePeriod(new DateTime('2020-01-01'), new DateInterval('P1D'), 1))->getIterator(),
//!     'WeakReference' => function () use (&$keep) { return WeakReference::create($keep[] = new stdClass); },
//!     'CurlHandle' => fn() => curl_init(),
//!     'CurlMultiHandle' => fn() => curl_multi_init(),
//!     'CurlShareHandle' => fn() => curl_share_init(),
//!     'CurlSharePersistentHandle' => fn() => curl_share_init_persistent([CURL_LOCK_DATA_DNS]),
//!     'Dba\Connection' => function () use (&$files) { return dba_open($files[] = tempnam(sys_get_temp_dir(), 'dba'), 'n', 'inifile'); },
//!     'Dom\Attr' => fn() => $x->createAttribute('a'),
//!     'Dom\CDATASection' => fn() => $x->createCDATASection('c'),
//!     'Dom\Comment' => fn() => $x->createComment('c'),
//!     'Dom\DocumentFragment' => fn() => $x->createDocumentFragment(),
//!     'Dom\DocumentType' => fn() => Dom\XMLDocument::createFromString('<!DOCTYPE r><r/>')->doctype,
//!     'Dom\Element' => fn() => $x->createElement('e'),
//!     'Dom\EntityReference' => function () use ($x) { static $i = 0; return $x->documentElement->childNodes[$i++]; },
//!     'Dom\HTMLDocument' => fn() => Dom\HTMLDocument::createEmpty(),
//!     'Dom\HTMLElement' => fn() => $h->createElement('p'),
//!     'Dom\NamespaceInfo' => fn() => $x->documentElement->getInScopeNamespaces()[0],
//!     'Dom\Notation' => fn() => $x->doctype->notations->item(0),
//!     'Dom\ProcessingInstruction' => fn() => $x->createProcessingInstruction('p', 'd'),
//!     'Dom\Text' => fn() => $x->createTextNode('t'),
//!     'Dom\TokenList' => fn() => $x->createElement('e')->classList,
//!     'Dom\XMLDocument' => fn() => Dom\XMLDocument::createEmpty(),
//!     'FFI' => fn() => FFI::cdef(),
//!     'FFI\CData' => fn() => $ffi->new('int'),
//!     'FFI\CType' => fn() => $ffi->type('int'),
//!     'GdFont' => fn() => imageloadfont($font),
//!     'GdImage' => fn() => imagecreate(1, 1),
//!     'HashContext' => fn() => hash_init('md5'),
//!     'IntlBreakIterator' => fn() => IntlBreakIterator::createWordInstance('en'),
//!     'IntlCodePointBreakIterator' => fn() => IntlBreakIterator::createCodePointInstance(),
//!     'IntlCalendar' => fn() => IntlCalendar::createInstance('UTC', 'en_US'),
//!     'IntlTimeZone' => fn() => IntlTimeZone::createTimeZone('UTC'),
//!     'Transliterator' => fn() => Transliterator::create('Any-Latin'),
//!     'OpenSSLAsymmetricKey' => fn() => openssl_pkey_get_private($key),
//!     'OpenSSLCertificate' => fn() => openssl_csr_sign($csr, null, $key, 1, ['digest_alg' => 'sha256'], 7),
//!     'OpenSSLCertificateSigningRequest' => fn() => openssl_csr_new(['commonName' => 'a'], $key),
//!     'PDORow' => fn() => $db->query('SELECT 1 AS a')->fetch(PDO::FETCH_LAZY),
//!     'ReflectionAttribute' => fn() => (new ReflectionFunction('attributed'))->getAttributes()[0],
//!     'ReflectionReference' => function () { $v = 1; $a = [&$v]; return ReflectionReference::fromArrayElement($a, 0); },
//!     'Shmop' => fn() => shmop_open(0, 'c', 0600, 8),
//!     'Soap\Sdl' => fn() => (new ReflectionProperty('SoapClient', 'sdl'))->getValue(new SoapClient($wsdl, ['cache_wsdl' => WSDL_CACHE_NONE])),
//!     'Soap\Url' => function () use ($soap) { $c = $soap(); $c->f(); return (new ReflectionProperty($c, 'httpurl'))->getValue($c); },
//!     'AddressInfo' => fn() => socket_addrinfo_lookup('127.0.0.1', null, ['ai_family' => AF_INET, 'ai_socktype' => SOCK_STREAM])[0],
//!     'Socket' => fn() => socket_create(AF_INET, SOCK_STREAM, SOL_TCP),
//!     'SQLite3Result' => fn() => $lite->query('SELECT 1'),
//!     'SQLite3Stmt' => fn() => $lite->prepare('SELECT 1'),
//!     'Directory' => fn() => dir('.'),
//!     'SysvMessageQueue' => fn() => msg_get_queue(0),
//!     'SysvSemaphore' => fn() => sem_get(0),
//!     'SysvSharedMemory' => fn() => shm_attach(0, 64),
//!     'tidyNode' => fn() => tidy_parse_string('<p>t</p>')->root(),
//!     'XMLParser' => fn() => xml_parser_create(),
//!     'DeflateContext' => fn() => deflate_init(ZLIB_ENCODING_DEFLATE),
//!     'InflateContext' => fn() => inflate_init(ZLIB_ENCODING_DEFLATE),
//! ];
//! $try = function ($f) { try { $r = $f(); return is_object($r) ? 'a ' . get_class($r) : var_export($r, true); } catch (Throwable $e) { return $e->getMessage(); } };
//! set_error_handler(function ($no, $msg) { echo "  [$msg]\n"; return true; });
//! foreach ($make as $class => $m) {
//!     [$a, $b] = $made[$class] = [$m(), $m()];
//!     echo $class, ' (', get_debug_type($a), ")\n  clone: ", $try(fn() => clone $a), "\n";
//!     echo '  ', implode(' ', array_map($try, [fn() => $a == $b, fn() => $a < $b, fn() => $a <=> $b, fn() => $b <=> $a, fn() => $a == $a, fn() => $a == new stdClass])), "\n";
//!     echo '  dynamic: ', $try(function () use ($a) { $a->x = 1; return isset($a->x); }), "\n";
//! }
//! array_map('shmop_delete', $made['Shmop']); array_map('msg_remove_queue', $made['SysvMessageQueue']);
//! array_map('sem_remove', $made['SysvSemaphore']); array_map('shm_remove', $made['SysvSharedMemory']);
//! array_map('unlink', [$font, ...$files]);
//! ```
//! ```expect
//! Closure (Closure)
//!   clone: a Closure
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property Closure::$x
//! FiberError (FiberError)
//!   clone: Trying to clone an uncloneable object of class FiberError
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property FiberError::$x is deprecated]
//! true
//! Generator (Generator)
//!   clone: Trying to clone an uncloneable object of class Generator
//!   true false 0 0 true false
//!   dynamic: Cannot create dynamic property Generator::$x
//! InternalIterator (InternalIterator)
//!   clone: Trying to clone an uncloneable object of class InternalIterator
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property InternalIterator::$x is deprecated]
//! true
//! WeakReference (WeakReference)
//!   clone: Trying to clone an uncloneable object of class WeakReference
//!   true false 0 0 true false
//!   dynamic: Cannot create dynamic property WeakReference::$x
//! CurlHandle (CurlHandle)
//!   clone: a CurlHandle
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property CurlHandle::$x
//! CurlMultiHandle (CurlMultiHandle)
//!   clone: Trying to clone an uncloneable object of class CurlMultiHandle
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property CurlMultiHandle::$x
//! CurlShareHandle (CurlShareHandle)
//!   clone: Trying to clone an uncloneable object of class CurlShareHandle
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property CurlShareHandle::$x
//! CurlSharePersistentHandle (CurlSharePersistentHandle)
//!   clone: Trying to clone an uncloneable object of class CurlSharePersistentHandle
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property CurlSharePersistentHandle::$x
//! Dba\Connection (Dba\Connection)
//!   clone: Trying to clone an uncloneable object of class Dba\Connection
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property Dba\Connection::$x
//! Dom\Attr (Dom\Attr)
//!   clone: a Dom\Attr
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\Attr::$x is deprecated]
//! true
//! Dom\CDATASection (Dom\CDATASection)
//!   clone: a Dom\CDATASection
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\CDATASection::$x is deprecated]
//! true
//! Dom\Comment (Dom\Comment)
//!   clone: a Dom\Comment
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\Comment::$x is deprecated]
//! true
//! Dom\DocumentFragment (Dom\DocumentFragment)
//!   clone: a Dom\DocumentFragment
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\DocumentFragment::$x is deprecated]
//! true
//! Dom\DocumentType (Dom\DocumentType)
//!   clone: a Dom\DocumentType
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\DocumentType::$x is deprecated]
//! true
//! Dom\Element (Dom\Element)
//!   clone: a Dom\Element
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\Element::$x is deprecated]
//! true
//! Dom\EntityReference (Dom\EntityReference)
//!   clone: a Dom\EntityReference
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\EntityReference::$x is deprecated]
//! true
//! Dom\HTMLDocument (Dom\HTMLDocument)
//!   clone: a Dom\HTMLDocument
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\HTMLDocument::$x is deprecated]
//! true
//! Dom\HTMLElement (Dom\HTMLElement)
//!   clone: a Dom\HTMLElement
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\HTMLElement::$x is deprecated]
//! true
//! Dom\NamespaceInfo (Dom\NamespaceInfo)
//!   clone: a Dom\NamespaceInfo
//!   true false 0 0 true false
//!   dynamic: Cannot create dynamic property Dom\NamespaceInfo::$x
//! Dom\Notation (Dom\Notation)
//!   clone: a Dom\Notation
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\Notation::$x is deprecated]
//! true
//! Dom\ProcessingInstruction (Dom\ProcessingInstruction)
//!   clone: a Dom\ProcessingInstruction
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\ProcessingInstruction::$x is deprecated]
//! true
//! Dom\Text (Dom\Text)
//!   clone: a Dom\Text
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\Text::$x is deprecated]
//! true
//! Dom\TokenList (Dom\TokenList)
//!   clone: Trying to clone an uncloneable object of class Dom\TokenList
//!   true false 0 0 true false
//!   dynamic: Cannot create dynamic property Dom\TokenList::$x
//! Dom\XMLDocument (Dom\XMLDocument)
//!   clone: a Dom\XMLDocument
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\XMLDocument::$x is deprecated]
//! true
//! FFI (FFI)
//!   clone: Trying to clone an uncloneable object of class FFI
//!   Cannot compare FFI objects Cannot compare FFI objects Cannot compare FFI objects Cannot compare FFI objects true Cannot compare FFI objects
//!   dynamic: Attempt to assign undefined C variable 'x'
//! FFI\CData (FFI\CData)
//!   clone: a FFI\CData
//!   Comparison of incompatible C types Comparison of incompatible C types Comparison of incompatible C types Comparison of incompatible C types true Comparison of incompatible C types
//!   dynamic: Only 'cdata' property may be set
//! FFI\CType (FFI\CType)
//!   clone: Trying to clone an uncloneable object of class FFI\CType
//!   true false 0 0 true Comparison of incompatible C types
//!   dynamic: Cannot use object of type FFI\CType as array
//! GdFont (GdFont)
//!   clone: Trying to clone an uncloneable object of class GdFont
//!   true false 0 0 true false
//!   dynamic: Cannot create dynamic property GdFont::$x
//! GdImage (GdImage)
//!   clone: Trying to clone an uncloneable object of class GdImage
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property GdImage::$x
//! HashContext (HashContext)
//!   clone: a HashContext
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property HashContext::$x is deprecated]
//! true
//! IntlBreakIterator (IntlRuleBasedBreakIterator)
//!   clone: a IntlRuleBasedBreakIterator
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property IntlRuleBasedBreakIterator::$x is deprecated]
//! true
//! IntlCodePointBreakIterator (IntlCodePointBreakIterator)
//!   clone: a IntlCodePointBreakIterator
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property IntlCodePointBreakIterator::$x is deprecated]
//! true
//! IntlCalendar (IntlGregorianCalendar)
//!   clone: a IntlGregorianCalendar
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property IntlGregorianCalendar::$x is deprecated]
//! true
//! IntlTimeZone (IntlTimeZone)
//!   clone: a IntlTimeZone
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property IntlTimeZone::$x is deprecated]
//! true
//! Transliterator (Transliterator)
//!   clone: a Transliterator
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Transliterator::$x is deprecated]
//! true
//! OpenSSLAsymmetricKey (OpenSSLAsymmetricKey)
//!   clone: Trying to clone an uncloneable object of class OpenSSLAsymmetricKey
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property OpenSSLAsymmetricKey::$x
//! OpenSSLCertificate (OpenSSLCertificate)
//!   clone: Trying to clone an uncloneable object of class OpenSSLCertificate
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property OpenSSLCertificate::$x
//! OpenSSLCertificateSigningRequest (OpenSSLCertificateSigningRequest)
//!   clone: Trying to clone an uncloneable object of class OpenSSLCertificateSigningRequest
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property OpenSSLCertificateSigningRequest::$x
//! PDORow (PDORow)
//!   clone: Trying to clone an uncloneable object of class PDORow
//!   false false 1 1 true false
//!   dynamic: Cannot write to PDORow property
//! ReflectionAttribute (ReflectionAttribute)
//!   clone: Trying to clone an uncloneable object of class ReflectionAttribute
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property ReflectionAttribute::$x is deprecated]
//! true
//! ReflectionReference (ReflectionReference)
//!   clone: Trying to clone an uncloneable object of class ReflectionReference
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property ReflectionReference::$x is deprecated]
//! true
//! Shmop (Shmop)
//!   clone: Trying to clone an uncloneable object of class Shmop
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property Shmop::$x
//! Soap\Sdl (Soap\Sdl)
//!   clone: Trying to clone an uncloneable object of class Soap\Sdl
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property Soap\Sdl::$x
//! Soap\Url (Soap\Url)
//!   clone: Trying to clone an uncloneable object of class Soap\Url
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property Soap\Url::$x
//! AddressInfo (AddressInfo)
//!   clone: Trying to clone an uncloneable object of class AddressInfo
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property AddressInfo::$x
//! Socket (Socket)
//!   clone: Trying to clone an uncloneable object of class Socket
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property Socket::$x
//! SQLite3Result (SQLite3Result)
//!   clone: Trying to clone an uncloneable object of class SQLite3Result
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property SQLite3Result::$x is deprecated]
//! true
//! SQLite3Stmt (SQLite3Stmt)
//!   clone: Trying to clone an uncloneable object of class SQLite3Stmt
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property SQLite3Stmt::$x is deprecated]
//! true
//! Directory (Directory)
//!   clone: Trying to clone an uncloneable object of class Directory
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property Directory::$x
//! SysvMessageQueue (SysvMessageQueue)
//!   clone: Trying to clone an uncloneable object of class SysvMessageQueue
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property SysvMessageQueue::$x
//! SysvSemaphore (SysvSemaphore)
//!   clone: Trying to clone an uncloneable object of class SysvSemaphore
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property SysvSemaphore::$x
//! SysvSharedMemory (SysvSharedMemory)
//!   clone: Trying to clone an uncloneable object of class SysvSharedMemory
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property SysvSharedMemory::$x
//! tidyNode (tidyNode)
//!   clone: Trying to clone an uncloneable object of class tidyNode
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property tidyNode::$x is deprecated]
//! true
//! XMLParser (XMLParser)
//!   clone: Trying to clone an uncloneable object of class XMLParser
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property XMLParser::$x
//! DeflateContext (DeflateContext)
//!   clone: Trying to clone an uncloneable object of class DeflateContext
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property DeflateContext::$x
//! InflateContext (InflateContext)
//!   clone: Trying to clone an uncloneable object of class InflateContext
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property InflateContext::$x
//! ```
//!
//! The one pair a server has to make: two `FTP\Connection`s to a greeter
//! the snippet forks, so the fence is served in a process of its own.
//!
//! ```php
//! $s = stream_socket_server('tcp://127.0.0.1:0'); $p = (int) explode(':', stream_socket_get_name($s, false))[1];
//! if (!pcntl_fork()) { foreach ([1, 2] as $_) { $c[] = $x = stream_socket_accept($s); fwrite($x, "220 hi\r\n"); } exit; }
//! $try = function ($f) { try { $r = $f(); return is_object($r) ? 'a ' . get_class($r) : var_export($r, true); } catch (Throwable $e) { return $e->getMessage(); } };
//! set_error_handler(function ($no, $msg) { echo "  [$msg]\n"; return true; });
//! [$a, $b] = [ftp_connect('127.0.0.1', $p), ftp_connect('127.0.0.1', $p)];
//! echo 'FTP\Connection (', get_debug_type($a), ")\n  clone: ", $try(fn() => clone $a), "\n";
//! echo '  ', implode(' ', array_map($try, [fn() => $a == $b, fn() => $a < $b, fn() => $a <=> $b, fn() => $b <=> $a, fn() => $a == $a, fn() => $a == new stdClass])), "\n";
//! echo '  dynamic: ', $try(function () use ($a) { $a->x = 1; return isset($a->x); }), "\n";
//! ```
//! ```expect process
//! FTP\Connection (FTP\Connection)
//!   clone: Trying to clone an uncloneable object of class FTP\Connection
//!   true false 0 0 true false
//!   dynamic: Cannot create dynamic property FTP\Connection::$x
//! ```
//!
//! The rest of the refusing handlers: classes that construct, but whose
//! php handler table refuses cloning or answers that two of them do not
//! compare, one class per table, held the same way. These are the
//! refusing methods that move beside the whole tables.
//!
//! ```php
//! enum Suit { case Hearts; case Spades; }
//! $make = [
//!     'Exception' => fn() => new Exception('e'),
//!     'Fiber' => fn() => new Fiber(fn() => 1),
//!     'Suit' => function () { static $i = 0; return Suit::cases()[$i++]; },
//!     'Collator' => fn() => new Collator('en'),
//!     'IntlListFormatter' => fn() => new IntlListFormatter('en'),
//!     'Dom\Implementation' => fn() => Dom\XMLDocument::createEmpty()->implementation,
//!     'Dom\NamedNodeMap' => fn() => Dom\XMLDocument::createEmpty()->createElement('e')->attributes,
//!     'Dom\NodeList' => fn() => Dom\XMLDocument::createEmpty()->childNodes,
//!     'Dom\HTMLCollection' => fn() => Dom\XMLDocument::createEmpty()->getElementsByTagName('e'),
//!     'Dom\XPath' => fn() => new Dom\XPath(Dom\XMLDocument::createEmpty()),
//!     'DOMNamedNodeMap' => fn() => (new DOMDocument)->createElement('e')->attributes,
//!     'DOMNodeList' => fn() => (new DOMDocument)->childNodes,
//!     'DOMXPath' => fn() => new DOMXPath(new DOMDocument),
//!     'finfo' => fn() => new finfo,
//!     'mysqli' => fn() => mysqli_init(),
//!     'PDO' => fn() => new PDO('sqlite::memory:'),
//!     'PDOStatement' => fn() => (new PDO('sqlite::memory:'))->prepare('SELECT 1'),
//!     'Random\Engine\Secure' => fn() => new Random\Engine\Secure,
//!     'Random\Randomizer' => fn() => new Random\Randomizer(new Random\Engine\Mt19937(1)),
//!     'ReflectionClass' => fn() => new ReflectionClass('stdClass'),
//!     'SoapServer' => fn() => new SoapServer(null, ['uri' => 'urn:t']),
//!     'IteratorIterator' => fn() => new IteratorIterator(new ArrayIterator([])),
//!     'RecursiveIteratorIterator' => fn() => new RecursiveIteratorIterator(new RecursiveArrayIterator([])),
//!     'SplFileObject' => fn() => new SplFileObject('php://memory'),
//!     'GlobIterator' => fn() => new GlobIterator(sys_get_temp_dir() . '/*.none'),
//!     'SQLite3' => fn() => new SQLite3(':memory:'),
//!     'tidy' => fn() => new tidy,
//!     'XMLReader' => fn() => new XMLReader,
//!     'XMLWriter' => fn() => new XMLWriter,
//!     'XSLTProcessor' => fn() => new XSLTProcessor,
//!     'ZipArchive' => fn() => new ZipArchive,
//! ];
//! $try = function ($f) { try { $r = $f(); return is_object($r) ? 'a ' . get_class($r) : var_export($r, true); } catch (Throwable $e) { return $e->getMessage(); } };
//! set_error_handler(function ($no, $msg) { echo "  [$msg]\n"; return true; });
//! foreach ($make as $class => $m) {
//!     [$a, $b] = [$m(), $m()];
//!     echo $class, ' (', get_debug_type($a), ")\n  clone: ", $try(fn() => clone $a), "\n";
//!     echo '  ', implode(' ', array_map($try, [fn() => $a == $b, fn() => $a < $b, fn() => $a <=> $b, fn() => $b <=> $a, fn() => $a == $a, fn() => $a == new stdClass])), "\n";
//!     echo '  dynamic: ', $try(function () use ($a) { $a->x = 1; return isset($a->x); }), "\n";
//! }
//! ```
//! ```expect
//! Exception (Exception)
//!   clone: Trying to clone an uncloneable object of class Exception
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Exception::$x is deprecated]
//! true
//! Fiber (Fiber)
//!   clone: Trying to clone an uncloneable object of class Fiber
//!   true false 0 0 true false
//!   dynamic: Cannot create dynamic property Fiber::$x
//! Suit (Suit)
//!   clone: Trying to clone an uncloneable object of class Suit
//!   false false 1 1 true false
//!   dynamic: Cannot create dynamic property Suit::$x
//! Collator (Collator)
//!   clone: Trying to clone an uncloneable object of class Collator
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Collator::$x is deprecated]
//! true
//! IntlListFormatter (IntlListFormatter)
//!   clone: Trying to clone an uncloneable object of class IntlListFormatter
//!   true false 0 0 true false
//!   dynamic: Cannot create dynamic property IntlListFormatter::$x
//! Dom\Implementation (Dom\Implementation)
//!   clone: Trying to clone an uncloneable object of class Dom\Implementation
//!   true false 0 0 true false
//!   dynamic: Cannot create dynamic property Dom\Implementation::$x
//! Dom\NamedNodeMap (Dom\NamedNodeMap)
//!   clone: Trying to clone an uncloneable object of class Dom\NamedNodeMap
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\NamedNodeMap::$x is deprecated]
//! true
//! Dom\NodeList (Dom\NodeList)
//!   clone: Trying to clone an uncloneable object of class Dom\NodeList
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\NodeList::$x is deprecated]
//! true
//! Dom\HTMLCollection (Dom\HTMLCollection)
//!   clone: Trying to clone an uncloneable object of class Dom\HTMLCollection
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\HTMLCollection::$x is deprecated]
//! true
//! Dom\XPath (Dom\XPath)
//!   clone: Trying to clone an uncloneable object of class Dom\XPath
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property Dom\XPath::$x is deprecated]
//! true
//! DOMNamedNodeMap (DOMNamedNodeMap)
//!   clone: Trying to clone an uncloneable object of class DOMNamedNodeMap
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property DOMNamedNodeMap::$x is deprecated]
//! true
//! DOMNodeList (DOMNodeList)
//!   clone: Trying to clone an uncloneable object of class DOMNodeList
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property DOMNodeList::$x is deprecated]
//! true
//! DOMXPath (DOMXPath)
//!   clone: Trying to clone an uncloneable object of class DOMXPath
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property DOMXPath::$x is deprecated]
//! true
//! finfo (finfo)
//!   clone: Trying to clone an uncloneable object of class finfo
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property finfo::$x is deprecated]
//! true
//! mysqli (mysqli)
//!   clone: Trying to clone an uncloneable object of class mysqli
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property mysqli::$x is deprecated]
//! true
//! PDO (PDO)
//!   clone: Trying to clone an uncloneable object of class PDO
//!   false false 1 1 true false
//!   dynamic:   [Creation of dynamic property PDO::$x is deprecated]
//! true
//! PDOStatement (PDOStatement)
//!   clone: Trying to clone an uncloneable object of class PDOStatement
//!   false false 1 1 true false
//!   dynamic:   [Creation of dynamic property PDOStatement::$x is deprecated]
//! true
//! Random\Engine\Secure (Random\Engine\Secure)
//!   clone: Trying to clone an uncloneable object of class Random\Engine\Secure
//!   true false 0 0 true false
//!   dynamic: Cannot create dynamic property Random\Engine\Secure::$x
//! Random\Randomizer (Random\Randomizer)
//!   clone: Trying to clone an uncloneable object of class Random\Randomizer
//!   true false 0 0 true false
//!   dynamic: Cannot create dynamic property Random\Randomizer::$x
//! ReflectionClass (ReflectionClass)
//!   clone: Trying to clone an uncloneable object of class ReflectionClass
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property ReflectionClass::$x is deprecated]
//! true
//! SoapServer (SoapServer)
//!   clone: Trying to clone an uncloneable object of class SoapServer
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property SoapServer::$x is deprecated]
//! true
//! IteratorIterator (IteratorIterator)
//!   clone: Trying to clone an uncloneable object of class IteratorIterator
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property IteratorIterator::$x is deprecated]
//! true
//! RecursiveIteratorIterator (RecursiveIteratorIterator)
//!   clone: Trying to clone an uncloneable object of class RecursiveIteratorIterator
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property RecursiveIteratorIterator::$x is deprecated]
//! true
//! SplFileObject (SplFileObject)
//!   clone: Trying to clone an uncloneable object of class SplFileObject
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property SplFileObject::$x is deprecated]
//! true
//! GlobIterator (GlobIterator)
//!   clone: Trying to clone an uncloneable object of class GlobIterator
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property GlobIterator::$x is deprecated]
//! true
//! SQLite3 (SQLite3)
//!   clone: Trying to clone an uncloneable object of class SQLite3
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property SQLite3::$x is deprecated]
//! true
//! tidy (tidy)
//!   clone: Trying to clone an uncloneable object of class tidy
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property tidy::$x is deprecated]
//! true
//! XMLReader (XMLReader)
//!   clone: Trying to clone an uncloneable object of class XMLReader
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property XMLReader::$x is deprecated]
//! true
//! XMLWriter (XMLWriter)
//!   clone: Trying to clone an uncloneable object of class XMLWriter
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property XMLWriter::$x is deprecated]
//! true
//! XSLTProcessor (XSLTProcessor)
//!   clone: Trying to clone an uncloneable object of class XSLTProcessor
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property XSLTProcessor::$x is deprecated]
//! true
//! ZipArchive (ZipArchive)
//!   clone: Trying to clone an uncloneable object of class ZipArchive
//!   true false 0 0 true false
//!   dynamic:   [Creation of dynamic property ZipArchive::$x is deprecated]
//! true
//! ```
