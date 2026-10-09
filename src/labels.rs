use crate::record::{
    AddressComponents, InterpolationAddressComponents, InterpolationRange, OsmObjectType,
};

pub fn address_name(components: &AddressComponents) -> String {
    [
        Some(components.number.as_str()),
        components.street.as_deref().or(components.place.as_deref()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
}

pub fn address_label(components: &AddressComponents) -> String {
    let primary = address_name(components);
    [
        Some(primary.as_str()),
        components.unit.as_deref(),
        components.locality.as_deref(),
        components.region.as_deref(),
        components.postcode.as_deref(),
        components.country.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(", ")
}

/// "Tim Hortons, 123 King Street West, Toronto, M5V 1A1". `locality` is used
/// when the POI's own address states none.
pub fn poi_label(
    name: &str,
    address: Option<&AddressComponents>,
    locality: Option<&str>,
) -> String {
    let primary = address.map(address_name);
    let field = |field: fn(&AddressComponents) -> &Option<String>| {
        address.and_then(|address| field(address).as_deref())
    };
    [
        Some(name),
        primary.as_deref(),
        field(|address| &address.unit),
        field(|address| &address.locality).or(locality),
        field(|address| &address.region),
        field(|address| &address.postcode),
        field(|address| &address.country),
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(", ")
}

pub fn interpolation_name(components: &InterpolationAddressComponents) -> String {
    components
        .street
        .as_deref()
        .or(components.place.as_deref())
        .unwrap_or("")
        .to_string()
}

/// "825 College Street": a house number estimated on an interpolation range.
pub fn estimated_address_name(
    number: u32,
    components: &InterpolationAddressComponents,
) -> Option<String> {
    components
        .street
        .as_deref()
        .or(components.place.as_deref())
        .map(|street_or_place| format!("{number} {street_or_place}"))
}

/// The label of an estimated address, laid out like an address label.
pub fn estimated_address_label(
    number: u32,
    components: &InterpolationAddressComponents,
) -> Option<String> {
    let primary = estimated_address_name(number, components)?;
    Some(
        [
            Some(primary.as_str()),
            components.locality.as_deref(),
            components.region.as_deref(),
            components.postcode.as_deref(),
            components.country.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(", "),
    )
}

pub fn interpolation_label(
    name: &str,
    range: &InterpolationRange,
    components: &InterpolationAddressComponents,
) -> String {
    let primary = format!(
        "{name} {start}-{end} {kind}",
        start = range.start,
        end = range.end,
        kind = range.kind
    );
    [
        Some(primary),
        components.locality.clone(),
        components.region.clone(),
        components.postcode.clone(),
        components.country.clone(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ")
}

pub fn osm_record_id(object_type: OsmObjectType, object_id: i64) -> String {
    format!("osm:{}:{object_id}", osm_object_type_name(object_type))
}

pub fn derived_postcode_id(postcode: &str) -> String {
    format!("derived:osm:postcode:{}", url_safe_id_component(postcode))
}

pub fn derived_country_id(code: &str) -> String {
    format!("derived:country:{code}")
}

/// A house number placed between two stated numbers, by the ids of their
/// records.
pub fn estimated_address_id(number: u32, low_id: &str, high_id: &str) -> String {
    format!("derived:estimate:{number}:{low_id}:{high_id}")
}

pub fn interpolation_record_id(way_id: i64, low_node_id: i64, high_node_id: i64) -> String {
    format!("osm:way:{way_id}:interp:{low_node_id}-{high_node_id}")
}

pub(crate) fn place_record_id(
    object_type: OsmObjectType,
    object_id: i64,
    place_type: &str,
) -> String {
    match place_type.strip_prefix("derived_country:") {
        Some(code) => derived_country_id(code),
        None => osm_record_id(object_type, object_id),
    }
}

pub fn osm_object_type_name(object_type: OsmObjectType) -> &'static str {
    match object_type {
        OsmObjectType::Node => "node",
        OsmObjectType::Way => "way",
        OsmObjectType::Relation => "relation",
    }
}

fn url_safe_id_component(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect::<Vec<_>>(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composes_poi_labels_from_name_address_and_locality() {
        let address = AddressComponents {
            number: "123".to_string(),
            street: Some("King Street West".to_string()),
            place: None,
            unit: None,
            locality: None,
            region: None,
            postcode: Some("M5V 1A1".to_string()),
            country: None,
        };
        assert_eq!(
            poi_label("Tim Hortons", Some(&address), Some("Toronto")),
            "Tim Hortons, 123 King Street West, Toronto, M5V 1A1"
        );
        let stated = AddressComponents {
            locality: Some("North York".to_string()),
            ..address
        };
        assert_eq!(
            poi_label("Tim Hortons", Some(&stated), Some("Toronto")),
            "Tim Hortons, 123 King Street West, North York, M5V 1A1",
            "the address's own locality wins"
        );
        assert_eq!(
            poi_label("Riverdale Farm", None, Some("Toronto")),
            "Riverdale Farm, Toronto"
        );
        assert_eq!(poi_label("Riverdale Farm", None, None), "Riverdale Farm");
    }
}
