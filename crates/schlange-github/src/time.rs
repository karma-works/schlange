//! Clocks and the RFC 3339 timestamps GitHub uses (`2026-10-27T18:00:00Z`).
//!
//! The engine counts time in seconds ([`Time`]). In production that is the Unix time; tests use
//! a [`ManualClock`] that only moves when the test says so, so a `batch_max_wait_time` of five
//! minutes does not take five minutes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use schlange_core::model::Time;

pub trait Clock: Send + Sync {
    fn now(&self) -> Time;
}

/// Unix time in seconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Time {
        unix_now()
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A clock for tests: starts at a given time and moves only through [`ManualClock::advance`].
#[derive(Debug, Default)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    pub fn new(start: Time) -> Self {
        ManualClock(AtomicU64::new(start))
    }

    pub fn advance(&self, secs: u64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Time {
        self.0.load(Ordering::SeqCst)
    }
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.fff](Z|±HH:MM)` into Unix seconds.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<u32>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        let digits = r.chars().take_while(|c| c.is_ascii_digit()).count();
        rest = &r[digits..];
    }
    let offset: i64 = match rest {
        "Z" | "z" => 0,
        _ if rest.len() == 6 && (rest.starts_with('+') || rest.starts_with('-')) => {
            let oh: i64 = rest[1..3].parse().ok()?;
            let om: i64 = rest[4..6].parse().ok()?;
            let o = oh * 3600 + om * 60;
            if rest.starts_with('-') { -o } else { o }
        }
        _ => return None,
    };
    let days = days_from_civil(y as i64, mo, d);
    let t = days * 86_400 + (h * 3600 + mi * 60 + se) as i64 - offset;
    u64::try_from(t).ok()
}

/// Formats Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_rfc3339(t: u64) -> String {
    let days = (t / 86_400) as i64;
    let secs = t % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_round_trip_and_offsets() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-10-27T18:00:00Z"), Some(1_793_124_000));
        assert_eq!(format_rfc3339(1_793_124_000), "2026-10-27T18:00:00Z");
        assert_eq!(
            parse_rfc3339("2026-10-27T20:00:00+02:00"),
            Some(1_793_124_000)
        );
        assert_eq!(
            parse_rfc3339("2026-10-27T18:00:00.123Z"),
            Some(1_793_124_000)
        );
        assert_eq!(parse_rfc3339("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        assert_eq!(parse_rfc3339("not a date"), None);
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
        for t in [0, 951_782_400, 1_709_164_800, 4_102_444_800] {
            assert_eq!(parse_rfc3339(&format_rfc3339(t)), Some(t));
        }
    }

    #[test]
    fn manual_clock_moves_only_when_told() {
        let c = ManualClock::new(100);
        assert_eq!(c.now(), 100);
        c.advance(300);
        assert_eq!(c.now(), 400);
    }
}
