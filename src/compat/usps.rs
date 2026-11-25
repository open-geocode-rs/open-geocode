use crate::search::road::direction_of;

pub(crate) fn zip_plus_four(postcode: &str) -> Option<(&str, &str)> {
    let (zip, plus_four) = postcode.split_once('-')?;
    let digits = |part: &str, length: usize| {
        part.len() == length && part.bytes().all(|byte| byte.is_ascii_digit())
    };
    (digits(zip, 5) && digits(plus_four, 4)).then_some((zip, plus_four))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Style {
    Postal,
    Display,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsAddress {
    number: String,
    prefix: Option<&'static str>,
    name: String,
    suffix_type: Option<String>,
    suffix: Option<&'static str>,
    unit: Option<Unit>,
    city: String,
    state: String,
    zip: Option<String>,
    zip4: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Unit {
    designator: Option<String>,
    value: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Parts<'a> {
    pub(crate) number: &'a str,
    pub(crate) prefix: Option<&'a str>,
    pub(crate) name: &'a str,
    pub(crate) kind: Option<&'a str>,
    pub(crate) suffix: Option<&'a str>,
    pub(crate) unit_type: Option<&'a str>,
    pub(crate) unit: Option<&'a str>,
    pub(crate) city: &'a str,
    pub(crate) state: &'a str,
    pub(crate) zip: Option<&'a str>,
    pub(crate) zip4: Option<&'a str>,
}

impl UsAddress {
    pub(crate) fn of(parts: &Parts<'_>) -> Self {
        let text = |value: &str| value.split_whitespace().collect::<Vec<_>>().join(" ");
        let (prefix, name, kind, suffix_word) =
            if parts.prefix.is_none() && parts.kind.is_none() && parts.suffix.is_none() {
                split(parts.name)
            } else {
                (parts.prefix, text(parts.name), parts.kind, parts.suffix)
            };
        Self {
            number: text(parts.number),
            prefix: prefix.and_then(direction),
            name,
            suffix_type: kind.map(suffix),
            suffix: suffix_word.and_then(direction),
            unit: parts.unit.map(|value| Unit {
                designator: parts.unit_type.map(designator),
                value: text(value),
            }),
            city: text(parts.city),
            state: parts.state.to_uppercase(),
            zip: parts.zip.map(str::to_string),
            zip4: parts.zip4.map(str::to_string),
        }
    }

    pub(crate) fn street_line(&self, style: Style) -> String {
        let unit = self.unit.as_ref();
        style.whole(&join([
            Some(self.number.clone()),
            self.prefix.map(str::to_string),
            Some(style.name(&self.name)),
            self.suffix_type
                .as_deref()
                .map(|kind| style.abbreviation(kind)),
            self.suffix.map(str::to_string),
            unit.and_then(|unit| unit.designator.as_deref())
                .map(|designator| style.abbreviation(designator)),
            unit.map(|unit| unit.value.clone()),
        ]))
    }

    pub(crate) fn street(&self, style: Style) -> String {
        match style {
            Style::Postal => style.whole(&self.name),
            Style::Display => join([
                self.prefix.map(str::to_string),
                Some(style.name(&self.name)),
                self.suffix_type
                    .as_deref()
                    .map(|kind| style.abbreviation(kind)),
                self.suffix.map(str::to_string),
            ]),
        }
    }

    pub(crate) fn street_type(&self, style: Style) -> Option<String> {
        self.suffix_type
            .as_deref()
            .map(|kind| style.abbreviation(kind))
    }

    pub(crate) fn unit(&self, style: Style) -> Option<String> {
        let unit = self.unit.as_ref()?;
        Some(join([
            unit.designator
                .as_deref()
                .map(|designator| style.abbreviation(designator)),
            Some(unit.value.clone()),
        ]))
    }

    pub(crate) fn city(&self, style: Style) -> String {
        style.whole(&style.name(&self.city))
    }

    fn zip(&self) -> Option<String> {
        let zip = self.zip.as_deref()?;
        Some(match self.zip4.as_deref() {
            Some(plus_four) => format!("{zip}-{plus_four}"),
            None => zip.to_string(),
        })
    }

    pub(crate) fn last_line(&self, style: Style) -> String {
        let city = self.city(style);
        let state_zip = join([Some(self.state.clone()), self.zip()]);
        format!("{city} {state_zip}")
    }

    /// `BURLINGTON, MA  01803-5231`, Precisely's `formattedLocationAddress`.
    /// The postal form sets the ZIP two spaces after the state, as the
    /// postal service's last line does.
    pub(crate) fn location_line(&self, style: Style) -> String {
        let city = self.city(style);
        let gap = match style {
            Style::Postal => "  ",
            Style::Display => " ",
        };
        match self.zip() {
            Some(zip) => format!("{city}, {}{gap}{zip}", self.state),
            None => format!("{city}, {}", self.state),
        }
    }
}

impl Style {
    fn whole(self, text: &str) -> String {
        match self {
            Self::Postal => text.to_uppercase(),
            Self::Display => text.to_string(),
        }
    }

    fn name(self, text: &str) -> String {
        match self {
            Self::Postal => text.to_string(),
            Self::Display => text.split(' ').map(word_case).collect::<Vec<_>>().join(" "),
        }
    }

    fn abbreviation(self, short: &str) -> String {
        match self {
            Self::Postal => short.to_string(),
            Self::Display => capitalized(short),
        }
    }
}

fn word_case(word: &str) -> String {
    let digits = word.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && ["ST", "ND", "RD", "TH"].contains(&&word[digits..]) {
        return word.to_lowercase();
    }
    let letters = word.chars().filter(|c| c.is_alphabetic()).count();
    let shouting = letters > 2 && !word.chars().any(char::is_lowercase);
    if !shouting {
        return word.to_string();
    }
    capitalized(word)
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or_else(String::new, |first| {
        first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect()
    })
}

fn join(words: impl IntoIterator<Item = Option<String>>) -> String {
    words
        .into_iter()
        .flatten()
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn split(street: &str) -> (Option<&str>, String, Option<&str>, Option<&str>) {
    let words = street.split_whitespace().collect::<Vec<_>>();
    let (mut first, mut last) = (0, words.len());
    let (mut prefix, mut kind, mut suffix) = (None, None, None);
    if last - first > 1 && direction(words[last - 1]).is_some() {
        suffix = Some(words[last - 1]);
        last -= 1;
    }
    if last - first > 1 && is_suffix(words[last - 1]) {
        kind = Some(words[last - 1]);
        last -= 1;
    }
    if last - first > 1 && direction(words[first]).is_some() {
        prefix = Some(words[first]);
        first += 1;
    }
    (prefix, words[first..last].join(" "), kind, suffix)
}

fn is_suffix(word: &str) -> bool {
    let key = word.trim_end_matches('.').to_uppercase();
    SUFFIXES
        .iter()
        .any(|(full, short)| *full == key || *short == key)
}

pub(crate) fn display_street(
    name: &str,
    prefix: Option<&str>,
    kind: Option<&str>,
    suffix: Option<&str>,
) -> String {
    UsAddress::of(&Parts {
        name,
        prefix,
        kind,
        suffix,
        ..Parts::default()
    })
    .street(Style::Display)
}

fn direction(word: &str) -> Option<&'static str> {
    Some(
        match direction_of(&word.trim_end_matches('.').to_lowercase())? {
            "north" => "N",
            "south" => "S",
            "east" => "E",
            "west" => "W",
            "northeast" => "NE",
            "northwest" => "NW",
            "southeast" => "SE",
            "southwest" => "SW",
            _ => return None,
        },
    )
}

fn suffix(word: &str) -> String {
    let key = word.trim_end_matches('.').to_uppercase();
    let listed = |key: &str| {
        SUFFIXES
            .iter()
            .find(|(full, short)| *full == key || *short == key)
            .map(|(_, short)| (*short).to_string())
    };
    listed(&key)
        .or_else(|| {
            crate::parse::lattice::other_forms(&key.to_lowercase())
                .iter()
                .find_map(|form| listed(&form.to_uppercase()))
        })
        .unwrap_or(key)
}

pub(crate) fn designator(word: &str) -> String {
    let key = word.trim_end_matches('.').to_uppercase();
    DESIGNATORS
        .iter()
        .find(|(full, short)| *full == key || *short == key)
        .map_or_else(|| key.clone(), |(_, short)| (*short).to_string())
}

const SUFFIXES: &[(&str, &str)] = &[
    ("ALLEY", "ALY"),
    ("ANEX", "ANX"),
    ("ARCADE", "ARC"),
    ("AVENUE", "AVE"),
    ("BAYOU", "BYU"),
    ("BEACH", "BCH"),
    ("BEND", "BND"),
    ("BLUFF", "BLF"),
    ("BLUFFS", "BLFS"),
    ("BOTTOM", "BTM"),
    ("BOULEVARD", "BLVD"),
    ("BRANCH", "BR"),
    ("BRIDGE", "BRG"),
    ("BROOK", "BRK"),
    ("BROOKS", "BRKS"),
    ("BURG", "BG"),
    ("BURGS", "BGS"),
    ("BYPASS", "BYP"),
    ("CAMP", "CP"),
    ("CANYON", "CYN"),
    ("CAPE", "CPE"),
    ("CAUSEWAY", "CSWY"),
    ("CENTER", "CTR"),
    ("CENTERS", "CTRS"),
    ("CIRCLE", "CIR"),
    ("CIRCLES", "CIRS"),
    ("CLIFF", "CLF"),
    ("CLIFFS", "CLFS"),
    ("CLUB", "CLB"),
    ("COMMON", "CMN"),
    ("COMMONS", "CMNS"),
    ("CORNER", "COR"),
    ("CORNERS", "CORS"),
    ("COURSE", "CRSE"),
    ("COURT", "CT"),
    ("COURTS", "CTS"),
    ("COVE", "CV"),
    ("COVES", "CVS"),
    ("CREEK", "CRK"),
    ("CRESCENT", "CRES"),
    ("CREST", "CRST"),
    ("CROSSING", "XING"),
    ("CROSSROAD", "XRD"),
    ("CROSSROADS", "XRDS"),
    ("CURVE", "CURV"),
    ("DALE", "DL"),
    ("DAM", "DM"),
    ("DIVIDE", "DV"),
    ("DRIVE", "DR"),
    ("DRIVES", "DRS"),
    ("ESTATE", "EST"),
    ("ESTATES", "ESTS"),
    ("EXPRESSWAY", "EXPY"),
    ("EXTENSION", "EXT"),
    ("EXTENSIONS", "EXTS"),
    ("FALL", "FALL"),
    ("FALLS", "FLS"),
    ("FERRY", "FRY"),
    ("FIELD", "FLD"),
    ("FIELDS", "FLDS"),
    ("FLAT", "FLT"),
    ("FLATS", "FLTS"),
    ("FORD", "FRD"),
    ("FORDS", "FRDS"),
    ("FOREST", "FRST"),
    ("FORGE", "FRG"),
    ("FORGES", "FRGS"),
    ("FORK", "FRK"),
    ("FORKS", "FRKS"),
    ("FORT", "FT"),
    ("FREEWAY", "FWY"),
    ("GARDEN", "GDN"),
    ("GARDENS", "GDNS"),
    ("GATEWAY", "GTWY"),
    ("GLEN", "GLN"),
    ("GLENS", "GLNS"),
    ("GREEN", "GRN"),
    ("GREENS", "GRNS"),
    ("GROVE", "GRV"),
    ("GROVES", "GRVS"),
    ("HARBOR", "HBR"),
    ("HARBORS", "HBRS"),
    ("HAVEN", "HVN"),
    ("HEIGHTS", "HTS"),
    ("HIGHWAY", "HWY"),
    ("HILL", "HL"),
    ("HILLS", "HLS"),
    ("HOLLOW", "HOLW"),
    ("INLET", "INLT"),
    ("ISLAND", "IS"),
    ("ISLANDS", "ISS"),
    ("ISLE", "ISLE"),
    ("JUNCTION", "JCT"),
    ("JUNCTIONS", "JCTS"),
    ("KEY", "KY"),
    ("KEYS", "KYS"),
    ("KNOLL", "KNL"),
    ("KNOLLS", "KNLS"),
    ("LAKE", "LK"),
    ("LAKES", "LKS"),
    ("LAND", "LAND"),
    ("LANDING", "LNDG"),
    ("LANE", "LN"),
    ("LIGHT", "LGT"),
    ("LIGHTS", "LGTS"),
    ("LOAF", "LF"),
    ("LOCK", "LCK"),
    ("LOCKS", "LCKS"),
    ("LODGE", "LDG"),
    ("LOOP", "LOOP"),
    ("MALL", "MALL"),
    ("MANOR", "MNR"),
    ("MANORS", "MNRS"),
    ("MEADOW", "MDW"),
    ("MEADOWS", "MDWS"),
    ("MEWS", "MEWS"),
    ("MILL", "ML"),
    ("MILLS", "MLS"),
    ("MISSION", "MSN"),
    ("MOTORWAY", "MTWY"),
    ("MOUNT", "MT"),
    ("MOUNTAIN", "MTN"),
    ("MOUNTAINS", "MTNS"),
    ("NECK", "NCK"),
    ("ORCHARD", "ORCH"),
    ("OVAL", "OVAL"),
    ("OVERPASS", "OPAS"),
    ("PARK", "PARK"),
    ("PARKS", "PARK"),
    ("PARKWAY", "PKWY"),
    ("PARKWAYS", "PKWY"),
    ("PASS", "PASS"),
    ("PASSAGE", "PSGE"),
    ("PATH", "PATH"),
    ("PIKE", "PIKE"),
    ("PINE", "PNE"),
    ("PINES", "PNES"),
    ("PLACE", "PL"),
    ("PLAIN", "PLN"),
    ("PLAINS", "PLNS"),
    ("PLAZA", "PLZ"),
    ("POINT", "PT"),
    ("POINTS", "PTS"),
    ("PORT", "PRT"),
    ("PORTS", "PRTS"),
    ("PRAIRIE", "PR"),
    ("RADIAL", "RADL"),
    ("RAMP", "RAMP"),
    ("RANCH", "RNCH"),
    ("RAPID", "RPD"),
    ("RAPIDS", "RPDS"),
    ("REST", "RST"),
    ("RIDGE", "RDG"),
    ("RIDGES", "RDGS"),
    ("RIVER", "RIV"),
    ("ROAD", "RD"),
    ("ROADS", "RDS"),
    ("ROUTE", "RTE"),
    ("ROW", "ROW"),
    ("RUE", "RUE"),
    ("RUN", "RUN"),
    ("SHOAL", "SHL"),
    ("SHOALS", "SHLS"),
    ("SHORE", "SHR"),
    ("SHORES", "SHRS"),
    ("SKYWAY", "SKWY"),
    ("SPRING", "SPG"),
    ("SPRINGS", "SPGS"),
    ("SPUR", "SPUR"),
    ("SPURS", "SPUR"),
    ("SQUARE", "SQ"),
    ("SQUARES", "SQS"),
    ("STATION", "STA"),
    ("STRAVENUE", "STRA"),
    ("STREAM", "STRM"),
    ("STREET", "ST"),
    ("STREETS", "STS"),
    ("SUMMIT", "SMT"),
    ("TERRACE", "TER"),
    ("THROUGHWAY", "TRWY"),
    ("TRACE", "TRCE"),
    ("TRACK", "TRAK"),
    ("TRAFFICWAY", "TRFY"),
    ("TRAIL", "TRL"),
    ("TRAILER", "TRLR"),
    ("TUNNEL", "TUNL"),
    ("TURNPIKE", "TPKE"),
    ("UNDERPASS", "UPAS"),
    ("UNION", "UN"),
    ("UNIONS", "UNS"),
    ("VALLEY", "VLY"),
    ("VALLEYS", "VLYS"),
    ("VIADUCT", "VIA"),
    ("VIEW", "VW"),
    ("VIEWS", "VWS"),
    ("VILLAGE", "VLG"),
    ("VILLAGES", "VLGS"),
    ("VILLE", "VL"),
    ("VISTA", "VIS"),
    ("WALK", "WALK"),
    ("WALKS", "WALK"),
    ("WALL", "WALL"),
    ("WAY", "WAY"),
    ("WAYS", "WAYS"),
    ("WELL", "WL"),
    ("WELLS", "WLS"),
];

const DESIGNATORS: &[(&str, &str)] = &[
    ("APARTMENT", "APT"),
    ("BASEMENT", "BSMT"),
    ("BUILDING", "BLDG"),
    ("DEPARTMENT", "DEPT"),
    ("FLOOR", "FL"),
    ("FRONT", "FRNT"),
    ("HANGAR", "HNGR"),
    ("KEY", "KEY"),
    ("LOBBY", "LBBY"),
    ("LOT", "LOT"),
    ("LOWER", "LOWR"),
    ("OFFICE", "OFC"),
    ("PENTHOUSE", "PH"),
    ("PIER", "PIER"),
    ("REAR", "REAR"),
    ("ROOM", "RM"),
    ("SIDE", "SIDE"),
    ("SLIP", "SLIP"),
    ("SPACE", "SPC"),
    ("STOP", "STOP"),
    ("SUITE", "STE"),
    ("TRAILER", "TRLR"),
    ("UNIT", "UNIT"),
    ("UPPER", "UPPR"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn burlington() -> UsAddress {
        UsAddress::of(&Parts {
            number: "1700",
            name: "District",
            kind: Some("Avenue"),
            unit_type: Some("Suite"),
            unit: Some("300"),
            city: "Burlington",
            state: "MA",
            zip: Some("01803"),
            zip4: Some("5231"),
            ..Parts::default()
        })
    }

    #[test]
    fn the_postal_form_is_written_as_precisely_documents_it() {
        let address = burlington();
        assert_eq!(
            address.street_line(Style::Postal),
            "1700 DISTRICT AVE STE 300"
        );
        assert_eq!(address.street(Style::Postal), "DISTRICT");
        assert_eq!(address.last_line(Style::Postal), "BURLINGTON MA 01803-5231");
        assert_eq!(
            address.location_line(Style::Postal),
            "BURLINGTON, MA  01803-5231"
        );
    }

    #[test]
    fn a_variant_suffix_and_a_capitalized_ordinal_take_the_postal_forms() {
        assert_eq!(
            display_street("BERNARDO", Some("S"), Some("AV"), None),
            "S Bernardo Ave"
        );
        assert_eq!(
            display_street("58TH", Some("SW"), Some("MNR"), None),
            "SW 58th Mnr"
        );
        assert_eq!(display_street("1ST", None, Some("ST"), None), "1st St");
        assert_eq!(
            display_street("Main", None, Some("Street"), None),
            "Main St"
        );
    }

    #[test]
    fn the_display_form_keeps_case_and_writes_a_capitalized_source_in_words() {
        let jacinto = UsAddress::of(&Parts {
            number: "350",
            prefix: Some("S"),
            name: "JORDAN",
            kind: Some("AVE"),
            city: "SAN JACINTO",
            state: "CA",
            zip: Some("92583"),
            ..Parts::default()
        });
        assert_eq!(jacinto.street_line(Style::Display), "350 S Jordan Ave");
        assert_eq!(jacinto.street(Style::Display), "S Jordan Ave");
        assert_eq!(jacinto.last_line(Style::Display), "San Jacinto CA 92583");
        assert_eq!(
            jacinto.location_line(Style::Display),
            "San Jacinto, CA 92583"
        );
        let view = UsAddress::of(&Parts {
            number: "1",
            name: "Global",
            kind: Some("View"),
            suffix: Some("North"),
            city: "Troy",
            state: "ny",
            ..Parts::default()
        });
        assert_eq!(view.street_line(Style::Display), "1 Global Vw N");
        assert_eq!(view.location_line(Style::Postal), "TROY, NY");
    }

    #[test]
    fn a_street_og_left_whole_is_split_by_the_suffix_table() {
        let street = |name| {
            UsAddress::of(&Parts {
                number: "1",
                name,
                city: "Troy",
                state: "NY",
                ..Parts::default()
            })
            .street_line(Style::Postal)
        };
        assert_eq!(street("Global View"), "1 GLOBAL VW");
        assert_eq!(street("North Park Row West"), "1 N PARK ROW W");
        assert_eq!(street("Broadway"), "1 BROADWAY");
        assert_eq!(street("Park"), "1 PARK");
    }

    #[test]
    fn a_suffix_or_designator_the_tables_do_not_name_stays_as_written() {
        assert_eq!(suffix("Strasse"), "STRASSE");
        assert_eq!(suffix("Ave."), "AVE");
        assert_eq!(designator("#"), "#");
        assert_eq!(designator("Ste"), "STE");
    }
}
