//! Response field masks: a comma-separated list of dotted paths naming the
//! fields to return, such as `places.displayName,places.id`, or `*` for all.

use std::collections::BTreeMap;

use serde_json::Value;

/// The fields a client asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FieldMask {
    All,
    Paths(Node),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Node {
    All,
    Fields(BTreeMap<String, Node>),
}

impl FieldMask {
    /// Reads a mask. `None` when it names no field, which is no mask.
    pub(crate) fn parse(mask: &str) -> Option<Self> {
        let mut root = Node::Fields(BTreeMap::new());
        let mut any = false;
        for path in mask
            .split(',')
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            if path == "*" {
                return Some(Self::All);
            }
            any = true;
            let mut parts = path
                .split('.')
                .filter(|part| !part.is_empty())
                .take_while(|part| *part != "*")
                .peekable();
            if parts.peek().is_none() {
                continue;
            }
            let mut node = &mut root;
            for part in parts {
                let Node::Fields(fields) = node else {
                    break;
                };
                node = fields
                    .entry(camel_case(part))
                    .or_insert_with(|| Node::Fields(BTreeMap::new()));
            }
            *node = Node::All;
        }
        match root {
            Node::Fields(fields) if fields.is_empty() => any.then_some(Self::Paths(Node::All)),
            _ => Some(Self::Paths(root)),
        }
    }

    /// Whether the mask keeps the field at `path`, or a field inside it.
    /// No mask, or `*`, keeps every field.
    pub(crate) fn asks(mask: Option<&Self>, path: &[&str]) -> bool {
        let Some(Self::Paths(node)) = mask else {
            return true;
        };
        let mut node = node;
        for part in path {
            let Node::Fields(fields) = node else {
                return true;
            };
            let Some(child) = fields.get(*part) else {
                return false;
            };
            node = child;
        }
        true
    }

    /// Drops every field the mask leaves out. Arrays are masked element by
    /// element, so `places.id` keeps the `id` of every place.
    pub(crate) fn apply(&self, value: &mut Value) {
        if let Self::Paths(node) = self {
            node.apply(value);
        }
    }
}

impl Node {
    fn apply(&self, value: &mut Value) {
        let Self::Fields(fields) = self else {
            return;
        };
        match value {
            Value::Object(object) => {
                object.retain(|key, child| {
                    let Some(node) = fields.get(key) else {
                        return false;
                    };
                    node.apply(child);
                    true
                });
            }
            Value::Array(items) => items.iter_mut().for_each(|item| self.apply(item)),
            _ => {}
        }
    }
}

/// Mask paths may be written in either case (`display_name` or
/// `displayName`); responses are lowerCamelCase.
fn camel_case(part: &str) -> String {
    let mut out = String::with_capacity(part.len());
    let mut upper = false;
    for character in part.chars() {
        if character == '_' {
            upper = true;
        } else if upper {
            out.extend(character.to_uppercase());
            upper = false;
        } else {
            out.push(character);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn masked(mask: &str, mut value: Value) -> Value {
        FieldMask::parse(mask).expect("a mask").apply(&mut value);
        value
    }

    #[test]
    fn keeps_only_the_named_paths_through_arrays() {
        let value = json!({
            "places": [
                {"id": "a", "displayName": {"text": "A", "languageCode": "en"}, "types": ["cafe"]},
                {"id": "b", "displayName": {"text": "B", "languageCode": "en"}, "types": []}
            ],
            "nextPageToken": "x"
        });
        assert_eq!(
            masked("places.id,places.displayName.text", value),
            json!({"places": [
                {"id": "a", "displayName": {"text": "A"}},
                {"id": "b", "displayName": {"text": "B"}}
            ]})
        );
    }

    #[test]
    fn a_field_without_children_keeps_its_whole_value() {
        let value = json!({"displayName": {"text": "A", "languageCode": "en"}, "id": "a"});
        assert_eq!(
            masked("displayName", value),
            json!({"displayName": {"text": "A", "languageCode": "en"}})
        );
    }

    #[test]
    fn a_parent_path_keeps_all_children_in_either_order() {
        let value = json!({"place": {"name": {"text": "A", "languageCode": "en"}, "id": "a"}});
        let expected = json!({"place": {"name": {"text": "A", "languageCode": "en"}}});
        for path in [
            "place.name,place.name.text",
            "place.name.text,place.name",
            "place.name.text,place.name.*",
        ] {
            let mask = FieldMask::parse(path).unwrap();
            assert!(FieldMask::asks(
                Some(&mask),
                &["place", "name", "languageCode"]
            ));
            assert!(!FieldMask::asks(Some(&mask), &["place", "id"]));
            assert_eq!(masked(path, value.clone()), expected);
        }
    }

    #[test]
    fn star_keeps_everything_and_snake_case_reads_as_camel_case() {
        let value = json!({"displayName": {"text": "A"}, "id": "a"});
        assert_eq!(masked("*", value.clone()), value);
        assert_eq!(
            masked("places.*", json!({"places": [1]})),
            json!({"places": [1]})
        );
        assert_eq!(
            masked("display_name", value),
            json!({"displayName": {"text": "A"}})
        );
    }

    #[test]
    fn a_mask_naming_nothing_is_no_mask() {
        assert_eq!(FieldMask::parse(""), None);
        assert_eq!(FieldMask::parse(" , "), None);
    }
}
