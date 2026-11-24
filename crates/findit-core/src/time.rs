//! Calendar helpers for `dm:` filters, without an external date library.
//!
//! All day arithmetic happens in *local* days; the caller supplies the current
//! UTC offset so "today" matches the user's wall clock.

pub const SECS_PER_DAY: i64 = 86_400;

/// Current time and the local offset from UTC (local = utc + offset).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Clock {
    pub now_unix: i64,
    pub tz_offset_secs: i64,
}

impl Clock {
    pub fn today(&self) -> i64 {
        (self.now_unix + self.tz_offset_secs).div_euclid(SECS_PER_DAY)
    }
    /// UTC timestamp of local midnight at the start of `day`.
    pub fn day_start(&self, day: i64) -> i64 {
        day * SECS_PER_DAY - self.tz_offset_secs
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(m);
    let d = i64::from(d);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        _ => 28,
    }
}

/// Monday = 0 ... Sunday = 6.
pub fn weekday(day: i64) -> i64 {
    (day + 3).rem_euclid(7)
}

fn month_start(y: i64, m: u32) -> i64 {
    days_from_civil(y, m, 1)
}

fn add_months(y: i64, m: u32, delta: i64) -> (i64, u32) {
    let total = y * 12 + i64::from(m) - 1 + delta;
    (total.div_euclid(12), (total.rem_euclid(12) + 1) as u32)
}

/// Parses one period (a keyword or a date) into a half-open range of local days.
fn period(token: &str, clock: &Clock) -> Result<(i64, i64), String> {
    let t = clock.today();
    let (y, m, _) = civil_from_days(t);
    let lower = token.to_ascii_lowercase();
    let range = match lower.as_str() {
        "today" => (t, t + 1),
        "yesterday" => (t - 1, t),
        "thisweek" => {
            let mon = t - weekday(t);
            (mon, mon + 7)
        }
        "lastweek" => {
            let mon = t - weekday(t);
            (mon - 7, mon)
        }
        "thismonth" => {
            let (ny, nm) = add_months(y, m, 1);
            (month_start(y, m), month_start(ny, nm))
        }
        "lastmonth" => {
            let (py, pm) = add_months(y, m, -1);
            (month_start(py, pm), month_start(y, m))
        }
        "thisyear" => (days_from_civil(y, 1, 1), days_from_civil(y + 1, 1, 1)),
        "lastyear" => (days_from_civil(y - 1, 1, 1), days_from_civil(y, 1, 1)),
        "pastweek" => (t - 6, t + 1),
        "pastmonth" => (t - 29, t + 1),
        "pastyear" => (t - 364, t + 1),
        _ => {
            if let Some(n) = lower.strip_prefix("last").or_else(|| lower.strip_prefix("past")).and_then(|r| r.strip_suffix("days")) {
                let n: i64 = n.parse().map_err(|_| format!("\"{token}\" is not a valid period"))?;
                if !(1..=36_500).contains(&n) {
                    return Err(format!("\"{token}\": the number of days must be between 1 and 36500"));
                }
                (t - (n - 1), t + 1)
            } else {
                return parse_date(token);
            }
        }
    };
    Ok(range)
}

fn parse_date(token: &str) -> Result<(i64, i64), String> {
    let bad = || format!("\"{token}\" is not a date. Use YYYY-MM-DD, YYYY-MM, YYYY or a word like today or last7days");
    let sep = token.chars().find(|c| matches!(c, '-' | '/' | '.'));
    let parts: Vec<&str> = match sep {
        Some(s) => token.split(s).collect(),
        None => vec![token],
    };
    if parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || p.len() > 4) {
        return Err(bad());
    }
    let num = |s: &str| s.parse::<i64>().map_err(|_| bad());
    let (y, m, d) = match parts.len() {
        1 if parts[0].len() == 4 => {
            let y = num(parts[0])?;
            return Ok((days_from_civil(y, 1, 1), days_from_civil(y + 1, 1, 1)));
        }
        2 if parts[0].len() == 4 => {
            let y = num(parts[0])?;
            let m = num(parts[1])?;
            if !(1..=12).contains(&m) {
                return Err(bad());
            }
            let (ny, nm) = add_months(y, m as u32, 1);
            return Ok((month_start(y, m as u32), month_start(ny, nm)));
        }
        3 if parts[0].len() == 4 => (num(parts[0])?, num(parts[1])?, num(parts[2])?),
        3 if parts[2].len() == 4 && sep == Some('.') => (num(parts[2])?, num(parts[1])?, num(parts[0])?),
        _ => return Err(bad()),
    };
    if !(1..=12).contains(&m) || d < 1 || d > i64::from(days_in_month(y, m as u32)) {
        return Err(bad());
    }
    let day = days_from_civil(y, m as u32, d as u32);
    Ok((day, day + 1))
}

/// Parses a `dm:` value into a half-open UTC range `[lo, hi)` in Unix seconds.
///
/// Accepted: `today`, `yesterday`, `thisweek`, `lastweek`, `thismonth`, `lastmonth`,
/// `thisyear`, `lastyear`, `last<N>days`, `pastweek`, dates (`2026-03-01`, `2026-03`,
/// `2026`, `01.03.2026`), ranges `a..b` and comparisons `>a`, `>=a`, `<a`, `<=a`, `=a`.
pub fn parse_date_filter(value: &str, clock: &Clock) -> Result<(i64, i64), String> {
    let v = value.trim();
    if v.is_empty() {
        return Err("dm: needs a value, for example dm:today or dm:2026-01-01..2026-03-31".into());
    }
    let to_secs = |(a, b): (i64, i64)| (clock.day_start(a), clock.day_start(b));
    if let Some((a, b)) = v.split_once("..") {
        let lo = if a.is_empty() { i64::MIN } else { clock.day_start(period(a, clock)?.0) };
        let hi = if b.is_empty() { i64::MAX } else { clock.day_start(period(b, clock)?.1) };
        if lo >= hi {
            return Err(format!("The date range \"{v}\" is empty"));
        }
        return Ok((lo, hi));
    }
    for (op, f) in [(">=", 0), ("<=", 1), (">", 2), ("<", 3), ("=", 4)] {
        if let Some(rest) = v.strip_prefix(op) {
            let (lo, hi) = to_secs(period(rest.trim(), clock)?);
            return Ok(match f {
                0 => (lo, i64::MAX),
                1 => (i64::MIN, hi),
                2 => (hi, i64::MAX),
                3 => (i64::MIN, lo),
                _ => (lo, hi),
            });
        }
    }
    Ok(to_secs(period(v, clock)?))
}

/// Formats a Unix time as `YYYY-MM-DD` in local time (used by tests and logs).
pub fn format_date(unix: i64, tz_offset_secs: i64) -> String {
    let (y, m, d) = civil_from_days((unix + tz_offset_secs).div_euclid(SECS_PER_DAY));
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-24 (a Tuesday) 15:00 local, UTC+2.
    fn clock() -> Clock {
        let day = days_from_civil(2026, 11, 24);
        Clock { now_unix: day * SECS_PER_DAY + 15 * 3600 - 7200, tz_offset_secs: 7200 }
    }

    #[test]
    fn civil_round_trip() {
        for z in [-800_000, -1, 0, 1, 20_000, 59_000, 2_000_000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(weekday(days_from_civil(2026, 11, 23)), 0);
    }

    #[test]
    fn keywords() {
        let c = clock();
        let (lo, hi) = parse_date_filter("today", &c).unwrap();
        assert_eq!(format_date(lo, c.tz_offset_secs), "2026-11-24");
        assert_eq!(hi - lo, SECS_PER_DAY);
        let (lo, _) = parse_date_filter("thisweek", &c).unwrap();
        assert_eq!(format_date(lo, c.tz_offset_secs), "2026-11-23");
        let (lo, hi) = parse_date_filter("lastmonth", &c).unwrap();
        assert_eq!(format_date(lo, c.tz_offset_secs), "2026-10-01");
        assert_eq!(format_date(hi, c.tz_offset_secs), "2026-11-01");
        let (lo, _) = parse_date_filter("last7days", &c).unwrap();
        assert_eq!(format_date(lo, c.tz_offset_secs), "2026-11-18");
    }

    #[test]
    fn dates_ranges_and_comparisons() {
        let c = clock();
        let (lo, hi) = parse_date_filter("2026-01-01..2026-03-31", &c).unwrap();
        assert_eq!(format_date(lo, c.tz_offset_secs), "2026-01-01");
        assert_eq!(format_date(hi, c.tz_offset_secs), "2026-04-01");
        let (lo, hi) = parse_date_filter(">2026-01-01", &c).unwrap();
        assert_eq!(format_date(lo, c.tz_offset_secs), "2026-01-02");
        assert_eq!(hi, i64::MAX);
        let (lo, hi) = parse_date_filter("2024-02", &c).unwrap();
        assert_eq!((hi - lo) / SECS_PER_DAY, 29);
        assert!(parse_date_filter("24.12.2025", &c).is_ok());
        assert!(parse_date_filter("2025-02-30", &c).is_err());
        assert!(parse_date_filter("soon", &c).is_err());
        assert!(parse_date_filter("2026-03-01..2026-01-01", &c).is_err());
    }
}
