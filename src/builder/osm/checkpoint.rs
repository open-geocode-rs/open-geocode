use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    builder::{
        addresses::AddressFile, plan::ItemCounts, progress::BuildStage, report::BuilderReport,
        report::SorterReport,
    },
    error::{Error, Result},
    extsort::{IndexedState, Ledger, RunState, SorterState, Spill},
    pack::{SealedState, SealingState},
    util::codec::{
        Delta, get_delta, get_i64, get_string, get_u8, get_u64, put_delta, put_i64, put_str,
        put_u64, unzigzag, zigzag,
    },
};

use super::{
    boundary::{
        BoundaryMemberRole, BoundaryRelationMember, BoundaryRelationStub, BoundaryWay, Vertex,
    },
    locations::{LocationsState, degrees},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(super) enum Step {
    Input { index: usize, pass: Pass },
    Boundaries,
    ImportAddresses,
    Contexts,
    Streets,
    AddressBook,
    Houses,
    Addresses,
    Records,
    Seal(u8),
    Sealed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(super) enum Pass {
    Scan,
    Members,
    Nodes,
    Ways,
}

pub(super) const SEALED_PARTS: u8 = 3;

impl Step {
    pub(super) fn all(inputs: usize) -> Vec<Self> {
        let mut steps = Vec::new();
        for index in 0..inputs {
            for pass in [Pass::Scan, Pass::Members, Pass::Nodes, Pass::Ways] {
                steps.push(Self::Input { index, pass });
            }
        }
        steps.extend([
            Self::Boundaries,
            Self::ImportAddresses,
            Self::Contexts,
            Self::Streets,
            Self::AddressBook,
            Self::Houses,
            Self::Addresses,
            Self::Records,
        ]);
        steps.extend((1..=SEALED_PARTS).map(Self::Seal));
        steps.push(Self::Sealed);
        steps
    }

    pub(super) fn stage(self) -> BuildStage {
        match self {
            Self::Input { pass, .. } => match pass {
                Pass::Scan => BuildStage::Scan,
                Pass::Members => BuildStage::BoundaryMembers,
                Pass::Nodes => BuildStage::NodeJoin,
                Pass::Ways => BuildStage::AssembleWays,
            },
            Self::Boundaries => BuildStage::Boundaries,
            Self::ImportAddresses => BuildStage::ImportAddresses,
            Self::Contexts | Self::Streets | Self::AddressBook | Self::Houses | Self::Addresses => {
                BuildStage::ContextRecords
            }
            Self::Records => BuildStage::Records,
            Self::Seal(_) | Self::Sealed => BuildStage::Indexes,
        }
    }

    pub(super) fn describe(self, inputs: usize) -> String {
        match self {
            Self::Input { index, .. } if inputs > 1 => format!(
                "{} of input {} of {inputs}",
                self.stage().description(),
                index + 1
            ),
            Self::Contexts => "order context records".to_string(),
            Self::Streets => "join streets".to_string(),
            Self::AddressBook => "write the address book".to_string(),
            Self::Houses => "find repeated houses".to_string(),
            Self::Addresses => "place addresses".to_string(),
            Self::Records => "write records and in-memory Pack sections".to_string(),
            Self::Seal(part) => format!("copy part {part} of {SEALED_PARTS} into the Pack"),
            Self::Sealed => "seal the Pack".to_string(),
            _ => self.stage().description().to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct State {
    pub step: Step,
    pub seq: u64,
    pub datasets: Vec<String>,
    pub addresses: Option<AddressState>,
    pub report: BuilderReport,
    pub audit: AuditState,
    pub sorters: Vec<SorterState>,
    pub sorted: BTreeMap<String, SorterReport>,
    pub next_file: u64,
    pub ledger: Ledger,
    pub scanned: Option<ScanCounts>,
    pub geometry: Vec<RunState>,
    pub input: Option<InputState>,
    pub boundaries: Option<RunState>,
    #[serde(default)]
    pub boundary_shapes: Option<(String, u64)>,
    pub imports: Option<Vec<AddressFile>>,
    pub contexts: Option<ContextState>,
    pub postcodes: Option<RunState>,
    #[serde(default)]
    pub town_votes: Vec<RunState>,
    pub sample: Option<(String, u64)>,
    pub sealing: Option<SealingState>,
    pub sealed: Option<SealedState>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct AddressState {
    pub book: Option<IndexedState>,
    pub placed: Option<(IndexedState, IndexedState)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct AuditState {
    pub bytes: u64,
    pub count: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ScanCounts {
    pub items: ItemCounts,
    pub read_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct InputState {
    pub index: usize,
    pub kept: KeptWays,
    pub node_blobs: RunState,
    pub way_blobs: RunState,
    pub kept_blobs: RunState,
    pub node_set: Option<RunState>,
    pub tag_ids: Option<RunState>,
    pub members: Option<RunState>,
    pub locations: Option<LocationsState>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct KeptWays {
    pub addresses: u64,
    pub streets: u64,
    pub records: u64,
    pub all: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ContextState {
    pub ordered: RunState,
    pub boundary_ids: Vec<u64>,
    pub country_ids: BTreeMap<String, u64>,
    pub settled: Vec<u64>,
    pub town_reach: Vec<(u64, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Id(pub i64);

impl Spill for Id {
    fn encode(&self, out: &mut Vec<u8>) {
        put_i64(out, self.0);
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Ok(Self(get_i64(input)?))
    }

    fn encode_after(&self, delta: &mut Delta, out: &mut Vec<u8>) {
        put_delta(out, delta, self.0);
    }

    fn decode_after(delta: &mut Delta, input: &mut &[u8]) -> Result<Self> {
        Ok(Self(get_delta(input, delta)?))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct NamedPoint {
    pub name: String,
    pub point: [f64; 2],
}

impl Spill for NamedPoint {
    fn encode(&self, out: &mut Vec<u8>) {
        put_str(out, &self.name);
        for value in self.point {
            out.extend_from_slice(&value.to_bits().to_le_bytes());
        }
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        let name = get_string(input)?;
        Ok(Self {
            name,
            point: [get_f64(input)?, get_f64(input)?],
        })
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.name.len()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum GeometryItem {
    Relation(BoundaryRelationStub),
    Feature(i64),
    Way(BoundaryWay),
    Member(i64, Option<Vec<Vertex>>),
    Entrance(i64, String),
}

impl Spill for GeometryItem {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Relation(relation) => {
                out.push(0);
                put_i64(out, relation.object_id);
                put_u64(out, relation.members.len() as u64);
                for member in &relation.members {
                    put_i64(out, member.way_id);
                    out.push(match member.role {
                        BoundaryMemberRole::Outer => 0,
                        BoundaryMemberRole::Inner => 1,
                    });
                }
                match relation.label {
                    Some(label) => {
                        out.push(1);
                        put_i64(out, label);
                    }
                    None => out.push(0),
                }
                put_tags(out, &relation.tags);
            }
            Self::Feature(way_id) => {
                out.push(1);
                put_i64(out, *way_id);
            }
            Self::Way(way) => {
                out.push(2);
                put_i64(out, way.object_id);
                put_tags(out, &way.tags);
                put_vertices(out, &way.vertices);
            }
            Self::Member(way_id, vertices) => {
                out.push(3);
                put_i64(out, *way_id);
                match vertices {
                    Some(vertices) => {
                        out.push(1);
                        put_vertices(out, vertices);
                    }
                    None => out.push(0),
                }
            }
            Self::Entrance(node_id, kind) => {
                out.push(4);
                put_i64(out, *node_id);
                put_str(out, kind);
            }
        }
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Ok(match get_u8(input)? {
            0 => {
                let object_id = get_i64(input)?;
                let count = get_u64(input)?;
                let mut members = Vec::new();
                for _ in 0..count {
                    let way_id = get_i64(input)?;
                    let role = match get_u8(input)? {
                        0 => BoundaryMemberRole::Outer,
                        1 => BoundaryMemberRole::Inner,
                        _ => return Err(Error::Decode("unknown boundary member role")),
                    };
                    members.push(BoundaryRelationMember { way_id, role });
                }
                let label = match get_u8(input)? {
                    0 => None,
                    1 => Some(get_i64(input)?),
                    _ => return Err(Error::Decode("a saved relation's label flag is not 0 or 1")),
                };
                Self::Relation(BoundaryRelationStub {
                    object_id,
                    members,
                    label,
                    tags: get_tags(input)?,
                })
            }
            1 => Self::Feature(get_i64(input)?),
            2 => {
                let object_id = get_i64(input)?;
                let tags = get_tags(input)?;
                Self::Way(BoundaryWay {
                    object_id,
                    tags,
                    vertices: get_vertices(input)?,
                })
            }
            3 => {
                let way_id = get_i64(input)?;
                let vertices = match get_u8(input)? {
                    0 => None,
                    _ => Some(get_vertices(input)?),
                };
                Self::Member(way_id, vertices)
            }
            4 => Self::Entrance(get_i64(input)?, get_string(input)?),
            _ => return Err(Error::Decode("unknown boundary geometry item")),
        })
    }

    fn resident_bytes(&self) -> usize {
        let vertices = |vertices: &[Vertex]| std::mem::size_of_val(vertices);
        std::mem::size_of::<Self>()
            + match self {
                Self::Relation(relation) => {
                    relation.members.len() * std::mem::size_of::<BoundaryRelationMember>()
                        + tags_bytes(&relation.tags)
                }
                Self::Feature(_) => 0,
                Self::Way(way) => tags_bytes(&way.tags) + vertices(&way.vertices),
                Self::Member(_, line) => line.as_deref().map_or(0, vertices),
                Self::Entrance(_, kind) => kind.len(),
            }
    }
}

fn tags_bytes(tags: &BTreeMap<String, String>) -> usize {
    tags.iter()
        .map(|(key, value)| key.len() + value.len() + 48)
        .sum()
}

pub(super) fn put_tags(out: &mut Vec<u8>, tags: &BTreeMap<String, String>) {
    put_u64(out, tags.len() as u64);
    for (key, value) in tags {
        put_str(out, key);
        put_str(out, value);
    }
}

pub(super) fn get_tags(input: &mut &[u8]) -> Result<BTreeMap<String, String>> {
    let count = get_u64(input)?;
    let mut tags = BTreeMap::new();
    for _ in 0..count {
        let key = get_string(input)?;
        tags.insert(key, get_string(input)?);
    }
    Ok(tags)
}

fn put_vertices(out: &mut Vec<u8>, vertices: &[Vertex]) {
    put_u64(out, vertices.len() as u64);
    let (mut id, mut lat, mut lon) = (0, 0, 0);
    for vertex in vertices {
        put_i64(out, vertex.node_id.wrapping_sub(id));
        id = vertex.node_id;
        put_degrees(out, &mut lat, vertex.lat);
        put_degrees(out, &mut lon, vertex.lon);
    }
}

fn get_vertices(input: &mut &[u8]) -> Result<Vec<Vertex>> {
    let count = get_u64(input)?;
    let mut vertices = Vec::new();
    let (mut id, mut lat, mut lon) = (0i64, 0, 0);
    for _ in 0..count {
        id = id.wrapping_add(get_i64(input)?);
        vertices.push(Vertex {
            node_id: id,
            lat: get_degrees(input, &mut lat)?,
            lon: get_degrees(input, &mut lon)?,
        });
    }
    Ok(vertices)
}

pub(super) fn put_degrees(out: &mut Vec<u8>, last: &mut i64, value: f64) {
    let fixed = (value * 1e7).round();
    if fixed.abs() <= f64::from(i32::MAX) && degrees(fixed as i32).to_bits() == value.to_bits() {
        let fixed = fixed as i64;
        put_u64(out, zigzag(fixed - *last) << 1);
        *last = fixed;
    } else {
        put_u64(out, 1);
        out.extend_from_slice(&value.to_bits().to_le_bytes());
    }
}

pub(super) fn get_degrees(input: &mut &[u8], last: &mut i64) -> Result<f64> {
    let tag = get_u64(input)?;
    if tag & 1 == 0 {
        *last += unzigzag(tag >> 1);
        let fixed =
            i32::try_from(*last).map_err(|_| Error::Decode("saved coordinate out of range"))?;
        return Ok(degrees(fixed));
    }
    get_f64(input)
}

pub(super) fn get_f64(input: &mut &[u8]) -> Result<f64> {
    let bytes = input
        .get(..8)
        .ok_or(Error::Decode("saved number truncated"))?;
    *input = &input[8..];
    Ok(f64::from_bits(u64::from_le_bytes(
        bytes.try_into().expect("8 bytes"),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T: Spill + PartialEq + std::fmt::Debug>(items: &[T]) {
        let mut out = Vec::new();
        let mut delta = Delta::default();
        for item in items {
            item.encode_after(&mut delta, &mut out);
        }
        let mut input = out.as_slice();
        let mut delta = Delta::default();
        for item in items {
            assert_eq!(
                &T::decode_after(&mut delta, &mut input).expect("decode"),
                item
            );
        }
        assert!(input.is_empty());
    }

    #[test]
    fn saved_geometry_reads_back_bit_for_bit() {
        let vertex = |node_id, lat_e7: i32, lon_e7: i32| Vertex {
            node_id,
            lat: degrees(lat_e7),
            lon: degrees(lon_e7),
        };
        round_trip(&[
            GeometryItem::Relation(BoundaryRelationStub {
                object_id: 900,
                members: vec![
                    BoundaryRelationMember {
                        way_id: 500,
                        role: BoundaryMemberRole::Outer,
                    },
                    BoundaryRelationMember {
                        way_id: 501,
                        role: BoundaryMemberRole::Inner,
                    },
                ],
                label: Some(29_546_940),
                tags: BTreeMap::from([("name".to_string(), "Toronto".to_string())]),
            }),
            GeometryItem::Relation(BoundaryRelationStub {
                object_id: 901,
                members: vec![BoundaryRelationMember {
                    way_id: 502,
                    role: BoundaryMemberRole::Outer,
                }],
                label: None,
                tags: BTreeMap::new(),
            }),
            GeometryItem::Feature(605),
            GeometryItem::Way(BoundaryWay {
                object_id: 605,
                tags: BTreeMap::new(),
                vertices: vec![
                    vertex(30, 436_200_000, -794_300_000),
                    vertex(31, 436_200_000, -794_298_000),
                    Vertex {
                        node_id: 7,
                        lat: 0.1 + 0.2,
                        lon: -180.0,
                    },
                    vertex(i64::MAX, i32::MIN + 1, i32::MAX),
                ],
            }),
            GeometryItem::Member(500, None),
            GeometryItem::Member(501, Some(vec![vertex(1, -1, 1)])),
        ]);
        round_trip(&[Id(5), Id(-3), Id(i64::MAX), Id(0)]);
        round_trip(&[NamedPoint {
            name: "M5V 1A1".into(),
            point: [-79.4, 43.6],
        }]);
    }

    #[test]
    fn steps_run_every_pass_input_by_input_and_end_sealed() {
        let input = |index, pass| Step::Input { index, pass };
        let steps = Step::all(2);
        assert_eq!(
            steps[..8],
            [
                input(0, Pass::Scan),
                input(0, Pass::Members),
                input(0, Pass::Nodes),
                input(0, Pass::Ways),
                input(1, Pass::Scan),
                input(1, Pass::Members),
                input(1, Pass::Nodes),
                input(1, Pass::Ways),
            ]
        );
        assert_eq!(steps.last(), Some(&Step::Sealed));
    }
}
