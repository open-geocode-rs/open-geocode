//! Minimal OSM PBF writer for builder tests: uncompressed blobs, dense nodes,
//! ways and relations. Each slice passed to [`write_pbf`] becomes one block, so
//! tests control how objects are split across blocks.

use std::{collections::BTreeMap, path::Path};

pub(crate) enum TestElement {
    Node {
        id: i64,
        lat: f64,
        lon: f64,
        tags: Vec<(&'static str, &'static str)>,
    },
    Way {
        id: i64,
        refs: Vec<i64>,
        tags: Vec<(&'static str, &'static str)>,
    },
    Relation {
        id: i64,
        members: Vec<(i64, &'static str)>,
        tags: Vec<(&'static str, &'static str)>,
    },
}

pub(crate) fn node(
    id: i64,
    lat: f64,
    lon: f64,
    tags: &[(&'static str, &'static str)],
) -> TestElement {
    TestElement::Node {
        id,
        lat,
        lon,
        tags: tags.to_vec(),
    }
}

pub(crate) fn way(id: i64, refs: &[i64], tags: &[(&'static str, &'static str)]) -> TestElement {
    TestElement::Way {
        id,
        refs: refs.to_vec(),
        tags: tags.to_vec(),
    }
}

pub(crate) fn relation(
    id: i64,
    members: &[(i64, &'static str)],
    tags: &[(&'static str, &'static str)],
) -> TestElement {
    TestElement::Relation {
        id,
        members: members.to_vec(),
        tags: tags.to_vec(),
    }
}

pub(crate) fn write_pbf(path: &Path, blocks: &[Vec<TestElement>]) {
    let mut file = Vec::new();
    let mut header = Vec::new();
    for feature in ["OsmSchema-V0.6", "DenseNodes"] {
        bytes_field(&mut header, 4, feature.as_bytes());
    }
    blob(&mut file, "OSMHeader", &header);
    for elements in blocks {
        blob(&mut file, "OSMData", &primitive_block(elements));
    }
    std::fs::write(path, file).expect("write test PBF");
}

fn blob(file: &mut Vec<u8>, kind: &str, data: &[u8]) {
    let mut blob = Vec::new();
    bytes_field(&mut blob, 1, data);
    varint_field(&mut blob, 2, data.len() as u64);
    let mut header = Vec::new();
    bytes_field(&mut header, 1, kind.as_bytes());
    varint_field(&mut header, 3, blob.len() as u64);
    file.extend_from_slice(&(header.len() as u32).to_be_bytes());
    file.extend_from_slice(&header);
    file.extend_from_slice(&blob);
}

fn primitive_block(elements: &[TestElement]) -> Vec<u8> {
    let mut strings = StringTable::default();
    let mut dense_ids = Vec::new();
    let mut dense_lats = Vec::new();
    let mut dense_lons = Vec::new();
    let mut dense_keys_vals = Vec::new();
    let mut ways = Vec::new();
    let mut relations = Vec::new();
    for element in elements {
        match element {
            TestElement::Node { id, lat, lon, tags } => {
                dense_ids.push(*id);
                dense_lats.push((lat * 1e7).round() as i64);
                dense_lons.push((lon * 1e7).round() as i64);
                for (key, value) in tags {
                    dense_keys_vals.push(u64::from(strings.id(key)));
                    dense_keys_vals.push(u64::from(strings.id(value)));
                }
                dense_keys_vals.push(0);
            }
            TestElement::Way { id, refs, tags } => {
                let mut way = Vec::new();
                varint_field(&mut way, 1, *id as u64);
                tag_fields(&mut way, &mut strings, tags);
                packed_field(&mut way, 8, &deltas(refs).map(zigzag).collect::<Vec<_>>());
                ways.push(way);
            }
            TestElement::Relation { id, members, tags } => {
                let mut relation = Vec::new();
                varint_field(&mut relation, 1, *id as u64);
                tag_fields(&mut relation, &mut strings, tags);
                let roles = members
                    .iter()
                    .map(|(_, role)| u64::from(strings.id(role)))
                    .collect::<Vec<_>>();
                packed_field(&mut relation, 8, &roles);
                let ids = members.iter().map(|(id, _)| *id).collect::<Vec<_>>();
                packed_field(
                    &mut relation,
                    9,
                    &deltas(&ids).map(zigzag).collect::<Vec<_>>(),
                );
                packed_field(&mut relation, 10, &vec![1; members.len()]);
                relations.push(relation);
            }
        }
    }

    let mut groups = Vec::new();
    if !dense_ids.is_empty() {
        let mut dense = Vec::new();
        packed_field(
            &mut dense,
            1,
            &deltas(&dense_ids).map(zigzag).collect::<Vec<_>>(),
        );
        packed_field(
            &mut dense,
            8,
            &deltas(&dense_lats).map(zigzag).collect::<Vec<_>>(),
        );
        packed_field(
            &mut dense,
            9,
            &deltas(&dense_lons).map(zigzag).collect::<Vec<_>>(),
        );
        packed_field(&mut dense, 10, &dense_keys_vals);
        let mut group = Vec::new();
        bytes_field(&mut group, 2, &dense);
        groups.push(group);
    }
    if !ways.is_empty() {
        let mut group = Vec::new();
        for way in &ways {
            bytes_field(&mut group, 3, way);
        }
        groups.push(group);
    }
    if !relations.is_empty() {
        let mut group = Vec::new();
        for relation in &relations {
            bytes_field(&mut group, 4, relation);
        }
        groups.push(group);
    }

    let mut table = Vec::new();
    for string in &strings.strings {
        bytes_field(&mut table, 1, string.as_bytes());
    }
    let mut block = Vec::new();
    bytes_field(&mut block, 1, &table);
    for group in &groups {
        bytes_field(&mut block, 2, group);
    }
    block
}

#[derive(Default)]
struct StringTable {
    strings: Vec<String>,
    ids: BTreeMap<String, u32>,
}

impl StringTable {
    fn id(&mut self, value: &str) -> u32 {
        if self.strings.is_empty() {
            self.strings.push(String::new());
        }
        if let Some(id) = self.ids.get(value) {
            return *id;
        }
        let id = self.strings.len() as u32;
        self.strings.push(value.to_string());
        self.ids.insert(value.to_string(), id);
        id
    }
}

fn tag_fields(out: &mut Vec<u8>, strings: &mut StringTable, tags: &[(&str, &str)]) {
    let keys = tags
        .iter()
        .map(|(key, _)| u64::from(strings.id(key)))
        .collect::<Vec<_>>();
    let values = tags
        .iter()
        .map(|(_, value)| u64::from(strings.id(value)))
        .collect::<Vec<_>>();
    packed_field(out, 2, &keys);
    packed_field(out, 3, &values);
}

fn deltas(values: &[i64]) -> impl Iterator<Item = i64> + '_ {
    values.iter().scan(0i64, |previous, value| {
        let delta = value - *previous;
        *previous = *value;
        Some(delta)
    })
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
    varint(out, u64::from(field << 3));
    varint(out, value);
}

fn bytes_field(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    varint(out, u64::from(field << 3 | 2));
    varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn packed_field(out: &mut Vec<u8>, field: u32, values: &[u64]) {
    if values.is_empty() {
        return;
    }
    let mut packed = Vec::new();
    for value in values {
        varint(&mut packed, *value);
    }
    bytes_field(out, field, &packed);
}
