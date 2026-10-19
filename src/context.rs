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
//!                   len u8, then len x (record_id u32, tiers u16)
//!                 tiers is a bit per PlaceLayer discriminant, so one area
//!                 can be two things at once; chains sorted by outermost
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
/// 3 gives each entry a set of tiers; 2 gave it one; 1 held a fixed
/// six-slot tuple.
pub const CONTEXT_VERSION: u32 = 3;
pub const CONTEXT_FLAG_AMBIGUOUS_ADMIN: u16 = 1;

const ENTRY_BYTES: usize = 6;
/// Bounds one chain so a corrupt length cannot drive a huge allocation. No
/// country maps anywhere near this many tiers.
const MAX_CHAIN: usize = 32;

/// The tiers one area is, as a bit per [`PlaceLayer`].
///
/// An area is usually one tier, but some are two at once and no ranking can
/// choose between them without losing something true: Berlin is a city and a
/// German state, Tokyo a city and a prefecture, Singapore a city and a
/// country, Washington DC a city and a top-level region. With one tier per
/// area a query for a city named Berlin and a query for the states of
/// Germany cannot both be answered -- whichever tier is stored, the other
/// fact is gone. Google's `address_components` carries the same idea as its
/// `types` array (`["locality", "administrative_area_level_1"]`); Nominatim
/// instead keeps the city and drops the state, which is why its address of a
/// Berlin street has no `state` at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TierSet(u16);

impl TierSet {
    pub fn of(tier: PlaceLayer) -> Self {
        Self(1 << tier as u16)
    }

    pub fn contains(self, tier: PlaceLayer) -> bool {
        self.0 & Self::of(tier).0 != 0
    }

    pub fn insert(&mut self, tier: PlaceLayer) {
        self.0 |= Self::of(tier).0;
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The tiers, outermost first.
    pub fn iter(self) -> impl Iterator<Item = PlaceLayer> {
        PlaceLayer::ALL
            .into_iter()
            .filter(move |tier| self.contains(*tier))
    }

    /// The outermost tier, which is where the area sorts in a chain.
    pub fn outermost(self) -> Option<PlaceLayer> {
        self.iter().next()
    }

    fn from_bits(bits: u16) -> Option<Self> {
        let known = PlaceLayer::ALL
            .into_iter()
            .fold(0, |known, tier| known | Self::of(tier).0);
        (bits & !known == 0).then_some(Self(bits))
    }

    fn bits(self) -> u16 {
        self.0
    }
}

impl FromIterator<PlaceLayer> for TierSet {
    fn from_iter<I: IntoIterator<Item = PlaceLayer>>(tiers: I) -> Self {
        let mut set = Self::default();
        for tier in tiers {
            set.insert(tier);
        }
        set
    }
}

/// One area a record lies in, and everything that area is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AdminEntry {
    pub record_id: RecordId,
    pub tiers: TierSet,
}

impl AdminEntry {
    /// Where the area sorts in the chain: its outermost tier.
    fn rank(&self) -> Option<PlaceLayer> {
        self.tiers.outermost()
    }
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

    /// Puts an area in the chain under one tier. An area already in the
    /// chain gains the tier rather than taking a second place in it, which
    /// is how one boundary comes to be both a city and a state.
    pub fn set(&mut self, tier: PlaceLayer, record_id: RecordId) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.record_id == record_id)
        {
            entry.tiers.insert(tier);
            self.sort();
            return;
        }
        // An area already holding the tier is replaced, so a caller walking
        // boundaries outermost-in keeps the innermost of a tier.
        match self
            .entries
            .iter_mut()
            .find(|entry| entry.tiers.contains(tier))
        {
            Some(entry) => {
                entry.record_id = record_id;
                self.sort();
            }
            None => {
                let entry = AdminEntry {
                    record_id,
                    tiers: TierSet::of(tier),
                };
                let at = self
                    .entries
                    .partition_point(|held| held.rank() < entry.rank());
                self.entries.insert(at, entry);
            }
        }
    }

    fn sort(&mut self) {
        self.entries.sort_by_key(AdminEntry::rank);
    }

    /// The area that is `tier`. An area that is two things is found by
    /// either, so a Berlin address answers both a query for its city and
    /// one for its state.
    pub fn get(&self, tier: PlaceLayer) -> Option<RecordId> {
        self.entries
            .iter()
            .find(|entry| entry.tiers.contains(tier))
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
            .find(|entry| {
                entry
                    .tiers
                    .iter()
                    .any(|at| at >= tier && at.record_layer() == group)
            })
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
    /// they want: tests, and the fixed-shape conversions at the edges. A
    /// record id given more than once holds every tier it was given.
    pub fn from_tiers<I>(tiers: I) -> Self
    where
        I: IntoIterator<Item = (PlaceLayer, RecordId)>,
    {
        let mut chain = Self::default();
        for (tier, record_id) in tiers {
            chain.set(tier, record_id);
        }
        chain
    }
}

impl FromIterator<AdminEntry> for AdminContextTuple {
    fn from_iter<I: IntoIterator<Item = AdminEntry>>(entries: I) -> Self {
        let mut chain = Self::default();
        for entry in entries {
            if entry.tiers.is_empty() {
                continue;
            }
            match chain
                .entries
                .iter_mut()
                .find(|held| held.record_id == entry.record_id)
            {
                Some(held) => {
                    for tier in entry.tiers.iter() {
                        held.tiers.insert(tier);
                    }
                }
                None => chain.entries.push(entry),
            }
        }
        chain.sort();
        chain
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
                bytes.extend_from_slice(&entry.tiers.bits().to_le_bytes());
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
            let bits = u16::from_le_bytes([self.tuples[at + 4], self.tuples[at + 5]]);
            let tiers = TierSet::from_bits(bits)
                .ok_or_else(|| Error::Inconsistent("unknown context tier".into()))?;
            entries.push(AdminEntry {
                record_id: RecordId::from(record_id),
                tiers,
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

    /// The bits are the on-disk encoding, so every tier has to survive the
    /// round trip and a bit no tier claims has to be refused rather than
    /// read as some other tier.
    #[test]
    fn every_tier_round_trips_through_its_bit() {
        for tier in PlaceLayer::ALL {
            let set = TierSet::of(tier);
            assert_eq!(TierSet::from_bits(set.bits()), Some(set), "{tier:?}");
            assert!(set.contains(tier));
            assert_eq!(set.outermost(), Some(tier));
        }
        let all = PlaceLayer::ALL.into_iter().collect::<TierSet>();
        assert_eq!(all.iter().count(), PlaceLayer::ALL.len(), "every tier");
        assert_eq!(
            all.outermost(),
            Some(PlaceLayer::Country),
            "outermost first"
        );
        assert_eq!(
            TierSet::from_bits(1 << PlaceLayer::ALL.len()),
            None,
            "unknown bit"
        );
        assert_eq!(TierSet::from_bits(u16::MAX), None);
    }

    /// One area that is two tiers is found by either, and sorts at its
    /// outermost. Berlin is a city and a German state; before an entry
    /// carried a set, storing one of those erased the other.
    #[test]
    fn an_area_can_be_two_tiers_at_once() {
        let chain = AdminContextTuple::from_tiers([
            (PlaceLayer::Country, 1),
            (PlaceLayer::Region, 2),
            (PlaceLayer::Locality, 2),
        ]);

        assert_eq!(chain.get(PlaceLayer::Region), Some(2), "Berlin the state");
        assert_eq!(chain.get(PlaceLayer::Locality), Some(2), "Berlin the city");
        assert_eq!(chain.entries().len(), 2, "one entry, not two: {chain:?}");
        let berlin = chain.entries()[1];
        assert_eq!(
            berlin.tiers.iter().collect::<Vec<_>>(),
            [PlaceLayer::Region, PlaceLayer::Locality]
        );
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
