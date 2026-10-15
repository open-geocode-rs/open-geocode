//! How libpostal's labels map onto og's eight. The table is the benchmark's
//! definition of a correct label, so it lives here once and is documented in
//! `docs/parse-eval.md` and `fixtures/parse/README.md`.

use crate::parse::types::Label;

/// The label set a raw row or a raw prediction is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Scheme {
    /// libpostal's parser labels, in its training data and its parse output.
    Libpostal,
}

/// What a source label becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mapped {
    Label(Label),
    /// A part boundary: libpostal's `SEP` and `FSEP`.
    Separator,
}

impl Scheme {
    pub fn map(self, label: &str) -> Option<Mapped> {
        match self {
            Scheme::Libpostal => libpostal(label),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Scheme::Libpostal => "libpostal",
        }
    }
}

impl std::fmt::Display for Scheme {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// A predicted label as the gold scheme writes it: libpostal labels a
/// district (`state_district`) as one of its regions, so a district is scored
/// as a region.
pub fn scored(label: Label) -> Label {
    match label {
        Label::District => Label::Region,
        label => label,
    }
}

fn libpostal(label: &str) -> Option<Mapped> {
    Some(Mapped::Label(match label {
        "house_number" => Label::HouseNumber,
        "unit" | "level" | "staircase" | "entrance" | "po_box" => Label::Unit,
        "road" => Label::Street,
        "suburb" | "city_district" | "city" | "island" => Label::Locality,
        "state_district" | "state" | "country_region" => Label::Region,
        "postcode" => Label::Postcode,
        "country" | "world_region" => Label::Country,
        "house" | "category" | "near" => Label::Other,
        "SEP" | "FSEP" => return Some(Mapped::Separator),
        _ => return None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libpostal_labels_map_as_documented() {
        let table = [
            ("house_number", Label::HouseNumber),
            ("unit", Label::Unit),
            ("level", Label::Unit),
            ("staircase", Label::Unit),
            ("entrance", Label::Unit),
            ("po_box", Label::Unit),
            ("road", Label::Street),
            ("suburb", Label::Locality),
            ("city_district", Label::Locality),
            ("city", Label::Locality),
            ("island", Label::Locality),
            ("state_district", Label::Region),
            ("state", Label::Region),
            ("country_region", Label::Region),
            ("postcode", Label::Postcode),
            ("country", Label::Country),
            ("world_region", Label::Country),
            ("house", Label::Other),
            ("category", Label::Other),
            ("near", Label::Other),
        ];
        for (name, expected) in table {
            assert_eq!(
                Scheme::Libpostal.map(name),
                Some(Mapped::Label(expected)),
                "{name}"
            );
        }
        assert_eq!(Scheme::Libpostal.map("SEP"), Some(Mapped::Separator));
        assert_eq!(Scheme::Libpostal.map("FSEP"), Some(Mapped::Separator));
        assert_eq!(Scheme::Libpostal.map("City"), None);
    }
}
