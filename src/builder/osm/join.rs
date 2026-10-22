//! The node join the node store replaced, kept only to compare their output.
//!
//! References were sorted by node id, merged against the node blocks into
//! coordinates per way position, sorted again by way, and read beside a spool
//! of the kept ways.

use std::{
    cell::Cell,
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
};

use osmpbf::{Element, PrimitiveBlock};

use crate::{
    builder::progress::BuildStage,
    error::{Error, Result},
    extsort::{ExternalSorter, Sorted, Spill, Spool, SpoolReader},
    util::codec::{
        get_i32, get_i64, get_tags, get_u8, get_u32, get_u64, put_i64, put_tags, put_u64,
    },
};

use super::{
    Build, Geometry, Passes,
    address::{collect_addr_tags_from_map, collect_clean_tags},
    boundary::{self, Vertex},
    emit::{FeatureOutput, WayNode, emit_feature},
    locations::Location,
    member_ways,
    pbf::for_each_block,
    scan::scan_block,
    spill::{WayFeature, WayKind},
};

thread_local! {
    static BY_JOIN: Cell<bool> = const { Cell::new(false) };
}

/// Whether builds on this thread read inputs through the join.
pub(super) fn enabled() -> bool {
    BY_JOIN.with(Cell::get)
}

pub(super) fn set(by_join: bool) {
    BY_JOIN.with(|cell| cell.set(by_join));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct NodeRequest {
    node_id: i64,
    owner: u64,
    pos: u32,
    want_tags: bool,
}

impl Spill for NodeRequest {
    fn encode(&self, out: &mut Vec<u8>) {
        put_i64(out, self.node_id);
        put_u64(out, self.owner);
        put_u64(out, u64::from(self.pos));
        out.push(u8::from(self.want_tags));
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Ok(Self {
            node_id: get_i64(input)?,
            owner: get_u64(input)?,
            pos: get_u32(input)?,
            want_tags: get_u8(input)? != 0,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ResolvedRef {
    owner: u64,
    pos: u32,
    node_id: i64,
    lat_e7: i32,
    lon_e7: i32,
    tags: Option<BTreeMap<String, String>>,
}

impl Spill for ResolvedRef {
    fn encode(&self, out: &mut Vec<u8>) {
        put_u64(out, self.owner);
        put_u64(out, u64::from(self.pos));
        put_i64(out, self.node_id);
        put_i64(out, i64::from(self.lat_e7));
        put_i64(out, i64::from(self.lon_e7));
        match &self.tags {
            Some(tags) => {
                out.push(1);
                put_tags(out, tags);
            }
            None => out.push(0),
        }
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Ok(Self {
            owner: get_u64(input)?,
            pos: get_u32(input)?,
            node_id: get_i64(input)?,
            lat_e7: get_i32(input)?,
            lon_e7: get_i32(input)?,
            tags: match get_u8(input)? {
                0 => None,
                _ => Some(get_tags(input)?),
            },
        })
    }
}

/// A kept way as the spool held it, with its node count.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SpooledWay {
    feature: WayFeature,
    node_count: u32,
}

impl Spill for SpooledWay {
    fn encode(&self, out: &mut Vec<u8>) {
        out.push(match self.feature.kind {
            WayKind::Address => 0,
            WayKind::Street => 1,
            WayKind::Interpolation => 2,
            WayKind::Boundary => 3,
            WayKind::Poi => 4,
        });
        put_i64(out, self.feature.way_id);
        put_u64(out, u64::from(self.node_count));
        put_tags(out, &self.feature.tags);
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        let kind = match get_u8(input)? {
            0 => WayKind::Address,
            1 => WayKind::Street,
            2 => WayKind::Interpolation,
            3 => WayKind::Boundary,
            4 => WayKind::Poi,
            _ => return Err(Error::Decode("unknown way feature kind")),
        };
        let way_id = get_i64(input)?;
        let node_count = get_u32(input)?;
        Ok(Self {
            feature: WayFeature {
                kind,
                way_id,
                tags: get_tags(input)?,
            },
            node_count,
        })
    }
}

fn request_nodes(
    requests: &mut ExternalSorter<NodeRequest>,
    owner: u64,
    refs: &[i64],
    want_tags: bool,
) -> Result<()> {
    for (pos, node_id) in refs.iter().enumerate() {
        requests.push(NodeRequest {
            node_id: *node_id,
            owner,
            pos: u32::try_from(pos)
                .map_err(|_| Error::BadInput("way has too many nodes".into()))?,
            want_tags,
        })?;
    }
    Ok(())
}

/// The old per-input passes, as `read_input` before the node store.
pub(super) fn read_input(
    input: &Path,
    build: &mut Build,
    geometry: &mut Geometry,
    passes: &Passes,
    budget: usize,
) -> Result<()> {
    let (scratch, memory, progress) = (passes.scratch, passes.memory, passes.progress);
    let mut requests = ExternalSorter::new(scratch, "node-requests", memory, budget);
    let mut features = Spool::<SpooledWay>::create(scratch, "way-features")?;
    let mut node_blobs = Vec::new();
    let mut way_blobs = Vec::new();
    for_each_block(
        input,
        progress,
        BuildStage::Scan,
        None,
        |offset, block| Ok((offset, scan_block(&block))),
        |(offset, output)| {
            if output.has_nodes {
                node_blobs.push(offset);
            }
            if output.has_ways {
                way_blobs.push(offset);
            }
            for (feature, refs) in output.ways {
                let resolution = &mut build.report.geometry_resolution;
                match feature.kind {
                    WayKind::Address => resolution.address_way_stubs += 1,
                    WayKind::Poi => resolution.poi_way_stubs += 1,
                    WayKind::Street => resolution.street_way_stubs += 1,
                    WayKind::Interpolation => resolution.interpolation_way_stubs += 1,
                    WayKind::Boundary => {
                        resolution.boundary_way_stubs += 1;
                        geometry.boundary_feature_ids.insert(feature.way_id);
                    }
                }
                request_nodes(
                    &mut requests,
                    features.written(),
                    &refs,
                    feature.kind == WayKind::Interpolation,
                )?;
                features.write(&SpooledWay {
                    node_count: refs.len() as u32,
                    feature,
                })?;
            }
            geometry.relations.extend(output.relations);
            build.absorb(output.emitted)
        },
    )?;

    let member_base = features.written();
    let member_ids = boundary::required_boundary_way_ids(&geometry.relations)
        .into_iter()
        .filter(|way_id| !geometry.boundary_feature_ids.contains(way_id))
        .collect::<HashSet<_>>();
    let mut members: HashMap<i64, (u64, usize)> = HashMap::new();
    if member_ids.is_empty() {
        progress.start(BuildStage::BoundaryMembers, Some(0))?;
        progress.finish(BuildStage::BoundaryMembers);
    } else {
        for_each_block(
            input,
            progress,
            BuildStage::BoundaryMembers,
            Some(&way_blobs),
            |_, block| Ok(member_ways(&block, &member_ids)),
            |found| {
                for (way_id, refs) in found {
                    if members.contains_key(&way_id) {
                        continue;
                    }
                    let owner = member_base + members.len() as u64;
                    request_nodes(&mut requests, owner, &refs, false)?;
                    members.insert(way_id, (owner, refs.len()));
                }
                Ok(())
            },
        )?;
    }

    build.report.geometry_resolution.required_node_refs += requests.stats().items;
    let mut resolved =
        ExternalSorter::<ResolvedRef>::new(scratch, "resolved-nodes", memory, budget);
    let mut join = Join {
        requests: requests.finish()?,
        current: None,
        last_node_id: None,
        resolved: 0,
    };
    join.advance()?;
    for_each_block(
        input,
        progress,
        BuildStage::NodeJoin,
        Some(&node_blobs),
        |_, block| Ok(decode_nodes(&block)),
        |block| join.block(block, &mut resolved),
    )?;
    build.report.geometry_resolution.resolved_node_refs += join.resolved;

    progress.start(BuildStage::AssembleWays, Some(0))?;
    let mut input_member_vertices = emit_features(
        features.into_reader()?,
        resolved.finish()?,
        |output: FeatureOutput| {
            geometry.boundary_ways.extend(output.boundary_ways);
            build.absorb(output.emitted)
        },
    )?;
    progress.finish(BuildStage::AssembleWays);
    for (way_id, (owner, count)) in members {
        let line = input_member_vertices
            .remove(&owner)
            .filter(|vertices| vertices.len() == count);
        geometry.member_vertices.insert(way_id, line);
    }
    Ok(())
}

#[derive(Debug, Default)]
struct NodeBlock {
    nodes: Vec<(i64, i32, i32)>,
    addr_tags: Vec<(usize, BTreeMap<String, String>)>,
}

fn decode_nodes(block: &PrimitiveBlock) -> NodeBlock {
    let mut output = NodeBlock::default();
    for element in block.elements() {
        let (id, lat, lon, addr_tags) = match element {
            Element::DenseNode(node) => (
                node.id(),
                node.decimicro_lat(),
                node.decimicro_lon(),
                node.tags()
                    .any(|(key, _)| key.starts_with("addr:"))
                    .then(|| collect_addr_tags_from_map(&collect_clean_tags(node.tags()))),
            ),
            Element::Node(node) => (
                node.id(),
                node.decimicro_lat(),
                node.decimicro_lon(),
                node.tags()
                    .any(|(key, _)| key.starts_with("addr:"))
                    .then(|| collect_addr_tags_from_map(&collect_clean_tags(node.tags()))),
            ),
            Element::Way(_) | Element::Relation(_) => continue,
        };
        if let Some(tags) = addr_tags.filter(|tags| !tags.is_empty()) {
            output.addr_tags.push((output.nodes.len(), tags));
        }
        output.nodes.push((id, lat, lon));
    }
    output
}

struct Join {
    requests: Sorted<NodeRequest>,
    current: Option<NodeRequest>,
    last_node_id: Option<i64>,
    resolved: u64,
}

impl Join {
    fn advance(&mut self) -> Result<()> {
        self.current = self.requests.next().transpose()?;
        Ok(())
    }

    fn block(
        &mut self,
        block: NodeBlock,
        resolved: &mut ExternalSorter<ResolvedRef>,
    ) -> Result<()> {
        let mut tags = block.addr_tags.into_iter().peekable();
        for (position, (node_id, lat_e7, lon_e7)) in block.nodes.into_iter().enumerate() {
            if let Some(last) = self.last_node_id
                && node_id <= last
            {
                return Err(Error::BadInput(format!(
                    "node {node_id} follows node {last}: the input PBF must be sorted by id \
                     (sort it with `osmium sort`)"
                )));
            }
            self.last_node_id = Some(node_id);
            while let Some(request) = self.current
                && request.node_id < node_id
            {
                self.advance()?;
            }
            while tags
                .next_if(|(tag_position, _)| *tag_position < position)
                .is_some()
            {}
            while let Some(request) = self.current
                && request.node_id == node_id
            {
                let node_tags = match tags.peek() {
                    Some((tag_position, tags))
                        if request.want_tags && *tag_position == position =>
                    {
                        Some(tags.clone())
                    }
                    _ => None,
                };
                resolved.push(ResolvedRef {
                    owner: request.owner,
                    pos: request.pos,
                    node_id,
                    lat_e7,
                    lon_e7,
                    tags: node_tags,
                })?;
                self.resolved += 1;
                self.advance()?;
            }
        }
        Ok(())
    }
}

fn emit_features(
    features: SpoolReader<SpooledWay>,
    resolved: Sorted<ResolvedRef>,
    mut sink: impl FnMut(FeatureOutput) -> Result<()>,
) -> Result<HashMap<u64, Vec<Vertex>>> {
    let mut resolved = resolved.peekable();
    for (owner, way) in features.enumerate() {
        let way = way?;
        let mut refs = Vec::new();
        while let Some(next) = resolved.next_if(|next| {
            next.as_ref()
                .map_or(true, |next| next.owner <= owner as u64)
        }) {
            refs.push(next?);
        }
        let nodes = (refs.len() == way.node_count as usize).then(|| {
            refs.into_iter()
                .map(|node| WayNode {
                    id: node.node_id,
                    location: Location {
                        lat_e7: node.lat_e7,
                        lon_e7: node.lon_e7,
                    },
                    tags: node.tags,
                })
                .collect()
        });
        let mut output = FeatureOutput::default();
        emit_feature(way.feature, nodes, &mut output);
        sink(output)?;
    }
    let mut members: HashMap<u64, Vec<Vertex>> = HashMap::new();
    for item in resolved {
        let item = item?;
        let location = Location {
            lat_e7: item.lat_e7,
            lon_e7: item.lon_e7,
        };
        members.entry(item.owner).or_default().push(Vertex {
            node_id: item.node_id,
            lat: location.lat(),
            lon: location.lon(),
        });
    }
    Ok(members)
}
