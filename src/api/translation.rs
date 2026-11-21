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

/// Every lookup one answer needs, bound to its point and its own id.
/// Pure closures let the translation rules be tested without building a Pack.
pub(crate) struct Lookups<'a, S, A, I> {
    /// Translation of the result's own source id, when it names a thing.
    pub own: Option<&'a str>,
    /// The name `own` translates; the same text elsewhere must not change.
    pub own_name: Option<String>,
    /// Street name of an address, found by proximity to its point.
    pub street: S,
    /// Name of an answer field found in the answer's context, by area id.
    pub area: A,
    pub by_id: I,
}

impl<'a, S, A, I> Lookups<'a, S, A, I>
where
    S: for<'b> Fn(&'b str) -> Option<String>,
    A: for<'b> Fn(&'b str) -> Option<String>,
{
    fn of(&self, name: &str) -> Option<String> {
        self.own
            .filter(|_| self.own_name.as_deref() == Some(name))
            .map(str::to_string)
            .or_else(|| (self.street)(name))
            .or_else(|| (self.area)(name))
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
/// while `València` takes the name of the locality area of that id.
fn address<S, A, I>(lookups: &Lookups<'_, S, A, I>, address: &mut Address)
where
    S: for<'b> Fn(&'b str) -> Option<String>,
    A: for<'b> Fn(&'b str) -> Option<String>,
{
    field(&lookups.street, &mut address.street);
    field(&lookups.area, &mut address.place);
    for value in &mut address.areas {
        if let Some(translated) = (lookups.area)(value) {
            value.clone_from(&translated);
        }
    }
    field(&lookups.area, &mut address.locality);
    field(&lookups.area, &mut address.region);
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
    A: for<'b> Fn(&'b str) -> Option<String>,
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
fn replace<S, A, I>(label: &str, lookups: &Lookups<'_, S, A, I>, names: &[String]) -> String
where
    S: for<'b> Fn(&'b str) -> Option<String>,
    A: for<'b> Fn(&'b str) -> Option<String>,
{
    let mut replaced = label.to_string();
    for name in names {
        if let Some(translated) = lookups.of(name)
            && translated.as_str() != name
        {
            replaced = replaced.replace(name, &translated);
        }
    }
    replaced
}

/// Apply a language to one result's address parts, its context and its own
/// name and label. All pure: the lookups are closures, so the work is tested
/// without building a Pack.
pub(crate) fn apply<S, A, I>(lookups: &Lookups<'_, S, A, I>, common: &mut Common)
where
    S: for<'b> Fn(&'b str) -> Option<String>,
    A: for<'b> Fn(&'b str) -> Option<String>,
    I: Fn(ObjectType, i64) -> Option<String>,
{
    let layer = common.layer;
    let before = (layer != crate::record::Layer::Address).then(|| common.clone());
    address(lookups, &mut common.address);
    contexts(&lookups.by_id, &mut common.context);

    // An address's name is its number and its subject; a street, place or
    // area's name is the thing itself. A POI's own name stays as it is.
    if common.address.housenumber.is_some() {
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
        common.label = replace(&before.label, lookups, &names);
    }
}

/// Reverse writes its own label, so its names are replaced in place instead
/// of rebuilding the label from record parts.
pub(crate) fn apply_reverse<S, A, I>(lookups: &Lookups<'_, S, A, I>, common: &mut Common)
where
    S: for<'b> Fn(&'b str) -> Option<String>,
    A: for<'b> Fn(&'b str) -> Option<String>,
    I: Fn(ObjectType, i64) -> Option<String>,
{
    let original = common.clone();
    address(lookups, &mut common.address);
    contexts(&lookups.by_id, &mut common.context);
    if common.address.housenumber.is_some() {
        common.name = crate::labels::address_name(&to_components(&common.address));
    } else if let Some(translated) = lookups.own {
        common.name = translated.to_string();
    }
    let names = label_names(lookups, &original, true);
    common.label = replace(&original.label, lookups, &names);
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
            area: |name: &str| {
                area_named(&areas, name).and_then(|area| self.of(&area.id).map(str::to_string))
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
            area: |name: &str| {
                area_named(&areas, name).and_then(|area| self.of(&area.id).map(str::to_string))
            },
            by_id: |kind, id| self.by_id(kind, id).map(str::to_string),
        };
        apply_reverse(&lookups, common);
    }
}

fn area_named<'a>(context: &'a Context, name: &str) -> Option<&'a ContextArea> {
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
        Box<dyn for<'b> Fn(&'b str) -> Option<String> + 'a>,
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
                area: Box::new(|_: &str| None),
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
            area: |name: &str| {
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
            area: Box::new(|_: &str| None),
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
            area: |name: &str| {
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
}
