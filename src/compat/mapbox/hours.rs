//! A place's opening hours as Search Box's `metadata.open_hours`.
//!
//! [`Week`] reads OpenStreetMap's value. A value it does not read is not
//! sent at all.

use serde::Serialize;

use crate::compat::hours::{DAY_MINUTES, Week, Weekday};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct OpenHours {
    periods: Vec<Period>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Period {
    open: Moment,
    close: Moment,
}

/// A time of the week, `day` counting from 0 for Sunday.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Moment {
    day: u8,
    /// `HHMM`.
    time: String,
}

impl Moment {
    /// `minutes` past the start of `day`, carried into the day after.
    fn at(day: Weekday, minutes: u16) -> Self {
        let day = if minutes >= DAY_MINUTES {
            day.next()
        } else {
            day
        };
        let minutes = minutes % DAY_MINUTES;
        Self {
            day: day.sunday_number(),
            time: format!("{:02}{:02}", minutes / 60, minutes % 60),
        }
    }
}

impl OpenHours {
    /// The weekly periods `value` states, Sunday first, or `None` when
    /// [`Week`] does not read it.
    pub(super) fn parse(value: &str) -> Option<Self> {
        let week = Week::parse(value)?;
        let mut days = week.days().collect::<Vec<_>>();
        days.sort_by_key(|(day, _)| day.sunday_number());
        let periods = days
            .into_iter()
            .flat_map(|(day, spans)| {
                spans.iter().map(move |span| Period {
                    open: Moment::at(day, span.open()),
                    close: Moment::at(day, span.close()),
                })
            })
            .collect();
        Some(Self { periods })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn weekly_rules_are_periods_from_sunday() {
        let hours = OpenHours::parse("Mo-Fr 08:00-18:00; Sa 09:00-13:00,14:00-02:00; Su off")
            .expect("plain rules");
        let periods = serde_json::to_value(hours).expect("json")["periods"].clone();
        assert_eq!(periods.as_array().map(Vec::len), Some(7));
        assert_eq!(
            periods[0],
            json!({"open": {"day": 1, "time": "0800"}, "close": {"day": 1, "time": "1800"}})
        );
        assert_eq!(
            periods[6],
            json!({"open": {"day": 6, "time": "1400"}, "close": {"day": 0, "time": "0200"}})
        );
    }

    #[test]
    fn a_later_rule_replaces_an_earlier_one_and_a_range_may_wrap() {
        let hours = OpenHours::parse("Fr-Mo 10:00-24:00; Su off").expect("plain rules");
        let days = hours
            .periods
            .iter()
            .map(|period| period.open.day)
            .collect::<Vec<_>>();
        assert_eq!(days, [1, 5, 6]);
        assert_eq!(
            hours.periods[0].close,
            Moment::at(Weekday::Monday, DAY_MINUTES)
        );
        assert_eq!(
            OpenHours::parse("24/7").map(|hours| hours.periods.len()),
            Some(7)
        );
    }

    #[test]
    fn a_rule_without_days_is_every_day_and_a_comma_adds_a_rule() {
        let every = OpenHours::parse("05:00-19:00").expect("every day");
        assert_eq!(every.periods.len(), 7);
        assert_eq!(every.periods[0].open, Moment::at(Weekday::Sunday, 5 * 60));
        assert_eq!(every.periods[0].close, Moment::at(Weekday::Sunday, 19 * 60));
        let hours = OpenHours::parse("Mo-Fr 06:00-20:00, Sa,Su 07:00-12:00,13:00-18:00")
            .expect("added rules");
        let days = hours
            .periods
            .iter()
            .map(|period| period.open.day)
            .collect::<Vec<_>>();
        assert_eq!(days, [0, 0, 1, 2, 3, 4, 5, 6, 6]);
        let closed = OpenHours::parse("Mo-Su 08:00-17:00; Sa,Su off").expect("weekdays");
        assert_eq!(closed.periods.len(), 5);
        let late = OpenHours::parse("Sa 18:00-25:00").expect("extended time");
        assert_eq!(late.periods[0].close, Moment::at(Weekday::Sunday, 60));
    }
}
