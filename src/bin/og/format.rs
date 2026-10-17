//! Numbers as the interface shows them.

/// `812 KB`, `4.2 MB`, `752 MB`, `6.5 GB`.
pub(crate) fn bytes(bytes: u64) -> String {
    let mb = bytes as f64 / 1e6;
    if bytes < 1_000 {
        format!("{bytes} B")
    } else if mb < 1.0 {
        format!("{:.0} KB", mb * 1_000.0)
    } else if mb < 10.0 {
        format!("{mb:.1} MB")
    } else if mb < 1_000.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{:.1} GB", mb / 1_000.0)
    }
}

/// `0.4s`, `9s`, `4m05s`, `2h10m`.
pub(crate) fn duration(secs: f64) -> String {
    if secs < 1.0 {
        return format!("{:.1}s", secs.max(0.0));
    }
    let secs = secs.round() as u64;
    match secs {
        0..60 => format!("{secs}s"),
        60..3_600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3_600, secs % 3_600 / 60),
    }
}

/// A duration for estimates, rounded up: `< 1 min`, `~ 9 min`, `~ 14 h`.
pub(crate) fn about(secs: f64) -> String {
    if secs < 60.0 {
        "< 1 min".to_string()
    } else if secs >= 3_600.0 {
        format!("~ {:.0} h", (secs / 3_600.0).ceil())
    } else {
        format!("~ {:.0} min", (secs / 60.0).ceil().max(1.0))
    }
}

/// `950`, `48.2K`, `7.6M`.
pub(crate) fn count(value: u64) -> String {
    let value = value as f64;
    if value >= 1e6 {
        format!("{:.1}M", value / 1e6)
    } else if value >= 1e3 {
        format!("{:.1}K", value / 1e3)
    } else {
        format!("{value:.0}")
    }
}

/// `10,000`.
pub(crate) fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A day as `2026-10-16`, from seconds since the Unix epoch.
pub(crate) fn date(unix: i64) -> String {
    let (year, month, day) = civil(unix.div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

/// How long ago, short: `5m ago`, `3h ago`, `2d ago`.
pub(crate) fn ago(unix: i64, now: i64) -> String {
    let secs = (now - unix).max(0);
    match secs {
        0..60 => "just now".to_string(),
        60..3_600 => format!("{}m ago", secs / 60),
        3_600..86_400 => format!("{}h ago", secs / 3_600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// Seconds since the Unix epoch, now.
pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// The calendar day of a day number counted from 1970-01-01 (Howard
/// Hinnant's `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Seconds since the Unix epoch of an RFC 3339 UTC time such as
/// `2026-10-16T04:50:38Z`, or of an HTTP date such as
/// `Fri, 16 Oct 2026 04:50:38 GMT`.
pub(crate) fn parse_time(text: &str) -> Option<i64> {
    let text = text.trim().replace("\\:", ":");
    let rfc = text.as_bytes().get(4) == Some(&b'-');
    let (year, month, day, clock) =
        if let Some((date, clock)) = text.split_once('T').filter(|_| rfc) {
            let mut parts = date.split('-');
            let year = parts.next()?.parse().ok()?;
            let month = parts.next()?.parse().ok()?;
            let day = parts.next()?.parse().ok()?;
            (year, month, day, clock.trim_end_matches('Z').to_string())
        } else {
            // `Fri, 16 Oct 2026 04:50:38 GMT`
            let mut parts = text.split_whitespace().skip(1);
            let day = parts.next()?.parse().ok()?;
            let month = match parts.next()? {
                "Jan" => 1,
                "Feb" => 2,
                "Mar" => 3,
                "Apr" => 4,
                "May" => 5,
                "Jun" => 6,
                "Jul" => 7,
                "Aug" => 8,
                "Sep" => 9,
                "Oct" => 10,
                "Nov" => 11,
                "Dec" => 12,
                _ => return None,
            };
            let year = parts.next()?.parse().ok()?;
            (year, month, day, parts.next()?.to_string())
        };
    let mut clock = clock.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.get(..2)?.parse().ok()?;
    Some(days(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days from 1970-01-01 to a calendar day (`days_from_civil`).
fn days(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let month = i64::from(month);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_time_formats() {
        let rfc = parse_time("2026-10-16T04:50:38Z").expect("rfc 3339");
        let http = parse_time("Fri, 16 Oct 2026 04:50:38 GMT").expect("http date");
        assert_eq!(rfc, http);
        assert_eq!(date(rfc), "2026-10-16");
        // Osmosis state files escape the colons.
        assert_eq!(parse_time("2026-10-16T04\\:50\\:38Z"), Some(rfc));
    }

    #[test]
    fn groups_thousands() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(10_000), "10,000");
        assert_eq!(thousands(7_590_748), "7,590,748");
    }
}
