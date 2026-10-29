use std::collections::BTreeMap;

use super::spill::PendingAddress;
use crate::{
    builder::report::{BuilderReport, CandidateIssue},
    error::Result,
    record::{ObjectType, Record, RejectedRecord, SourceProvenance},
};

/// Everything one worker produces for a batch of OSM objects. Workers run in
/// parallel; their outputs are merged in input order.
#[derive(Debug, Default)]
pub(crate) struct Emitted {
    pub report: BuilderReport,
    pub records: Vec<Record>,
    /// What [`Emitted::prepare`] made of `records`, which it empties.
    pub ready: Vec<Ready>,
    pub rejections: Vec<RejectedRecord>,
}

/// A record as the merge takes it.
#[derive(Debug)]
pub(crate) enum Ready {
    /// An address cut into its sort key, but for the sequence number the
    /// merge gives it.
    Address(PendingAddress),
    /// Boxed: a record is several times an address key's size, and most
    /// of a batch is addresses.
    Record(Box<Record>),
}

impl Emitted {
    /// Cut every address into its sort key. The merge of a batch's outputs
    /// is one thread's work in input order; this is the part of it that
    /// needs no order, left to the workers.
    pub(crate) fn prepare(&mut self) -> Result<()> {
        self.ready = std::mem::take(&mut self.records)
            .into_iter()
            .map(|record| match record {
                // A row's dataset is numbered in input order, by the merge.
                Record::Address(address) if address.source.object_type != ObjectType::Row => {
                    PendingAddress::new(address, 0, 0).map(Ready::Address)
                }
                record => Ok(Ready::Record(Box::new(record))),
            })
            .collect::<Result<_>>()?;
        Ok(())
    }

    /// Count a rejection in the report and keep the rejected object for the
    /// build audit.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reject(
        &mut self,
        issue: CandidateIssue,
        object_type: ObjectType,
        object_id: i64,
        tags: &BTreeMap<String, String>,
        addr_tags: Option<&BTreeMap<String, String>>,
        layer_hint: Option<&'static str>,
    ) {
        self.report.reject_with_context(
            issue,
            object_type,
            object_id,
            tags,
            addr_tags,
            layer_hint,
            true,
        );
        self.rejections.push(RejectedRecord {
            reason: issue.as_str().to_string(),
            layer_hint: layer_hint.map(str::to_string),
            source: SourceProvenance::osm_with_tags(object_type, object_id, tags.clone()),
        });
    }

    /// Count a rejection in the report only; nothing is kept for the audit.
    pub(crate) fn reject_in_report(
        &mut self,
        issue: CandidateIssue,
        object_type: ObjectType,
        object_id: i64,
        tags: &BTreeMap<String, String>,
        addr_tags: Option<&BTreeMap<String, String>>,
        layer_hint: Option<&'static str>,
    ) {
        self.report.reject_with_context(
            issue,
            object_type,
            object_id,
            tags,
            addr_tags,
            layer_hint,
            false,
        );
    }
}
