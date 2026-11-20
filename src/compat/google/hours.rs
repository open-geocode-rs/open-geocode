//! A place's OpenStreetMap `opening_hours` as Google's `OpeningHours`.
//!
//! Only the weekly form is read: rules of weekdays and times, such as
//! `Mo-Fr 09:00-17:00; Sa 10:00-14:00`, a rule of times alone, which holds
//! for every day, and `24/7`. A value with anything
//! else (holidays, months, weeks, sunrise, comments, `+`) states hours this
//! reading would get wrong, so it gives none and the slot stays empty.

use serde::Serialize;

/// Google's `OpeningHours`, as weekly periods only. `weekdayDescriptions` is
/// text Google writes in the language asked, which og does not render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct OpeningHours {
    pub(super) periods: Vec<Period>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(super) struct Period {
    pub(super) open: Point,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) close: Option<Point>,
}

/// `day` counts from 0 for Sunday.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(super) struct Point {
    pub(super) day: u8,
    pub(super) hour: u8,
    pub(super) minute: u8,
}

/// OpenStreetMap's weekdays, Monday first, as Google numbers them.
const DAYS: [(&str, u8); 7] = [
    ("Mo", 1),
    ("Tu", 2),
    ("We", 3),
    ("Th", 4),
    ("Fr", 5),
    ("Sa", 6),
    ("Su", 0),
];

/// `value` as weekly periods, `None` when it holds anything this reading
/// does not read exactly.
pub(super) fn parse(value: &str) -> Option<OpeningHours> {
    let value = value.trim();
    if value == "24/7" {
        // Google writes always open as one period opening on Sunday at
        // midnight with no close.
        return Some(OpeningHours {
            periods: vec![Period {
                open: Point {
                    day: 0,
                    hour: 0,
                    minute: 0,
                },
                close: None,
            }],
        });
    }
    // Each rule sets the days it names, so a later rule overrides an earlier
    // one for its days.
    let mut week: [Option<Vec<(u16, u16)>>; 7] = Default::default();
    for rule in value
        .split(';')
        .map(str::trim)
        .filter(|rule| !rule.is_empty())
    {
        // A rule that names no weekday holds for every day.
        let (days, times) = match rule.starts_with(|character: char| character.is_ascii_digit()) {
            true => ((0..DAYS.len()).collect(), rule),
            false => {
                let (days, times) = rule.split_once(' ')?;
                (weekdays(days)?, times.trim())
            }
        };
        let spans = if times == "off" || times == "closed" {
            Vec::new()
        } else {
            times
                .split(',')
                .map(|span| span_of(span.trim()))
                .collect::<Option<Vec<_>>>()?
        };
        for day in days {
            week[day] = Some(spans.clone());
        }
    }
    if week.iter().all(Option::is_none) {
        return None;
    }
    let mut periods = Vec::new();
    for (index, spans) in week.iter().enumerate() {
        let day = DAYS[index].1;
        for &(open, close) in spans.iter().flatten() {
            // A span past midnight ("22:00-02:00", "18:00-24:00") closes on
            // the next day.
            let (close_day, close) = if close <= open {
                ((day + 1) % 7, close)
            } else if close == 24 * 60 {
                ((day + 1) % 7, 0)
            } else {
                (day, close)
            };
            periods.push(Period {
                open: point(day, open),
                close: Some(point(close_day, close)),
            });
        }
    }
    periods.sort_by_key(|period| (period.open.day, period.open.hour, period.open.minute));
    Some(OpeningHours { periods })
}

/// The weekdays a rule names, as indices into [`DAYS`]: `Mo`, `Mo-Fr`,
/// `Mo,We,Fr` or a mix. A range may wrap past Sunday (`Sa-Mo`).
fn weekdays(text: &str) -> Option<Vec<usize>> {
    let index = |name: &str| DAYS.iter().position(|(day, _)| *day == name);
    let mut days = Vec::new();
    for part in text.split(',') {
        match part.split_once('-') {
            Some((from, to)) => {
                let (from, to) = (index(from)?, index(to)?);
                let mut day = from;
                loop {
                    days.push(day);
                    if day == to {
                        break;
                    }
                    day = (day + 1) % 7;
                }
            }
            None => days.push(index(part)?),
        }
    }
    Some(days)
}

/// `HH:MM-HH:MM` as minutes after midnight. The close may be `24:00`.
fn span_of(text: &str) -> Option<(u16, u16)> {
    let (open, close) = text.split_once('-')?;
    let (open, close) = (minutes(open)?, minutes(close)?);
    (open < 24 * 60 && open != close).then_some((open, close))
}

fn minutes(text: &str) -> Option<u16> {
    let (hour, minute) = text.split_once(':')?;
    if hour.len() != 2 || minute.len() != 2 {
        return None;
    }
    let (hour, minute) = (hour.parse::<u16>().ok()?, minute.parse::<u16>().ok()?);
    (minute < 60 && (hour < 24 || (hour == 24 && minute == 0))).then_some(hour * 60 + minute)
}

fn point(day: u8, minutes: u16) -> Point {
    Point {
        day,
        hour: u8::try_from(minutes / 60).unwrap_or_default(),
        minute: u8::try_from(minutes % 60).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Day, hour and minute.
    type At = (u8, u8, u8);

    fn periods(value: &str) -> Option<Vec<(At, Option<At>)>> {
        parse(value).map(|hours| {
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
        // Times alone hold for every day.
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

    #[test]
    fn anything_but_weekly_hours_gives_none() {
        for value in [
            "Mo-Fr 09:00-17:00; PH off",
            "Jan-Mar Mo 09:00-12:00",
            "sunrise-sunset",
            "Mo-Fr 09:00-17:00 \"by appointment\"",
            "Mo-Fr 09:00+",
            "Mo-Fr 9:00-17:00",
            "",
            "open",
        ] {
            assert_eq!(parse(value), None, "{value}");
        }
    }
}
