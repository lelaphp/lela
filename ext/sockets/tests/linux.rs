//! What ext/sockets does under Linux's `#ifdef`s (#1096), each program
//! held to what `php -n` of a Linux build of the pinned tag printed for it
//! (the `dax-php-oracle` image of `ci/oracle/Dockerfile`, aarch64). The
//! oracle of record is macOS's, so these are held on a Linux host alone,
//! the CI Linux lane among them. A warning is held to its text, without
//! the file and line it names.
#![cfg(target_os = "linux")]

use php_ext::test::{Capture, Engine};

/// `source` served by an engine with this crate alone and `php -n`'s
/// plain-text errors: what it printed, each warning's ` in <file> on
/// line <n>` cut.
fn served(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_sockets::extension()])
        .ini("html_errors", "0")
        .build();
    let out = engine.serve_capture(source, Capture::default()).stdout;
    normalised(&String::from_utf8_lossy(&out))
}

/// The process's real uid or gid, as `/proc/self/status` names it under
/// `field`: what the kernel holds `SCM_CREDENTIALS` to.
fn credential(field: &str) -> String {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_default()
        .to_owned()
}

fn normalised(text: &str) -> String {
    text.lines()
        .map(|line| match line.rfind(" in ") {
            Some(at) if line.starts_with("Warning: ") && line.ends_with(char::is_numeric) => {
                &line[..at]
            }
            _ => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `socket_create` and `socket_create_pair` mask `SOCK_CLOEXEC` and
/// `SOCK_NONBLOCK` out of the type, and the Linux errnos have their text.
#[test]
fn the_type_takes_sock_cloexec_and_sock_nonblock() {
    let script = r#"<?php
$s = socket_create(AF_INET, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, SOL_TCP);
var_dump($s instanceof Socket);
try { socket_create(AF_INET, 11, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
try { socket_create_pair(AF_UNIX, 11 | SOCK_NONBLOCK, 0, $p); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
var_dump(socket_create_pair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, $p), @socket_read($p[0], 10), socket_last_error($p[0]) === SOCKET_EAGAIN);
var_dump(SOCKET_ENOMEDIUM, socket_strerror(SOCKET_ENOMEDIUM));
"#;
    let want = r#"bool(true)
socket_create(): Argument #2 ($type) must be one of SOCK_STREAM, SOCK_DGRAM, SOCK_SEQPACKET, SOCK_RAW, or SOCK_RDM optionally OR'ed with SOCK_CLOEXEC, SOCK_NONBLOCK
socket_create_pair(): Argument #2 ($type) must be one of SOCK_STREAM, SOCK_DGRAM, SOCK_SEQPACKET, SOCK_RAW, or SOCK_RDM optionally OR'ed with SOCK_CLOEXEC, SOCK_NONBLOCK
bool(true)
bool(false)
bool(true)
int(123)
string(15) "No medium found""#;
    assert_eq!(served(script), want);
}

/// `TCP_CONGESTION`, `SO_MEMINFO`, `UDP_SEGMENT`, `SO_ATTACH_REUSEPORT_CBPF`,
/// `IP_MTU_DISCOVER` and `SO_BINDTODEVICE`.
#[test]
fn the_linux_options_read_and_write_as_php_s_do() {
    let script = r#"<?php
$s = socket_create(AF_INET, SOCK_STREAM, SOL_TCP);
var_dump(socket_set_option($s, SOL_TCP, TCP_CONGESTION, 'reno'), socket_get_option($s, SOL_TCP, TCP_CONGESTION));
try { socket_set_option($s, SOL_TCP, TCP_CONGESTION, 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
var_dump(socket_set_option($s, SOL_TCP, TCP_CONGESTION, 'nonesuch'), socket_last_error($s) === SOCKET_ENOENT);
foreach (socket_get_option($s, SOL_SOCKET, SO_MEMINFO) as $k => $v) echo $k, "\n";
var_dump(socket_set_option($s, SOL_TCP, TCP_KEEPIDLE, 30), socket_get_option($s, SOL_TCP, TCP_KEEPIDLE));
$u = socket_create(AF_INET, SOCK_DGRAM, SOL_UDP);
var_dump(socket_set_option($u, SOL_UDP, UDP_SEGMENT, 1400), socket_get_option($u, SOL_UDP, UDP_SEGMENT));
try { socket_set_option($u, SOL_UDP, UDP_SEGMENT, 70000); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
var_dump(socket_set_option($u, SOL_UDP, UDP_SEGMENT, (1 << 32) + 1200), socket_get_option($u, SOL_UDP, UDP_SEGMENT));
try { socket_set_option($u, SOL_UDP, UDP_SEGMENT, (1 << 32) - 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
var_dump(socket_set_option($u, SOL_SOCKET, SO_REUSEPORT, 1));
var_dump(socket_set_option($u, SOL_SOCKET, SO_ATTACH_REUSEPORT_CBPF, SKF_AD_CPU));
var_dump(socket_set_option($u, SOL_SOCKET, SO_ATTACH_REUSEPORT_CBPF, 99));
var_dump(socket_set_option($u, SOL_SOCKET, SO_ATTACH_REUSEPORT_CBPF, 0));
var_dump(socket_set_option($u, IPPROTO_IP, IP_MTU_DISCOVER, IP_PMTUDISC_DO), socket_get_option($u, IPPROTO_IP, IP_MTU_DISCOVER));
try { socket_set_option($u, SOL_SOCKET, SO_BINDTODEVICE, 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
"#;
    let want = r#"bool(true)
array(1) {
  ["name"]=>
  string(4) "reno"
}
socket_set_option(): Argument #4 ($value) must be of type string when argument #3 ($option) is TCP_CONGESTION, int given

Warning: socket_set_option(): Unable to set socket option [2]: No such file or directory
bool(false)
bool(true)
rmem_alloc
rcvbuf
wmem_alloc
sndbuf
fwd_alloc
wmem_queued
optmem
backlog
drops
bool(true)
int(30)
bool(true)
int(1400)
socket_set_option(): Argument #4 ($value) must be between 0 and 65535
bool(true)
int(1200)
socket_set_option(): Argument #4 ($value) must be between 0 and 65535
bool(true)
bool(true)

Warning: socket_set_option(): Unsupported CBPF filter
bool(false)

Warning: socket_set_option(): Unable to set socket option [2]: No such file or directory
bool(false)
bool(true)
int(2)
socket_set_option(): Argument #4 ($value) must be of type string when argument #3 ($option) is SO_BINDTODEVICE, int given"#;
    assert_eq!(served(script), want);
}

/// `MCAST_JOIN_GROUP` and the four source options through `group_req` and
/// `group_source_req`.
#[test]
fn multicast_takes_rfc_3678_s_group_and_source_requests() {
    let script = r#"<?php
$u = socket_create(AF_INET, SOCK_DGRAM, SOL_UDP);
socket_bind($u, '0.0.0.0', 0);
$g = ['group' => '239.1.1.1', 'interface' => 0];
var_dump(socket_set_option($u, IPPROTO_IP, MCAST_JOIN_GROUP, $g));
var_dump(socket_set_option($u, IPPROTO_IP, MCAST_BLOCK_SOURCE, $g + ['source' => '10.9.9.9']));
var_dump(socket_set_option($u, IPPROTO_IP, MCAST_UNBLOCK_SOURCE, $g + ['source' => '10.9.9.9']));
var_dump(socket_set_option($u, IPPROTO_IP, MCAST_LEAVE_GROUP, $g));
$sg = ['group' => '232.1.1.1', 'source' => '10.9.9.9', 'interface' => 0];
var_dump(socket_set_option($u, IPPROTO_IP, MCAST_JOIN_SOURCE_GROUP, $sg));
var_dump(socket_set_option($u, IPPROTO_IP, MCAST_LEAVE_SOURCE_GROUP, $sg));
try { socket_set_option($u, IPPROTO_IP, MCAST_JOIN_SOURCE_GROUP, ['group' => '232.1.1.1']); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
try { socket_set_option($u, IPPROTO_IP, MCAST_BLOCK_SOURCE, 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
try { socket_set_option($u, IPPROTO_IP, MCAST_JOIN_GROUP, 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
var_dump(MCAST_JOIN_GROUP, MCAST_LEAVE_GROUP, MCAST_BLOCK_SOURCE, MCAST_JOIN_SOURCE_GROUP);
"#;
    let want = r#"bool(true)
bool(true)
bool(true)
bool(true)
bool(true)
bool(true)
No key "source" passed in optval
socket_set_option(): Argument #4 ($value) must be of type array when argument #3 ($option) is MCAST_LEAVE_SOURCE_GROUP, int given
socket_set_option(): Argument #4 ($value) must be of type array when argument #3 ($option) is MCAST_LEAVE_GROUP, int given
int(42)
int(45)
int(43)
int(46)"#;
    assert_eq!(served(script), want);
}

/// `SO_PASSCRED` with `SCM_CREDENTIALS` in the ancillary table.
#[test]
fn scm_credentials_goes_and_comes_back_a_ucred() {
    let (uid, gid) = (credential("Uid:"), credential("Gid:"));
    let script = r#"<?php
socket_create_pair(AF_UNIX, SOCK_DGRAM, 0, $p);
var_dump(socket_set_option($p[1], SOL_SOCKET, SO_PASSCRED, 1));
$c = ['pid' => PID, 'uid' => UID, 'gid' => GID];
var_dump(socket_sendmsg($p[0], ['iov' => ['hi'], 'control' => [['level' => SOL_SOCKET, 'type' => SCM_CREDENTIALS, 'data' => $c]]]));
$m = ['controllen' => socket_cmsg_space(SOL_SOCKET, SCM_CREDENTIALS), 'buffer_size' => 10];
var_dump(socket_cmsg_space(SOL_SOCKET, SCM_CREDENTIALS), socket_recvmsg($p[1], $m));
$d = $m['control'][0];
var_dump($d['level'] === SOL_SOCKET, $d['type'] === SCM_CREDENTIALS, $d['data'] == $c);
foreach ($d['data'] as $k => $v) echo $k, "\n";
var_dump(socket_sendmsg($p[0], ['iov' => ['x'], 'control' => [['level' => SOL_SOCKET, 'type' => SCM_CREDENTIALS, 'data' => ['pid' => -1, 'uid' => 0, 'gid' => 0]]]]));
var_dump(socket_sendmsg($p[0], ['iov' => ['x'], 'control' => [['level' => SOL_SOCKET, 'type' => SCM_CREDENTIALS, 'data' => ['pid' => 1]]]]));
"#
        .replace("PID", &std::process::id().to_string())
        .replace("UID", &uid)
        .replace("GID", &gid);
    let want = r"bool(true)
int(2)
int(32)
int(2)
bool(true)
bool(true)
bool(true)
pid
uid
gid

Warning: socket_sendmsg(): error converting user data (path: msghdr > control > element #0 > data > pid): given PHP integer is out of bounds for a pid_t value
bool(false)

Warning: socket_sendmsg(): error converting user data (path: msghdr > control > element #0 > data): The key 'uid' is required
bool(false)";
    assert_eq!(served(&script), want);
}

/// `socket_accept` through `accept4`, and `AF_PACKET` a domain `socket_create`
/// takes.
#[test]
fn accept4_makes_the_socket_non_blocking_under_a_non_blocking_listener() {
    let script = r#"<?php
$l = socket_create_listen(0); socket_getsockname($l, $a, $port);
socket_set_nonblock($l);
$c = socket_create(AF_INET, SOCK_STREAM, SOL_TCP);
socket_connect($c, '127.0.0.1', $port);
$n = socket_accept($l);
var_dump($n instanceof Socket, @socket_read($n, 10), socket_last_error($n) === SOCKET_EAGAIN);
$p = @socket_create(AF_PACKET, SOCK_RAW, ETH_P_ALL);
var_dump($p === false || $p instanceof Socket);
try { socket_create(99, SOCK_STREAM, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
"#;
    let want = r"bool(true)
bool(false)
bool(true)
bool(true)
socket_create(): Argument #1 ($domain) must be one of AF_UNIX, AF_PACKET, AF_INET6, or AF_INET";
    assert_eq!(served(script), want);
}
