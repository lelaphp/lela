//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine};

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_date::extension()])
        .build();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
}

/// `DateInterval::__unserialize` reads `days` as `DATE_A64I` reads it,
/// `strtoll` over the value's string, and gives the string up after
/// (#2100).
#[test]
fn the_string_days_is_read_through_is_given_up() {
    let stdout = armed(
        r#"<?php
$i = new DateInterval('P1D'); $i->__unserialize(['days' => 10]); echo $i->days, "\n";
"#,
    );
    assert_eq!(stdout, "10\n");
}

/// timelib allocates on the request's heap under its lend (ADR-0014), and
/// every time, zone and error container it made is freed under one: by
/// the object that holds it, by the native that made it for a moment,
/// or by `rshutdown`, which frees the zone cache and the last parse's
/// messages. An armed engine ends each request below with no block of
/// its heap held, a missed free being the `InternalError` naming one.
#[test]
fn what_timelib_made_on_the_request_s_heap_is_freed_by_the_request_s_end() {
    let stdout = armed(
        r#"<?php
date_default_timezone_set('Europe/Amsterdam');
$d = new DateTime('2020-01-01 10:00 America/New_York');
$d->modify('+1 day'); $d->setTimezone(new DateTimeZone('Asia/Tokyo'));
$d->setTime(1, 2)->setDate(2001, 2, 3)->setISODate(2001, 5)->setTimestamp(86400);
$i = $d->diff(new DateTime('1970-01-03 12:00 UTC'));
$e = DateTimeImmutable::createFromMutable($d)->add(new DateInterval('P1M'))->sub(new DateInterval('PT1H'));
$z = [new DateTimeZone('EST'), new DateTimeZone('+01:00'), clone $e->getTimezone()];
$p = new DatePeriod(new DateTime('2020-01-01'), new DateInterval('P1D'), 3);
foreach ($p as $x) {} $q = clone $p; $q->__unserialize($p->__serialize());
$s = DatePeriod::createFromISO8601String('R2/2012-07-01T00:00:00Z/P7D');
var_dump(date_parse('+1 day')['relative']['day'], date_parse('foo')['error_count']);
var_dump(DateTime::createFromFormat('Y-m-d', 'bad'), DateTime::getLastErrors()['error_count']);
var_dump(strtotime('next monday', 0) > 0, date('Y', 0), idate('Y', 0), mktime(0, 0, 0, 1, 1, 2000) > 0);
var_dump(localtime(0)[5], getdate(0)['year'], date_sun_info(0, 52.3, 4.9)['transit'] > 0, @strftime('%Y', 0));
var_dump($d->format('c T'), $d->getOffset(), $z[0]->getOffset($d), timezone_transitions_get($z[2], 0, 100000)[0]['ts']);
try { new DateTime('foo'); } catch (Exception $ex) { echo get_class($ex), "\n"; }
try { new DatePeriod('R2/P7D'); } catch (Exception $ex) { echo get_class($ex), "\n"; }
var_dump(DateInterval::createFromDateString('3 days')->d, (array) $d === (array) $d, $d == clone $d);
"#,
    );
    assert!(stdout.contains("int(1)\nint(1)\n"), "{stdout}");
    assert!(
        stdout.ends_with("int(3)\nbool(true)\nbool(true)\n"),
        "{stdout}"
    );
}

/// An interval's and a period's `timelib_rel_time` and an interval's
/// date string are freed on every path that replaces or drops them
/// (#2855): a constructor called again, a clone whose `__clone` throws, an
/// `__unserialize` whose error handler throws mid-read, of a date string
/// over another and of fields over a date string, a refused date
/// string, `__wakeup`, `__set_state`, and a period's clone, reads and
/// ISO form. What it prints is the oracle's.
#[test]
fn an_interval_s_relative_time_and_date_string_are_freed_on_every_path() {
    let stdout = armed(
        r#"<?php
class I extends DateInterval { public $n = 0; function __clone() { if ($this->n++ > 0) throw new Exception('clone'); } }
$i = new I('P1D'); $i->__construct('P2D'); $i->__construct('2008-05-11T15:30:00Z/2008-06-12T16:31:01Z');
$c = clone $i; try { $c->n = 1; $d = clone $c; } catch (Exception $e) { echo $e->getMessage(), "\n"; }
set_error_handler(function ($n, $s) { throw new Exception($s); });
try { $i->__unserialize(['y' => 7, 'f' => new stdClass, 'invert' => 1]); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
restore_error_handler();
$s = DateInterval::createFromDateString('+1 day +1 day +1 day');
$s->__unserialize(['date_string' => '+2 days']); $s->__unserialize(['date_string' => '+3 days']); $s->__unserialize(['d' => 4]);
try { $s->__unserialize(['date_string' => 'foo bar']); } catch (Error $e) { echo $e->getMessage(), "\n"; }
@$s->__wakeup(); $u = new DateInterval('P1D'); $u->__unserialize($s->__serialize()); $v = DateInterval::__set_state((array) $i);
$p = new DatePeriod(new DateTime('2020-01-01'), $i, 2); $q = clone $p; $q->__unserialize($p->__serialize());
$x = [$p->interval, $p->start, $q->getDateInterval(), DatePeriod::createFromISO8601String('R2/2012-07-01T00:00:00Z/P7D')->interval];
var_dump($i->y, $i->invert, $s->d, $s->date_string, $u->d, $v->days, $x[0]->d);
"#,
    );
    assert_eq!(
        stdout,
        "clone\nObject of class stdClass could not be converted to float\nUnknown or bad format (foo bar) at position 0 (f) while unserializing: The timezone could not be found in the database\nint(7)\nint(1)\nint(3)\nstring(7) \"+3 days\"\nint(3)\nint(-1)\nint(-1)\n"
    );
}
