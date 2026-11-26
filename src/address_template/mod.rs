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

use std::{collections::BTreeMap, sync::OnceLock};

const TEMPLATES: &str = include_str!("templates.tsv");

#[derive(Debug, Clone)]
pub(crate) enum Node<F> {
    Text(String),
    Field(F),
    First(Vec<Vec<Node<F>>>),
}

#[derive(Debug)]
pub(crate) enum Piece<'a, F> {
    Text(&'a str),
    Field(&'a F),
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
        assert!(table.contains_key("ZZ"));
        assert!(table.keys().all(|code| code.len() == 2));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateForm {
    Code,
    Name,
    Unwritten,
}

pub(crate) fn state_form(country: &str) -> StateForm {
    static FORMS: OnceLock<BTreeMap<String, StateForm>> = OnceLock::new();
    let forms = FORMS.get_or_init(|| {
        table(str::to_string)
            .into_iter()
            .map(|(code, nodes)| (code, form_of(&nodes)))
            .collect()
    });
    forms
        .get(&country.to_ascii_uppercase())
        .or_else(|| forms.get("ZZ"))
        .copied()
        .unwrap_or(StateForm::Unwritten)
}

fn form_of(nodes: &[Node<String>]) -> StateForm {
    let state = |field: &str| matches!(field, "state" | "state_code" | "state_district");
    for node in nodes {
        match node {
            Node::Field(field) if field == "state_code" => return StateForm::Code,
            Node::Field(field) if field == "state" => return StateForm::Name,
            Node::First(alternatives)
                if alternatives.iter().flatten().all(|node| match node {
                    Node::Field(field) => state(field),
                    _ => true,
                }) =>
            {
                let first = alternatives.iter().flatten().find_map(|node| match node {
                    Node::Field(field) => Some(field.as_str()),
                    _ => None,
                });
                match first {
                    Some("state_code") => return StateForm::Code,
                    Some("state") => return StateForm::Name,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    StateForm::Unwritten
}

#[cfg(test)]
mod state_tests {
    use super::*;

    #[test]
    fn a_country_writes_its_state_as_its_addresses_do() {
        for (country, form) in [
            ("US", StateForm::Code),
            ("CA", StateForm::Code),
            ("AU", StateForm::Code),
            ("IN", StateForm::Name),
            ("JP", StateForm::Name),
            ("GB", StateForm::Unwritten),
            ("FR", StateForm::Unwritten),
            ("DE", StateForm::Unwritten),
        ] {
            assert_eq!(state_form(country), form, "{country}");
        }
    }
}
