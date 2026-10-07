use std::collections::BTreeMap;

use crate::{
    builder::report::{BuilderReport, CandidateIssue},
    record::{OsmObjectType, Record, RejectedRecord, SourceProvenance},
};

use super::postcode::PostcodeAccumulator;

/// Everything one worker produces for a batch of OSM objects. Workers run in
/// parallel; their outputs are merged in input order.
#[derive(Debug, Default)]
pub(crate) struct Emitted {
    pub report: BuilderReport,
    pub records: Vec<Record>,
    pub rejections: Vec<RejectedRecord>,
    pub postcodes: PostcodeAccumulator,
}

impl Emitted {
    /// Count a rejection in the report and keep the rejected object for the
    /// build audit.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reject(
        &mut self,
        issue: CandidateIssue,
        object_type: OsmObjectType,
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
        object_type: OsmObjectType,
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
