use std::{
    fs::{File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Write},
    sync::Arc,
};

use memmap2::{Mmap, MmapMut};
use ph::fmph::{GOBuildConf, GOFunction};
use rayon::prelude::*;

use crate::{
    container::{Bytes, Container},
    error::{Error, Result},
    extsort::{ExternalSorter, Scratch, ScratchFile, Spill},
    memory::Budget,
    pack::{self, RecordId},
    record::ObjectType,
    records,
    util::codec::{get_u64, put_u64},
};

pub const SECTION: &str = "places/ids";
const VERSION: u32 = 3;

fn key(object_type: ObjectType, object_id: i64) -> Option<[u8; 8]> {
    let kind: u64 = match object_type {
        ObjectType::Node => 0,
        ObjectType::Way => 1,
        ObjectType::Relation => 2,
        ObjectType::Row => 3,
    };
    let number = u64::try_from(object_id)
        .ok()
        .filter(|number| number >> 62 == 0)?;
    Some((kind << 62 | number).to_le_bytes())
}

/// `Ok(None)`: a well-formed id the catalogue never holds (OpenAddresses row,
/// derived postcode).
pub fn parse(id: &str) -> Result<Option<(ObjectType, i64)>> {
    let malformed =
        || Error::BadOptions(format!("{id:?} is no record id, such as osm:node:123").into());
    let mut parts = id.trim().splitn(3, ':');
    let (Some(dataset), Some(kind), Some(number)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(malformed());
    };
    if dataset == "overture" && kind == "places" {
        return uuid::Uuid::parse_str(number)
            .map(|id| Some((ObjectType::Row, crate::record::gers_object_id(&id))))
            .map_err(|_| malformed());
    }
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

pub fn add_to(
    sealed: &mut pack::SealedPack,
    scratch: &Arc<Scratch>,
    memory: &Arc<Budget>,
    share_bytes: usize,
) -> Result<()> {
    let section = {
        let pack = pack::Reader::open(sealed.path())?;
        let ids = ExternalSorter::new(scratch, LABEL, memory, share_bytes);
        build(pack.records(), scratch, ids)?
    };
    sealed.add_section_files(&[(SECTION, VERSION, section.path().to_path_buf())])
}

const LABEL: &str = "places-ids";
const BLOCKS_PER_THREAD: u64 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Catalogued {
    key: [u8; 8],
    street: bool,
    block: u64,
}

impl Spill for Catalogued {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.key);
        out.push(u8::from(self.street));
        put_u64(out, self.block);
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        let truncated = || Error::Decode("a spilled id is truncated");
        let (key, rest) = input.split_first_chunk::<8>().ok_or_else(truncated)?;
        let (street, rest) = rest.split_first().ok_or_else(truncated)?;
        let key = *key;
        let street = *street != 0;
        *input = rest;
        Ok(Self {
            key,
            street,
            block: get_u64(input)?,
        })
    }
}

fn build(
    records: &records::Reader,
    scratch: &Arc<Scratch>,
    mut ids: ExternalSorter<Catalogued>,
) -> Result<ScratchFile> {
    let blocks = records.block_count();
    let round = rayon::current_num_threads() as u64 * BLOCKS_PER_THREAD;
    for first in (0..blocks).step_by(round.max(1) as usize) {
        let read = (first..(first + round).min(blocks))
            .into_par_iter()
            .map(|block| {
                Ok(records
                    .block_ids(block)?
                    .into_iter()
                    .filter_map(|id| {
                        Some(Catalogued {
                            key: key(id.object_type, id.object_id)?,
                            street: id.street,
                            block,
                        })
                    })
                    .collect::<Vec<_>>())
            })
            .collect::<Result<Vec<_>>>()?;
        for catalogued in read.into_iter().flatten() {
            ids.push(catalogued)?;
        }
    }
    let width = (u64::BITS - blocks.leading_zeros()).div_ceil(8).max(1) as usize;
    let mut keys = BufWriter::new(ScratchFile::create(scratch, LABEL, "places-ids.keys")?);
    let mut firsts = BufWriter::new(ScratchFile::create(scratch, LABEL, "places-ids.firsts")?);
    let mut more = BufWriter::new(ScratchFile::create(scratch, LABEL, "places-ids.more")?);
    let (mut places, mut more_count) = (0u64, 0u64);
    let mut held: Option<Catalogued> = None;
    let mut others = Vec::new();
    let mut flush = |held: Catalogued, others: &mut Vec<u64>| -> Result<()> {
        keys.write_all(&held.key)?;
        firsts.write_all(&held.block.to_le_bytes()[..width])?;
        places += 1;
        others.sort_unstable();
        others.dedup();
        for block in others.drain(..) {
            more.write_all(&held.key)?;
            more.write_all(&block.to_le_bytes()[..width])?;
            more_count += 1;
        }
        Ok(())
    };
    for catalogued in ids.finish()? {
        let catalogued = catalogued?;
        match held {
            Some(first) if first.key == catalogued.key => {
                if catalogued.block != first.block {
                    others.push(catalogued.block);
                }
            }
            _ => {
                if let Some(first) = held.replace(catalogued) {
                    flush(first, &mut others)?;
                }
            }
        }
    }
    if let Some(first) = held {
        flush(first, &mut others)?;
    }
    let keys = finished(keys)?;
    let firsts = finished(firsts)?;
    let more = finished(more)?;
    let mut values = ScratchFile::create(scratch, LABEL, "places-ids.values")?;
    io::copy(&mut io::repeat(0).take(places * width as u64), &mut values)?;
    let function = {
        let keys_map = mapped(&keys)?;
        let all_keys = keys_map.as_deref().unwrap_or_default().as_chunks::<8>().0;
        let function = perfect_hash(all_keys);
        let mut values_map = mapped_mut(&values)?;
        let slots = values_map.as_deref_mut().unwrap_or_default();
        let mut blocks_in = BufReader::new(File::open(firsts.path())?);
        let mut block = [0u8; 8];
        for key in all_keys {
            blocks_in.read_exact(&mut block[..width])?;
            let slot = function
                .get(key)
                .ok_or_else(|| Error::Inconsistent("a catalogued id has no slot".into()))?
                as usize;
            slots[slot * width..(slot + 1) * width].copy_from_slice(&block[..width]);
        }
        function
    };
    drop((keys, firsts));
    let mut section = BufWriter::new(ScratchFile::create(scratch, LABEL, "places-ids.section")?);
    section.write_all(&[width as u8])?;
    section.write_all(&places.to_le_bytes())?;
    io::copy(&mut File::open(values.path())?, &mut section)?;
    section.write_all(&more_count.to_le_bytes())?;
    io::copy(&mut File::open(more.path())?, &mut section)?;
    function.write(&mut section)?;
    finished(section)
}

fn perfect_hash(keys: &[[u8; 8]]) -> GOFunction {
    GOFunction::from_slice_with_conf(keys, GOBuildConf::with_ct(Default::default(), 0))
}

fn finished(writer: BufWriter<ScratchFile>) -> Result<ScratchFile> {
    writer
        .into_inner()
        .map_err(|error| Error::Io(error.into_error()))
}

fn mapped(file: &ScratchFile) -> Result<Option<Mmap>> {
    if file.bytes() == 0 {
        return Ok(None);
    }
    // SAFETY: a scratch file of this build, written and closed, that no one
    // writes while it is mapped.
    Ok(Some(unsafe { Mmap::map(&File::open(file.path())?) }?))
}

fn mapped_mut(file: &ScratchFile) -> Result<Option<MmapMut>> {
    if file.bytes() == 0 {
        return Ok(None);
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(file.path())?;
    // SAFETY: a scratch file of this build that only this mapping writes.
    Ok(Some(unsafe { MmapMut::map_mut(&file) }?))
}

pub struct Catalogue {
    function: GOFunction,
    width: usize,
    count: usize,
    values: Bytes,
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

    /// Find a place, area or street by its external ID.
    pub fn find_id(&self, records: &records::Reader, source_id: &str) -> Result<Vec<RecordId>> {
        let Some((object_type, object_id)) = parse(source_id)? else {
            return Ok(Vec::new());
        };
        let found = self.find(records, object_type, object_id)?;
        if object_type != ObjectType::Row {
            return Ok(found);
        }
        let Some(requested) = gers_uuid(source_id.trim()) else {
            return Ok(Vec::new());
        };
        let mut exact = Vec::new();
        for id in found {
            if gers_uuid(&records.summary(id)?.id) == Some(requested) {
                exact.push(id);
            }
        }
        Ok(exact)
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

fn gers_uuid(id: &str) -> Option<uuid::Uuid> {
    uuid::Uuid::parse_str(id.strip_prefix(crate::licences::OVERTURE_PREFIX)?).ok()
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use crate::{
        pack::{Reader, Writer},
        record::{
            AddressComponents, AddressRecord, LocationPrecision, PlaceLayer, PlaceRecord,
            PoiRecord, Record, RoadClass, SourceProvenance, StreetRecord, point_geometry,
        },
        test_support::TempDir,
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
    fn the_hash_built_without_a_key_cache_is_the_one_built_with_it() {
        let keys: Vec<[u8; 8]> = (0..5000u64).map(|id| (id * 7 + 1).to_le_bytes()).collect();
        let (mut plain, mut uncached) = (Vec::new(), Vec::new());
        GOFunction::from_slice(&keys[..])
            .write(&mut plain)
            .expect("write");
        perfect_hash(&keys).write(&mut uncached).expect("write");
        assert_eq!(plain, uncached);
    }

    static IDS_PACK: LazyLock<TempDir> = LazyLock::new(|| {
        let dir = TempDir::new("ids");
        let mut writer = Writer::create(&dir).expect("writer");
        for record in two_blocks_records()
            .into_iter()
            .chain(every_place_records())
            .chain(street_records())
        {
            writer.write(&record, None).expect("write");
        }
        writer.finish().expect("finish");
        dir
    });

    fn open_catalogue() -> (Reader, Arc<Catalogue>) {
        let pack = Reader::open(&*IDS_PACK).expect("pack");
        let catalogue = Catalogue::open(pack.container())
            .expect("open")
            .expect("a catalogue");
        (pack, catalogue)
    }

    fn two_blocks_records() -> Vec<Record> {
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
        let mut records = vec![poi("Historic District", district.clone(), -79.0)];
        for index in 0..2_000 {
            let source = SourceProvenance::osm(ObjectType::Node, i64::from(index) + 100_000);
            records.push(poi("Old Mill", source, -78.0));
        }
        records.push(Record::Place(
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
        ));
        records
    }

    fn every_place_records() -> Vec<Record> {
        (0..300)
            .map(|index| {
                let lon = 10.0 + f64::from(index) * 1e-4;
                match index % 3 {
                    0 => PoiRecord {
                        alt_names: Vec::new(),
                        name: format!("Cafe {index}"),
                        category: "amenity:cafe".to_string(),
                        address: None,
                        geometry: point_geometry(lon, 50.0),
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
                            geometry: point_geometry(lon, 50.0),
                            source: SourceProvenance::osm(
                                ObjectType::Relation,
                                i64::from(index) + 1,
                            ),
                            prominence: Default::default(),
                            wikidata: None,
                        },
                    ),
                    _ => AddressRecord {
                        building: None,
                        access: Vec::new(),
                        disputed: false,
                        street_postcodes: Vec::new(),
                        address: AddressComponents {
                            block_number_len: None,
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
                        geometry: point_geometry(lon, 50.0),
                        location_precision: LocationPrecision::Point,
                        source: SourceProvenance::osm(ObjectType::Node, i64::from(index) + 1),
                    }
                    .into(),
                }
            })
            .collect()
    }

    fn street_records() -> Vec<Record> {
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
        let mut records = vec![
            street(700, vec![701, 702], 174.74),
            PoiRecord {
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
            .into(),
        ];
        for index in 0..100 {
            records.push(street(10_000 + index, Vec::new(), 175.0));
        }
        records.push(street(20_000, vec![1_400_000_000], 175.1));
        records
    }

    #[test]
    fn finds_an_object_written_as_records_in_two_blocks() {
        let (pack, catalogue) = open_catalogue();
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
    }

    #[test]
    fn finds_every_place_by_its_id_and_nothing_else() {
        let (pack, catalogue) = open_catalogue();
        let mut by_source = std::collections::HashMap::new();
        for record_id in 0..pack.records().record_count() {
            let summary = pack.record_summary(record_id).expect("summary");
            by_source.entry(summary.id).or_insert(record_id);
        }
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
            assert_eq!(found, vec![by_source[&id]], "{id}");
            assert_eq!(pack.record_summary(found[0]).expect("summary").layer, layer);
        }
        assert!(
            catalogue
                .find(pack.records(), ObjectType::Way, 12_345)
                .expect("find")
                .is_empty(),
            "an id no record has"
        );
    }

    #[test]
    fn a_street_answers_to_each_of_its_ways_and_a_place_comes_first() {
        let (pack, catalogue) = open_catalogue();
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
    }
}
