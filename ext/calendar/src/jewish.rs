//! The Jewish calendar, `jewish.c`: the molad arithmetic in halakim,
//! the four postponement rules of Tishri 1, and the month names, the
//! Hebrew ones in ISO-8859-8 as php writes them. php's `int` locals stay
//! `i32` and wrap as the C compiles, since `jewishtojd` of a huge year
//! answers what that arithmetic leaves.

use crate::constants::{
    CAL_JEWISH_ADD_ALAFIM, CAL_JEWISH_ADD_ALAFIM_GERESH, CAL_JEWISH_ADD_GERESHAYIM,
};
use crate::sdn::{Date, JulianDay};

const HALAKIM_PER_HOUR: i64 = 1080;
const HALAKIM_PER_DAY: i64 = 25920;
const HALAKIM_PER_LUNAR_CYCLE: i64 = (29 * HALAKIM_PER_DAY) + 13753;
const HALAKIM_PER_METONIC_CYCLE: i64 = HALAKIM_PER_LUNAR_CYCLE * (12 * 19 + 7);

const JEWISH_SDN_OFFSET: i64 = 347_997;
/// 12/13/887605; a greater day overflows php's `int` year.
const JEWISH_SDN_MAX: i64 = 324_542_846;
const NEW_MOON_OF_CREATION: u64 = 31524;

const NOON: i64 = 18 * HALAKIM_PER_HOUR;
const AM3_11_20: i64 = (9 * HALAKIM_PER_HOUR) + 204;
const AM9_32_43: i64 = (15 * HALAKIM_PER_HOUR) + 589;

const SUNDAY: i64 = 0;
const MONDAY: i64 = 1;
const TUESDAY: i64 = 2;
const WEDNESDAY: i64 = 3;
const FRIDAY: i64 = 5;

/// `monthsPerYear`: the months of each year of the 19-year metonic cycle.
const MONTHS_PER_YEAR: [i64; 19] = [
    12, 12, 13, 12, 12, 13, 12, 13, 12, 12, 13, 12, 12, 13, 12, 12, 13, 12, 13,
];

/// `yearOffset`: the months before each year of the cycle.
const YEAR_OFFSET: [i64; 19] = [
    0, 12, 24, 37, 49, 61, 74, 86, 99, 111, 123, 136, 148, 160, 173, 185, 197, 210, 222,
];

/// `JewishMonthNameLeap`, the names of a 13-month year.
pub(crate) const MONTH_NAME_LEAP: [&str; 14] = [
    "", "Tishri", "Heshvan", "Kislev", "Tevet", "Shevat", "Adar I", "Adar II", "Nisan", "Iyyar",
    "Sivan", "Tammuz", "Av", "Elul",
];

/// `JewishMonthName`, the names of a 12-month year.
const MONTH_NAME: [&str; 14] = [
    "", "Tishri", "Heshvan", "Kislev", "Tevet", "Shevat", "", "Adar", "Nisan", "Iyyar", "Sivan",
    "Tammuz", "Av", "Elul",
];

/// `JewishMonthHebNameLeap`, in ISO-8859-8.
const HEB_MONTH_NAME_LEAP: [&[u8]; 14] = [
    b"",
    b"\xFA\xF9\xF8\xE9",
    b"\xE7\xF9\xE5\xEF",
    b"\xEB\xF1\xEC\xE5",
    b"\xE8\xE1\xFA",
    b"\xF9\xE1\xE8",
    b"\xE0\xE3\xF8 \xE0'",
    b"\xE0\xE3\xF8 \xE1'",
    b"\xF0\xE9\xF1\xEF",
    b"\xE0\xE9\xE9\xF8",
    b"\xF1\xE9\xE5\xEF",
    b"\xFA\xEE\xE5\xE6",
    b"\xE0\xE1",
    b"\xE0\xEC\xE5\xEC",
];

/// `JewishMonthHebName`, in ISO-8859-8.
const HEB_MONTH_NAME: [&[u8]; 14] = [
    b"",
    b"\xFA\xF9\xF8\xE9",
    b"\xE7\xF9\xE5\xEF",
    b"\xEB\xF1\xEC\xE5",
    b"\xE8\xE1\xFA",
    b"\xF9\xE1\xE8",
    b"",
    b"\xE0\xE3\xF8",
    b"\xF0\xE9\xF1\xEF",
    b"\xE0\xE9\xE9\xF8",
    b"\xF1\xE9\xE5\xEF",
    b"\xFA\xEE\xE5\xE6",
    b"\xE0\xE1",
    b"\xE0\xEC\xE5\xEC",
];

/// `monthsPerYear[(year - 1) % 19]`: 13 in a leap year.
fn months_in(year: i32) -> i64 {
    let at = usize::try_from(year.wrapping_sub(1).rem_euclid(19)).unwrap_or(0);
    MONTHS_PER_YEAR[at]
}

/// `JEWISH_MONTH_NAME(year)`: the names for `year`'s length.
pub(crate) fn month_names(year: i32) -> &'static [&'static str] {
    if months_in(year) == 13 {
        &MONTH_NAME_LEAP
    } else {
        &MONTH_NAME
    }
}

/// `JEWISH_HEB_MONTH_NAME(year)[month]`.
pub(crate) fn hebrew_month_name(date: Date) -> &'static [u8] {
    let names = if months_in(date.year) == 13 {
        &HEB_MONTH_NAME_LEAP
    } else {
        &HEB_MONTH_NAME
    };
    usize::try_from(date.month)
        .ok()
        .and_then(|m| names.get(m))
        .copied()
        .unwrap_or(b"")
}

/// `Tishri1`: the day of Tishri 1 for the year whose molad is given,
/// the three delays of rules 2 to 4 and then rule 1's.
fn tishri1(metonic_year: i32, molad_day: i64, molad_halakim: i64) -> i64 {
    let mut tishri1 = molad_day;
    let mut dow = tishri1 % 7;
    let leap_year = matches!(metonic_year, 2 | 5 | 7 | 10 | 13 | 16 | 18);
    let last_was_leap_year = matches!(metonic_year, 3 | 6 | 8 | 11 | 14 | 17 | 0);
    if molad_halakim >= NOON
        || (!leap_year && dow == TUESDAY && molad_halakim >= AM3_11_20)
        || (last_was_leap_year && dow == MONDAY && molad_halakim >= AM9_32_43)
    {
        tishri1 += 1;
        dow += 1;
        if dow == 7 {
            dow = 0;
        }
    }
    if dow == WEDNESDAY || dow == FRIDAY || dow == SUNDAY {
        tishri1 += 1;
    }
    tishri1
}

/// A molad: the day and the halakim into it.
#[derive(Clone, Copy)]
struct Molad {
    day: i64,
    halakim: i64,
}

impl Molad {
    /// The molad `halakim` later, carried into days.
    const fn after(self, halakim: i64) -> Molad {
        let halakim = self.halakim + halakim;
        Molad {
            day: self.day + halakim / HALAKIM_PER_DAY,
            halakim: halakim % HALAKIM_PER_DAY,
        }
    }

    /// The molad of Tishri of the next year, `metonic_year` this one's.
    fn next_year(self, metonic_year: i32) -> Molad {
        let at = usize::try_from(metonic_year).unwrap_or(0).min(18);
        self.after(HALAKIM_PER_LUNAR_CYCLE * MONTHS_PER_YEAR[at])
    }
}

/// `MoladOfMetonicCycle`: the molad starting `cycle`, computed in
/// 16-bit halves as php does, or day zero where php's overflow guards
/// refuse.
#[expect(
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "php's `zend_ulong` registers"
)]
fn molad_of_metonic_cycle(cycle: i32) -> Molad {
    const LOW: i64 = HALAKIM_PER_METONIC_CYCLE & 0xFFFF;
    const HIGH: i64 = (HALAKIM_PER_METONIC_CYCLE >> 16) & 0xFFFF;
    let per_day = HALAKIM_PER_DAY as u64;
    let refused = Molad { day: 0, halakim: 0 };
    let chk = i64::from(cycle);
    if chk > (i64::MAX - NEW_MOON_OF_CREATION as i64) / LOW {
        return refused;
    }
    let mut r1 = NEW_MOON_OF_CREATION.wrapping_add(chk.wrapping_mul(LOW) as u64);
    if (chk as u64) > (i64::MAX as u64).wrapping_sub(r1 >> 16) / HIGH as u64 {
        return refused;
    }
    let mut r2 = (r1 >> 16).wrapping_add(chk.wrapping_mul(HIGH) as u64);
    let d2 = r2 / per_day;
    r2 -= d2 * per_day;
    r1 = (r2 << 16) | (r1 & 0xFFFF);
    let d1 = r1 / per_day;
    r1 -= d1 * per_day;
    Molad {
        day: ((d2 << 16) | d1) as i64,
        halakim: r1 as i64,
    }
}

/// `FindTishriMolad`: the metonic cycle, the year in it, and the molad
/// of the Tishri nearest `input_day`.
#[expect(clippy::cast_possible_truncation, reason = "php's `int` metonic cycle")]
fn find_tishri_molad(input_day: i64) -> (i32, i32, Molad) {
    let mut cycle = ((input_day + 310) / 6940) as i32;
    let mut molad = molad_of_metonic_cycle(cycle);
    while molad.day < input_day - 6940 + 310 {
        cycle = cycle.wrapping_add(1);
        molad = molad.after(HALAKIM_PER_METONIC_CYCLE);
    }
    let mut year = 0;
    while year < 18 {
        if molad.day > input_day - 74 {
            break;
        }
        molad = molad.next_year(year);
        year += 1;
    }
    (cycle, year, molad)
}

/// `FindStartOfYear`: the metonic year of `year`, its molad of Tishri,
/// and Tishri 1 in php's `int`.
#[expect(clippy::cast_possible_truncation, reason = "php's `int *pTishri1`")]
fn find_start_of_year(year: i32) -> (i32, Molad, i32) {
    let cycle = year.wrapping_sub(1) / 19;
    let metonic_year = year.wrapping_sub(1) % 19;
    let at = usize::try_from(metonic_year).unwrap_or(0);
    let molad = molad_of_metonic_cycle(cycle).after(HALAKIM_PER_LUNAR_CYCLE * YEAR_OFFSET[at]);
    (
        metonic_year,
        molad,
        tishri1(metonic_year, molad.day, molad.halakim) as i32,
    )
}

/// `SdnToJewish`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "php's `int` out-parameters"
)]
pub(crate) fn sdn_to_jewish(sdn: JulianDay) -> Option<Date> {
    if sdn.0 <= JEWISH_SDN_OFFSET || sdn.0 > JEWISH_SDN_MAX {
        return None;
    }
    let input_day = sdn.0 - JEWISH_SDN_OFFSET;
    let (cycle, metonic_year, mut molad) = find_tishri_molad(input_day);
    let mut tishri1_now = tishri1(metonic_year, molad.day, molad.halakim) as i32;
    let at = |year: i32, month: i32, day: i64| {
        Some(Date {
            year,
            month,
            day: day as i32,
        })
    };
    let year;
    let tishri1_after;
    if input_day >= i64::from(tishri1_now) {
        // Tishri 1 found at the start of the year.
        year = cycle * 19 + metonic_year + 1;
        let t = i64::from(tishri1_now);
        if input_day < t + 59 {
            return if input_day < t + 30 {
                at(year, 1, input_day - t + 1)
            } else {
                at(year, 2, input_day - t - 29)
            };
        }
        molad = molad.next_year(metonic_year);
        tishri1_after = tishri1((metonic_year + 1) % 19, molad.day, molad.halakim) as i32;
    } else {
        // Tishri 1 found at the end of the year.
        year = cycle * 19 + metonic_year;
        let t = i64::from(tishri1_now);
        if input_day >= t - 177 {
            // One of the last six months.
            return match input_day {
                d if d > t - 30 => at(year, 13, d - t + 30),
                d if d > t - 60 => at(year, 12, d - t + 60),
                d if d > t - 89 => at(year, 11, d - t + 89),
                d if d > t - 119 => at(year, 10, d - t + 119),
                d if d > t - 148 => at(year, 9, d - t + 148),
                d => at(year, 8, d - t + 178),
            };
        }
        let mut month = 7;
        let mut day = input_day - t + 207;
        if day > 0 {
            return at(year, month, day);
        }
        if months_in(year) == 13 {
            month -= 1;
            day += 30;
            if day > 0 {
                return at(year, month, day);
            }
            month -= 1;
        } else {
            month -= 2;
        }
        day += 30;
        if day > 0 {
            return at(year, month, day);
        }
        month -= 1;
        day += 29;
        if day > 0 {
            return at(year, month, day);
        }
        // The length of the year decides it: find this year's Tishri 1.
        tishri1_after = tishri1_now;
        let (_, earlier_year, earlier) = find_tishri_molad(molad.day - 365);
        tishri1_now = tishri1(earlier_year, earlier.day, earlier.halakim) as i32;
    }
    let year_length = tishri1_after.wrapping_sub(tishri1_now);
    let mut day = input_day - i64::from(tishri1_now) - 29;
    let heshvan = if year_length == 355 || year_length == 385 {
        30
    } else {
        29
    };
    if day <= heshvan {
        return at(year, 2, day);
    }
    day -= heshvan;
    // It has to be Kislev.
    at(year, 3, day)
}

/// `JewishToSdn`.
pub(crate) fn jewish_to_sdn(year: i32, month: i32, day: i32) -> Option<JulianDay> {
    if year <= 0 || year >= i32::MAX - 1 || day <= 0 || day > 30 {
        return None;
    }
    let sdn: i32 = match month {
        // Tishri or Heshvan: the year's length is not needed.
        1 | 2 => {
            let (_, _, tishri1) = find_start_of_year(year);
            let before = if month == 1 { -1 } else { 29 };
            tishri1.wrapping_add(day).wrapping_add(before)
        }
        // Kislev: the year's length is.
        3 => {
            let (metonic_year, molad, tishri1_now) = find_start_of_year(year);
            let molad = molad.next_year(metonic_year);
            #[expect(clippy::cast_possible_truncation, reason = "php's `int tishri1After`")]
            let tishri1_after = tishri1((metonic_year + 1) % 19, molad.day, molad.halakim) as i32;
            let year_length = tishri1_after.wrapping_sub(tishri1_now);
            let before = if year_length == 355 || year_length == 385 {
                59
            } else {
                58
            };
            tishri1_now.wrapping_add(day).wrapping_add(before)
        }
        // Tevet, Shevat or Adar I: counted back from next Tishri 1.
        4..=6 => {
            let (_, _, tishri1_after) = find_start_of_year(year + 1);
            let adar = if months_in(year) == 12 { 29 } else { 59 };
            let back = match month {
                4 => 237,
                5 => 208,
                _ => 178,
            };
            tishri1_after
                .wrapping_add(day)
                .wrapping_sub(adar)
                .wrapping_sub(back)
        }
        // Adar II or later.
        _ => {
            let (_, _, tishri1_after) = find_start_of_year(year + 1);
            let back = match month {
                7 => 207,
                8 => 178,
                9 => 148,
                10 => 119,
                11 => 89,
                12 => 60,
                13 => 30,
                _ => return None,
            };
            tishri1_after.wrapping_add(day).wrapping_sub(back)
        }
    };
    JulianDay::counted(i64::from(sdn) + JEWISH_SDN_OFFSET)
}

/// `alef_bet`: the ISO-8859-8 letters `heb_number_to_chars` spells with,
/// their numeric values 1 to 9, then the tens, then 100 to 400.
const ALEF_BET: &[u8; 23] =
    b"0\xE0\xE1\xE2\xE3\xE4\xE5\xE6\xE7\xE8\xE9\xEB\xEC\xEE\xF0\xF1\xF2\xF4\xF6\xF7\xF8\xF9\xFA";

/// `jdtojewish`'s `$flags`, `CAL_JEWISH_ADD_*`, decoded once.
#[derive(Clone, Copy, Default)]
pub(crate) struct HebrewFlags {
    /// `CAL_JEWISH_ADD_ALAFIM_GERESH`: a geresh after the thousands.
    alafim_geresh: bool,
    /// `CAL_JEWISH_ADD_ALAFIM`: the word "alafim" after the thousands.
    alafim: bool,
    /// `CAL_JEWISH_ADD_GERESHAYIM`: a geresh or gershayim in the rest.
    gereshayim: bool,
}

impl HebrewFlags {
    pub(crate) const fn decode(flags: i64) -> HebrewFlags {
        HebrewFlags {
            alafim_geresh: flags & CAL_JEWISH_ADD_ALAFIM_GERESH != 0,
            alafim: flags & CAL_JEWISH_ADD_ALAFIM != 0,
            gereshayim: flags & CAL_JEWISH_ADD_GERESHAYIM != 0,
        }
    }
}

/// `heb_number_to_chars`: `n` in Hebrew letters, 1 to 9999; outside
/// that php answers `NULL`, which no caller reaches.
pub(crate) fn hebrew_number(n: i32, flags: HebrewFlags) -> Vec<u8> {
    let letter = |at: i32| ALEF_BET[usize::try_from(at).unwrap_or(0)];
    let mut out = Vec::with_capacity(18);
    if !(1..=9999).contains(&n) {
        return out;
    }
    let mut n = n;
    let mut end_of_alafim = 0;
    // The thousands.
    if n / 1000 != 0 {
        out.push(letter(n / 1000));
        if flags.alafim_geresh {
            out.push(b'\'');
        }
        if flags.alafim {
            out.extend_from_slice(b" \xE0\xEC\xF4\xE9\xED ");
        }
        end_of_alafim = out.len();
        n %= 1000;
    }
    // Tav-tav: 400 each.
    while n >= 400 {
        out.push(letter(22));
        n -= 400;
    }
    // The hundreds.
    if n >= 100 {
        out.push(letter(18 + n / 100));
        n %= 100;
    }
    if n == 15 || n == 16 {
        // Tet-vav and tet-zain, never the name's letters.
        out.push(letter(9));
        out.push(letter(n - 9));
    } else {
        if n >= 10 {
            out.push(letter(9 + n / 10));
            n %= 10;
        }
        if n > 0 {
            out.push(letter(n));
        }
    }
    if flags.gereshayim {
        match out.len() - end_of_alafim {
            0 => {}
            1 => out.push(b'\''),
            _ => {
                let last = out.pop().unwrap_or(b'"');
                out.push(b'"');
                out.push(last);
            }
        }
    }
    out
}
