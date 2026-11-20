//! Stable id to record block.
//!
//! Only OSM places, areas and streets: OpenAddresses row ids renumber every
//! release, so a saved one would open another house. A street answers to any
//! of its ways' ids: a tile names the way a tap lands on.
//!
//! Stores no id. A minimal perfect hash (FMPHGO, `ph` crate, ~2-3 bits an id)
//! maps each id to a slot holding its first block number. The hash sends
//! unknown ids to some slot too, so a lookup checks the record's own id.
//! One object can be records in several blocks (a way that is a POI and a
//! district): its other blocks are listed apart, by key, few as they are.
//!
//! ```text
//! places/ids   width u8 (bytes of a block number) | count u64 | count block
//!              numbers, `width` bytes each, little-endian | more u64 | more
//!              (key, block number) pairs, by key, the key 8 bytes and the
//!              block `width` | the hash, as `ph` writes it
//! ```

use std::sync::Arc;

use ph::fmph::GOFunction;
use rayon::prelude::*;

use crate::{
    container::{Bytes, Container},
    error::{Error, Result},
    pack::{self, RecordId},
    record::ObjectType,
    records,
};

pub const SECTION: &str = "places/ids";
const VERSION: u32 = 3;

// Little-endian: a Pack hashes alike on every machine.
fn key(object_type: ObjectType, object_id: i64) -> Option<[u8; 8]> {
    let kind: u64 = match object_type {
        ObjectType::Node => 0,
        ObjectType::Way => 1,
        ObjectType::Relation => 2,
        ObjectType::Row => return None,
    };
    let number = u64::try_from(object_id)
        .ok()
        .filter(|number| number >> 62 == 0)?;
    Some((kind << 62 | number).to_le_bytes())
}

/// `Ok(None)`: a well-formed id the catalogue never holds (imported row,
/// derived postcode).
pub fn parse(id: &str) -> Result<Option<(ObjectType, i64)>> {
    let malformed =
        || Error::BadOptions(format!("{id:?} is no record id, such as osm:node:123").into());
    let mut parts = id.trim().splitn(3, ':');
    let (Some(dataset), Some(kind), Some(number)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(malformed());
    };
    if dataset != crate::record::OSM_DATASET {
        return Ok(None);
    }
    let object_type = match kind {
        "node" => ObjectType::Node,
        "way" => ObjectType::Way,
        "relation" => ObjectType::Relation,
        _ => return Err(malformed()),
    };
    let number = number
        .parse::<i64>()
        .ok()
        .filter(|number| *number > 0)
        .ok_or_else(malformed)?;
    Ok(Some((object_type, number)))
}

pub fn add_to(sealed: &mut pack::SealedPack) -> Result<()> {
    let bytes = {
        let pack = pack::Reader::open(sealed.path())?;
        build(pack.records())?
    };
    sealed.add_section(SECTION, VERSION, &bytes)
}

fn entries(records: &records::Reader) -> Result<Vec<([u8; 8], bool, u64)>> {
    let per_block = (0..records.block_count())
        .into_par_iter()
        .map(|block| {
            Ok(records
                .block_ids(block)?
                .into_iter()
                .filter_map(|id| Some((key(id.object_type, id.object_id)?, id.street, block)))
                .collect::<Vec<_>>())
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(per_block.into_iter().flatten().collect())
}

fn build(records: &records::Reader) -> Result<Vec<u8>> {
    let mut places = entries(records)?;
    // One object can be two records (a node that is a town and a POI, a way
    // that is a market and a road), in one block or in two. A place's block
    // comes before a street's, and every block holding the object is kept.
    places.sort_unstable_by_key(|(key, street, block)| (*key, *street, *block));
    let mut more = Vec::new();
    places.dedup_by(|(key, _, block), (held_key, _, held_block)| {
        let same = key == held_key;
        if same && block != held_block {
            more.push((*key, *block));
        }
        same
    });
    more.sort_unstable();
    more.dedup();
    let keys = places.iter().map(|(key, ..)| *key).collect::<Vec<_>>();
    let function = GOFunction::from_slice(&keys);
    drop(keys);
    let width = (u64::BITS - records.block_count().leading_zeros())
        .div_ceil(8)
        .max(1) as usize;
    let mut values = vec![0u8; places.len() * width];
    for (key, _, block) in &places {
        let slot = function
            .get(key)
            .ok_or_else(|| Error::Inconsistent("a catalogued id has no slot".into()))?
            as usize;
        values[slot * width..(slot + 1) * width].copy_from_slice(&block.to_le_bytes()[..width]);
    }
    let mut bytes =
        Vec::with_capacity(17 + values.len() + more.len() * (8 + width) + function.write_bytes());
    bytes.push(width as u8);
    bytes.extend_from_slice(&(places.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&values);
    bytes.extend_from_slice(&(more.len() as u64).to_le_bytes());
    for (key, block) in &more {
        bytes.extend_from_slice(key);
        bytes.extend_from_slice(&block.to_le_bytes()[..width]);
    }
    function.write(&mut bytes)?;
    Ok(bytes)
}

pub struct Catalogue {
    function: GOFunction,
    width: usize,
    count: usize,
    values: Bytes,
    /// The other blocks of an object in several, `(key, block)` by key.
    more: Bytes,
}

impl std::fmt::Debug for Catalogue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Catalogue")
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
}

impl Catalogue {
    pub fn open(container: &Container) -> Result<Option<Arc<Self>>> {
        if !container.sections().contains_key(SECTION) {
            return Ok(None);
        }
        let bytes = container.section(SECTION, VERSION)?;
        let truncated = || Error::Decode("the id catalogue is truncated");
        let width = usize::from(*bytes.first().ok_or_else(truncated)?);
        let count = crate::util::codec::read_u64_le(&bytes, 1).ok_or_else(truncated)?;
        let count = usize::try_from(count).map_err(|_| truncated())?;
        let values_end = count
            .checked_mul(width)
            .and_then(|len| len.checked_add(9))
            .filter(|end| *end <= bytes.len() && (1..=8).contains(&width))
            .ok_or_else(truncated)?;
        let more_count = crate::util::codec::read_u64_le(&bytes, values_end)
            .and_then(|more| usize::try_from(more).ok())
            .ok_or_else(truncated)?;
        let more_end = more_count
            .checked_mul(8 + width)
            .and_then(|len| len.checked_add(values_end + 8))
            .filter(|end| *end <= bytes.len())
            .ok_or_else(truncated)?;
        let mut hash = &bytes[more_end..];
        let function = GOFunction::read(&mut hash)?;
        Ok(Some(Arc::new(Self {
            function,
            width,
            count,
            values: bytes.slice(9..values_end),
            more: bytes.slice(values_end + 8..more_end),
        })))
    }

    pub fn find(
        &self,
        records: &records::Reader,
        object_type: ObjectType,
        object_id: i64,
    ) -> Result<Vec<RecordId>> {
        let Some(key) = key(object_type, object_id) else {
            return Ok(Vec::new());
        };
        let Some(slot) = self.function.get(&key).map(|slot| slot as usize) else {
            return Ok(Vec::new());
        };
        if slot >= self.count {
            return Ok(Vec::new());
        }
        let block_at = |bytes: &[u8]| {
            let mut block = [0u8; 8];
            block[..self.width].copy_from_slice(&bytes[..self.width]);
            u64::from_le_bytes(block)
        };
        let mut blocks = vec![block_at(&self.values[slot * self.width..])];
        let pair = 8 + self.width;
        let pairs = self.more.len() / pair;
        let key_at = |at: usize| &self.more[at * pair..at * pair + 8];
        // The first pair of the key, by binary search.
        let (mut at, mut end) = (0, pairs);
        while at < end {
            let middle = at + (end - at) / 2;
            if key_at(middle) < key.as_slice() {
                at = middle + 1;
            } else {
                end = middle;
            }
        }
        while at < pairs && key_at(at) == key.as_slice() {
            blocks.push(block_at(&self.more[at * pair + 8..]));
            at += 1;
        }
        let mut found = Vec::new();
        for block in blocks {
            if block >= records.block_count() {
                continue;
            }
            found.extend(
                records
                    .block_ids(block)?
                    .into_iter()
                    .filter(|id| id.object_type == object_type && id.object_id == object_id),
            );
        }
        let place = found.iter().any(|id| !id.street);
        Ok(found
            .into_iter()
            .filter(|id| !(place && id.street))
            .map(|id| id.record_id)
            .collect())
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        pack::{Reader, Writer},
        record::{
            AddressComponents, AddressRecord, LocationPrecision, PlaceLayer, PlaceRecord,
            PoiRecord, Record, RoadClass, SourceProvenance, StreetRecord, point_geometry,
        },
        util::geo::line_geometry,
    };

    use super::*;

    #[test]
    fn an_id_is_parsed_only_as_a_record_id() {
        assert_eq!(
            parse("osm:node:7").expect("id"),
            Some((ObjectType::Node, 7))
        );
        assert_eq!(
            parse(" osm:relation:9 ").expect("id"),
            Some((ObjectType::Relation, 9))
        );
        assert_eq!(
            parse("openaddresses:nz/countrywide:row:7").expect("id"),
            None
        );
        for malformed in ["7", "osm:node", "osm:node:x", "osm:node:-1", "osm:area:1"] {
            assert!(parse(malformed).is_err(), "{malformed}");
        }
    }

    #[test]
    fn finds_an_object_written_as_records_in_two_blocks() {
        let dir = std::env::temp_dir().join(format!("open-geocode-ids-{}", uuid::Uuid::new_v4()));
        let mut writer = Writer::create(&dir).expect("writer");
        let district = SourceProvenance::osm(ObjectType::Way, 7);
        let poi = |name: &str, source: SourceProvenance, lon: f64| -> Record {
            PoiRecord {
                alt_names: Vec::new(),
                name: name.to_string(),
                category: "historic:district".to_string(),
                address: None,
                geometry: point_geometry(lon, 43.0),
                location_precision: LocationPrecision::Point,
                source,
                prominence: Default::default(),
                wikidata: None,
                brand: None,
                details: None,
                aliases: Vec::new(),
            }
            .into()
        };
        // The way as a POI, many records, then the same way as a district.
        writer
            .write(&poi("Historic District", district.clone(), -79.0), None)
            .expect("poi");
        for index in 0..2_000 {
            let source = SourceProvenance::osm(ObjectType::Node, i64::from(index) + 100);
            writer
                .write(&poi("Cafe", source, -78.0), None)
                .expect("filler");
        }
        writer
            .write(
                &Record::Place(
                    PlaceLayer::Neighbourhood,
                    PlaceRecord {
                        alt_names: Vec::new(),
                        name: "Historic District".to_string(),
                        place_type: "neighbourhood".to_string(),
                        geometry: point_geometry(-79.0, 43.0),
                        source: district,
                        prominence: Default::default(),
                        wikidata: None,
                    },
                ),
                None,
            )
            .expect("district");
        writer.finish().expect("finish");
        let pack = Reader::open(&dir).expect("pack");
        let catalogue = Catalogue::open(pack.container())
            .expect("open")
            .expect("a catalogue");
        let found = catalogue
            .find(pack.records(), ObjectType::Way, 7)
            .expect("find");
        let layers = found
            .iter()
            .map(|id| pack.record_summary(*id).expect("summary").layer)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            layers,
            ["neighbourhood".to_string(), "poi".to_string()].into(),
            "the object's records in every block"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_every_place_by_its_id_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("open-geocode-ids-{}", uuid::Uuid::new_v4()));
        let mut writer = Writer::create(&dir).expect("writer");
        for index in 0..300 {
            let lon = -79.0 + f64::from(index) * 1e-4;
            let record: Record = match index % 3 {
                0 => PoiRecord {
                    alt_names: Vec::new(),
                    name: format!("Cafe {index}"),
                    category: "amenity:cafe".to_string(),
                    address: None,
                    geometry: point_geometry(lon, 43.0),
                    location_precision: LocationPrecision::Point,
                    source: SourceProvenance::osm(ObjectType::Node, i64::from(index) + 1),
                    prominence: Default::default(),
                    wikidata: None,
                    brand: None,
                    details: None,
                    aliases: Vec::new(),
                }
                .into(),
                1 => Record::Place(
                    PlaceLayer::Locality,
                    PlaceRecord {
                        alt_names: Vec::new(),
                        name: format!("Town {index}"),
                        place_type: "town".to_string(),
                        geometry: point_geometry(lon, 43.0),
                        source: SourceProvenance::osm(ObjectType::Relation, i64::from(index) + 1),
                        prominence: Default::default(),
                        wikidata: None,
                    },
                ),
                _ => AddressRecord {
                    access: Vec::new(),
                    disputed: false,
                    street_postcodes: Vec::new(),
                    address: AddressComponents {
                        areas: Vec::new(),
                        number: index.to_string(),
                        street: Some("King Street".to_string()),
                        place: None,
                        unit: None,
                        locality: None,
                        region: None,
                        postcode: None,
                        country: None,
                    },
                    geometry: point_geometry(lon, 43.0),
                    location_precision: LocationPrecision::Point,
                    source: SourceProvenance::osm(ObjectType::Node, i64::from(index) + 1),
                }
                .into(),
            };
            writer.write(&record, None).expect("write");
        }
        writer.finish().expect("finish");
        let pack = Reader::open(&dir).expect("pack");
        let catalogue = Catalogue::open(pack.container())
            .expect("open")
            .expect("a catalogue");
        assert_eq!(catalogue.len(), 200);
        for index in 0..300 {
            let (object_type, layer) = match index % 3 {
                0 => (ObjectType::Node, "poi"),
                1 => (ObjectType::Relation, "locality"),
                _ => (ObjectType::Node, "address"),
            };
            let object_id = i64::from(index) + 1;
            let found = catalogue
                .find(pack.records(), object_type, object_id)
                .expect("find");
            if layer == "address" {
                assert!(found.is_empty(), "an address is not catalogued");
                continue;
            }
            let id = crate::labels::osm_record_id(object_type, object_id);
            let oracle = pack
                .find_by_source_id(&id)
                .expect("scan")
                .expect("a record");
            assert_eq!(found, vec![oracle], "{id}");
            assert_eq!(pack.record_summary(found[0]).expect("summary").layer, layer);
        }
        assert!(
            catalogue
                .find(pack.records(), ObjectType::Way, 12_345)
                .expect("find")
                .is_empty(),
            "an id no record has"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn a_street_answers_to_each_of_its_ways_and_a_place_comes_first() {
        let dir = std::env::temp_dir().join(format!("open-geocode-ids-{}", uuid::Uuid::new_v4()));
        let mut writer = Writer::create(&dir).expect("writer");
        let street = |own: i64, other_ways: Vec<i64>, lon: f64| -> Record {
            Record::Street(StreetRecord {
                name: "Ponsonby Road".to_string(),
                road_class: RoadClass::Secondary,
                alt_names: Vec::new(),
                locality: None,
                geometry: line_geometry(vec![vec![
                    vec![lon, -36.85].into(),
                    vec![lon + 0.001, -36.85].into(),
                ]]),
                representative_point: [lon, -36.85],
                source: SourceProvenance::osm(ObjectType::Way, own),
                other_ways,
            })
        };
        writer
            .write(&street(700, vec![701, 702], 174.74), None)
            .expect("write");
        let market: Record = PoiRecord {
            alt_names: Vec::new(),
            name: "Ponsonby Market".to_string(),
            category: "amenity:marketplace".to_string(),
            address: None,
            geometry: point_geometry(174.7405, -36.85),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Way, 701),
            prominence: Default::default(),
            wikidata: None,
            brand: None,
            details: None,
            aliases: Vec::new(),
        }
        .into();
        writer.write(&market, None).expect("write");
        for index in 0..100 {
            writer
                .write(&street(10_000 + index, Vec::new(), 175.0), None)
                .expect("write");
        }
        writer
            .write(&street(20_000, vec![1_400_000_000], 175.1), None)
            .expect("write");
        writer.finish().expect("finish");
        let pack = Reader::open(&dir).expect("pack");
        let catalogue = Catalogue::open(pack.container())
            .expect("open")
            .expect("a catalogue");
        let find = |object_id| {
            catalogue
                .find(pack.records(), ObjectType::Way, object_id)
                .expect("find")
                .into_iter()
                .map(|id| pack.record_summary(id).expect("summary"))
                .map(|summary| (summary.layer, summary.name))
                .collect::<Vec<_>>()
        };
        let road = vec![("street".to_string(), "Ponsonby Road".to_string())];
        assert_eq!(find(700), road, "its own way");
        assert_eq!(find(702), road, "another of its ways");
        assert_eq!(
            find(701),
            [("poi".to_string(), "Ponsonby Market".to_string())],
            "a place before the street that shares its id"
        );
        assert_eq!(find(1_400_000_000)[0].1, "Ponsonby Road");
        assert!(find(703).is_empty(), "a way of no street");
        let _ = std::fs::remove_dir_all(dir);
    }
}
