//! Boundary-derived admin context.
//!
//! Most records share their admin context with many neighbours, so contexts are
//! interned as tuples of place record ids. A record body stores only the tuple
//! id ([`ContextRef`]); this module stores the tuple table.
//!
//! ```text
//! context/tuples  tuple_count u64, then per tuple 6 x u32 record ids
//!                 (country, region, district, locality, neighbourhood, place),
//!                 u32::MAX for absent
//! ```

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    container::{Bytes, Container, ContainerWriter},
    pack::RecordId,
    records::ContextRef,
    util::codec::{read_u32_le, read_u64_le},
};

pub const SECTION_TUPLES: &str = "context/tuples";
pub const CONTEXT_VERSION: u32 = 1;
pub const CONTEXT_FLAG_AMBIGUOUS_ADMIN: u16 = 1;

const TUPLE_BYTES: usize = 24;
const MISSING_ID: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AdminContextTuple {
    pub country_record_id: Option<RecordId>,
    pub region_record_id: Option<RecordId>,
    pub district_record_id: Option<RecordId>,
    pub locality_record_id: Option<RecordId>,
    pub neighbourhood_record_id: Option<RecordId>,
    pub place_record_id: Option<RecordId>,
}

/// Admin context of one record plus assignment flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordContext {
    pub admin_context: AdminContextTuple,
    pub flags: u16,
}

impl AdminContextTuple {
    pub fn is_empty(self) -> bool {
        self.ids().iter().all(Option::is_none)
    }

    /// Most specific first: place, neighbourhood, locality, district, region,
    /// country.
    pub fn parent_record_ids(self) -> impl Iterator<Item = RecordId> {
        let [country, region, district, locality, neighbourhood, place] = self.ids();
        [place, neighbourhood, locality, district, region, country]
            .into_iter()
            .flatten()
    }

    fn ids(self) -> [Option<RecordId>; 6] {
        [
            self.country_record_id,
            self.region_record_id,
            self.district_record_id,
            self.locality_record_id,
            self.neighbourhood_record_id,
            self.place_record_id,
        ]
    }

    fn from_ids(ids: [Option<RecordId>; 6]) -> Self {
        let [
            country_record_id,
            region_record_id,
            district_record_id,
            locality_record_id,
            neighbourhood_record_id,
            place_record_id,
        ] = ids;
        Self {
            country_record_id,
            region_record_id,
            district_record_id,
            locality_record_id,
            neighbourhood_record_id,
            place_record_id,
        }
    }
}

#[derive(Debug, Default)]
pub struct ContextTupleWriter {
    tuples: Vec<AdminContextTuple>,
    ids: HashMap<AdminContextTuple, u32>,
}

impl ContextTupleWriter {
    /// Reference for a record's context; `None` when there is no admin context.
    pub fn intern(&mut self, context: RecordContext) -> Result<Option<ContextRef>> {
        if context.admin_context.is_empty() {
            return Ok(None);
        }
        let tuple_id = match self.ids.get(&context.admin_context) {
            Some(id) => *id,
            None => {
                for id in context.admin_context.ids().into_iter().flatten() {
                    if id >= u64::from(MISSING_ID) {
                        bail!("context record id {id} does not fit the context table");
                    }
                }
                let id = u32::try_from(self.tuples.len()).context("context table is full")?;
                self.tuples.push(context.admin_context);
                self.ids.insert(context.admin_context, id);
                id
            }
        };
        Ok(Some(ContextRef {
            tuple_id,
            ambiguous: context.flags & CONTEXT_FLAG_AMBIGUOUS_ADMIN != 0,
        }))
    }

    pub fn tuple_count(&self) -> u64 {
        self.tuples.len() as u64
    }

    pub fn finish(self, pack: &mut ContainerWriter) -> Result<()> {
        let mut bytes = Vec::with_capacity(8 + self.tuples.len() * TUPLE_BYTES);
        bytes.extend_from_slice(&(self.tuples.len() as u64).to_le_bytes());
        for tuple in &self.tuples {
            for id in tuple.ids() {
                let id = id.map_or(MISSING_ID, |id| id as u32);
                bytes.extend_from_slice(&id.to_le_bytes());
            }
        }
        pack.add(SECTION_TUPLES, CONTEXT_VERSION, &bytes)
    }
}

#[derive(Clone)]
pub struct ContextReader {
    tuples: Bytes,
    count: u64,
}

impl ContextReader {
    pub fn open(container: &Container) -> Result<Self> {
        let tuples = container.section(SECTION_TUPLES, CONTEXT_VERSION)?;
        let count = read_u64_le(&tuples, 0).context("context table is truncated")?;
        if tuples.len() as u64 != 8 + count * TUPLE_BYTES as u64 {
            bail!("context table does not match its tuple count");
        }
        Ok(Self { tuples, count })
    }

    pub fn resolve(&self, reference: ContextRef) -> Result<RecordContext> {
        let id = u64::from(reference.tuple_id);
        if id >= self.count {
            bail!("context tuple {id} is out of range");
        }
        let offset = 8 + id as usize * TUPLE_BYTES;
        let ids = std::array::from_fn(|index| {
            let value = read_u32_le(&self.tuples, offset + index * 4).expect("validated table");
            (value != MISSING_ID).then_some(RecordId::from(value))
        });
        Ok(RecordContext {
            admin_context: AdminContextTuple::from_ids(ids),
            flags: if reference.ambiguous {
                CONTEXT_FLAG_AMBIGUOUS_ADMIN
            } else {
                0
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_tuples_and_resolves_references() {
        let path =
            std::env::temp_dir().join(format!("open-geocode-context-{}", uuid::Uuid::new_v4()));
        let tuple = AdminContextTuple {
            country_record_id: Some(1),
            region_record_id: Some(2),
            locality_record_id: Some(3),
            ..AdminContextTuple::default()
        };
        let mut writer = ContextTupleWriter::default();
        let first = writer
            .intern(RecordContext {
                admin_context: tuple,
                flags: 0,
            })
            .expect("intern")
            .expect("reference");
        let second = writer
            .intern(RecordContext {
                admin_context: tuple,
                flags: CONTEXT_FLAG_AMBIGUOUS_ADMIN,
            })
            .expect("intern")
            .expect("reference");
        assert_eq!(first.tuple_id, second.tuple_id);
        assert!(second.ambiguous);
        assert_eq!(
            writer.intern(RecordContext::default()).expect("empty"),
            None
        );
        assert_eq!(writer.tuple_count(), 1);

        let mut pack = ContainerWriter::create(&path).expect("pack");
        writer.finish(&mut pack).expect("finish");
        pack.finish().expect("pack finish");
        let reader = ContextReader::open(&Container::open(&path).expect("open")).expect("reader");
        let resolved = reader.resolve(second).expect("resolve");
        assert_eq!(resolved.admin_context, tuple);
        assert_eq!(resolved.flags, CONTEXT_FLAG_AMBIGUOUS_ADMIN);
        assert_eq!(
            resolved
                .admin_context
                .parent_record_ids()
                .collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
    }
}
