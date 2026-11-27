use std::collections::BTreeMap;

use super::spill::PendingAddress;
use crate::{
    builder::{
        places::Place,
        report::{BuilderReport, CandidateIssue},
    },
    error::Result,
    record::{ObjectType, Record, RejectedRecord, SourceProvenance},
};

#[derive(Debug, Default)]
pub(crate) struct Emitted {
    pub report: BuilderReport,
    pub records: Vec<Record>,
    pub ready: Vec<Ready>,
    pub places: Vec<Place>,
    pub rejections: Vec<RejectedRecord>,
}

#[derive(Debug)]
pub(crate) enum Ready {
    Address(PendingAddress),
    Record(Box<Record>),
}

impl Emitted {
    pub(crate) fn prepare(&mut self) -> Result<()> {
        self.ready = std::mem::take(&mut self.records)
            .into_iter()
            .map(|record| match record {
                Record::Address(address) if address.source.object_type != ObjectType::Row => {
                    PendingAddress::new(&address, 0, 0).map(Ready::Address)
                }
                record => Ok(Ready::Record(Box::new(record))),
            })
            .collect::<Result<_>>()?;
        Ok(())
    }

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
