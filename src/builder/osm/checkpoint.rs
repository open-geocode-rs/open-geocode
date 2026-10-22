//! What a build carries from one stage to the next, as a checkpoint saves
//! it (see [`crate::builder::resume`]).
//!
//! Each stage's outputs are declared once, in [`State`]: the sorters still
//! open, the spools later stages read, the node store, the boundary set,
//! the Pack being sealed, the counters and the report. A checkpoint saves
//! the cheap in-memory state whole and names the scratch files holding the
//! rest. What is cheaper to compute again than to keep is not saved: the
//! boundary index, the context names and country codes are derived again
//! from the saved boundary set and record ids in a fraction of the time
//! the boundary stage took, and the node store's rank index from its
//! bitmap in a pass over it.

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

/// A point a build can go on from: after a stage of one input's passes, or
/// after a stage run once over every input. Ordered as the build runs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(super) enum Step {
    Input {
        index: usize,
        pass: Pass,
    },
    Boundaries,
    ImportAddresses,
    /// Context records ordered and given their ids.
    Contexts,
    Streets,
    /// Addresses in street order written to the address book, when
    /// imported rows are matched against them.
    AddressBook,
    /// Repeated houses found among them.
    Houses,
    /// Addresses placed among the records.
    Addresses,
    /// Every record written, the parts held in memory sealed.
    Records,
    /// That many parts streamed to scratch copied into the Pack.
    Seal(u8),
    Sealed,
}

/// The passes over one input, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(super) enum Pass {
    Scan,
    Members,
    Nodes,
    Ways,
}

/// Parts [`crate::pack::Sealing`] copies in from scratch.
pub(super) const SEALED_PARTS: u8 = 3;

impl Step {
    /// Every step of a build of `inputs` inputs, in order.
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

    /// The stage of the build's progress the step completes or is part of.
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

    /// The step as people read it.
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

/// Everything a checkpoint saves. Each field is the output of one stage,
/// live until the stage that reads it last.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct State {
    pub step: Step,
    pub seq: u64,
    /// The imported datasets addresses name by index.
    pub datasets: Vec<String>,
    /// The address books, from the stage that writes them to the record
    /// write.
    pub addresses: Option<AddressState>,
    pub report: BuilderReport,
    pub audit: AuditState,
    /// Sorters still open: every record, address, street and context record
    /// until the stage that orders them.
    pub sorters: Vec<SorterState>,
    /// Sorters already read, as the report counts them.
    pub sorted: BTreeMap<String, SorterReport>,
    pub next_file: u64,
    pub ledger: Ledger,
    /// What the scans counted, for the plan.
    pub scanned: Option<ScanCounts>,
    /// Boundary geometry, each input's part saved at its scan and its way
    /// pass, until the boundary stage.
    pub geometry: Vec<RunState>,
    /// The passes over the input being read.
    pub input: Option<InputState>,
    /// From the boundary stage to the record write.
    pub boundaries: Option<RunState>,
    /// The address files imported, once they are.
    pub imports: Option<Vec<AddressFile>>,
    /// From context ordering to the record write.
    pub contexts: Option<ContextState>,
    /// Postcode points, from address placement to the record write.
    pub postcodes: Option<RunState>,
    /// The training sample, from the record write to the training.
    pub sample: Option<(String, u64)>,
    pub sealing: Option<SealingState>,
    pub sealed: Option<SealedState>,
}

/// The address books a checkpoint names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct AddressState {
    /// The addresses in street order, before imported rows are matched.
    pub book: Option<IndexedState>,
    /// The addresses and the postcodes lent to their streets, as the
    /// records sorter points into them once they are placed.
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

/// One input's passes, between them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct InputState {
    pub index: usize,
    pub kept: KeptWays,
    pub node_blobs: RunState,
    pub way_blobs: RunState,
    pub kept_blobs: RunState,
    /// Until the node pass.
    pub node_set: Option<RunState>,
    pub tag_ids: Option<RunState>,
    /// From the member pass.
    pub members: Option<RunState>,
    /// From the node pass.
    pub locations: Option<LocationsState>,
}

/// Kept ways one scan counted, by what they become.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct KeptWays {
    pub addresses: u64,
    pub streets: u64,
    /// Ways that become records: every kind but boundaries.
    pub records: u64,
    pub all: u64,
}

/// Context records in order, and the ids they were given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ContextState {
    pub ordered: RunState,
    /// The record id of each boundary, by its index in the boundary set.
    pub boundary_ids: Vec<u64>,
    /// The record id of each derived country, by its code.
    pub country_ids: BTreeMap<String, u64>,
    /// Areas that are settlements.
    pub settled: Vec<u64>,
}

/// A number in a saved list, coded against the one before.
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

/// A postcode's point, from address placement to the record write.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PostcodePoint(pub String, pub [f64; 2]);

impl Spill for PostcodePoint {
    fn encode(&self, out: &mut Vec<u8>) {
        put_str(out, &self.0);
        for value in self.1 {
            out.extend_from_slice(&value.to_bits().to_le_bytes());
        }
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        let postcode = get_string(input)?;
        Ok(Self(postcode, [get_f64(input)?, get_f64(input)?]))
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.0.len()
    }
}

/// A part of the boundary geometry one input adds.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum GeometryItem {
    Relation(BoundaryRelationStub),
    /// A boundary way the scan kept.
    Feature(i64),
    Way(BoundaryWay),
    /// A relation member way's vertices, `None` when a node is missing.
    Member(i64, Option<Vec<Vertex>>),
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
                Self::Relation(BoundaryRelationStub {
                    object_id,
                    members,
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

/// Code a coordinate the node store gave, which is its 1e-7 fixed point
/// read as degrees, as that fixed point against `last`: most of a line's
/// points lie a few metres from the one before. Any other value is kept
/// whole, so every value reads back with the same bits.
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
                tags: BTreeMap::from([("name".to_string(), "Toronto".to_string())]),
            }),
            GeometryItem::Feature(605),
            GeometryItem::Way(BoundaryWay {
                object_id: 605,
                tags: BTreeMap::new(),
                vertices: vec![
                    vertex(30, 436_200_000, -794_300_000),
                    vertex(31, 436_200_000, -794_298_000),
                    // A point from elsewhere than the node store is kept whole.
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
        round_trip(&[PostcodePoint("M5V 1A1".into(), [-79.4, 43.6])]);
    }

    #[test]
    fn steps_run_in_order_input_by_input() {
        let steps = Step::all(2);
        assert!(steps.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(
            steps[..5],
            [
                Step::Input {
                    index: 0,
                    pass: Pass::Scan
                },
                Step::Input {
                    index: 0,
                    pass: Pass::Members
                },
                Step::Input {
                    index: 0,
                    pass: Pass::Nodes
                },
                Step::Input {
                    index: 0,
                    pass: Pass::Ways
                },
                Step::Input {
                    index: 1,
                    pass: Pass::Scan
                },
            ]
        );
        assert_eq!(steps.last(), Some(&Step::Sealed));
    }
}
