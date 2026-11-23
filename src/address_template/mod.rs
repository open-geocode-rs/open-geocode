//! How each country writes an address, which is its fields, their order and
//! their lines. The templates are those of OpenCageData's address-formatting
//! (<https://github.com/OpenCageData/address-formatting>, MIT, see `NOTICE`),
//! vendored as `address_template/templates.tsv`.
//!
//! A template is text with `\n` between its lines, `{{{field}}}` where a
//! field goes, and `{{#first}} a || b {{/first}}` where the first
//! alternative with a value goes. Each reader names the fields in its own
//! terms through [`table`] and reads a template through [`write`], so the
//! format and the choice between alternatives have one definition.

pub(crate) mod lines;

use std::collections::BTreeMap;

/// `CC<TAB>template` rows, `ZZ` the default.
const TEMPLATES: &str = include_str!("templates.tsv");

/// A parsed address template, its fields named as `F`.
#[derive(Debug, Clone)]
pub(crate) enum Node<F> {
    Text(String),
    Field(F),
    /// The alternatives of `{{#first}} a || b {{/first}}`, of which the
    /// first with a value is written.
    First(Vec<Vec<Node<F>>>),
}

/// What a template writes, in order.
#[derive(Debug)]
pub(crate) enum Piece<'a, F> {
    Text(&'a str),
    /// A field, whether or not it has a value.
    Field(&'a F),
    /// A choice none of whose alternatives has a value.
    Unfilled,
}

fn parse<F>(template: &str, field: &impl Fn(&str) -> F) -> Vec<Node<F>> {
    let mut nodes = Vec::new();
    let mut rest = template;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("{{#first}}") {
            let (inner, tail) = after.split_once("{{/first}}").unwrap_or((after, ""));
            nodes.push(Node::First(
                inner
                    .split("||")
                    .map(|alternative| parse(alternative.trim(), field))
                    .collect(),
            ));
            rest = tail;
        } else if let Some(after) = rest.strip_prefix("{{{") {
            let (name, tail) = after.split_once("}}}").unwrap_or((after, ""));
            nodes.push(Node::Field(field(name.trim())));
            rest = tail;
        } else {
            let first = rest.chars().next().map_or(1, char::len_utf8);
            let end = rest[first..]
                .find("{{")
                .map_or(rest.len(), |end| end + first);
            nodes.push(Node::Text(rest[..end].to_string()));
            rest = &rest[end..];
        }
    }
    nodes
}

/// Every country's template by its ISO 3166-1 alpha-2 code, each field
/// named by `field` from its name in the template.
pub(crate) fn table<F>(field: impl Fn(&str) -> F) -> BTreeMap<String, Vec<Node<F>>> {
    TEMPLATES
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(code, template)| {
            (
                code.to_string(),
                parse(&template.replace("\\n", "\n"), &field),
            )
        })
        .collect()
}

fn has_value<F>(nodes: &[Node<F>], has: &impl Fn(&F) -> bool) -> bool {
    nodes.iter().any(|node| match node {
        Node::Field(field) => has(field),
        Node::First(alternatives) => alternatives.iter().any(|nodes| has_value(nodes, has)),
        Node::Text(_) => false,
    })
}

/// Hands `out` what `nodes` write, each choice settled on its first
/// alternative with a field `has` a value for. `has` is asked when the
/// choice is reached, after `out` got everything before it.
pub(crate) fn write<'a, F>(
    nodes: &'a [Node<F>],
    has: &impl Fn(&F) -> bool,
    out: &mut impl FnMut(Piece<'a, F>),
) {
    for node in nodes {
        match node {
            Node::Text(text) => out(Piece::Text(text)),
            Node::Field(field) => out(Piece::Field(field)),
            Node::First(alternatives) => {
                match alternatives
                    .iter()
                    .find(|alternative| has_value(alternative, has))
                {
                    Some(alternative) => write(alternative, has, out),
                    None => out(Piece::Unfilled),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(template: &str, with: &[&str]) -> String {
        let nodes = parse(&template.replace("\\n", "\n"), &str::to_string);
        let mut text = String::new();
        write(
            &nodes,
            &|field| with.contains(&field.as_str()),
            &mut |piece| match piece {
                Piece::Text(literal) => text.push_str(literal),
                Piece::Field(field) => text.push_str(&format!("<{field}>")),
                Piece::Unfilled => text.push('?'),
            },
        );
        text
    }

    #[test]
    fn a_choice_writes_its_first_alternative_with_a_value() {
        let template = "{{{road}}}, {{{house_number}}}\\n{{#first}} {{{village}}} {{{postal_city}}} || {{{town}}}, x || {{{city}}} {{/first}} - {{{postcode}}}";
        assert_eq!(
            written(template, &["postal_city", "town"]),
            "<road>, <house_number>\n<village> <postal_city> - <postcode>"
        );
        assert_eq!(
            written(template, &["town", "city"]),
            "<road>, <house_number>\n<town>, x - <postcode>"
        );
        assert_eq!(
            written(template, &["city"]),
            "<road>, <house_number>\n<city> - <postcode>"
        );
        assert_eq!(
            written(template, &["road"]),
            "<road>, <house_number>\n? - <postcode>"
        );
    }

    #[test]
    fn a_tag_left_open_ends_the_template_without_a_panic() {
        assert_eq!(written("{{{road}}} {{{house", &[]), "<road> <house>");
        assert_eq!(
            written("a {{#first}} {{{city}}} || {{{town", &["town"]),
            "a <town>"
        );
        assert_eq!(written("", &[]), "");
    }

    #[test]
    fn every_country_has_a_template_and_the_default_one() {
        let table = table(str::to_string);
        assert_eq!(table.len(), 252);
        assert!(table.contains_key("ZZ"));
        assert!(table.keys().all(|code| code.len() == 2));
    }
}
