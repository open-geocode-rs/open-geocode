//! Applying a requested language to an answer (issue #102).
//!
//! A Pack built with `--lang` holds the `name:<lang>` of an OSM object by its
//! source id, and the vertices of the ways a street address stands on. Every
//! name an answer shows is looked up from the thing it names, never from a
//! global name table: two towns with the same street name get their own
//! translations. Address parts without an object are matched against the
//! answer's own context areas, which do have ids. A name with no entry keeps
//! its local form.
//!
//! Reverse geocoding writes its own label, so the label is rewritten by
//! replacing the translated parts it contains rather than rebuilt.

use std::sync::Arc;

use crate::{
    api::{Address, Common, Context, ContextArea},
    ids,
    record::ObjectType,
};

/// One language shared by every result of a request.
#[derive(Debug, Clone)]
pub struct Language {
    translations: Arc<crate::translations::Translations>,
    tag: String,
}

impl Language {
    pub(crate) fn new(translations: Arc<crate::translations::Translations>, tag: String) -> Self {
        Self { translations, tag }
    }

    pub(crate) fn by_id(&self, object_type: ObjectType, object_id: i64) -> Option<&str> {
        self.translations.by_id(&self.tag, object_type, object_id)
    }

    pub(crate) fn street(&self, name: &str, lon: f64, lat: f64) -> Option<&str> {
        self.translations.street_near(&self.tag, name, lon, lat)
    }

    pub(crate) fn of(&self, id: &str) -> Option<&str> {
        let (kind, number) = ids::parse(id).ok().flatten()?;
        self.by_id(kind, number)
    }
}

/// The context levels an address field can name. A field looks only at its
/// own levels, finest first, so a province that shares a neighbourhood's
/// name never wins `areas`. `Any` is a label's text, which no field owns: it
/// reads the locality first, as `address.locality` does, so a label and the
/// field it repeats agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Place,
    Areas,
    Locality,
    Region,
    Any,
}

/// Every lookup one answer needs, bound to its point and its own id.
/// Pure closures let the translation rules be tested without building a Pack.
pub(crate) struct Lookups<'a, S, A, I> {
    /// Translation of the result's own source id, when it names a thing.
    pub own: Option<&'a str>,
    /// The name `own` translates; the same text elsewhere must not change.
    pub own_name: Option<String>,
    /// Street name of an address, found by proximity to its point.
    pub street: S,
    /// Translation of a name an answer field shows, taken from the context
    /// area of that [`Level`] that carries the name.
    pub area: A,
    pub by_id: I,
}

impl<'a, S, A, I> Lookups<'a, S, A, I>
where
    S: for<'b> Fn(&'b str) -> Option<String>,
    A: for<'b> Fn(&'b str, Level) -> Option<String>,
{
    fn of(&self, name: &str) -> Option<String> {
        self.own
            .filter(|_| self.own_name.as_deref() == Some(name))
            .map(str::to_string)
            .or_else(|| (self.street)(name))
            .or_else(|| (self.area)(name, Level::Any))
    }
}

/// Translate a field, allocating only when the language has another name.
fn field(lookup: impl Fn(&str) -> Option<String>, slot: &mut Option<String>) {
    if let Some(value) = slot.as_deref()
        && let Some(translated) = lookup(value)
        && translated != value
    {
        *slot = Some(translated);
    }
}

fn contexts(by_id: impl Fn(ObjectType, i64) -> Option<String>, context: &mut Context) {
    for area in [
        &mut context.neighbourhood,
        &mut context.sublocality,
        &mut context.locality,
        &mut context.subdistrict,
        &mut context.district,
        &mut context.subregion,
        &mut context.region,
        &mut context.country,
        &mut context.place,
    ] {
        if let Some(area) = area
            && let Some((kind, id)) = ids::parse(&area.id).ok().flatten()
            && let Some(translated) = by_id(kind, id)
        {
            area.name = translated;
        }
    }
}

/// An address states text, not an object id. The answer's own context areas
/// are the authority: a village `Aldaia` with no `name:es` stays as it is,
/// while `València` takes the name of the locality area of that id. Each
/// field looks at its own levels first, so a province named like the
/// neighbourhood does not lend its name to `areas`.
fn address<S, A, I>(lookups: &Lookups<'_, S, A, I>, address: &mut Address)
where
    S: for<'b> Fn(&'b str) -> Option<String>,
    A: for<'b> Fn(&'b str, Level) -> Option<String>,
{
    field(&lookups.street, &mut address.street);
    field(
        |name| (lookups.area)(name, Level::Place),
        &mut address.place,
    );
    for value in &mut address.areas {
        if let Some(translated) = (lookups.area)(value, Level::Areas) {
            value.clone_from(&translated);
        }
    }
    field(
        |name| (lookups.area)(name, Level::Locality),
        &mut address.locality,
    );
    field(
        |name| (lookups.area)(name, Level::Region),
        &mut address.region,
    );
}

/// Names a label may show, in one order, so replacement does not depend on
/// hash iteration. Longer names come first: a name inside another name is
/// otherwise replaced twice.
fn label_names<S, A, I>(
    lookups: &Lookups<'_, S, A, I>,
    common: &Common,
    include_name: bool,
) -> Vec<String>
where
    S: for<'b> Fn(&'b str) -> Option<String>,
    A: for<'b> Fn(&'b str, Level) -> Option<String>,
{
    let mut names: Vec<String> = Vec::new();
    let own = include_name.then_some(common.name.as_str());
    for name in [
        own,
        common.address.street.as_deref(),
        common.address.place.as_deref(),
        common.address.locality.as_deref(),
        common.address.region.as_deref(),
    ]
    .into_iter()
    .flatten()
    .chain(common.address.areas.iter().map(String::as_str))
    .chain(context_names(&common.context).iter().map(String::as_str))
    {
        if lookups.of(name).is_some() && !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }
    names.sort_unstable_by(|left, right| right.len().cmp(&left.len()).then(left.cmp(right)));
    names
}

fn context_names(context: &Context) -> Vec<String> {
    [
        &context.neighbourhood,
        &context.sublocality,
        &context.locality,
        &context.subdistrict,
        &context.district,
        &context.subregion,
        &context.region,
        &context.country,
        &context.place,
    ]
    .into_iter()
    .flatten()
    .map(|area| area.name.clone())
    .collect()
}

/// Replace the names a label shows, keeping the layout its writer chose.
/// Reverse writes the label itself, so there is no record to rebuild it from.
/// One pass over the original label picks the matches, so text a translation
/// inserts is never translated again; `protected` text, such as a POI's own
/// name, is never rewritten.
fn replace<S, A, I>(
    label: &str,
    lookups: &Lookups<'_, S, A, I>,
    names: &[String],
    protected: &[String],
) -> String
where
    S: for<'b> Fn(&'b str) -> Option<String>,
    A: for<'b> Fn(&'b str, Level) -> Option<String>,
{
    let mut protected_mask = vec![false; label.len()];
    for name in protected {
        let mut from = 0;
        while let Some(at) = label[from..].find(name) {
            let start = from + at;
            let end = start + name.len();
            for slot in &mut protected_mask[start..end] {
                *slot = true;
            }
            from = start + name.len().max(1);
        }
    }
    let mut sorted = names.to_vec();
    sorted.sort_unstable_by(|left, right| right.len().cmp(&left.len()).then(left.cmp(right)));
    let mut result = String::new();
    let mut index = 0;
    let bytes = label.as_bytes();
    while index < bytes.len() {
        if protected_mask[index] {
            let ch = label[index..].chars().next().expect("char");
            result.push(ch);
            index += ch.len_utf8();
            continue;
        }
        let mut matched = false;
        for name in &sorted {
            if label[index..].starts_with(name)
                && !protected_mask[index..index + name.len()].iter().any(|b| *b)
                && let Some(translated) = lookups.of(name)
            {
                result.push_str(&translated);
                index += name.len();
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        let ch = label[index..].chars().next().expect("char");
        result.push(ch);
        index += ch.len_utf8();
    }
    result
}

/// Apply a language to one result's address parts, its context and its own
/// name and label. All pure: the lookups are closures, so the work is tested
/// without building a Pack.
pub(crate) fn apply<S, A, I>(lookups: &Lookups<'_, S, A, I>, common: &mut Common)
where
    S: for<'b> Fn(&'b str) -> Option<String>,
    A: for<'b> Fn(&'b str, Level) -> Option<String>,
    I: Fn(ObjectType, i64) -> Option<String>,
{
    let layer = common.layer;
    let before = (layer != crate::record::Layer::Address).then(|| common.clone());
    address(lookups, &mut common.address);
    contexts(&lookups.by_id, &mut common.context);

    // An address's name is its number and its subject; a street, place or
    // area's name is the thing itself. A POI's own name stays as it is.
    if common.address.housenumber.is_some() && layer != crate::record::Layer::Poi {
        common.name = crate::labels::address_name(&to_components(&common.address));
    } else if layer != crate::record::Layer::Poi
        && let Some(translated) = lookups.own
    {
        common.name = translated.to_string();
    }
    if layer == crate::record::Layer::Address {
        common.label = crate::labels::address_label(&to_components(&common.address));
    } else if let Some(before) = &before {
        let own_name = layer != crate::record::Layer::Poi;
        let names = label_names(lookups, before, own_name);
        let protected = if layer == crate::record::Layer::Poi {
            vec![before.name.clone()]
        } else {
            Vec::new()
        };
        common.label = replace(&before.label, lookups, &names, &protected);
    }
}

/// Reverse writes its own label, so its names are replaced in place instead
/// of rebuilding the label from record parts.
pub(crate) fn apply_reverse<S, A, I>(lookups: &Lookups<'_, S, A, I>, common: &mut Common)
where
    S: for<'b> Fn(&'b str) -> Option<String>,
    A: for<'b> Fn(&'b str, Level) -> Option<String>,
    I: Fn(ObjectType, i64) -> Option<String>,
{
    let layer = common.layer;
    let original = common.clone();
    address(lookups, &mut common.address);
    contexts(&lookups.by_id, &mut common.context);
    if common.address.housenumber.is_some() && layer != crate::record::Layer::Poi {
        common.name = crate::labels::address_name(&to_components(&common.address));
    } else if layer != crate::record::Layer::Poi
        && let Some(translated) = lookups.own
    {
        common.name = translated.to_string();
    }
    let names = label_names(lookups, &original, true);
    let protected = if layer == crate::record::Layer::Poi {
        vec![original.name.clone()]
    } else {
        Vec::new()
    };
    common.label = replace(&original.label, lookups, &names, &protected);
}

fn to_components(address: &Address) -> crate::record::AddressComponents {
    crate::record::AddressComponents {
        number: address.housenumber.clone().unwrap_or_default(),
        street: address.street.clone(),
        place: address.place.clone(),
        unit: address.unit.clone(),
        locality: address.locality.clone(),
        areas: address.areas.clone(),
        region: address.region.clone(),
        postcode: address.postcode.clone(),
        country: address.country.clone(),
    }
}

impl Language {
    pub(crate) fn apply(&self, common: &mut Common, lon: f64, lat: f64) {
        let areas = common.context.clone();
        let own = self.of(&common.id);
        let lookups = Lookups {
            own,
            own_name: (!common.address.housenumber.is_some()
                && common.layer != crate::record::Layer::Poi)
                .then(|| common.name.clone()),
            street: |name: &str| self.street(name, lon, lat).map(str::to_string),
            area: |name: &str, level: Level| {
                area_named(&areas, name, level)
                    .and_then(|area| self.of(&area.id).map(str::to_string))
            },
            by_id: |kind, id| self.by_id(kind, id).map(str::to_string),
        };
        apply(&lookups, common);
    }

    pub(crate) fn apply_reverse(&self, common: &mut Common, lon: f64, lat: f64) {
        let areas = common.context.clone();
        let own = self.of(&common.id);
        let lookups = Lookups {
            own,
            own_name: (!common.address.housenumber.is_some()).then(|| common.name.clone()),
            street: |name: &str| self.street(name, lon, lat).map(str::to_string),
            area: |name: &str, level: Level| {
                area_named(&areas, name, level)
                    .and_then(|area| self.of(&area.id).map(str::to_string))
            },
            by_id: |kind, id| self.by_id(kind, id).map(str::to_string),
        };
        apply_reverse(&lookups, common);
    }
}

/// The first context area that carries `name`, looking at the levels `level`
/// names in order, finest first. `Any` is a label's text, which no field
/// owns.
fn area_named<'a>(context: &'a Context, name: &str, level: Level) -> Option<&'a ContextArea> {
    let levels: &[&Option<ContextArea>] = match level {
        Level::Place => &[
            &context.place,
            &context.neighbourhood,
            &context.sublocality,
            &context.locality,
        ],
        Level::Areas => &[
            &context.neighbourhood,
            &context.sublocality,
            &context.subdistrict,
            &context.district,
            &context.locality,
        ],
        Level::Locality => &[
            &context.locality,
            &context.sublocality,
            &context.subdistrict,
            &context.district,
            &context.neighbourhood,
        ],
        Level::Region => &[&context.region, &context.subregion, &context.country],
        Level::Any => &[
            &context.locality,
            &context.neighbourhood,
            &context.sublocality,
            &context.subdistrict,
            &context.district,
            &context.subregion,
            &context.region,
            &context.country,
            &context.place,
        ],
    };
    levels
        .iter()
        .copied()
        .flatten()
        .find(|area| area.name == name)
}

#[cfg(test)]
mod tests {
    use crate::{
        api::{Accuracy, Source},
        labels,
        record::{AddressComponents, Layer, OSM_DATASET, ObjectType},
    };

    use super::*;

    struct Tables {
        by_id: std::collections::HashMap<(ObjectType, i64), String>,
        streets: std::collections::HashMap<String, String>,
    }

    type TestLookups<'a> = Lookups<
        'a,
        Box<dyn for<'b> Fn(&'b str) -> Option<String> + 'a>,
        Box<dyn for<'b> Fn(&'b str, Level) -> Option<String> + 'a>,
        Box<dyn Fn(ObjectType, i64) -> Option<String> + 'a>,
    >;

    impl Tables {
        fn lookups(&self, own: &str, own_name: &str, in_a: bool) -> TestLookups<'_> {
            let translated = ids::parse(own)
                .ok()
                .flatten()
                .and_then(|(kind, id)| self.by_id.get(&(kind, id)))
                .map(String::as_str);
            Lookups {
                own: translated
                    .filter(|_| own_name == "Carrer Major" || own_name.starts_with("26")),
                own_name: Some(own_name.to_string()),
                street: Box::new(move |name: &str| {
                    if in_a {
                        self.streets.get(name).cloned()
                    } else {
                        None
                    }
                }),
                area: Box::new(|_: &str, _: Level| None),
                by_id: Box::new(|kind, id| self.by_id.get(&(kind, id)).cloned()),
            }
        }
    }

    fn components() -> AddressComponents {
        AddressComponents {
            number: "26".to_string(),
            street: Some("Carrer de l'Almirall Cadarso".to_string()),
            place: None,
            unit: None,
            locality: Some("València".to_string()),
            region: Some("Comunitat Valenciana".to_string()),
            postcode: Some("46004".to_string()),
            country: Some("Spain".to_string()),
            areas: Vec::new(),
        }
    }

    fn common() -> Common {
        let components = components();
        Common {
            id: "osm:node:1".to_string(),
            layer: Layer::Address,
            accuracy: Accuracy::Rooftop,
            name: labels::address_name(&components),
            label: labels::address_label(&components),
            category: None,
            road_class: None,
            brand: None,
            address: Address::from(&components),
            context: Context::default(),
            source: Source {
                dataset: OSM_DATASET.to_string(),
            },
        }
    }

    fn empty_tables() -> Tables {
        Tables {
            by_id: Default::default(),
            streets: Default::default(),
        }
    }

    #[test]
    fn a_street_and_number_are_rewritten_in_name_label_and_address() {
        let mut tables = empty_tables();
        tables.streets.insert(
            "Carrer de l'Almirall Cadarso".into(),
            "Calle del Almirante Cadarso".into(),
        );
        let mut result = common();
        apply(&tables.lookups("", "", true), &mut result);
        assert_eq!(result.name, "26 Calle del Almirante Cadarso");
        assert_eq!(
            result.label,
            "26 Calle del Almirante Cadarso, València, Comunitat Valenciana, 46004, Spain"
        );
        assert_eq!(
            result.address.street.as_deref(),
            Some("Calle del Almirante Cadarso")
        );
    }

    #[test]
    fn a_locality_is_translated_only_when_the_result_has_that_area() {
        let mut tables = empty_tables();
        tables
            .by_id
            .insert((ObjectType::Node, 42), "Valencia".into());
        let areas =
            std::collections::HashMap::from([("València".to_string(), "osm:node:42".to_string())]);
        let mut result = common();
        result.context.locality = Some(ContextArea {
            id: "osm:node:42".to_string(),
            name: "València".to_string(),
        });
        let lookups = Lookups {
            own: None,
            own_name: None,
            street: |_: &str| None,
            area: |name: &str, _: Level| {
                areas
                    .get(name)
                    .and_then(|id| ids::parse(id).ok().flatten())
                    .and_then(|(kind, id)| tables.by_id.get(&(kind, id)).cloned())
            },
            by_id: |kind, id| tables.by_id.get(&(kind, id)).cloned(),
        };
        apply(&lookups, &mut result);
        assert_eq!(result.address.locality.as_deref(), Some("Valencia"));
        assert_eq!(
            result.context.locality.as_ref().map(|a| a.name.as_str()),
            Some("Valencia")
        );

        let mut no_area = common();
        let no_area_lookups = Lookups {
            own: None,
            own_name: None,
            street: Box::new(|_: &str| None),
            area: Box::new(|_: &str, _: Level| None),
            by_id: Box::new(|kind, id| tables.by_id.get(&(kind, id)).cloned()),
        };
        apply(&no_area_lookups, &mut no_area);
        assert_eq!(
            no_area.address.locality.as_deref(),
            Some("València"),
            "no matching context area: the village keeps its name"
        );
    }

    #[test]
    fn a_same_named_address_in_another_town_is_not_translated() {
        let mut tables = empty_tables();
        tables
            .streets
            .insert("Carrer Major".into(), "Calle Mayor".into());
        let mut in_a = common();
        in_a.address.street = Some("Carrer Major".into());
        apply(&tables.lookups("", "", true), &mut in_a);
        assert_eq!(in_a.address.street.as_deref(), Some("Calle Mayor"));

        let mut in_b = common();
        in_b.address.street = Some("Carrer Major".into());
        apply(&tables.lookups("", "", false), &mut in_b);
        assert_eq!(in_b.address.street.as_deref(), Some("Carrer Major"));
    }

    #[test]
    fn a_context_area_is_translated_by_its_id() {
        let mut tables = empty_tables();
        tables
            .by_id
            .insert((ObjectType::Node, 42), "Valencia".into());
        let mut result = common();
        result.context.locality = Some(ContextArea {
            id: "osm:node:42".to_string(),
            name: "València".to_string(),
        });
        contexts(
            |kind, id| tables.by_id.get(&(kind, id)).cloned(),
            &mut result.context,
        );
        assert_eq!(
            result.context.locality.as_ref().map(|a| a.name.as_str()),
            Some("Valencia")
        );
    }

    #[test]
    fn a_street_record_is_translated_by_its_own_id() {
        let mut tables = empty_tables();
        tables
            .by_id
            .insert((ObjectType::Way, 3), "Calle Mayor".into());
        let mut result = common();
        result.id = "osm:way:3".into();
        result.layer = Layer::Street;
        result.name = "Carrer Major".into();
        result.label = "Carrer Major, València".into();
        result.address = Address::default();
        apply(
            &tables.lookups("osm:way:3", "Carrer Major", false),
            &mut result,
        );
        assert_eq!(result.name, "Calle Mayor");
        assert_eq!(result.label, "Calle Mayor, València");
    }

    #[test]
    fn a_no_language_path_leaves_the_feature_unchanged() {
        let mut result = common();
        let before = serde_json::to_string(&result).expect("json");
        apply(&empty_tables().lookups("", "", false), &mut result);
        assert_eq!(serde_json::to_string(&result).expect("json"), before);
    }

    #[test]
    fn a_reverse_label_keeps_its_layout() {
        let mut tables = empty_tables();
        tables
            .by_id
            .insert((ObjectType::Node, 4), "Valencia".into());
        let areas =
            std::collections::HashMap::from([("València".to_string(), "osm:node:4".to_string())]);
        let mut result = common();
        result.context.locality = Some(ContextArea {
            id: "osm:node:4".to_string(),
            name: "València".to_string(),
        });
        result.label = "26 Carrer Major, València, 46004, Spain".into();
        let lookups = Lookups {
            own: None,
            own_name: None,
            street: |_: &str| None,
            area: |name: &str, _: Level| {
                areas
                    .get(name)
                    .and_then(|id| ids::parse(id).ok().flatten())
                    .and_then(|(kind, id)| tables.by_id.get(&(kind, id)).cloned())
            },
            by_id: |kind, id| tables.by_id.get(&(kind, id)).cloned(),
        };
        apply_reverse(&lookups, &mut result);
        assert_eq!(result.label, "26 Carrer Major, Valencia, 46004, Spain");
    }

    #[test]
    fn text_inserted_by_one_replacement_is_not_replaced_again() {
        // Carrer del Port -> Calle del Port; locality Port -> Puerto.
        // The result must be "Calle del Port, Puerto", not "Calle del Puerto, Puerto".
        let mut tables = empty_tables();
        tables
            .streets
            .insert("Carrer del Port".into(), "Calle del Port".into());
        let mut result = common();
        result.address.street = Some("Carrer del Port".into());
        result.address.locality = Some("Port".into());
        result.address.region = None;
        result.label = "Carrer del Port, Port, 46001, Spain".into();
        let lookups = Lookups {
            own: None,
            own_name: None,
            street: Box::new(move |name: &str| {
                if name == "Carrer del Port" {
                    Some("Calle del Port".into())
                } else {
                    None
                }
            }),
            area: Box::new(|name: &str, _: Level| {
                if name == "Port" {
                    Some("Puerto".into())
                } else {
                    None
                }
            }),
            by_id: Box::new(|_, _| None),
        };
        apply_reverse(&lookups, &mut result);
        assert_eq!(
            result.label, "Calle del Port, Puerto, 46001, Spain",
            "the street's translated name must not be re-translated by the locality"
        );
    }

    #[test]
    fn a_pois_own_name_is_not_replaced_by_its_address() {
        // A POI named "Museu del Barri" with house number 26: its name must stay.
        let mut result = common();
        result.layer = Layer::Poi;
        result.name = "Museu del Barri".into();
        result.label = "Museu del Barri, Carrer Major, València".into();
        result.address.housenumber = Some("26".into());
        result.address.street = Some("Carrer Major".into());
        let lookups = Lookups {
            own: None,
            own_name: None,
            street: Box::new(|_: &str| None),
            area: Box::new(|_: &str, _: Level| None),
            by_id: Box::new(|_, _| None),
        };
        apply(&lookups, &mut result);
        assert_eq!(
            result.name, "Museu del Barri",
            "a POI keeps its own name even when it has a house number"
        );
    }

    #[test]
    fn a_pois_name_inside_its_label_is_protected_from_replacement() {
        // POI label "València Museum, València" where locality València -> Valencia.
        // The POI name must stay "València Museum" while the locality changes.
        let mut result = common();
        result.layer = Layer::Poi;
        result.name = "València Museum".into();
        result.label = "València Museum, València".into();
        result.address.locality = Some("València".into());
        result.context.locality = Some(ContextArea {
            id: "osm:node:42".to_string(),
            name: "València".to_string(),
        });
        let lookups = Lookups {
            own: None,
            own_name: None,
            street: Box::new(|_: &str| None),
            area: Box::new(|name: &str, _: Level| {
                if name == "València" {
                    Some("Valencia".into())
                } else {
                    None
                }
            }),
            by_id: Box::new(|_, _| None),
        };
        apply(&lookups, &mut result);
        assert_eq!(
            result.label, "València Museum, Valencia",
            "the POI name must not be rewritten by the locality replacement"
        );
    }

    /// Lookups that resolve areas as a request does: through the answer's
    /// own context, by level, to the area's id. No closure decides the answer.
    fn context_lookups<'a>(context: &'a Context, tables: &'a Tables) -> TestLookups<'a> {
        Lookups {
            own: None,
            own_name: None,
            street: Box::new(|_: &str| None),
            area: Box::new(move |name: &str, level: Level| {
                area_named(context, name, level)
                    .and_then(|area| ids::parse(&area.id).ok().flatten())
                    .and_then(|(kind, id)| tables.by_id.get(&(kind, id)).cloned())
            }),
            by_id: Box::new(|kind, id| tables.by_id.get(&(kind, id)).cloned()),
        }
    }

    fn context_area(id: &str, name: &str) -> ContextArea {
        ContextArea {
            id: id.to_string(),
            name: name.to_string(),
        }
    }

    #[test]
    fn a_field_takes_the_translation_of_the_area_at_its_own_level() {
        // One name on three levels, each translated differently. Looking
        // finest first gives `locality` the neighbourhood's name; looking
        // broadest first gives `areas` the province's.
        let mut tables = empty_tables();
        tables
            .by_id
            .insert((ObjectType::Node, 10), "Barrio San Juan".into());
        tables
            .by_id
            .insert((ObjectType::Node, 42), "San Juan".into());
        tables
            .by_id
            .insert((ObjectType::Relation, 7), "Provincia San Juan".into());
        let mut result = common();
        result.context.neighbourhood = Some(context_area("osm:node:10", "Sant Joan"));
        result.context.locality = Some(context_area("osm:node:42", "Sant Joan"));
        result.context.region = Some(context_area("osm:relation:7", "Sant Joan"));
        result.address.locality = Some("Sant Joan".into());
        result.address.areas = vec!["Sant Joan".into()];
        result.address.region = Some("Sant Joan".into());
        let context = result.context.clone();
        apply(&context_lookups(&context, &tables), &mut result);
        assert_eq!(result.address.locality.as_deref(), Some("San Juan"));
        assert_eq!(result.address.areas, ["Barrio San Juan"]);
        assert_eq!(result.address.region.as_deref(), Some("Provincia San Juan"));
    }

    #[test]
    fn a_province_named_like_the_locality_does_not_lend_its_name() {
        let mut tables = empty_tables();
        tables
            .by_id
            .insert((ObjectType::Relation, 7), "Provincia de Valencia".into());
        tables
            .by_id
            .insert((ObjectType::Node, 42), "Valencia".into());
        let mut result = common();
        result.context.region = Some(context_area("osm:relation:7", "València"));
        result.context.locality = Some(context_area("osm:node:42", "València"));
        result.address.locality = Some("València".into());
        let context = result.context.clone();
        apply(&context_lookups(&context, &tables), &mut result);
        assert_eq!(result.address.locality.as_deref(), Some("Valencia"));
    }

    #[test]
    fn a_label_and_the_field_it_repeats_take_the_same_translation() {
        let mut tables = empty_tables();
        tables
            .by_id
            .insert((ObjectType::Node, 10), "Barrio San Juan".into());
        tables
            .by_id
            .insert((ObjectType::Node, 42), "San Juan".into());
        let mut result = common();
        result.context.neighbourhood = Some(context_area("osm:node:10", "Sant Joan"));
        result.context.locality = Some(context_area("osm:node:42", "Sant Joan"));
        result.address.street = None;
        result.address.region = None;
        result.address.locality = Some("Sant Joan".into());
        result.label = "Sant Joan, 46001, Spain".into();
        let context = result.context.clone();
        apply_reverse(&context_lookups(&context, &tables), &mut result);
        assert_eq!(result.address.locality.as_deref(), Some("San Juan"));
        assert_eq!(result.label, "San Juan, 46001, Spain");
    }
}
