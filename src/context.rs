//! Boundary-derived admin context.
//!
//! A record's context is the chain of areas it lies in, outermost first. The
//! chain is variable length because a country's hierarchy is: Germany maps
//! nine admin levels, Monaco two. A fixed tuple of named slots has to choose
//! which of a country's levels to keep, and the ones it drops are gone from
//! every address in that country for the life of the Pack. A chain never
//! chooses, so the build keeps every boundary and the lossy step moves to the
//! response, where it is recomputed per request and costs nothing permanent.
//!
//! Most records share their chain with many neighbours, so chains are
//! interned. A record body stores only the chain id ([`ContextRef`]); this
//! module stores the chain table.
//!
//! ```text
//! context/tuples  chain_count u64, then per chain:
//!                   len u8, then len x (record_id u32, tier u8)
//!                 tiers are PlaceLayer discriminants, chains sorted by tier
//! ```

use std::collections::HashMap;

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::{
    container::{Bytes, Container, Writer},
    pack::RecordId,
    record::PlaceLayer,
    records::ContextRef,
    util::codec::{read_u32_le, read_u64_le},
};

pub const SECTION_TUPLES: &str = "context/tuples";
/// 2 holds a variable-length chain; 1 held a fixed six-slot tuple.
pub const CONTEXT_VERSION: u32 = 2;
pub const CONTEXT_FLAG_AMBIGUOUS_ADMIN: u16 = 1;

const ENTRY_BYTES: usize = 5;
/// Bounds one chain so a corrupt length cannot drive a huge allocation. No
/// country maps anywhere near this many tiers.
const MAX_CHAIN: usize = 32;

/// One area a record lies in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AdminEntry {
    pub record_id: RecordId,
    pub tier: PlaceLayer,
}

/// The tier a stored byte names. The discriminants are the on-disk encoding,
/// so a variant may not be reordered without a format version.
const fn tier_from_byte(byte: u8) -> Option<PlaceLayer> {
    Some(match byte {
        0 => PlaceLayer::Country,
        1 => PlaceLayer::Region,
        2 => PlaceLayer::SubRegion,
        3 => PlaceLayer::District,
        4 => PlaceLayer::SubDistrict,
        5 => PlaceLayer::Locality,
        6 => PlaceLayer::SubLocality,
        7 => PlaceLayer::Neighbourhood,
        8 => PlaceLayer::SubNeighbourhood,
        9 => PlaceLayer::Place,
        _ => return None,
    })
}

/// The areas a record lies in, outermost first, one per tier.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AdminContextTuple {
    entries: Vec<AdminEntry>,
}

/// Admin context of one record plus assignment flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordContext {
    pub admin_context: AdminContextTuple,
    pub flags: u16,
}

impl AdminContextTuple {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The chain, outermost first.
    pub fn entries(&self) -> &[AdminEntry] {
        &self.entries
    }

    /// Puts an area in the chain. A tier already held is replaced, so a
    /// caller that walks boundaries outermost-in keeps the innermost of a
    /// tier, as the fixed tuple did.
    pub fn set(&mut self, tier: PlaceLayer, record_id: RecordId) {
        // Placed by binary search rather than pushed and re-sorted: the
        // chain is short but this runs for every area of every record.
        match self.entries.binary_search_by_key(&tier, |entry| entry.tier) {
            Ok(at) => self.entries[at].record_id = record_id,
            Err(at) => self.entries.insert(at, AdminEntry { record_id, tier }),
        }
    }

    pub fn get(&self, tier: PlaceLayer) -> Option<RecordId> {
        self.entries
            .iter()
            .find(|entry| entry.tier == tier)
            .map(|entry| entry.record_id)
    }

    /// The area filling a tier, else the nearest one below it within the tier
    /// group: a `District` query takes a `SubDistrict` when no district
    /// covers the record. Lets a caller that wants one conventional tier stay
    /// one call, whatever depth the country maps.
    pub fn get_or_finer(&self, tier: PlaceLayer) -> Option<RecordId> {
        let group = tier.record_layer();
        self.entries
            .iter()
            .find(|entry| entry.tier >= tier && entry.tier.record_layer() == group)
            .map(|entry| entry.record_id)
    }

    /// Most specific first, place before the admin tiers.
    pub fn parent_record_ids(&self) -> impl Iterator<Item = RecordId> + '_ {
        self.entries.iter().rev().map(|entry| entry.record_id)
    }

    pub fn country_record_id(&self) -> Option<RecordId> {
        self.get(PlaceLayer::Country)
    }

    pub fn region_record_id(&self) -> Option<RecordId> {
        self.get(PlaceLayer::Region)
    }

    pub fn district_record_id(&self) -> Option<RecordId> {
        self.get(PlaceLayer::District)
    }

    pub fn locality_record_id(&self) -> Option<RecordId> {
        self.get(PlaceLayer::Locality)
    }

    pub fn neighbourhood_record_id(&self) -> Option<RecordId> {
        self.get(PlaceLayer::Neighbourhood)
    }

    pub fn place_record_id(&self) -> Option<RecordId> {
        self.get(PlaceLayer::Place)
    }
}

impl AdminContextTuple {
    /// A chain of `(tier, record id)` pairs, for callers that name the tiers
    /// they want: tests, and the fixed-shape conversions at the edges.
    pub fn from_tiers<I>(tiers: I) -> Self
    where
        I: IntoIterator<Item = (PlaceLayer, RecordId)>,
    {
        tiers
            .into_iter()
            .map(|(tier, record_id)| AdminEntry { record_id, tier })
            .collect()
    }
}

impl FromIterator<AdminEntry> for AdminContextTuple {
    fn from_iter<I: IntoIterator<Item = AdminEntry>>(entries: I) -> Self {
        let mut entries = entries.into_iter().collect::<Vec<_>>();
        entries.sort_unstable_by_key(|entry| entry.tier);
        entries.dedup_by_key(|entry| entry.tier);
        Self { entries }
    }
}

#[derive(Debug, Default)]
pub struct TupleWriter {
    tuples: Vec<AdminContextTuple>,
    ids: HashMap<AdminContextTuple, u32>,
}

impl TupleWriter {
    /// Reference for a record's context; `None` when there is no admin context.
    pub fn intern(&mut self, context: RecordContext) -> Result<Option<ContextRef>> {
        if context.admin_context.is_empty() {
            return Ok(None);
        }
        let tuple_id = match self.ids.get(&context.admin_context) {
            Some(id) => *id,
            None => {
                for entry in context.admin_context.entries() {
                    if entry.record_id >= u64::from(u32::MAX) {
                        return Err(Error::IdOutOfRange {
                            what: "context record",
                            id: entry.record_id,
                        });
                    }
                }
                if context.admin_context.entries().len() > MAX_CHAIN {
                    return Err(Error::Inconsistent(
                        "context chain is longer than the format allows".into(),
                    ));
                }
                let id = u32::try_from(self.tuples.len())
                    .map_err(|_| Error::Inconsistent("context table is full".into()))?;
                self.tuples.push(context.admin_context.clone());
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

    pub fn finish(self, pack: &mut Writer) -> Result<()> {
        let mut bytes = Vec::with_capacity(8 + self.tuples.len() * (1 + 4 * ENTRY_BYTES));
        bytes.extend_from_slice(&(self.tuples.len() as u64).to_le_bytes());
        for tuple in &self.tuples {
            bytes.push(tuple.entries().len() as u8);
            for entry in tuple.entries() {
                bytes.extend_from_slice(&(entry.record_id as u32).to_le_bytes());
                bytes.push(entry.tier as u8);
            }
        }
        pack.add(SECTION_TUPLES, CONTEXT_VERSION, &bytes)
    }
}

#[derive(Debug, Clone)]
pub struct Reader {
    tuples: Bytes,
    /// Byte offset of each chain. Chains vary in length, so there is no
    /// stride to multiply; the offsets are walked once at open.
    offsets: Vec<u32>,
}

impl Reader {
    pub fn open(container: &Container) -> Result<Self> {
        let tuples = container.section(SECTION_TUPLES, CONTEXT_VERSION)?;
        let count = read_u64_le(&tuples, 0)
            .ok_or(Error::Inconsistent("context table is truncated".into()))?;
        let truncated = || Error::Inconsistent("context table is truncated".into());
        let mut offsets = Vec::with_capacity(count as usize);
        let mut offset = 8usize;
        for _ in 0..count {
            let len = *tuples.get(offset).ok_or_else(truncated)? as usize;
            if len > MAX_CHAIN {
                return Err(Error::Inconsistent("context chain is too long".into()));
            }
            offsets.push(
                u32::try_from(offset)
                    .map_err(|_| Error::Inconsistent("context table is too large".into()))?,
            );
            offset = offset
                .checked_add(1 + len * ENTRY_BYTES)
                .ok_or_else(truncated)?;
            if offset > tuples.len() {
                return Err(truncated());
            }
        }
        if offset != tuples.len() {
            return Err(Error::Inconsistent(
                "context table does not match its chain count".into(),
            ));
        }
        Ok(Self { tuples, offsets })
    }

    pub fn resolve(&self, reference: ContextRef) -> Result<RecordContext> {
        let Some(&offset) = self.offsets.get(reference.tuple_id as usize) else {
            return Err(Error::IdOutOfRange {
                what: "context tuple",
                id: u64::from(reference.tuple_id),
            });
        };
        let offset = offset as usize;
        let len = self.tuples[offset] as usize;
        let mut entries = Vec::with_capacity(len);
        for index in 0..len {
            let at = offset + 1 + index * ENTRY_BYTES;
            let record_id = read_u32_le(&self.tuples, at).expect("validated table");
            let tier = tier_from_byte(self.tuples[at + 4])
                .ok_or_else(|| Error::Inconsistent("unknown context tier".into()))?;
            entries.push(AdminEntry {
                record_id: RecordId::from(record_id),
                tier,
            });
        }
        Ok(RecordContext {
            admin_context: entries.into_iter().collect(),
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

    /// The byte decoder is written out rather than searched, so it has to
    /// agree with the discriminants it mirrors. They are the on-disk
    /// encoding: a tier that decodes to the wrong one silently rewrites
    /// every chain of every Pack built before the drift.
    #[test]
    fn every_tier_round_trips_through_its_byte() {
        for tier in PlaceLayer::ALL {
            assert_eq!(tier_from_byte(tier as u8), Some(tier), "{tier:?}");
        }
        assert_eq!(
            tier_from_byte(PlaceLayer::ALL.len() as u8),
            None,
            "past the end"
        );
        assert_eq!(tier_from_byte(u8::MAX), None);
    }

    #[test]
    fn interns_tuples_and_resolves_references() {
        let path =
            std::env::temp_dir().join(format!("open-geocode-context-{}", uuid::Uuid::new_v4()));
        let tuple = AdminContextTuple::from_tiers([
            (PlaceLayer::Country, 1),
            (PlaceLayer::Region, 2),
            (PlaceLayer::Locality, 3),
        ]);
        let mut writer = TupleWriter::default();
        let first = writer
            .intern(RecordContext {
                admin_context: tuple.clone(),
                flags: 0,
            })
            .expect("intern")
            .expect("reference");
        let second = writer
            .intern(RecordContext {
                admin_context: tuple.clone(),
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

        let mut pack = Writer::create(&path).expect("pack");
        writer.finish(&mut pack).expect("finish");
        pack.finish().expect("pack finish");
        let reader = Reader::open(&Container::open(&path).expect("open")).expect("reader");
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
