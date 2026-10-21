//! End-to-end checks of acceptance before the cut and of dropping query
//! words by label, on Packs written here.

use super::{
    tests::{address_record, temp_pack_path},
    *,
};
use crate::{pack::Writer, record::point_geometry};

fn geocode_with(
    searcher: &TextSearcher,
    address: &str,
    region: &str,
    limit: usize,
) -> Option<String> {
    searcher
        .geocode_address(AddressGeocodeOptions {
            address: address.to_string(),
            locality: None,
            region: Some(region.to_string()),
            postcode: None,
            limit,
            layer: None,
            countries: Vec::new(),
        })
        .expect("geocode")
        .map(|hit| hit.hit.record.id)
}

#[test]
fn an_acceptable_hit_ranked_past_the_limit_still_returns() {
    let temp_dir = temp_pack_path("smoke-accept-before-cut");
    let _ = std::fs::remove_dir_all(&temp_dir);
    let mut writer = Writer::create(&temp_dir).expect("writer");
    // Twelve addresses in a town named Ontario tie on the region a row
    // names, and sort ahead of the one address in it on their ids, but lie
    // in no region of the name.
    for id in 1..=12 {
        let mut record = address_record(
            &format!("osm:node:{id}"),
            "",
            "10",
            "King Street",
            Some("Ontario"),
            None,
        );
        record.address.region = Some("Quebec".to_string());
        record.geometry = point_geometry(-90.0 - id as f64, 43.0);
        writer.write(&record.into(), None).expect("decoy");
    }
    let mut right = address_record(
        "osm:node:99",
        "",
        "10",
        "King Street",
        Some("Kingston"),
        None,
    );
    right.address.region = Some("Ontario".to_string());
    right.geometry = point_geometry(-76.0, 43.0);
    writer.write(&right.into(), None).expect("address");
    writer.finish().expect("finish");
    let searcher = TextSearcher::open(&temp_dir).expect("searcher");

    for limit in [1, 10] {
        assert_eq!(
            geocode_with(&searcher, "10 King Street", "Ontario", limit).as_deref(),
            Some("osm:node:99"),
            "limit {limit}"
        );
    }
    let _ = std::fs::remove_dir_all(temp_dir);
}
