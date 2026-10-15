//! Address token labels shared by the parser and the parse benchmark.

/// The role a query token plays in an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Label {
    HouseNumber,
    Unit,
    Street,
    Locality,
    /// An area between the locality and the region: a county, a district,
    /// a Kreis, a département.
    District,
    Region,
    Postcode,
    Country,
    Other,
}

impl Label {
    pub const ALL: [Label; 9] = [
        Label::HouseNumber,
        Label::Unit,
        Label::Street,
        Label::Locality,
        Label::District,
        Label::Region,
        Label::Postcode,
        Label::Country,
        Label::Other,
    ];

    /// The snake_case name used in TSV fixtures.
    pub fn name(self) -> &'static str {
        match self {
            Label::HouseNumber => "house_number",
            Label::Unit => "unit",
            Label::Street => "street",
            Label::Locality => "locality",
            Label::District => "district",
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

/// One query token with its label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tagged {
    pub token: String,
    pub label: Label,
}

/// Anything that labels the tokens of a free-text address query.
pub trait Tagger {
    /// Label every whitespace-separated token of the normalized query, in order.
    fn tag(&self, query: &str) -> Vec<Tagged>;
}
