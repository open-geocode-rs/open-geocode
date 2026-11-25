use crate::parse::types::Label;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Scheme {
    /// libpostal's parser labels, in its training data and its parse output.
    Libpostal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mapped {
    Label(Label),
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
