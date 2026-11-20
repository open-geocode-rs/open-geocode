//! A place's OpenStreetMap `opening_hours` as HERE's `openingHours`.
//!
//! Only the weekly form is read: rules of weekdays and times, such as
//! `Mo-Fr 09:00-17:00; Sa 10:00-14:00`, a rule with times alone, which holds
//! every day, and `24/7`. A value with anything else (holidays, months,
//! weeks, sunrise, comments, `+`) states hours this reading would get
//! wrong, so it gives none and the slot stays empty.

use serde::Serialize;

/// HERE's `openingHours` entry. `isOpen` is left out: it is the hours at
/// request time in the place's own zone, which og does not compute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct OpeningHours {
    /// One line per group of days that share their hours, Monday first.
    text: Vec<String>,
    structured: Vec<Structured>,
}

/// One span of hours in iCalendar terms, on every day of `recurrence`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Structured {
    /// The opening time, `T093000`.
    start: String,
    /// How long it stays open, `PT08H30M`.
    duration: String,
    /// The days, `FREQ:DAILY;BYDAY:MO,TU`.
    recurrence: String,
}

/// OpenStreetMap's weekdays, Monday first, with HERE's text and iCalendar
/// names for each.
const DAYS: [(&str, &str, &str); 7] = [
    ("Mo", "Mon", "MO"),
    ("Tu", "Tue", "TU"),
    ("We", "Wed", "WE"),
    ("Th", "Thu", "TH"),
    ("Fr", "Fri", "FR"),
    ("Sa", "Sat", "SA"),
    ("Su", "Sun", "SU"),
];

/// A span of a day, as minutes after midnight. `close` is at most 24:00 and
/// is at or before `open` for a span that runs past midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    open: u16,
    close: u16,
}

/// `value` as HERE's opening hours, `None` when it holds anything this
/// reading does not read exactly, or no day open.
pub(super) fn parse(value: &str) -> Option<OpeningHours> {
    let week = week(value.trim())?;
    // Days that share their spans are one line, ordered by their first day.
    let mut groups: Vec<(Vec<usize>, &[Span])> = Vec::new();
    for (day, spans) in week.iter().enumerate() {
        if spans.is_empty() {
            continue;
        }
        match groups
            .iter_mut()
            .find(|(_, shared)| *shared == spans.as_slice())
        {
            Some((days, _)) => days.push(day),
            None => groups.push((vec![day], spans)),
        }
    }
    if groups.is_empty() {
        return None;
    }
    let text = groups
        .iter()
        .map(|(days, spans)| {
            let times = spans
                .iter()
                .map(|span| format!("{} - {}", clock(span.open), clock(span.close)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}: {times}", day_names(days))
        })
        .collect();
    let structured = groups
        .iter()
        .flat_map(|(days, spans)| {
            let by_day = days
                .iter()
                .map(|&day| DAYS[day].2)
                .collect::<Vec<_>>()
                .join(",");
            spans.iter().map(move |span| {
                let open = span.open;
                let length = match span.close > open {
                    true => span.close - open,
                    false => 24 * 60 - open + span.close,
                };
                Structured {
                    start: format!("T{:02}{:02}00", open / 60, open % 60),
                    duration: format!("PT{:02}H{:02}M", length / 60, length % 60),
                    recurrence: format!("FREQ:DAILY;BYDAY:{by_day}"),
                }
            })
        })
        .collect();
    Some(OpeningHours { text, structured })
}

/// Each weekday's spans, Monday first. A later rule overrides an earlier
/// one for the days it names.
fn week(value: &str) -> Option<[Vec<Span>; 7]> {
    let mut week: [Vec<Span>; 7] = Default::default();
    if value == "24/7" {
        week.fill(vec![Span {
            open: 0,
            close: 24 * 60,
        }]);
        return Some(week);
    }
    let mut any = false;
    for rule in value
        .split(';')
        .map(str::trim)
        .filter(|rule| !rule.is_empty())
    {
        // A rule with no day selector holds every day.
        let (days, times) = match rule.split_once(' ') {
            Some((days, times)) => (weekdays(days)?, times.trim()),
            None => ((0..7).collect(), rule),
        };
        let spans = match times {
            "off" | "closed" => Vec::new(),
            times => times
                .split(',')
                .map(|span| span_of(span.trim()))
                .collect::<Option<Vec<_>>>()?,
        };
        for day in days {
            week[day].clone_from(&spans);
        }
        any = true;
    }
    any.then_some(week)
}

/// The weekdays a selector names, as indices into [`DAYS`]: `Mo`, `Mo-Fr`,
/// `Mo,We,Fr` or a mix. A range may wrap past Sunday (`Sa-Mo`).
fn weekdays(text: &str) -> Option<Vec<usize>> {
    let index = |name: &str| DAYS.iter().position(|(day, _, _)| *day == name);
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

/// `HH:MM-HH:MM`. The close may be `24:00`, or before the open for a span
/// past midnight.
fn span_of(text: &str) -> Option<Span> {
    let (open, close) = text.split_once('-')?;
    let (open, close) = (minutes(open)?, minutes(close)?);
    (open < 24 * 60 && open != close).then_some(Span { open, close })
}

fn minutes(text: &str) -> Option<u16> {
    let (hour, minute) = text.split_once(':')?;
    if hour.len() != 2 || minute.len() != 2 {
        return None;
    }
    let (hour, minute) = (hour.parse::<u16>().ok()?, minute.parse::<u16>().ok()?);
    (minute < 60 && (hour < 24 || (hour == 24 && minute == 0))).then_some(hour * 60 + minute)
}

fn clock(minutes: u16) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Days as HERE writes them: a run of three or more as `Mon-Thu`, any
/// other day on its own, comma separated (`Fri, Sat`).
fn day_names(days: &[usize]) -> String {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for &day in days {
        match runs.last_mut() {
            Some((_, last)) if *last + 1 == day => *last = day,
            _ => runs.push((day, day)),
        }
    }
    runs.iter()
        .flat_map(|&(first, last)| match last - first {
            0 => vec![DAYS[first].1.to_string()],
            1 => vec![DAYS[first].1.to_string(), DAYS[last].1.to_string()],
            _ => vec![format!("{}-{}", DAYS[first].1, DAYS[last].1)],
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::parse;

    fn hours(value: &str) -> Value {
        parse(value).map_or(Value::Null, |hours| {
            serde_json::to_value(hours).expect("JSON")
        })
    }

    #[test]
    fn weekly_rules_are_text_and_icalendar_spans() {
        assert_eq!(
            hours("Mo-Sa 08:00-19:00; Su 10:00-16:00"),
            json!({
                "text": ["Mon-Sat: 08:00 - 19:00", "Sun: 10:00 - 16:00"],
                "structured": [
                    {"start": "T080000", "duration": "PT11H00M", "recurrence": "FREQ:DAILY;BYDAY:MO,TU,WE,TH,FR,SA"},
                    {"start": "T100000", "duration": "PT06H00M", "recurrence": "FREQ:DAILY;BYDAY:SU"},
                ],
            })
        );
        assert_eq!(
            hours("Mo-Th 09:30-18:00; Fr,Sa 09:00-19:00; Su 09:30-16:00")["text"],
            json!([
                "Mon-Thu: 09:30 - 18:00",
                "Fri, Sat: 09:00 - 19:00",
                "Sun: 09:30 - 16:00"
            ])
        );
    }

    #[test]
    fn times_alone_hold_every_day_and_a_later_rule_wins() {
        assert_eq!(
            hours("05:00-19:00"),
            json!({
                "text": ["Mon-Sun: 05:00 - 19:00"],
                "structured": [{"start": "T050000", "duration": "PT14H00M", "recurrence": "FREQ:DAILY;BYDAY:MO,TU,WE,TH,FR,SA,SU"}],
            })
        );
        assert_eq!(
            hours("Mo-Su 08:00-12:00,13:00-17:00; We off")["text"],
            json!(["Mon, Tue, Thu-Sun: 08:00 - 12:00, 13:00 - 17:00"])
        );
        assert_eq!(
            hours("Fr-Sa 22:00-02:00")["structured"],
            json!([
                {"start": "T220000", "duration": "PT04H00M", "recurrence": "FREQ:DAILY;BYDAY:FR,SA"},
            ])
        );
        assert_eq!(
            hours("24/7")["structured"],
            json!([{"start": "T000000", "duration": "PT24H00M", "recurrence": "FREQ:DAILY;BYDAY:MO,TU,WE,TH,FR,SA,SU"}])
        );
    }

    #[test]
    fn what_the_weekly_form_cannot_say_gives_no_hours() {
        for value in [
            "Mo-Fr 08:00-17:00; PH off",
            "Jan-Mar Mo-Fr 08:00-17:00",
            "Mo-Fr sunrise-sunset",
            "Mo-Fr 08:00+",
            "Mo-Fr 08:00-17:00 \"by appointment\"",
            "Mo off",
            "",
            "8-17",
        ] {
            assert_eq!(hours(value), Value::Null, "{value}");
        }
    }
}
