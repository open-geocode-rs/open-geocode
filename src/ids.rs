//! The id catalogue: from a place's stable id to the block of the record
//! store that holds it.
//!
//! Map clients keep a place's id, from a shared link, a saved place or a tap
//! on a map tile, and ask for that place again after any rebuild. The
//! catalogue holds the places alone, POIs and areas drawn from OpenStreetMap,
//! whose ids are stable; an address or an imported row is found by its words
//! or its point.
//!
//! It stores no id. A minimal perfect hash (FMPHGO, from the `ph` crate,
//! about 2 to 3 bits an id) gives each catalogued id a slot of its own, and
//! the slot holds the number of the block of 64 records the place is in. A
//! lookup decodes that block and keeps the record whose own id is the one
//! asked for, so an id never catalogued, which the hash sends to some slot
//! anyway, finds nothing.
//!
//! ```text
//! places/ids   width u8 (bytes of a block number) | count u64 | count block
//!              numbers, `width` bytes each, little-endian | the hash, as
//!              `ph` writes it
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
const VERSION: u32 = 1;
/// The most ids one lookup takes.
pub const MAX_IDS: usize = 50;

/// An OSM object as the hash takes it: its kind in the top two bits, its
/// number below, little-endian so a Pack reads alike on every machine.
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

/// The OSM object `id` names: `osm:node:123`. `Err` for text no id is
/// written as; `Ok(None)` for the id of a record the catalogue does not hold
/// (an imported row, a derived postcode).
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

/// Build the catalogue of the sealed Pack and add it.
pub fn add_to(sealed: &mut pack::SealedPack) -> Result<()> {
    let bytes = {
        let pack = pack::Reader::open(sealed.path())?;
        build(pack.records())?
    };
    sealed.add_section(SECTION, VERSION, &bytes)
}

/// The places of every block, in parallel.
fn places(records: &records::Reader) -> Result<Vec<([u8; 8], u64)>> {
    let per_block = (0..records.block_count())
        .into_par_iter()
        .map(|block| {
            Ok(records
                .block_places(block)?
                .into_iter()
                .filter_map(|(_, object_type, object_id)| {
                    Some((key(object_type, object_id)?, block))
                })
                .collect::<Vec<_>>())
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(per_block.into_iter().flatten().collect())
}

fn build(records: &records::Reader) -> Result<Vec<u8>> {
    let mut places = places(records)?;
    // One object can be two places (a node that is a town and a POI); the
    // block of the first keeps it.
    places.sort_unstable_by_key(|(key, block)| (*key, *block));
    places.dedup_by_key(|(key, _)| *key);
    let keys = places.iter().map(|(key, _)| *key).collect::<Vec<_>>();
    let function = GOFunction::from_slice(&keys);
    drop(keys);
    let width = (u64::BITS - records.block_count().leading_zeros())
        .div_ceil(8)
        .max(1) as usize;
    let mut values = vec![0u8; places.len() * width];
    for (key, block) in &places {
        let slot = function
            .get(key)
            .ok_or_else(|| Error::Inconsistent("a catalogued id has no slot".into()))?
            as usize;
        values[slot * width..(slot + 1) * width].copy_from_slice(&block.to_le_bytes()[..width]);
    }
    let mut bytes = Vec::with_capacity(9 + values.len() + function.write_bytes());
    bytes.push(width as u8);
    bytes.extend_from_slice(&(places.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&values);
    function.write(&mut bytes)?;
    Ok(bytes)
}

/// The catalogue of a Pack.
pub struct Catalogue {
    function: GOFunction,
    width: usize,
    count: usize,
    values: Bytes,
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
    /// The Pack's catalogue, `None` for a Pack built before it had one.
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
        let mut hash = &bytes[values_end..];
        let function = GOFunction::read(&mut hash)?;
        Ok(Some(Arc::new(Self {
            function,
            width,
            count,
            values: bytes.slice(9..values_end),
        })))
    }

    /// The records whose own id is `object_type`/`object_id`, by id order.
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
        let mut block = [0u8; 8];
        block[..self.width]
            .copy_from_slice(&self.values[slot * self.width..(slot + 1) * self.width]);
        let block = u64::from_le_bytes(block);
        if block >= records.block_count() {
            return Ok(Vec::new());
        }
        Ok(records
            .block_places(block)?
            .into_iter()
            .filter(|(_, kind, id)| *kind == object_type && *id == object_id)
            .map(|(record_id, _, _)| record_id)
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
            PoiRecord, Record, SourceProvenance, point_geometry,
        },
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
    fn finds_every_place_by_its_id_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("open-geocode-ids-{}", uuid::Uuid::new_v4()));
        let mut writer = Writer::create(&dir).expect("writer");
        // Enough records for several blocks.
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
                    },
                ),
                _ => AddressRecord {
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
}
