#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Label {
    HouseNumber,
    Unit,
    Street,
    Locality,
    Region,
    Postcode,
    Country,
    Other,
}

impl Label {
    pub const ALL: [Label; 8] = [
        Label::HouseNumber,
        Label::Unit,
        Label::Street,
        Label::Locality,
        Label::Region,
        Label::Postcode,
        Label::Country,
        Label::Other,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Label::HouseNumber => "house_number",
            Label::Unit => "unit",
            Label::Street => "street",
            Label::Locality => "locality",
            Label::Region => "region",
            Label::Postcode => "postcode",
            Label::Country => "country",
            Label::Other => "other",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|label| label.name() == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tagged {
    pub token: String,
    pub label: Label,
}

pub trait Tagger {
    fn tag(&self, query: &str) -> Vec<Tagged>;
}
