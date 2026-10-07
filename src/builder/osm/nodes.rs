//! Pass 3: join node coordinates onto way references.
//!
//! The node section of a PBF is sorted by id, and the references are sorted by
//! node id by an external sort, so the join is a single forward merge: no node
//! table is ever held in memory, and nodes nobody asked for are skipped as they
//! stream past. Workers decode coordinates, plus address tags for the few
//! nodes that have any, so interpolation anchors need no second read.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, bail};
use osmpbf::{Element, PrimitiveBlock};

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

/// Nodes of one PBF block, in file order.
#[derive(Debug, Default)]
struct NodeBlock {
    nodes: Vec<(i64, i32, i32)>,
    /// Address tags by position in `nodes`, ascending; only nodes that have
    /// an `addr:` key, about one in a hundred.
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

pub(crate) fn join_nodes(
    input: &Path,
    node_blobs: &[u64],
    requests: Sorted<NodeRequest>,
    resolved: &mut ExternalSorter<ResolvedRef>,
) -> Result<JoinStats> {
    let mut join = Join {
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
        |_, block| Ok(decode_nodes(&block)),
        |block| join.block(block, resolved),
    )?;
    while join.current.is_some() {
        join.stats.missing += 1;
        join.advance()?;
    }
    Ok(join.stats)
}

struct Join {
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
        let mut tags = block.addr_tags.into_iter().peekable();
        for (position, (node_id, lat_e7, lon_e7)) in block.nodes.into_iter().enumerate() {
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
                self.stats.resolved += 1;
                self.advance()?;
            }
        }
        Ok(())
    }
}
