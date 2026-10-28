//! Timestamps as the configuration and pull request bodies write them (docs:
//! configuration/data-types, "Timestamp" and "Relative Timestamp"), as Unix seconds.
//!
//! Supported: `2021-04-05`, `2012-09-17T22:02:51`, fractions of a second, `Z`, numeric offsets
//! (`-08:00`, `+0100`), and a bracketed zone that is UTC (`[UTC]`, `[Etc/UTC]`, `[GMT]`).
//! Without a zone the time is UTC. Other IANA zones (`[Europe/Paris]`) need the time zone
//! database, which Schlange does not ship: they are reported as unsupported and a
//! `Merge-After` with one holds the pull request (it never merges early).
//!
//! The engine's clock counts seconds from `epoch` (the host sets it: Unix time on GitHub,
//! a fixed date in the simulation), so `unix = epoch + now`.

/// Why a timestamp could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimeError {
    Invalid,
    /// A named zone Schlange cannot convert without a time zone database.
    UnsupportedZone(String),
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
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

/// `YYYY-MM-DDTHH:MM:SSZ` for Unix seconds.
pub fn format_utc(unix: i64) -> String {
    let (days, secs) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

fn num(s: &str) -> Result<u32, TimeError> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TimeError::Invalid);
    }
    s.parse().map_err(|_| TimeError::Invalid)
}

/// Parses an ISO 8601 timestamp of the supported forms into Unix seconds.
pub fn parse_timestamp(text: &str) -> Result<i64, TimeError> {
    let mut s = text.trim();
    let mut zone_offset: Option<i64> = None;
    if let Some(open) = s.find('[') {
        let Some(zone) = s[open..]
            .strip_prefix('[')
            .and_then(|z| z.strip_suffix(']'))
        else {
            return Err(TimeError::Invalid);
        };
        match zone {
            "UTC" | "Etc/UTC" | "GMT" | "Etc/GMT" | "Z" | "Zulu" | "Universal" => {
                zone_offset = Some(0)
            }
            other if !other.is_empty() => return Err(TimeError::UnsupportedZone(other.into())),
            _ => return Err(TimeError::Invalid),
        }
        s = s[..open].trim_end();
    }
    let (date, time) = match s.find(['T', 't', ' ']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let parts: Vec<&str> = date.split('-').collect();
    let [y, m, d] = parts[..] else {
        return Err(TimeError::Invalid);
    };
    if y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return Err(TimeError::Invalid);
    }
    let (y, m, d) = (num(y)? as i64, num(m)?, num(d)?);
    if !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return Err(TimeError::Invalid);
    }
    let mut secs = days_from_civil(y, m, d) * 86_400;
    if let Some(t) = time {
        // Split off the zone designator.
        let (clock, offset) = if let Some(c) = t.strip_suffix(['Z', 'z']) {
            (c, Some(0))
        } else if let Some(i) = t.rfind(['+', '-']) {
            let o = &t[i + 1..];
            let sign = if &t[i..=i] == "-" { -1 } else { 1 };
            let (h, mi) = match o.len() {
                5 if &o[2..3] == ":" => (num(&o[..2])?, num(&o[3..])?),
                4 => (num(&o[..2])?, num(&o[2..])?),
                2 => (num(o)?, 0),
                _ => return Err(TimeError::Invalid),
            };
            if h > 23 || mi > 59 {
                return Err(TimeError::Invalid);
            }
            (&t[..i], Some(sign * (h as i64 * 3600 + mi as i64 * 60)))
        } else {
            (t, None)
        };
        if offset.is_some() && zone_offset.is_some() {
            return Err(TimeError::Invalid);
        }
        let clock = clock.split(['.', ',']).next().unwrap_or("");
        let fields: Vec<&str> = clock.split(':').collect();
        let (h, mi, se) = match fields[..] {
            [h, mi] => (num(h)?, num(mi)?, 0),
            [h, mi, se] => (num(h)?, num(mi)?, num(se)?),
            _ => return Err(TimeError::Invalid),
        };
        if h > 23 || mi > 59 || se > 60 {
            return Err(TimeError::Invalid);
        }
        secs += h as i64 * 3600 + mi as i64 * 60 + se as i64;
        secs -= offset.unwrap_or(0);
    }
    Ok(secs - zone_offset.unwrap_or(0))
}

/// Parses a relative timestamp `[D days] [H hours] [M minutes] ago` against `now` (Unix).
pub fn parse_relative(text: &str, now: i64) -> Option<i64> {
    let rest = text.trim().strip_suffix("ago")?.trim();
    if rest.is_empty() {
        return None;
    }
    let words: Vec<&str> = rest.split_whitespace().collect();
    if !words.len().is_multiple_of(2) {
        return None;
    }
    let mut total = 0i64;
    for pair in words.chunks(2) {
        let n: i64 = pair[0].parse().ok()?;
        let unit = match pair[1] {
            "day" | "days" => 86_400,
            "hour" | "hours" => 3600,
            "minute" | "minutes" => 60,
            _ => return None,
        };
        total += n * unit;
    }
    Some(now - total)
}

/// A timestamp value in a condition: absolute or relative to `now`.
pub fn parse_condition_time(text: &str, now: i64) -> Option<i64> {
    parse_relative(text, now).or_else(|| parse_timestamp(text).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_forms() {
        let base = parse_timestamp("2021-04-05").unwrap();
        assert_eq!(format_utc(base), "2021-04-05T00:00:00Z");
        assert_eq!(
            format_utc(parse_timestamp("2012-09-17T22:02:51").unwrap()),
            "2012-09-17T22:02:51Z"
        );
        assert_eq!(
            format_utc(parse_timestamp("2008-09-22T14:01:54Z").unwrap()),
            "2008-09-22T14:01:54Z"
        );
        assert_eq!(
            format_utc(parse_timestamp("2013-12-05T07:19:04-08:00").unwrap()),
            "2013-12-05T15:19:04Z"
        );
        assert_eq!(
            format_utc(parse_timestamp("2013-12-05T07:19:04+0100").unwrap()),
            "2013-12-05T06:19:04Z"
        );
        assert_eq!(
            parse_timestamp("2013-12-05T07:19:04[Europe/Paris]"),
            Err(TimeError::UnsupportedZone("Europe/Paris".into()))
        );
        assert_eq!(
            format_utc(parse_timestamp("2030-09-01T09:00:00[UTC]").unwrap()),
            "2030-09-01T09:00:00Z"
        );
        assert_eq!(
            format_utc(parse_timestamp("2030-09-01T09:00:00.123Z").unwrap()),
            "2030-09-01T09:00:00Z"
        );
        assert_eq!(
            format_utc(parse_timestamp("2030-09-01T09:00").unwrap()),
            "2030-09-01T09:00:00Z"
        );
    }

    #[test]
    fn invalid_values() {
        for bad in [
            "",
            "soon",
            "2030-13-01",
            "2030-02-30",
            "2030-9-1",
            "2030-09-01T25:00:00Z",
            "2030-09-01T09:00:00+25:00",
            "2030-09-01T09:00:00Z[UTC]",
            "tomorrow 9am",
            "2030-09-01T09",
        ] {
            assert!(parse_timestamp(bad).is_err(), "{bad}");
        }
        assert!(parse_timestamp("2024-02-29").is_ok(), "leap day");
        assert!(parse_timestamp("2023-02-29").is_err());
    }

    #[test]
    fn relative() {
        assert_eq!(
            parse_relative("1 day 2 hours ago", 100_000),
            Some(100_000 - 93_600)
        );
        assert_eq!(parse_relative("3 minutes ago", 600), Some(420));
        assert_eq!(parse_relative("ago", 600), None);
        assert_eq!(parse_relative("3 weeks ago", 600), None);
        assert_eq!(parse_condition_time("1970-01-02", 0), Some(86_400));
    }
}
