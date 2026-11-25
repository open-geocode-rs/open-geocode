use serde::{Deserialize, Deserializer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Param {
    Missing,
    Empty,
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
}
