//! The build date `php -v` carries: php's `php_print_version` prints
//! `__DATE__ " " __TIME__` (`Z/main/main.c`), the clock of the compile
//! that last rebuilt that file, as `Jul 28 2026 13:06:52`, the day padded
//! with a space as `__DATE__` pads it. Stamped here in UTC, into
//! `PHP_BUILD_DATE` for `php_sapi::cli::version`, and re-stamped when
//! this file or the host changes, which is when php's `__DATE__` moves.

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/cli.rs");
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    println!("cargo:rustc-env=PHP_BUILD_DATE={}", stamp(seconds));
}

/// `__DATE__ " " __TIME__` for a Unix time: the civil date by Howard
/// Hinnant's `civil_from_days`, then the clock.
fn stamp(seconds: u64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let clock = seconds % 86_400;
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    let name = MONTHS
        .get(usize::try_from(month - 1).unwrap_or(0))
        .copied()
        .unwrap_or("Jan");
    format!(
        "{name} {day:>2} {year} {:02}:{:02}:{:02}",
        clock / 3_600,
        clock % 3_600 / 60,
        clock % 60
    )
}
