//! A time zone's offset from UTC at the moment a vendor is answered.
//!
//! og's Pack names the IANA zone a result lies in. Vendors also write the
//! zone's offset in effect when they answer, which takes daylight saving
//! time into account, so it is computed per request from the zone's rules.

use jiff::{Timestamp, tz::TimeZone};

/// A zone's offset from UTC, in seconds east.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Offset(i32);

impl Offset {
    /// The offset in effect now in the IANA zone `name`, none when og knows
    /// no such zone.
    pub(crate) fn now(name: &str) -> Option<Self> {
        Self::at(name, Timestamp::now())
    }

    fn at(name: &str, at: Timestamp) -> Option<Self> {
        let zone = TimeZone::get(name).ok()?;
        Some(Self(zone.to_offset(at).seconds()))
    }

    /// Whole minutes east of UTC.
    pub(crate) fn minutes(self) -> i32 {
        self.0 / 60
    }

    /// `+02:00`, `-05:00`, `+05:45`.
    pub(crate) fn text(self) -> String {
        let sign = if self.0 < 0 { '-' } else { '+' };
        let minutes = self.0.unsigned_abs() / 60;
        format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(name: &str, instant: &str) -> Option<Offset> {
        Offset::at(name, instant.parse().expect("a timestamp"))
    }

    #[test]
    fn an_offset_follows_daylight_saving_time() {
        let offset =
            |name, instant| at(name, instant).map(|offset| (offset.text(), offset.minutes()));
        assert_eq!(
            offset("Europe/Berlin", "2026-07-01T12:00:00Z"),
            Some(("+02:00".to_string(), 120))
        );
        assert_eq!(
            offset("Europe/Berlin", "2026-01-15T12:00:00Z"),
            Some(("+01:00".to_string(), 60))
        );
        assert_eq!(
            offset("America/Chicago", "2026-07-01T12:00:00Z"),
            Some(("-05:00".to_string(), -300))
        );
        assert_eq!(
            offset("America/Chicago", "2026-01-15T12:00:00Z"),
            Some(("-06:00".to_string(), -360))
        );
    }

    #[test]
    fn a_zone_off_the_hour_or_unknown() {
        assert_eq!(
            at("Asia/Kathmandu", "2026-07-01T12:00:00Z").map(Offset::text),
            Some("+05:45".to_string())
        );
        assert_eq!(
            at("America/St_Johns", "2026-01-15T12:00:00Z")
                .map(|offset| (offset.text(), offset.minutes())),
            Some(("-03:30".to_string(), -210))
        );
        assert_eq!(at("Mars/Olympus_Mons", "2026-07-01T12:00:00Z"), None);
    }
}
