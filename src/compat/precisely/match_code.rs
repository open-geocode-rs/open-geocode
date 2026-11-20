//! Precisely's Global Match Code, `V4-P4S4A0T4R6Z4C4-5`.
//!
//! A verify result carries one, built from how og judged each address part
//! the request stated, and `/v1/explain-addresscode` reads one back. The
//! code is three segments. The address match status and verification
//! level, a value per address component, and the match confidence.

use std::{collections::BTreeMap, fmt::Write as _};

use serde::Serialize;

use crate::api::{PartMatch, QueryMatch};

/// The parts og judged of the parts a request stated.
fn judged_parts(judged: QueryMatch) -> impl Iterator<Item = PartMatch> {
    [
        judged.housenumber,
        judged.street,
        judged.unit,
        judged.locality,
        judged.region,
        judged.postcode,
        judged.country,
    ]
    .into_iter()
    .flatten()
}

/// A result's `score`, 0 to 100, from og's own judgement of the parts the
/// request stated: each matched part earns two, each partial one, each
/// unmatched none, out of two a part. `None` when og judged no part, as of
/// a record looked up by its key, since nothing then says how well it
/// answers.
pub(super) fn score(judged: QueryMatch) -> Option<u8> {
    let (earned, stated) = judged_parts(judged).fold((0u32, 0u32), |(earned, stated), part| {
        let points = match part {
            PartMatch::Matched => 2,
            PartMatch::Partial => 1,
            PartMatch::Unmatched => 0,
        };
        (earned + points, stated + 1)
    });
    (stated > 0).then(|| u8::try_from((100 * earned + stated) / (2 * stated)).unwrap_or(100))
}

/// Every component the extended code may carry, in the order the code
/// writes them, with the label Precisely gives each.
const COMPONENTS: [(char, &str); 19] = [
    ('D', "Department"),
    ('M', "Company"),
    ('B', "Building"),
    ('N', "Place name"),
    ('U', "Sub-premise/Unit"),
    ('F', "Sub-premise2/Floor"),
    ('G', "Sub-premise3/Room"),
    ('P', "Premise/House Number"),
    ('S', "Street"),
    ('Q', "Sub-street"),
    ('X', "PO Box"),
    ('A', "Sub-city"),
    ('T', "City"),
    ('E', "Extra City"),
    ('R', "Region/State"),
    ('W', "Principality"),
    ('Z', "Postcode/Zip Code"),
    ('Y', "Extra Postcode/Zip+4"),
    ('C', "Country"),
];

/// The components of the simple code, a subset in the same order.
const SIMPLE: [char; 7] = ['P', 'S', 'A', 'T', 'R', 'Z', 'C'];

/// The components og writes in an extended code. `N` is left out, since
/// og judges no place name apart from the street line.
const EXTENDED: [char; 18] = [
    'D', 'M', 'B', 'U', 'F', 'G', 'P', 'S', 'Q', 'X', 'A', 'T', 'E', 'R', 'W', 'Z', 'Y', 'C',
];

const NOT_FOUND: u8 = 0;
const NOT_IDENTIFIED: u8 = 1;
const CORRECTED: u8 = 3;
const VERIFIED: u8 = 4;
const APPENDED: u8 = 6;

/// What a result holds for each component og can fill.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Holds {
    pub(super) housenumber: bool,
    pub(super) street: bool,
    pub(super) unit: bool,
    pub(super) sub_city: bool,
    pub(super) city: bool,
    pub(super) region: bool,
    pub(super) postcode: bool,
    pub(super) postcode_ext: bool,
    pub(super) country: bool,
}

/// A Global Match Code og derives for one result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MatchCode {
    level: u8,
    values: BTreeMap<char, u8>,
    confidence: u8,
}

impl MatchCode {
    /// The code for a result og judged against the stated parts, or `None`
    /// when no stated part was verified or one contradicts the result. A
    /// result in another town than the one asked for is no verified
    /// address, and Precisely's `M` and `U` statuses are not issued, so it
    /// has no code at all.
    ///
    /// A stated part og matched exactly is verified without change, one it
    /// accepted under another spelling is corrected. A part the request
    /// left out and the result fills in is appended from og's data, which
    /// stands in for the reference data the code speaks of. The confidence
    /// is the result's `score` in fifths, none without one.
    pub(super) fn derive(judged: QueryMatch, holds: Holds, score: Option<u8>) -> Option<Self> {
        if judged_parts(judged).any(|part| part == PartMatch::Unmatched) {
            return None;
        }
        let value = |stated: Option<PartMatch>, has: bool| match (stated, has) {
            (Some(PartMatch::Matched), _) => VERIFIED,
            (Some(PartMatch::Partial), _) => CORRECTED,
            (Some(PartMatch::Unmatched), _) => NOT_IDENTIFIED,
            (None, true) => APPENDED,
            (None, false) => NOT_FOUND,
        };
        let unjudged = |has: bool| if has { APPENDED } else { NOT_FOUND };
        let values = BTreeMap::from([
            ('U', value(judged.unit, holds.unit)),
            ('P', value(judged.housenumber, holds.housenumber)),
            ('S', value(judged.street, holds.street)),
            ('A', unjudged(holds.sub_city)),
            ('T', value(judged.locality, holds.city)),
            ('R', value(judged.region, holds.region)),
            ('Z', value(judged.postcode, holds.postcode)),
            ('Y', unjudged(holds.postcode_ext)),
            ('C', value(judged.country, holds.country)),
        ]);
        let verified =
            |part: Option<PartMatch>| matches!(part, Some(PartMatch::Matched | PartMatch::Partial));
        let level = if verified(judged.housenumber) {
            4
        } else if verified(judged.street) {
            3
        } else if verified(judged.postcode) {
            2
        } else if verified(judged.locality) || verified(judged.country) {
            1
        } else {
            return None;
        };
        let confidence = score.map_or(0, |score| (score + 10) / 20);
        Some(Self {
            level,
            values,
            confidence,
        })
    }

    fn write(&self, components: &[char]) -> String {
        let mut code = format!("V{}-", self.level);
        for letter in components {
            let value = self.values.get(letter).copied().unwrap_or(NOT_FOUND);
            let _ = write!(code, "{letter}{value}");
        }
        let _ = write!(code, "-{}", self.confidence);
        code
    }

    pub(super) fn simple(&self) -> String {
        self.write(&SIMPLE)
    }

    pub(super) fn extended(&self) -> String {
        self.write(&EXTENDED)
    }
}

/// A Global Match Code read from a client, any status Precisely defines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedCode {
    status: char,
    level: u8,
    components: Vec<(char, u8)>,
    confidence: u8,
}

fn label(letter: char) -> &'static str {
    COMPONENTS
        .iter()
        .find(|(known, _)| *known == letter)
        .map_or("", |(_, label)| label)
}

fn position(letter: char) -> Option<usize> {
    COMPONENTS.iter().position(|(known, _)| *known == letter)
}

/// Read a code, or say what is wrong with it, as the detail after
/// Precisely's `(DIS-LI-1040):`.
pub(super) fn parse(code: &str) -> Result<ParsedCode, String> {
    let segments = code.trim().split('-').collect::<Vec<_>>();
    let [status, components, confidence] = segments[..] else {
        return Err("code must have three segments separated by dashes".to_string());
    };
    let mut status_chars = status.chars();
    let (Some(letter), Some(level), None) = (
        status_chars.next(),
        status_chars.next(),
        status_chars.next(),
    ) else {
        return Err("address match status must be a letter and a level".to_string());
    };
    if !matches!(letter, 'V' | 'M' | 'U') {
        return Err("address match status must be V, M or U".to_string());
    }
    let level = level
        .to_digit(10)
        .and_then(|level| u8::try_from(level).ok())
        .filter(|level| (1..=4).contains(level))
        .ok_or_else(|| "verification level must be 1-4".to_string())?;
    let chars = components.chars().collect::<Vec<_>>();
    if chars.is_empty() || chars.len() % 2 != 0 {
        return Err("component match status must be letter and digit pairs".to_string());
    }
    let mut parsed = Vec::with_capacity(chars.len() / 2);
    let mut last: Option<(usize, char)> = None;
    for pair in chars.chunks(2) {
        let (component, value) = (pair[0], pair[1]);
        let at = position(component).ok_or_else(|| format!("unknown component {component}"))?;
        let value = value
            .to_digit(10)
            .and_then(|value| u8::try_from(value).ok())
            .filter(|value| *value <= APPENDED)
            .ok_or_else(|| format!("{} ({component}) value must be 0-6", label(component)))?;
        if let Some((before, previous)) = last {
            if at == before {
                return Err(format!(
                    "duplicate component {} ({component})",
                    label(component)
                ));
            }
            if at < before {
                return Err(format!(
                    "component order violation - {} ({component}) must come before {} \
                     ({previous})",
                    label(component),
                    label(previous)
                ));
            }
        }
        last = Some((at, component));
        parsed.push((component, value));
    }
    let confidence = match confidence.as_bytes() {
        [digit @ b'0'..=b'5'] => digit - b'0',
        _ => return Err("match confidence must be 0-5".to_string()),
    };
    Ok(ParsedCode {
        status: letter,
        level,
        components: parsed,
        confidence,
    })
}

const fn meaning(value: u8) -> &'static str {
    match value {
        0 => "Not found or empty",
        1 => "Parsed and standardized but not identified in reference data",
        2 => "Not identified; reference data suggests the component is out of range or unknown",
        3 => "Verified and corrected using reference data",
        4 => "Verified without changes",
        5 => "Verified using a reference data alias",
        _ => "Appended using reference data",
    }
}

const fn level_label(level: u8) -> &'static str {
    match level {
        4 => "Premise verified",
        3 => "Street verified",
        2 => "Postcode/City/Country verified",
        _ => "City and country verified",
    }
}

const fn confidence_label(confidence: u8) -> &'static str {
    match confidence {
        5 => "High Confidence",
        4 => "Moderate-High Confidence",
        3 => "Medium Confidence",
        2 => "Medium-Low Confidence",
        1 => "Low Confidence",
        _ => "No Confidence",
    }
}

const fn status_name(status: char) -> &'static str {
    match status {
        'V' => "Verified",
        'M' => "Multiple Matches",
        _ => "Unmatched",
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct OfficialDescription {
    address_match_status: AddressMatchStatus,
    component_match_status: BTreeMap<char, ComponentStatus>,
    match_confidence: MatchConfidence,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AddressMatchStatus {
    symbol: String,
    status: &'static str,
    verification_level: u8,
    verification_level_label: &'static str,
}

#[derive(Debug, Serialize)]
struct ComponentStatus {
    label: &'static str,
    value: u8,
    meaning: &'static str,
}

#[derive(Debug, Serialize)]
struct MatchConfidence {
    value: u8,
    label: &'static str,
}

impl ParsedCode {
    /// One line a person reads, such as `Address premise verified with high
    /// confidence.`, then a sentence for each component that changed.
    pub(super) fn summary(&self) -> String {
        let mut summary = match self.status {
            'V' => format!(
                "Address {} with {}.",
                level_label(self.level).to_lowercase(),
                confidence_label(self.confidence).to_lowercase()
            ),
            'M' => "Address matches more than one record.".to_string(),
            _ => "Address not matched.".to_string(),
        };
        for &(component, value) in &self.components {
            let change = match value {
                1 | 2 => "not identified in reference data",
                3 => "corrected from reference data",
                5 => "verified through an alias",
                6 => "appended from reference data",
                _ => continue,
            };
            let _ = write!(summary, " {} {change}.", label(component));
        }
        summary
    }

    pub(super) fn official(&self) -> OfficialDescription {
        OfficialDescription {
            address_match_status: AddressMatchStatus {
                symbol: format!("{}{}", self.status, self.level),
                status: status_name(self.status),
                verification_level: self.level,
                verification_level_label: level_label(self.level),
            },
            component_match_status: self
                .components
                .iter()
                .map(|&(component, value)| {
                    (
                        component,
                        ComponentStatus {
                            label: label(component),
                            value,
                            meaning: meaning(value),
                        },
                    )
                })
                .collect(),
            match_confidence: MatchConfidence {
                value: self.confidence,
                label: confidence_label(self.confidence),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn verified_street_address() -> MatchCode {
        let judged = QueryMatch {
            housenumber: Some(PartMatch::Matched),
            street: Some(PartMatch::Partial),
            locality: Some(PartMatch::Matched),
            ..QueryMatch::default()
        };
        let holds = Holds {
            housenumber: true,
            street: true,
            city: true,
            region: true,
            postcode: true,
            country: true,
            ..Holds::default()
        };
        MatchCode::derive(judged, holds, score(judged)).expect("a verified address")
    }

    #[test]
    fn a_verified_address_writes_both_forms() {
        let code = verified_street_address();
        assert_eq!(code.simple(), "V4-P4S3A0T4R6Z6C6-4");
        assert_eq!(code.extended(), "V4-D0M0B0U0F0G0P4S3Q0X0A0T4E0R6W0Z6Y0C6-4");
    }

    #[test]
    fn the_level_is_the_finest_part_verified() {
        let holds = Holds::default();
        let level = |judged: QueryMatch| {
            MatchCode::derive(judged, holds, None).map(|code| code.simple()[..2].to_string())
        };
        let street = QueryMatch {
            street: Some(PartMatch::Matched),
            ..QueryMatch::default()
        };
        assert_eq!(level(street).as_deref(), Some("V3"));
        let postcode = QueryMatch {
            postcode: Some(PartMatch::Partial),
            ..QueryMatch::default()
        };
        assert_eq!(level(postcode).as_deref(), Some("V2"));
        let town = QueryMatch {
            locality: Some(PartMatch::Matched),
            ..QueryMatch::default()
        };
        assert_eq!(level(town).as_deref(), Some("V1"));
        let nothing = QueryMatch {
            street: Some(PartMatch::Unmatched),
            ..QueryMatch::default()
        };
        assert_eq!(level(nothing), None);
        // The house in another town than the one asked for is no verified
        // address, though its number and street match.
        let elsewhere = QueryMatch {
            housenumber: Some(PartMatch::Matched),
            street: Some(PartMatch::Matched),
            locality: Some(PartMatch::Unmatched),
            ..QueryMatch::default()
        };
        assert_eq!(level(elsewhere), None);
    }

    #[test]
    fn the_confidence_is_the_score_in_fifths() {
        let judged = QueryMatch {
            housenumber: Some(PartMatch::Matched),
            street: Some(PartMatch::Partial),
            ..QueryMatch::default()
        };
        assert_eq!(score(judged), Some(75));
        let code = MatchCode::derive(judged, Holds::default(), score(judged)).expect("verified");
        assert!(code.simple().ends_with("-4"), "{}", code.simple());
        assert_eq!(score(QueryMatch::default()), None);
    }

    #[test]
    fn a_code_og_writes_reads_back() {
        let code = verified_street_address();
        for written in [code.simple(), code.extended()] {
            let parsed = parse(&written).expect("og's own code");
            assert_eq!(parsed.level, 4);
            assert_eq!(parsed.confidence, 4);
        }
    }

    #[test]
    fn a_code_explains_as_its_documented_example() {
        let parsed = parse("V4-D0M0B0U4F0G0P4S4Q0X0A0T4E0R6W0Z4Y4C4-5").expect("valid");
        assert_eq!(
            parsed.summary(),
            "Address premise verified with high confidence. Region/State appended from \
             reference data."
        );
        let official = serde_json::to_value(parsed.official()).expect("JSON");
        assert_eq!(
            official["addressMatchStatus"],
            json!({"symbol": "V4", "status": "Verified", "verificationLevel": 4,
                   "verificationLevelLabel": "Premise verified"})
        );
        assert_eq!(
            official["componentMatchStatus"]["R"],
            json!({"label": "Region/State", "value": 6, "meaning": "Appended using reference data"})
        );
        assert_eq!(
            official["componentMatchStatus"]["P"],
            json!({"label": "Premise/House Number", "value": 4, "meaning": "Verified without changes"})
        );
        assert_eq!(
            official["matchConfidence"],
            json!({"value": 5, "label": "High Confidence"})
        );
        let letters = official["componentMatchStatus"]
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect::<String>();
        assert_eq!(letters, "ABCDEFGMPQRSTUWXYZ");
    }

    #[test]
    fn a_malformed_code_says_what_is_wrong() {
        let cases = [
            ("V5-P4S4-5", "verification level must be 1-4"),
            ("X4-P4-5", "address match status must be V, M or U"),
            (
                "V4-P4S4",
                "code must have three segments separated by dashes",
            ),
            (
                "V4-P4S-5",
                "component match status must be letter and digit pairs",
            ),
            ("V4-P4K4-5", "unknown component K"),
            ("V4-P7-5", "Premise/House Number (P) value must be 0-6"),
            ("V4-P4P4-5", "duplicate component Premise/House Number (P)"),
            (
                "V4-D0U4M0-5",
                "component order violation - Company (M) must come before Sub-premise/Unit (U)",
            ),
            ("V4-P4-6", "match confidence must be 0-5"),
            ("V4-P4-05", "match confidence must be 0-5"),
        ];
        for (code, message) in cases {
            assert_eq!(parse(code), Err(message.to_string()), "{code}");
        }
    }
}
