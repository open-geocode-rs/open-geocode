//! Pass 3: join node coordinates onto way references.
//!
//! The node section of a PBF is sorted by id, and the references are sorted by
//! node id by an external sort, so the join is a single forward merge: no node
//! table is ever held in memory, and nodes nobody asked for are skipped as they
//! stream past.

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
    /// Address tags by index into `nodes`, ascending.
    addr_tags: Vec<(usize, BTreeMap<String, String>)>,
}

fn decode_nodes(block: &PrimitiveBlock) -> NodeBlock {
    let mut output = NodeBlock::default();
    let mut push = |id, lat, lon, tags: BTreeMap<String, String>| {
        let addr_tags = collect_addr_tags_from_map(&tags);
        if !addr_tags.is_empty() {
            output.addr_tags.push((output.nodes.len(), addr_tags));
        }
        output.nodes.push((id, lat, lon));
    };
    for element in block.elements() {
        match element {
            Element::DenseNode(node) => {
                let tags = if node.tags().next().is_some() {
                    collect_clean_tags(node.tags())
                } else {
                    BTreeMap::new()
                };
                push(node.id(), node.decimicro_lat(), node.decimicro_lon(), tags);
            }
            Element::Node(node) => push(
                node.id(),
                node.decimicro_lat(),
                node.decimicro_lon(),
                collect_clean_tags(node.tags()),
            ),
            Element::Way(_) | Element::Relation(_) => {}
        }
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
        |_, block| Ok(decode_nodes(block)),
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
        for (index, (node_id, lat_e7, lon_e7)) in block.nodes.into_iter().enumerate() {
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
            while tags.peek().is_some_and(|(tag_index, _)| *tag_index < index) {
                tags.next();
            }
            while let Some(request) = self.current
                && request.node_id == node_id
            {
                let node_tags = match tags.peek() {
                    Some((tag_index, tags)) if request.want_tags && *tag_index == index => {
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
