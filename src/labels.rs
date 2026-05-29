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

pub fn interpolation_name(components: &InterpolationAddressComponents) -> String {
    components
        .street
        .as_deref()
        .or(components.place.as_deref())
        .unwrap_or("")
        .to_string()
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

pub fn interpolation_record_id(way_id: i64, low_node_id: i64, high_node_id: i64) -> String {
    format!("osm:way:{way_id}:interp:{low_node_id}-{high_node_id}")
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
