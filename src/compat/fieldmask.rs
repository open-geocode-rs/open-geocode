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

/// One level of the mask. A field with no children keeps its whole value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Node(BTreeMap<String, Node>);

impl FieldMask {
    /// Reads a mask. `None` when it names no field, which is no mask.
    pub(crate) fn parse(mask: &str) -> Option<Self> {
        let mut root = Node::default();
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
            let mut node = &mut root;
            for part in path.split('.').filter(|part| !part.is_empty()) {
                if part == "*" {
                    break;
                }
                node = node.0.entry(camel_case(part)).or_default();
            }
        }
        any.then_some(Self::Paths(root))
    }

    /// Whether the mask keeps the field at `path`, or a field inside it.
    /// No mask, or `*`, keeps every field.
    pub(crate) fn asks(mask: Option<&Self>, path: &[&str]) -> bool {
        let Some(Self::Paths(mut node)) = mask.cloned() else {
            return true;
        };
        for part in path {
            if node.0.is_empty() {
                return true;
            }
            match node.0.remove(*part) {
                Some(child) => node = child,
                None => return false,
            }
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
        if self.0.is_empty() {
            return;
        }
        match value {
            Value::Object(object) => {
                object.retain(|key, _| self.0.contains_key(key));
                for (key, child) in object.iter_mut() {
                    if let Some(node) = self.0.get(key) {
                        node.apply(child);
                    }
                }
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
