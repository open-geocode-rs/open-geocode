//! A place's OpenStreetMap `opening_hours` read as a week of spans, for
//! every vendor translator to write in its own shape.
//!
//! Only the plain weekly forms are read: `24/7`, and rules of days and
//! times such as `Mo-Fr 08:00-18:00; Sa 09:00-13:00,14:00-02:00; Su off`.
//! A rule with times alone holds every day. A rule after a `;` replaces
//! what earlier rules said of its days, and one after a `,` adds to them.
//! A value with anything else (holidays, months, weeks, sunrise, comments,
//! open ends) states hours a week cannot hold, so it is not read at all and
//! every vendor slot for it stays empty.

/// A day of the week, Monday first as OpenStreetMap lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Weekday {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

impl Weekday {
    /// Every day, Monday first.
    pub(crate) const ALL: [Self; 7] = [
        Self::Monday,
        Self::Tuesday,
        Self::Wednesday,
        Self::Thursday,
        Self::Friday,
        Self::Saturday,
        Self::Sunday,
    ];

    /// Its place in the week counting from 0 for Monday.
    fn monday_index(self) -> usize {
        self as usize
    }

    /// Its number counting from 0 for Sunday.
    pub(crate) fn sunday_number(self) -> u8 {
        match self {
            Self::Sunday => 0,
            day => u8::try_from(day.monday_index() + 1).unwrap_or_default(),
        }
    }

    /// The day after it.
    pub(crate) fn next(self) -> Self {
        Self::ALL[(self.monday_index() + 1) % 7]
    }

    /// Its OpenStreetMap name, `Mo` to `Su`.
    fn osm(self) -> &'static str {
        ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"][self.monday_index()]
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|day| day.osm() == name)
    }
}

/// Minutes in a day.
pub(crate) const DAY_MINUTES: u16 = 24 * 60;

/// One stretch a place is open, starting on the day that holds it.
///
/// `open` is minutes after that day's midnight, below 24:00. `close` is
/// minutes after the same midnight, after `open` and at most a day later,
/// so a span past midnight closes above 24:00.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Span {
    open: u16,
    close: u16,
}

impl Span {
    /// `08:00-18:00` as a span. A close at or before the open is on the
    /// next day, as is `24:00` and a close past it such as `25:00` for 1
    /// am. A close equal to the open, or more than a day after it, says
    /// nothing a reader can trust, so it is not a span.
    fn parse(text: &str) -> Option<Self> {
        let (open, close) = text.trim().split_once('-')?;
        let (open, close) = (clock(open)?, clock(close)?);
        if open >= DAY_MINUTES || open == close {
            return None;
        }
        let close = if close < open {
            close + DAY_MINUTES
        } else {
            close
        };
        (close - open <= DAY_MINUTES).then_some(Self { open, close })
    }

    /// Minutes after its day's midnight that it opens, below 24:00.
    pub(crate) fn open(self) -> u16 {
        self.open
    }

    /// Minutes after its day's midnight that it closes, above 24:00 when
    /// it closes on the next day.
    pub(crate) fn close(self) -> u16 {
        self.close
    }

    /// How long it stays open, in minutes, at most a day.
    pub(crate) fn minutes(self) -> u16 {
        self.close - self.open
    }
}

/// `HH:MM` as minutes after midnight, up to `48:00`.
fn clock(text: &str) -> Option<u16> {
    let (hours, minutes) = text.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let (hours, minutes) = (hours.parse::<u16>().ok()?, minutes.parse::<u16>().ok()?);
    let total = hours.checked_mul(60)? + minutes;
    (minutes < 60 && total <= 2 * DAY_MINUTES).then_some(total)
}

/// The spans a place is open on each day of a week, with at least one day
/// open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Week {
    /// Monday first, each day's spans by opening time.
    days: [Vec<Span>; 7],
}

/// The days one rule names and the spans open on each.
type Rule = (Vec<Weekday>, Vec<Span>);

impl Week {
    /// The week `value` states, or `None` when it is not one of the plain
    /// forms or opens on no day.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let mut days: [Vec<Span>; 7] = Default::default();
        if value == "24/7" {
            days.fill(vec![Span {
                open: 0,
                close: DAY_MINUTES,
            }]);
            return Some(Self { days });
        }
        for rule in value
            .split(';')
            .map(str::trim)
            .filter(|rule| !rule.is_empty())
        {
            // `Mo-Fr 08:00-18:00, Sa 09:00-12:00` adds a rule after the
            // comma, which a span never starts with. A part that is only
            // days, `Sa` of `Sa,Su off`, lists its days with the next part's.
            let mut rules = Vec::<Rule>::new();
            let mut listed = String::new();
            for part in rule.split(',').map(str::trim) {
                if !starts_with_time(part) && !part.contains(' ') {
                    listed.push_str(part);
                    listed.push(',');
                    continue;
                }
                let part = std::mem::take(&mut listed) + part;
                match rules.last_mut() {
                    Some((_, spans)) if starts_with_time(&part) => {
                        spans.push(Span::parse(&part)?);
                    }
                    _ => rules.push(rule_of(&part)?),
                }
            }
            if !listed.is_empty() {
                return None;
            }
            for (at, (named, spans)) in rules.into_iter().enumerate() {
                for day in named {
                    let held = &mut days[day.monday_index()];
                    if at == 0 {
                        held.clear();
                    }
                    held.extend_from_slice(&spans);
                }
            }
        }
        for spans in &mut days {
            spans.sort_unstable();
            spans.dedup();
        }
        days.iter()
            .any(|spans| !spans.is_empty())
            .then_some(Self { days })
    }

    /// Each day and its spans, Monday first, closed days included.
    pub(crate) fn days(&self) -> impl Iterator<Item = (Weekday, &[Span])> {
        Weekday::ALL
            .into_iter()
            .zip(self.days.iter().map(Vec::as_slice))
    }

    /// Whether the place never closes: open from midnight to midnight on
    /// every day.
    pub(crate) fn is_always_open(&self) -> bool {
        let whole_day = Span {
            open: 0,
            close: DAY_MINUTES,
        };
        self.days
            .iter()
            .all(|spans| spans.as_slice() == [whole_day])
    }
}

/// Whether `text` starts with a time, as a span does and a day never does.
fn starts_with_time(text: &str) -> bool {
    text.bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_digit())
}

/// One rule, `Mo-Fr 08:00-18:00`, `Sa off` or `05:00-19:00`, as the days it
/// names and the spans open on each. A rule naming no day is every day.
fn rule_of(text: &str) -> Option<Rule> {
    let (days, times) = match starts_with_time(text) {
        true => (Weekday::ALL.to_vec(), text),
        false => {
            let (days, times) = text.split_once(' ')?;
            (days_of(days)?, times.trim())
        }
    };
    let spans = match times {
        "off" | "closed" => Vec::new(),
        _ => vec![Span::parse(times)?],
    };
    Some((days, spans))
}

/// The days of `Mo-Fr,Su`. A range may wrap past Sunday (`Sa-Mo`).
fn days_of(text: &str) -> Option<Vec<Weekday>> {
    let mut days = Vec::new();
    for part in text.split(',') {
        match part.split_once('-') {
            Some((from, to)) => {
                let (mut day, to) = (Weekday::parse(from)?, Weekday::parse(to)?);
                loop {
                    days.push(day);
                    if day == to {
                        break;
                    }
                    day = day.next();
                }
            }
            None => days.push(Weekday::parse(part)?),
        }
    }
    Some(days)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each day's spans as `(open, close)` minutes, Monday first.
    fn week(value: &str) -> Option<Vec<Vec<(u16, u16)>>> {
        Week::parse(value).map(|week| {
            week.days()
                .map(|(_, spans)| spans.iter().map(|span| (span.open, span.close)).collect())
                .collect()
        })
    }

    const H: u16 = 60;

    #[test]
    fn rules_set_their_days_and_a_later_rule_replaces_an_earlier_one() {
        assert_eq!(
            week("Mo-Fr 08:00-18:00; Sa 09:00-13:00,14:00-02:00; Su off"),
            Some(vec![
                vec![(8 * H, 18 * H)],
                vec![(8 * H, 18 * H)],
                vec![(8 * H, 18 * H)],
                vec![(8 * H, 18 * H)],
                vec![(8 * H, 18 * H)],
                vec![(9 * H, 13 * H), (14 * H, 26 * H)],
                vec![],
            ])
        );
        let wrapped = week("Fr-Mo 10:00-24:00; Su off").expect("plain rules");
        let open = wrapped
            .iter()
            .map(|spans| !spans.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(open, [true, false, false, false, true, true, false]);
        assert_eq!(wrapped[0], [(10 * H, 24 * H)]);
        assert_eq!(
            week("Mo-Su 08:00-17:00; Sa,Su off").map(|days| days[5].len() + days[6].len()),
            Some(0)
        );
    }

    #[test]
    fn times_alone_hold_every_day_and_a_comma_adds_a_rule() {
        assert_eq!(week("05:00-19:00"), Some(vec![vec![(5 * H, 19 * H)]; 7]));
        let added = week("Mo-Fr 06:00-20:00, Sa,Su 07:00-12:00,13:00-18:00").expect("added rules");
        assert_eq!(added[0], [(6 * H, 20 * H)]);
        assert_eq!(added[6], [(7 * H, 12 * H), (13 * H, 18 * H)]);
        assert_eq!(
            week("Mo-Su 08:00-12:00,13:00-17:00; We off").map(|days| days[2].len()),
            Some(0)
        );
    }

    #[test]
    fn a_span_past_midnight_closes_on_the_next_day() {
        assert_eq!(
            week("Sa 18:00-25:00").map(|days| days[5].clone()),
            Some(vec![(18 * H, 25 * H)])
        );
        assert_eq!(
            week("Fr 22:00-02:00").map(|days| days[4].clone()),
            Some(vec![(22 * H, 26 * H)])
        );
        let always = Week::parse("24/7").expect("24/7");
        assert!(always.is_always_open());
        assert_eq!(Week::parse("Mo-Su 00:00-24:00"), Some(always));
        assert!(
            !Week::parse("Mo-Sa 00:00-24:00")
                .expect("six days")
                .is_always_open()
        );
    }

    #[test]
    fn what_a_week_cannot_hold_is_not_read() {
        for value in [
            "Mo-Fr 08:00-18:00; PH off",
            "Jan-Mar Mo 10:00-12:00",
            "Mo 08:00+",
            "Mo-Fr 8:00-18:00",
            "8-17",
            "sunrise-sunset",
            "Mo-Fr sunrise-sunset",
            "Mo-Fr 08:00-18:00 \"by appointment\"",
            "Su[2] 10:00-12:00",
            "Su off",
            "Mo,Tu",
            "Mo 24:00-25:00",
            "Mo 08:00-08:00",
            "Mo 08:00-49:00",
            "Mo 08:00-33:00",
            "",
        ] {
            assert_eq!(Week::parse(value), None, "{value}");
        }
    }
}
