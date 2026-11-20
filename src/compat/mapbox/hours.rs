//! OpenStreetMap `opening_hours` as Search Box's `metadata.open_hours`.
//!
//! Only the plain weekly forms are read: `24/7`, and rules of days and
//! times such as `Mo-Fr 08:00-18:00; Sa 09:00-13:00; Su off`. Anything
//! else (holidays, months, weeks, comments, open ends) says more than a
//! weekly table holds, so a value with any of it is not sent at all.

use serde::Serialize;

/// The week, Sunday first as Mapbox numbers its days.
const DAYS: [&str; 7] = ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"];
const DAY_MINUTES: u16 = 24 * 60;

/// The days a rule names, Sunday as 0, and the minutes open on each, from
/// and to.
type Rule = (Vec<u8>, Vec<(u16, u16)>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct OpenHours {
    periods: Vec<Period>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Period {
    open: Moment,
    close: Moment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Moment {
    day: u8,
    /// `HHMM`.
    time: String,
}

impl Moment {
    /// `minutes` past the start of `day`, carried into the days after.
    fn at(day: u8, minutes: u16) -> Self {
        let day = (u16::from(day) + minutes / DAY_MINUTES) % 7;
        let minutes = minutes % DAY_MINUTES;
        Self {
            day: u8::try_from(day).unwrap_or_default(),
            time: format!("{:02}{:02}", minutes / 60, minutes % 60),
        }
    }
}

impl OpenHours {
    /// The weekly periods `value` states, or `None` when it is not one of
    /// the plain forms.
    pub(super) fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        // Minutes open, from and to, per day. A later rule replaces what an
        // earlier one said of its days.
        let mut week: [Vec<(u16, u16)>; 7] = Default::default();
        if value == "24/7" {
            for day in &mut week {
                *day = vec![(0, DAY_MINUTES)];
            }
        } else {
            for rule in value
                .split(';')
                .map(str::trim)
                .filter(|rule| !rule.is_empty())
            {
                // `Mo-Fr 08:00-18:00, Sa 09:00-12:00` adds a rule after the
                // comma, which a time span never starts with.
                // A part that is only days, `Sa` of `Sa,Su off`, lists its
                // days with the next part's.
                let mut additions = Vec::<Rule>::new();
                let mut listed = String::new();
                for part in rule.split(',').map(str::trim) {
                    if !starts_with_time(part) && !part.contains(' ') {
                        listed.push_str(part);
                        listed.push(',');
                        continue;
                    }
                    let part = std::mem::take(&mut listed) + part;
                    match additions.last_mut() {
                        Some((_, spans)) if starts_with_time(&part) => {
                            spans.push(span_of(&part)?);
                        }
                        _ => additions.push(rule_of(&part)?),
                    }
                }
                if !listed.is_empty() {
                    return None;
                }
                // The first rule after a `;` replaces what earlier ones said
                // of its days; one after a `,` adds to them.
                for (at, (days, spans)) in additions.into_iter().enumerate() {
                    for day in days {
                        let held = &mut week[usize::from(day)];
                        if at == 0 {
                            held.clear();
                        }
                        held.extend_from_slice(&spans);
                    }
                }
            }
        }
        let periods = week
            .iter()
            .zip(0u8..)
            .flat_map(|(spans, day)| {
                spans.iter().map(move |&(from, to)| Period {
                    open: Moment::at(day, from),
                    close: Moment::at(day, to),
                })
            })
            .collect::<Vec<_>>();
        (!periods.is_empty()).then_some(Self { periods })
    }
}

/// Whether `text` starts with a time, as a span does and a day never does.
fn starts_with_time(text: &str) -> bool {
    text.bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_digit())
}

/// One rule, `Mo-Fr 08:00-18:00`, `Sa off` or `05:00-19:00`, as the days
/// it names and the spans open on each. A rule naming no day is every day.
fn rule_of(text: &str) -> Option<Rule> {
    let (days, times) = match starts_with_time(text) {
        true => ((0..7).collect(), text),
        false => {
            let (days, times) = text.split_once(' ')?;
            (days_of(days)?, times.trim())
        }
    };
    let spans = match times {
        "off" | "closed" => Vec::new(),
        _ => vec![span_of(times)?],
    };
    Some((days, spans))
}

/// The days of `Mo-Fr,Su`, Sunday as 0. A range may wrap past Sunday.
fn days_of(text: &str) -> Option<Vec<u8>> {
    let day = |name: &str| {
        DAYS.iter()
            .position(|day| *day == name)
            .and_then(|at| u8::try_from(at).ok())
    };
    let mut days = Vec::new();
    for part in text.split(',') {
        match part.split_once('-') {
            Some((from, to)) => {
                let (from, to) = (day(from)?, day(to)?);
                let mut at = from;
                loop {
                    days.push(at);
                    if at == to {
                        break;
                    }
                    at = (at + 1) % 7;
                }
            }
            None => days.push(day(part)?),
        }
    }
    Some(days)
}

/// `08:00-18:00` as minutes from and to. An end at or before the start is
/// on the next day, as is `24:00` and an end past it, such as `25:00` for
/// 1 am.
fn span_of(text: &str) -> Option<(u16, u16)> {
    let (from, to) = text.trim().split_once('-')?;
    let minutes = |time: &str, most: u16| {
        let (hours, minutes) = time.split_once(':')?;
        if hours.len() != 2 || minutes.len() != 2 {
            return None;
        }
        let (hours, minutes) = (hours.parse::<u16>().ok()?, minutes.parse::<u16>().ok()?);
        (minutes < 60 && hours * 60 + minutes <= most).then_some(hours * 60 + minutes)
    };
    let (from, to) = (
        minutes(from, DAY_MINUTES - 1)?,
        minutes(to, 2 * DAY_MINUTES)?,
    );
    Some((from, if to <= from { to + DAY_MINUTES } else { to }))
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
        // Past midnight closes on the next day, Saturday's on Sunday.
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
        assert_eq!(hours.periods[0].close, Moment::at(1, DAY_MINUTES));
        assert_eq!(
            OpenHours::parse("24/7").map(|hours| hours.periods.len()),
            Some(7)
        );
    }

    #[test]
    fn a_rule_without_days_is_every_day_and_a_comma_adds_a_rule() {
        let every = OpenHours::parse("05:00-19:00").expect("every day");
        assert_eq!(every.periods.len(), 7);
        assert_eq!(every.periods[0].open, Moment::at(0, 5 * 60));
        assert_eq!(every.periods[0].close, Moment::at(0, 19 * 60));
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
        assert_eq!(late.periods[0].close, Moment::at(0, 60));
    }

    #[test]
    fn what_a_week_cannot_hold_is_not_sent() {
        for value in [
            "Mo-Fr 08:00-18:00; PH off",
            "Jan-Mar Mo 10:00-12:00",
            "Mo 08:00+",
            "Mo-Fr 8:00-18:00",
            "sunrise-sunset",
            "Mo-Fr 08:00-18:00 \"by appointment\"",
            "Su off",
            "Mo,Tu",
            "Mo 24:00-25:00",
            "Mo 08:00-49:00",
            "",
        ] {
            assert_eq!(OpenHours::parse(value), None, "{value}");
        }
    }
}
