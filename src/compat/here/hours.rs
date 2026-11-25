use serde::Serialize;

use crate::compat::hours::{DAY_MINUTES, Span, Week, Weekday};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct OpeningHours {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    categories: Vec<CategoryId>,
    text: Vec<String>,
    structured: Vec<Structured>,
    #[serde(rename = "isOpen", skip_serializing_if = "Option::is_none")]
    is_open: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct CategoryId {
    pub(super) id: String,
}

impl OpeningHours {
    pub(super) fn of_kind(mut self, id: Option<&str>) -> Self {
        self.categories = id
            .map(|id| CategoryId { id: id.to_string() })
            .into_iter()
            .collect();
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Structured {
    start: String,
    duration: String,
    recurrence: String,
}

fn names(day: Weekday) -> (&'static str, &'static str) {
    match day {
        Weekday::Monday => ("Mon", "MO"),
        Weekday::Tuesday => ("Tue", "TU"),
        Weekday::Wednesday => ("Wed", "WE"),
        Weekday::Thursday => ("Thu", "TH"),
        Weekday::Friday => ("Fri", "FR"),
        Weekday::Saturday => ("Sat", "SA"),
        Weekday::Sunday => ("Sun", "SU"),
    }
}

pub(super) fn parse(value: &str, zone: Option<&str>) -> Option<OpeningHours> {
    let week = Week::parse(value)?;
    let is_open = week.open_at(zone, jiff::Timestamp::now());
    let mut groups: Vec<(Vec<Weekday>, &[Span])> = Vec::new();
    for (day, spans) in week.days() {
        if spans.is_empty() {
            continue;
        }
        match groups.iter_mut().find(|(_, shared)| *shared == spans) {
            Some((days, _)) => days.push(day),
            None => groups.push((vec![day], spans)),
        }
    }
    let text = groups
        .iter()
        .map(|(days, spans)| {
            let times = spans
                .iter()
                .map(|span| format!("{} - {}", clock(span.open()), clock(span.close())))
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
                .map(|&day| names(day).1)
                .collect::<Vec<_>>()
                .join(",");
            spans.iter().map(move |span| {
                let (open, length) = (span.open(), span.minutes());
                Structured {
                    start: format!("T{:02}{:02}00", open / 60, open % 60),
                    duration: format!("PT{:02}H{:02}M", length / 60, length % 60),
                    recurrence: format!("FREQ:DAILY;BYDAY:{by_day}"),
                }
            })
        })
        .collect();
    Some(OpeningHours {
        categories: Vec::new(),
        text,
        structured,
        is_open,
    })
}

/// A time of day as HERE writes it. A close at midnight is `24:00`, and a
/// close past it is the next day's clock time.
fn clock(minutes: u16) -> String {
    let minutes = match minutes > DAY_MINUTES {
        true => minutes - DAY_MINUTES,
        false => minutes,
    };
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

fn day_names(days: &[Weekday]) -> String {
    let mut runs: Vec<(Weekday, Weekday)> = Vec::new();
    for &day in days {
        match runs.last_mut() {
            Some((_, last)) if last.next() == day && day != Weekday::Monday => *last = day,
            _ => runs.push((day, day)),
        }
    }
    runs.iter()
        .flat_map(|&(first, last)| {
            let (first_name, last_name) = (names(first).0, names(last).0);
            match (first, last) {
                _ if first == last => vec![first_name.to_string()],
                _ if first.next() == last => vec![first_name.to_string(), last_name.to_string()],
                _ => vec![format!("{first_name}-{last_name}")],
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::parse;

    fn hours(value: &str) -> Value {
        parse(value, None).map_or(Value::Null, |hours| {
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
}
