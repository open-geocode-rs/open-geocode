//! A place's opening hours as Google's `OpeningHours`.
//!
//! [`Week`] reads OpenStreetMap's value. A value it does not read gives no
//! hours, so the slot stays empty.

use serde::Serialize;

use crate::compat::hours::{DAY_MINUTES, Week, Weekday};

/// Google's `OpeningHours`, as weekly periods only. `weekdayDescriptions` is
/// text Google writes in the language asked, which og does not render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct OpeningHours {
    pub(super) periods: Vec<Period>,
    #[serde(rename = "openNow", skip_serializing_if = "Option::is_none")]
    open_now: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(super) struct Period {
    pub(super) open: Point,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) close: Option<Point>,
}

/// `day` counts from 0 for Sunday.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(super) struct Point {
    pub(super) day: u8,
    pub(super) hour: u8,
    pub(super) minute: u8,
}

impl Point {
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
            hour: u8::try_from(minutes / 60).unwrap_or_default(),
            minute: u8::try_from(minutes % 60).unwrap_or_default(),
        }
    }
}

/// `value` as weekly periods, `None` when [`Week`] does not read it.
pub(super) fn parse(value: &str, zone: Option<&str>) -> Option<OpeningHours> {
    let week = Week::parse(value)?;
    let open_now = week.open_at(zone, jiff::Timestamp::now());
    if week.is_always_open() {
        // Google writes always open as one period opening on Sunday at
        // midnight with no close.
        return Some(OpeningHours {
            open_now,
            periods: vec![Period {
                open: Point::at(Weekday::Sunday, 0),
                close: None,
            }],
        });
    }
    let mut periods = week
        .days()
        .flat_map(|(day, spans)| {
            spans.iter().map(move |span| Period {
                open: Point::at(day, span.open()),
                close: Some(Point::at(day, span.close())),
            })
        })
        .collect::<Vec<_>>();
    periods.sort_by_key(|period| period.open);
    Some(OpeningHours { periods, open_now })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Day, hour and minute.
    type At = (u8, u8, u8);

    fn periods(value: &str) -> Option<Vec<(At, Option<At>)>> {
        parse(value, None).map(|hours| {
            hours
                .periods
                .iter()
                .map(|period| {
                    (
                        (period.open.day, period.open.hour, period.open.minute),
                        period
                            .close
                            .map(|close| (close.day, close.hour, close.minute)),
                    )
                })
                .collect()
        })
    }

    #[test]
    fn weekly_rules_are_periods_from_sunday() {
        assert_eq!(
            periods("Mo-Fr 09:00-17:00; Sa 10:00-14:30"),
            Some(vec![
                ((1, 9, 0), Some((1, 17, 0))),
                ((2, 9, 0), Some((2, 17, 0))),
                ((3, 9, 0), Some((3, 17, 0))),
                ((4, 9, 0), Some((4, 17, 0))),
                ((5, 9, 0), Some((5, 17, 0))),
                ((6, 10, 0), Some((6, 14, 30))),
            ])
        );
        assert_eq!(periods("24/7"), Some(vec![((0, 0, 0), None)]));
        assert_eq!(
            periods("Sa-Su 22:00-02:00"),
            Some(vec![
                ((0, 22, 0), Some((1, 2, 0))),
                ((6, 22, 0), Some((0, 2, 0))),
            ])
        );
        assert_eq!(
            periods("Mo 08:00-12:00,13:00-24:00"),
            Some(vec![
                ((1, 8, 0), Some((1, 12, 0))),
                ((1, 13, 0), Some((2, 0, 0)))
            ])
        );
        assert_eq!(
            periods("05:00-19:00"),
            Some(
                (0..7)
                    .map(|day| ((day, 5, 0), Some((day, 19, 0))))
                    .collect()
            )
        );
        // A later rule overrides an earlier one for its days.
        assert_eq!(
            periods("Mo-We 09:00-17:00; Tu off"),
            Some(vec![
                ((1, 9, 0), Some((1, 17, 0))),
                ((3, 9, 0), Some((3, 17, 0)))
            ])
        );
    }
}
