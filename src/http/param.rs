//! A text parameter of a query string, as the client sent it.

use serde::{Deserialize, Deserializer};

/// Not sent, sent empty, or sent with a value.
///
/// A form that always sends every key sends `locality=` for a field left
/// blank, so empty and missing are different answers to "did the caller give
/// this?". They are told apart once, here, and every handler matches all
/// three, saying what each means for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Param {
    Missing,
    /// Sent with nothing but whitespace. [`super::bounds::validate_query`]
    /// collapses whitespace, so a blank value reads as no words at all.
    Empty,
    /// Sent with at least one non-whitespace character, kept as sent.
    Value(String),
}

impl Param {
    pub(crate) fn is_sent(&self) -> bool {
        match self {
            Self::Missing => false,
            Self::Empty | Self::Value(_) => true,
        }
    }
}

impl<'de> Deserialize<'de> for Param {
    // Read through `Option`: serde gives an absent struct field to it as
    // `None`, so a field of this type is `Missing` without `#[serde(default)]`.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<String>::deserialize(deserializer)? {
            None => Self::Missing,
            Some(value) if value.trim().is_empty() => Self::Empty,
            Some(value) => Self::Value(value),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct Params {
        q: Param,
    }

    fn read(query: &str) -> Param {
        serde_urlencoded::from_str::<Params>(query)
            .expect("parses")
            .q
    }

    #[test]
    fn missing_empty_and_value_are_told_apart() {
        assert_eq!(read(""), Param::Missing);
        assert_eq!(read("other=1"), Param::Missing);
        assert_eq!(read("q="), Param::Empty);
        assert_eq!(read("q"), Param::Empty);
        assert_eq!(read("q=%20%09%0A"), Param::Empty);
        assert_eq!(read("q=+"), Param::Empty);
        assert_eq!(read("q=%20cafe%20"), Param::Value(" cafe ".to_string()));
    }

    #[test]
    fn a_repeated_parameter_does_not_parse() {
        assert!(serde_urlencoded::from_str::<Params>("q=a&q=b").is_err());
    }
}
