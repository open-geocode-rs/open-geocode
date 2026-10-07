//! Pass 3: join node coordinates onto way references.
//!
//! The node section of a PBF is sorted by id, and the references are sorted by
//! node id by an external sort, so the join is a single forward merge: no node
//! table is ever held in memory, and nodes nobody asked for are skipped as they
//! stream past. Workers decode only coordinates and drop the block; the rare
//! block holding a node whose tags an interpolation way asked about is read
//! again from the file for just those tags.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use std::{fs::File, io::BufReader};

use anyhow::{Context, Result, bail};
use osmpbf::{BlobReader, ByteOffset, Element, PrimitiveBlock};

use crate::extsort::{ExternalSorter, Sorted};

use super::{
    address::{collect_addr_tags_from_map, collect_clean_tags},
    pbf::for_each_block,
    spill::{NodeRequest, ResolvedRef},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct JoinStats {
    pub resolved: u64,
    pub missing: u64,
}

/// Node coordinates of one PBF block in file order, and where the block is.
struct NodeBlock {
    offset: u64,
    nodes: Vec<(i64, i32, i32)>,
}

fn decode_nodes(offset: u64, block: PrimitiveBlock) -> NodeBlock {
    let mut nodes = Vec::new();
    for element in block.elements() {
        match element {
            Element::DenseNode(node) => {
                nodes.push((node.id(), node.decimicro_lat(), node.decimicro_lon()));
            }
            Element::Node(node) => {
                nodes.push((node.id(), node.decimicro_lat(), node.decimicro_lon()));
            }
            Element::Way(_) | Element::Relation(_) => {}
        }
    }
    NodeBlock { offset, nodes }
}

/// Address tags of the nodes at the given ascending positions within the
/// block's nodes.
fn addr_tags_at(
    block: &PrimitiveBlock,
    positions: &[usize],
) -> HashMap<usize, BTreeMap<String, String>> {
    let mut wanted = positions.iter().copied().peekable();
    let mut tags = HashMap::new();
    let mut position = 0;
    for element in block.elements() {
        let wanted_here = wanted.peek() == Some(&position);
        let node_tags = match element {
            Element::DenseNode(node) => wanted_here.then(|| collect_clean_tags(node.tags())),
            Element::Node(node) => wanted_here.then(|| collect_clean_tags(node.tags())),
            Element::Way(_) | Element::Relation(_) => continue,
        };
        if let Some(node_tags) = node_tags {
            wanted.next();
            let addr_tags = collect_addr_tags_from_map(&node_tags);
            if !addr_tags.is_empty() {
                tags.insert(position, addr_tags);
            }
            if wanted.peek().is_none() {
                break;
            }
        }
        position += 1;
    }
    tags
}

pub(crate) fn join_nodes(
    input: &Path,
    node_blobs: &[u64],
    requests: Sorted<NodeRequest>,
    resolved: &mut ExternalSorter<ResolvedRef>,
) -> Result<JoinStats> {
    let mut join = Join {
        blobs: BlobReader::seekable_from_path(input)
            .with_context(|| format!("failed to open {}", input.display()))?,
        requests,
        current: None,
        last_node_id: None,
        stats: JoinStats::default(),
    };
    join.advance()?;
    for_each_block(
        input,
        "3/7 join node coordinates",
        Some(node_blobs),
        |offset, block| Ok(decode_nodes(offset, block)),
        |block| join.block(block, resolved),
    )?;
    while join.current.is_some() {
        join.stats.missing += 1;
        join.advance()?;
    }
    Ok(join.stats)
}

struct Join {
    /// Re-reads blocks whose node tags were asked for.
    blobs: BlobReader<BufReader<File>>,
    requests: Sorted<NodeRequest>,
    current: Option<NodeRequest>,
    last_node_id: Option<i64>,
    stats: JoinStats,
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
        // Matches whose node tags were asked for, by position in the block.
        let mut needs_tags: Vec<(usize, ResolvedRef)> = Vec::new();
        for (position, (node_id, lat_e7, lon_e7)) in block.nodes.iter().copied().enumerate() {
            if let Some(last) = self.last_node_id
                && node_id <= last
            {
                bail!(
                    "node {node_id} follows node {last}: the input PBF must be sorted by id \
                     (sort it with `osmium sort`)"
                );
            }
            self.last_node_id = Some(node_id);

            while let Some(request) = self.current
                && request.node_id < node_id
            {
                self.stats.missing += 1;
                self.advance()?;
            }
            while let Some(request) = self.current
                && request.node_id == node_id
            {
                let reference = ResolvedRef {
                    owner: request.owner,
                    pos: request.pos,
                    node_id,
                    lat_e7,
                    lon_e7,
                    tags: None,
                };
                if request.want_tags {
                    needs_tags.push((position, reference));
                } else {
                    resolved.push(reference)?;
                }
                self.stats.resolved += 1;
                self.advance()?;
            }
        }

        if !needs_tags.is_empty() {
            let mut positions = needs_tags
                .iter()
                .map(|(position, _)| *position)
                .collect::<Vec<_>>();
            positions.dedup();
            let blob = self
                .blobs
                .blob_from_offset(ByteOffset(block.offset))
                .context("failed to re-read a node block")?;
            let tags = addr_tags_at(&blob.to_primitiveblock()?, &positions);
            for (position, mut reference) in needs_tags {
                reference.tags = tags.get(&position).cloned();
                resolved.push(reference)?;
            }
        }
        Ok(())
    }
}
