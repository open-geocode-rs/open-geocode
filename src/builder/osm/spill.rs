//! Scratch-file types for the streaming build.
//!
//! The build never holds all records in memory: finished records wait in
//! Hilbert-ordered sorters until they are written.

use std::collections::{BTreeMap, HashMap};

use crate::error::{Error, Result};
use geojson::{Geometry, GeometryValue};

use crate::{
    extsort::Spill,
    parse::lattice::CountryCode,
    record::{
        AddressComponents, AddressRecord, DerivedSourceProvenance, InterpolationAddressComponents,
        InterpolationRange, InterpolationRecord, Layer, LocationPrecision, ObjectType, PlaceRecord,
        PoiRecord, PostcodeRecord, Prominence, Record, SourceProvenance, StreetRecord,
        point_geometry,
    },
    util::{
        codec::{
            get_bytes, get_i64, get_opt_string, get_string, get_u8, get_u32, get_u64, put_bytes,
            put_i64, put_opt_str, put_str, put_u64,
        },
        geo::line_parts,
    },
};

use super::{book::Entry, boundary::ContextOrigin};

const COORDINATE_SCALE: f64 = 10_000_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WayKind {
    Address,
    Street,
    Interpolation,
    Boundary,
    Poi,
}

/// A way kept by the scan, without its node list: the way pass reads that
/// again from the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WayFeature {
    pub kind: WayKind,
    pub way_id: i64,
    pub tags: BTreeMap<String, String>,
}

/// A finished record waiting for its final position. `key` is the Hilbert
/// index of its display point; `seq` is emission order, which breaks ties and
/// keeps the build deterministic.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingRecord {
    pub key: u64,
    pub seq: u64,
    pub origin: Option<ContextOrigin>,
    pub body: Body,
}

/// What a pending record holds: the record, or for an address where the
/// address book keeps it, so an address is not copied into every sort it
/// passes through.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Body {
    Record(Box<Record>),
    Address(PlacedAddress),
}

impl From<Record> for Body {
    fn from(record: Record) -> Self {
        Self::Record(Box::new(record))
    }
}

/// Addresses group by street within a grid cell this many degrees wide
/// (about 2 km): a street with one name in two towns, or a long rural road,
/// becomes runs that each stay compact, so the record blocks a run fills
/// cover a small area for the spatial index.
const ADDRESS_GROUP_CELL_DEGREES: f64 = 0.02;

/// Bytes of a street order key that name its cell: latitude, then longitude.
const CELL_KEY_LEN: usize = 8;

/// Where an address comes from, naming an imported row's dataset by its
/// index in the build's [`Datasets`] instead of by name: a row would
/// otherwise carry its dataset's name through every scratch file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Source {
    pub object_type: ObjectType,
    pub object_id: i64,
    /// 0 for OSM.
    pub dataset: u32,
}

/// The datasets imported rows come from, each name stored once.
#[derive(Debug, Default)]
pub(crate) struct Datasets {
    names: Vec<String>,
    ids: HashMap<String, u32>,
}

impl Datasets {
    /// The index of `name`, given one the first time it is seen.
    pub(crate) fn id(&mut self, name: &str) -> u32 {
        if let Some(id) = self.ids.get(name) {
            return *id;
        }
        let id = self.names.len() as u32;
        self.names.push(name.to_string());
        self.ids.insert(name.to_string(), id);
        id
    }

    pub(crate) fn name(&self, id: u32) -> Result<&str> {
        self.names
            .get(id as usize)
            .map(String::as_str)
            .ok_or(Error::Decode("unknown address dataset"))
    }

    fn provenance(&self, source: Source) -> Result<SourceProvenance> {
        Ok(match source.object_type {
            ObjectType::Row => {
                SourceProvenance::imported(self.name(source.dataset)?, source.object_id)
            }
            object_type => SourceProvenance::osm(object_type, source.object_id),
        })
    }
}

/// An address on its way to being written with the rest of its street.
/// Sorted by its street, then by what it states but its number and unit,
/// then by number, so the addresses of one street come out together and in
/// order.
///
/// The sort key holds every component the address states, so the address is
/// its key, its point and where it comes from: nothing is stored twice. An
/// address arrives undisputed and lent no postcodes; both are decided after
/// it is sorted, and travel beside it from then on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingAddress {
    /// The group key, then the number key.
    pub order: Vec<u8>,
    /// Length of the start of `order` its street shares in street order: the
    /// cell and the street as search spells it.
    pub street_len: usize,
    /// Length of the start of `order` its group shares: its street's key and
    /// everything else it states.
    pub group_len: usize,
    pub seq: u64,
    pub point: [f64; 2],
    pub precision: LocationPrecision,
    pub source: Source,
}

/// The components a street order key holds, borrowed from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fields<'a> {
    pub street: Option<&'a str>,
    pub place: Option<&'a str>,
    pub locality: Option<&'a str>,
    pub region: Option<&'a str>,
    pub postcode: Option<&'a str>,
    pub country: Option<&'a str>,
    pub areas: Vec<&'a str>,
    pub number: &'a str,
    pub unit: Option<&'a str>,
}

impl<'a> Fields<'a> {
    /// What the house number is on, as [`AddressComponents::subject`].
    pub(crate) fn subject(&self) -> Option<&'a str> {
        self.street.or(self.place).or(self.areas.last().copied())
    }

    fn parse(order: &'a [u8]) -> Result<Self> {
        let mut input = order
            .get(CELL_KEY_LEN..)
            .ok_or(Error::Decode("address sort key is truncated"))?;
        get_order_text(&mut input)?;
        get_u8(&mut input)?;
        let mut texts = [None; 6];
        for text in &mut texts {
            *text = get_order_text(&mut input)?;
        }
        let [street, place, locality, region, postcode, country] = texts;
        let count = get_order_len(&mut input)?;
        if count > input.len() {
            return Err(Error::Decode("address sort key is truncated"));
        }
        let areas = (0..count)
            .map(|_| get_order_text(&mut input)?.ok_or(Error::Decode("address area is absent")))
            .collect::<Result<Vec<_>>>()?;
        input = input
            .get(4..)
            .ok_or(Error::Decode("address sort key is truncated"))?;
        let number =
            get_order_text(&mut input)?.ok_or(Error::Decode("address number is absent"))?;
        let unit = get_order_text(&mut input)?;
        if !input.is_empty() {
            return Err(Error::Decode("address sort key has trailing bytes"));
        }
        Ok(Self {
            street,
            place,
            locality,
            region,
            postcode,
            country,
            areas,
            number,
            unit,
        })
    }

    fn to_address(&self) -> AddressComponents {
        let owned = |value: Option<&str>| value.map(str::to_string);
        AddressComponents {
            areas: self.areas.iter().map(|area| area.to_string()).collect(),
            number: self.number.to_string(),
            street: owned(self.street),
            place: owned(self.place),
            unit: owned(self.unit),
            locality: owned(self.locality),
            region: owned(self.region),
            postcode: owned(self.postcode),
            country: owned(self.country),
        }
    }
}

impl PendingAddress {
    pub(super) fn new(record: AddressRecord, seq: u64, dataset: u32) -> Result<Self> {
        let point = crate::util::geo::point_lon_lat(&record.geometry)
            .ok_or_else(|| Error::BadGeometry(format!("address {} has no point", record.id())))?;
        debug_assert!(!record.disputed && record.street_postcodes.is_empty());
        let source = Source {
            object_type: record.source.object_type,
            object_id: record.source.object_id,
            dataset,
        };
        Ok(Self::from_parts(
            &record.address,
            point,
            record.location_precision,
            source,
            seq,
        ))
    }

    fn from_parts(
        address: &AddressComponents,
        point: [f64; 2],
        precision: LocationPrecision,
        source: Source,
        seq: u64,
    ) -> Self {
        let [lon, lat] = point;
        let mut order = Vec::new();
        // Two's complement, big-endian: the cells sort as they did as i64.
        let cell = |degrees: f64| (degrees / ADDRESS_GROUP_CELL_DEGREES).floor() as i32;
        order.extend_from_slice(&cell(lat).to_be_bytes());
        order.extend_from_slice(&cell(lon).to_be_bytes());
        put_order_text(
            &mut order,
            address
                .subject()
                .and_then(crate::text_index::street_search_text)
                .as_deref(),
        );
        let street_len = order.len();
        // Addresses stating a postcode first: the street's postcodes are all
        // known by the time its addresses without one come out.
        order.push(u8::from(address.postcode.is_none()));
        for field in [
            &address.street,
            &address.place,
            &address.locality,
            &address.region,
            &address.postcode,
            &address.country,
        ] {
            put_order_text(&mut order, field.as_deref());
        }
        put_order_len(&mut order, address.areas.len());
        for area in &address.areas {
            put_order_text(&mut order, Some(area));
        }
        let group_len = order.len();
        let base = crate::text_index::base_house_number(&address.number).unwrap_or(u32::MAX);
        order.extend_from_slice(&base.to_be_bytes());
        put_order_text(&mut order, Some(&address.number));
        put_order_text(&mut order, address.unit.as_deref());
        Self {
            order,
            street_len,
            group_len,
            seq,
            point,
            precision,
            source,
        }
    }

    /// The same address stating `address` instead.
    pub(super) fn restated(&self, address: &AddressComponents) -> Self {
        Self::from_parts(address, self.point, self.precision, self.source, self.seq)
    }

    /// The components its key holds.
    pub(crate) fn fields(&self) -> Fields<'_> {
        Fields::parse(&self.order).expect("a pending address's key was checked")
    }

    pub(crate) fn address(&self) -> AddressComponents {
        self.fields().to_address()
    }

    /// The address record it stands for, undisputed and lent no postcodes.
    pub(crate) fn record(&self, datasets: &Datasets) -> Result<AddressRecord> {
        Ok(AddressRecord {
            address: self.address(),
            street_postcodes: Vec::new(),
            location_precision: self.precision,
            disputed: false,
            source: datasets.provenance(self.source)?,
            geometry: point_geometry(self.point[0], self.point[1]),
        })
    }

    /// The start of its key its street shares.
    pub(crate) fn street_key(&self) -> &[u8] {
        &self.order[..self.street_len]
    }

    /// The start of its key its group shares.
    pub(crate) fn group_key(&self) -> &[u8] {
        &self.order[..self.group_len]
    }

    /// Whether `other` is in the same group: on the same street in the same
    /// cell, stating the same.
    pub(super) fn same_group(&self, other: &Self) -> bool {
        self.group_key() == other.group_key()
    }

    /// Whether both lie in one grid cell.
    pub(super) fn same_cell(&self, other: &Self) -> bool {
        self.order[..CELL_KEY_LEN] == other.order[..CELL_KEY_LEN]
    }

    /// Its house, for finding the addresses that repeat it: the street as
    /// search spells it, the number and the unit. No cell: one house on both
    /// sides of a cell edge is still one house, and distance tells apart the
    /// towns that share a street name. `at` is where it waits in the address
    /// book.
    pub(super) fn house(&self, at: u64) -> House {
        let fields = self.fields();
        let normalized = |value: Option<&str>| value.and_then(crate::util::fold::search_key);
        let mut key = self.order[CELL_KEY_LEN..self.street_len].to_vec();
        let base = crate::text_index::base_house_number(fields.number).unwrap_or(u32::MAX);
        key.extend_from_slice(&base.to_be_bytes());
        put_order_text(&mut key, normalized(Some(fields.number)).as_deref());
        put_order_text(&mut key, normalized(fields.unit).as_deref());
        let hash = |value: &[u8]| xxhash_rust::xxh3::xxh3_64(value);
        let areas = fields
            .areas
            .iter()
            .filter_map(|area| crate::util::fold::search_key(area))
            .fold(Vec::new(), |mut bytes, area| {
                put_order_text(&mut bytes, Some(&area));
                bytes
            });
        House {
            key: xxhash_rust::xxh3::xxh3_128(&key),
            imported: self.source.object_type == ObjectType::Row,
            seq: self.seq,
            point: self.point,
            at,
            dataset: self.source.dataset,
            postcode: fields
                .postcode
                .and_then(crate::text_index::compact_postcode)
                .map(|postcode| hash(postcode.as_bytes())),
            locality: normalized(fields.locality).map(|locality| hash(locality.as_bytes())),
            areas: (!areas.is_empty()).then(|| hash(&areas)),
        }
    }

    /// Its encoding, after `previous` in a chunk of the address book: the
    /// keys of one street share most of their bytes, so there only the rest
    /// of each is written.
    fn encode_rest(&self, previous: Option<&Self>, out: &mut Vec<u8>) {
        let shared = previous.map(|previous| {
            previous
                .order
                .iter()
                .zip(&self.order)
                .take_while(|(left, right)| left == right)
                .count()
        });
        if let Some(shared) = shared {
            put_u64(out, shared as u64);
        }
        put_bytes(out, &self.order[shared.unwrap_or(0)..]);
        put_u64(out, self.street_len as u64);
        put_u64(out, self.group_len as u64);
        put_u64(out, self.seq);
        let kind = match self.source.object_type {
            ObjectType::Node => 0,
            ObjectType::Way => 1,
            ObjectType::Relation => 2,
            ObjectType::Row => 3,
        };
        let precision = match self.precision {
            LocationPrecision::Point => 0,
            LocationPrecision::Centroid => 4,
        };
        out.push(kind | precision);
        put_i64(out, self.source.object_id);
        if self.source.object_type == ObjectType::Row {
            put_u64(out, u64::from(self.source.dataset));
        }
        encode_compact_point(out, self.point);
    }

    fn decode_rest(previous: Option<&Self>, input: &mut &[u8]) -> Result<Self> {
        let start = match previous {
            Some(previous) => previous
                .order
                .get(..usize::try_from(get_u64(input)?)?)
                .ok_or(Error::Decode(
                    "address sort key shares more than it follows",
                ))?,
            None => &[],
        };
        let shared = start.len();
        let rest = get_bytes(input)?;
        let mut order = Vec::with_capacity(shared + rest.len());
        order.extend_from_slice(start);
        order.extend_from_slice(rest);
        let street_len = usize::try_from(get_u64(input)?)?;
        let group_len = usize::try_from(get_u64(input)?)?;
        if street_len < CELL_KEY_LEN || street_len > group_len || group_len > order.len() {
            return Err(Error::Decode(
                "address group key is longer than its sort key",
            ));
        }
        Fields::parse(&order)?;
        let seq = get_u64(input)?;
        let kind = get_u8(input)?;
        let object_type = match kind & 3 {
            0 => ObjectType::Node,
            1 => ObjectType::Way,
            2 => ObjectType::Relation,
            _ => ObjectType::Row,
        };
        let object_id = get_i64(input)?;
        let dataset = match object_type {
            ObjectType::Row => u32::try_from(get_u64(input)?)?,
            _ => 0,
        };
        Ok(Self {
            order,
            street_len,
            group_len,
            seq,
            precision: match kind & 4 {
                0 => LocationPrecision::Point,
                _ => LocationPrecision::Centroid,
            },
            source: Source {
                object_type,
                object_id,
                dataset,
            },
            point: decode_compact_point(input)?,
        })
    }
}

/// An address in house order, while the imported rows that repeat an
/// address the Pack already has are found: a hash of its house, the
/// rows of one house OSM ones first, and what tells two of them apart.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct House {
    /// Hash of the street as search spells it, the number and the unit.
    pub key: u128,
    pub imported: bool,
    pub seq: u64,
    pub point: [f64; 2],
    /// Where the address waits in the address book.
    pub at: u64,
    pub dataset: u32,
    /// Hashes of the postcode, the locality and the areas it states, as
    /// [`crate::text_index::compact_postcode`] and
    /// [`crate::util::fold::search_key`] write them.
    pub postcode: Option<u64>,
    pub locality: Option<u64>,
    pub areas: Option<u64>,
}

impl House {
    /// Whether two addresses of one house key state where they are
    /// differently: both state a postcode, a locality or areas, and they
    /// differ. Then they are two houses, however close.
    pub(super) fn other_house(&self, other: &Self) -> bool {
        fn differ(left: Option<u64>, right: Option<u64>) -> bool {
            left.zip(right).is_some_and(|(left, right)| left != right)
        }
        differ(self.postcode, other.postcode)
            || differ(self.locality, other.locality)
            || differ(self.areas, other.areas)
    }
}

impl Eq for House {}

impl PartialOrd for House {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for House {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.key, self.imported, self.seq).cmp(&(other.key, other.imported, other.seq))
    }
}

impl Spill for House {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.key.to_be_bytes());
        let present = [self.postcode, self.locality, self.areas];
        let mut flags = u8::from(self.imported);
        for (bit, value) in present.iter().enumerate() {
            flags |= u8::from(value.is_some()) << (bit + 1);
        }
        out.push(flags);
        put_u64(out, self.seq);
        put_u64(out, self.at);
        put_u64(out, u64::from(self.dataset));
        encode_compact_point(out, self.point);
        for value in present.into_iter().flatten() {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        let (key, rest) = input
            .split_at_checked(16)
            .ok_or(Error::Decode("house key is truncated"))?;
        *input = rest;
        let key = u128::from_be_bytes(key.try_into().expect("16 bytes"));
        let flags = get_u8(input)?;
        let seq = get_u64(input)?;
        let at = get_u64(input)?;
        let dataset = u32::try_from(get_u64(input)?)?;
        let point = decode_compact_point(input)?;
        let mut present = [None; 3];
        for (bit, value) in present.iter_mut().enumerate() {
            if flags & (1 << (bit + 1)) != 0 {
                let (bytes, rest) = input
                    .split_at_checked(8)
                    .ok_or(Error::Decode("house hash is truncated"))?;
                *input = rest;
                *value = Some(u64::from_le_bytes(bytes.try_into().expect("8 bytes")));
            }
        }
        let [postcode, locality, areas] = present;
        Ok(Self {
            key,
            imported: flags & 1 != 0,
            seq,
            point,
            at,
            dataset,
            postcode,
            locality,
            areas,
        })
    }
}

/// An address placed in record order, standing for its record until it is
/// written: where it waits in the address book, the street postcodes lent to
/// it, and whether a source disputes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlacedAddress {
    pub at: u64,
    /// Where its street's postcodes wait, when it is lent any.
    pub postcodes: Option<u64>,
    pub disputed: bool,
}

/// The postcodes lent to the addresses of one street.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Postcodes(pub Vec<String>);

impl Spill for Postcodes {
    fn encode(&self, out: &mut Vec<u8>) {
        put_strs(out, &self.0);
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Ok(Self(get_strings(input)?))
    }
}

impl Entry for Postcodes {}

impl Entry for PendingAddress {
    fn encode_after(&self, previous: Option<&Self>, out: &mut Vec<u8>) {
        self.encode_rest(previous, out);
    }

    fn decode_after(previous: Option<&Self>, input: &mut &[u8]) -> Result<Self> {
        Self::decode_rest(previous, input)
    }
}

/// A point in 1e-7 degree steps when it is exactly one, as OSM's nodes and
/// most imported rows are, and in full otherwise: nothing a point decodes to
/// differs from what was encoded.
fn encode_compact_point(out: &mut Vec<u8>, point: [f64; 2]) {
    let compact = point.map(|degrees| {
        let steps = (degrees * COORDINATE_SCALE).round();
        if steps.is_nan() || steps.abs() > f64::from(i32::MAX) {
            return None;
        }
        let steps = steps as i64;
        [
            (1, steps as f64 / COORDINATE_SCALE),
            (2, 1e-9 * (steps * 100) as f64),
        ]
        .into_iter()
        .find(|(_, decoded)| decoded.to_bits() == degrees.to_bits())
        .map(|(kind, _)| (kind, steps))
    });
    out.push(compact[0].map_or(0, |(kind, _)| kind) | compact[1].map_or(0, |(kind, _)| kind) << 2);
    for (degrees, compact) in point.into_iter().zip(compact) {
        match compact {
            Some((_, steps)) => put_i64(out, steps),
            None => out.extend_from_slice(&degrees.to_bits().to_le_bytes()),
        }
    }
}

fn decode_compact_point(input: &mut &[u8]) -> Result<[f64; 2]> {
    let kinds = get_u8(input)?;
    let mut point = [0.0; 2];
    for (index, value) in point.iter_mut().enumerate() {
        *value = match (kinds >> (2 * index)) & 3 {
            0 => {
                let (bytes, rest) = input
                    .split_at_checked(8)
                    .ok_or(Error::Decode("spilled point is truncated"))?;
                *input = rest;
                f64::from_bits(u64::from_le_bytes(bytes.try_into().expect("8 bytes")))
            }
            1 => get_i64(input)? as f64 / COORDINATE_SCALE,
            2 => 1e-9 * (get_i64(input)? * 100) as f64,
            _ => return Err(Error::Decode("unknown spilled point")),
        };
    }
    Ok(point)
}

/// A way of a street waiting for the other ways of its name: sorted by the
/// name as search spells it, then along the Hilbert curve, so every way of
/// one name comes out together, the near ones close.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingStreet {
    /// The name key, then the Hilbert key of its representative point.
    pub order: Vec<u8>,
    /// Length of the start of `order` its name takes.
    pub name_len: usize,
    pub seq: u64,
    pub record: Record,
}

impl PendingStreet {
    pub(super) fn new(record: StreetRecord, seq: u64) -> Result<Self> {
        let mut order = Vec::new();
        put_order_text(
            &mut order,
            crate::text_index::street_search_text(&record.name).as_deref(),
        );
        let name_len = order.len();
        let [lon, lat] = record.representative_point;
        let key = crate::util::hilbert::hilbert_key(
            crate::records::quantize(lon)?,
            crate::records::quantize(lat)?,
        );
        order.extend_from_slice(&key.to_be_bytes());
        Ok(Self {
            order,
            name_len,
            seq,
            record: Record::Street(record),
        })
    }

    /// Its name as search spells it, as the sort key holds it.
    pub(super) fn name_key(&self) -> &[u8] {
        &self.order[..self.name_len]
    }

    pub(super) fn into_street(self) -> StreetRecord {
        match self.record {
            Record::Street(record) => record,
            _ => unreachable!("a pending street holds a street"),
        }
    }
}

impl Eq for PendingStreet {}

impl PartialOrd for PendingStreet {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PendingStreet {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.order, self.seq).cmp(&(&other.order, other.seq))
    }
}

impl Spill for PendingStreet {
    fn encode(&self, out: &mut Vec<u8>) {
        put_u64(out, self.order.len() as u64);
        out.extend_from_slice(&self.order);
        put_u64(out, self.name_len as u64);
        put_u64(out, self.seq);
        encode_record(out, &self.record);
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        let len = usize::try_from(get_u64(input)?)?;
        if input.len() < len {
            return Err(Error::Decode("street sort key is truncated"));
        }
        let (order, rest) = input.split_at(len);
        *input = rest;
        let name_len = usize::try_from(get_u64(input)?)?;
        if name_len > len {
            return Err(Error::Decode("street name key is longer than its sort key"));
        }
        Ok(Self {
            order: order.to_vec(),
            name_len,
            seq: get_u64(input)?,
            record: decode_record(input)?,
        })
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.order.capacity() + record_heap_bytes(&self.record)
    }
}

/// Text for a sort key: absent before present, and each value prefixed by
/// its length so no value can run into the next. Values sort by length, then
/// by their bytes.
fn put_order_text(order: &mut Vec<u8>, value: Option<&str>) {
    match value {
        None => order.push(0),
        Some(value) => {
            put_order_len(order, value.len() + 1);
            order.extend_from_slice(value.as_bytes());
        }
    }
}

fn get_order_text<'a>(input: &mut &'a [u8]) -> Result<Option<&'a str>> {
    let len = get_order_len(input)?;
    if len == 0 {
        return Ok(None);
    }
    let (value, rest) = input
        .split_at_checked(len - 1)
        .ok_or(Error::Decode("sort key text is truncated"))?;
    *input = rest;
    Ok(Some(std::str::from_utf8(value).map_err(|_| {
        Error::Decode("sort key text is not UTF-8")
    })?))
}

/// A length in one byte below [`LONG_ORDER_LEN`], else that byte and four
/// big-endian ones: either way a longer length sorts after a shorter one.
fn put_order_len(order: &mut Vec<u8>, len: usize) {
    match u8::try_from(len) {
        Ok(short) if short < LONG_ORDER_LEN => order.push(short),
        _ => {
            order.push(LONG_ORDER_LEN);
            order.extend_from_slice(&u32::try_from(len).unwrap_or(u32::MAX).to_be_bytes());
        }
    }
}

fn get_order_len(input: &mut &[u8]) -> Result<usize> {
    let short = get_u8(input)?;
    if short < LONG_ORDER_LEN {
        return Ok(usize::from(short));
    }
    let (bytes, rest) = input
        .split_at_checked(4)
        .ok_or(Error::Decode("sort key length is truncated"))?;
    *input = rest;
    Ok(u32::from_be_bytes(bytes.try_into().expect("4 bytes")) as usize)
}

/// The first length byte that says four more follow.
const LONG_ORDER_LEN: u8 = u8::MAX;

impl Eq for PendingAddress {}

impl PartialOrd for PendingAddress {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PendingAddress {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.order, self.seq).cmp(&(&other.order, other.seq))
    }
}

impl Spill for PendingAddress {
    fn encode(&self, out: &mut Vec<u8>) {
        self.encode_rest(None, out);
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Self::decode_rest(None, input)
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.order.capacity()
    }
}

impl Eq for PendingRecord {}

impl PartialOrd for PendingRecord {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PendingRecord {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.key, self.seq).cmp(&(other.key, other.seq))
    }
}

/// The origin byte of a placed address, which has none.
const PLACED_ADDRESS: u8 = 3;

impl Spill for PendingRecord {
    fn encode(&self, out: &mut Vec<u8>) {
        put_u64(out, self.key);
        put_u64(out, self.seq);
        if let Body::Address(placed) = &self.body {
            out.push(PLACED_ADDRESS);
            put_u64(out, placed.at);
            put_u64(
                out,
                placed.postcodes.map_or(0, |at| at + 1) << 1 | u64::from(placed.disputed),
            );
            return;
        }
        match &self.origin {
            None => out.push(0),
            Some(ContextOrigin::Boundary(index)) => {
                out.push(1);
                put_u64(out, u64::from(*index));
            }
            Some(ContextOrigin::DerivedCountry(code)) => {
                out.push(2);
                put_str(out, code);
            }
        }
        if let Body::Record(record) = &self.body {
            encode_record(out, record);
        }
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        let key = get_u64(input)?;
        let seq = get_u64(input)?;
        let origin = match get_u8(input)? {
            0 => None,
            1 => Some(ContextOrigin::Boundary(get_u32(input)?)),
            2 => Some(ContextOrigin::DerivedCountry(get_string(input)?)),
            PLACED_ADDRESS => {
                let at = get_u64(input)?;
                let lent = get_u64(input)?;
                return Ok(Self {
                    key,
                    seq,
                    origin: None,
                    body: Body::Address(PlacedAddress {
                        at,
                        postcodes: (lent >> 1).checked_sub(1),
                        disputed: lent & 1 != 0,
                    }),
                });
            }
            _ => return Err(Error::Decode("unknown record origin")),
        };
        Ok(Self {
            key,
            seq,
            origin,
            body: decode_record(input)?.into(),
        })
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + match &self.body {
                Body::Record(record) => std::mem::size_of::<Record>() + record_heap_bytes(record),
                Body::Address(_) => 0,
            }
    }
}

fn record_heap_bytes(record: &Record) -> usize {
    let strings = match record {
        Record::Address(_) => 0,
        Record::Poi(record) => {
            record.name.len()
                + strings_heap_bytes(&record.alt_names)
                + record.category.len()
                + record.address.as_ref().map_or(0, address_heap_bytes)
        }
        Record::Interpolation(_) => 96,
        Record::Street(record) => {
            record.name.len()
                + strings_heap_bytes(&record.alt_names)
                + record.locality.as_ref().map_or(0, String::len)
        }
        Record::Postcode(record) => record.postcode.len(),
        Record::Place(_, record) => {
            record.name.len() + strings_heap_bytes(&record.alt_names) + record.place_type.len()
        }
    };
    let positions = line_parts(record.geometry()).map_or(40, |parts| {
        parts.iter().map(|part| part.len() * 40 + 24).sum::<usize>()
    });
    strings + positions + 128
}

fn address_heap_bytes(address: &AddressComponents) -> usize {
    address.number.len()
        + [
            &address.street,
            &address.place,
            &address.unit,
            &address.locality,
            &address.region,
            &address.postcode,
            &address.country,
        ]
        .into_iter()
        .flatten()
        .map(String::len)
        .sum::<usize>()
        + strings_heap_bytes(&address.areas)
}

fn strings_heap_bytes(values: &[String]) -> usize {
    values.iter().map(|value| value.len() + 24).sum()
}

fn put_strs(out: &mut Vec<u8>, values: &[String]) {
    put_u64(out, values.len() as u64);
    for value in values {
        put_str(out, value);
    }
}

fn get_strings(input: &mut &[u8]) -> Result<Vec<String>> {
    let count = get_u64(input)?;
    if count > input.len() as u64 {
        return Err(Error::Decode("spilled string list is truncated"));
    }
    (0..count).map(|_| get_string(input)).collect()
}

fn encode_record(out: &mut Vec<u8>, record: &Record) {
    out.push(
        Layer::ALL
            .iter()
            .position(|layer| *layer == record.layer())
            .expect("layer has a code") as u8,
    );
    match record {
        Record::Address(_) => {
            unreachable!("an address is pending as its place in the address book")
        }
        Record::Poi(record) => {
            put_str(out, &record.name);
            put_strs(out, &record.alt_names);
            put_str(out, &record.category);
            match &record.address {
                Some(address) => {
                    out.push(1);
                    encode_address(out, address);
                }
                None => out.push(0),
            }
            encode_precision(out, record.location_precision);
            out.push(record.prominence.to_byte());
            encode_source(out, &record.source);
            encode_geometry(out, &record.geometry);
        }
        Record::Interpolation(record) => {
            let address = &record.address;
            for value in [
                &address.street,
                &address.place,
                &address.locality,
                &address.region,
                &address.postcode,
                &address.country,
            ] {
                put_opt_str(out, value.as_deref());
            }
            put_str(out, &record.interpolation.kind);
            put_u64(out, u64::from(record.interpolation.start));
            put_u64(out, u64::from(record.interpolation.end));
            put_u64(out, u64::from(record.interpolation.step));
            put_i64(out, record.anchor_node_ids[0]);
            put_i64(out, record.anchor_node_ids[1]);
            encode_point(out, record.representative_point);
            encode_source(out, &record.source);
            encode_geometry(out, &record.geometry);
        }
        Record::Street(record) => {
            put_str(out, &record.name);
            put_strs(out, &record.alt_names);
            put_opt_str(out, record.locality.as_deref());
            encode_point(out, record.representative_point);
            encode_source(out, &record.source);
            encode_geometry(out, &record.geometry);
        }
        Record::Postcode(record) => {
            put_str(out, &record.postcode);
            put_u64(
                out,
                record.country.map_or(0, |country| country.to_u16().into()),
            );
            put_u64(out, record.source.record_count);
            encode_geometry(out, &record.geometry);
        }
        Record::Place(_, record) => {
            put_str(out, &record.name);
            put_strs(out, &record.alt_names);
            put_str(out, &record.place_type);
            encode_source(out, &record.source);
            encode_geometry(out, &record.geometry);
        }
    }
}

fn decode_record(input: &mut &[u8]) -> Result<Record> {
    let layer = *Layer::ALL
        .get(usize::from(get_u8(input)?))
        .ok_or(Error::Decode("unknown spilled record layer"))?;
    Ok(match layer {
        Layer::Address => return Err(Error::Decode("an address is pending as a record")),
        Layer::Poi => Record::Poi(PoiRecord {
            name: get_string(input)?,
            alt_names: get_strings(input)?,
            category: get_string(input)?,
            address: match get_u8(input)? {
                0 => None,
                _ => Some(decode_address(input)?),
            },
            location_precision: decode_precision(input)?,
            prominence: Prominence::from_byte(get_u8(input)?),
            source: decode_source(input)?,
            geometry: decode_geometry(input)?,
        }),
        Layer::Interpolation => {
            let mut fields = [None, None, None, None, None, None];
            for field in &mut fields {
                *field = get_opt_string(input)?;
            }
            let [street, place, locality, region, postcode, country] = fields;
            Record::Interpolation(InterpolationRecord {
                address: InterpolationAddressComponents {
                    street,
                    place,
                    locality,
                    region,
                    postcode,
                    country,
                },
                interpolation: InterpolationRange {
                    kind: get_string(input)?,
                    start: get_u32(input)?,
                    end: get_u32(input)?,
                    step: get_u32(input)?,
                },
                anchor_node_ids: [get_i64(input)?, get_i64(input)?],
                representative_point: decode_point(input)?,
                source: decode_source(input)?,
                geometry: decode_geometry(input)?,
            })
        }
        Layer::Street => Record::Street(StreetRecord {
            name: get_string(input)?,
            alt_names: get_strings(input)?,
            locality: get_opt_string(input)?,
            representative_point: decode_point(input)?,
            source: decode_source(input)?,
            geometry: decode_geometry(input)?,
        }),
        Layer::Postcode => Record::Postcode(PostcodeRecord {
            postcode: get_string(input)?,
            country: u16::try_from(get_u64(input)?)
                .ok()
                .and_then(CountryCode::from_u16),
            source: DerivedSourceProvenance::osm_address_records(get_u64(input)?),
            geometry: decode_geometry(input)?,
        }),
        layer => Record::Place(
            layer.place_layer().expect("remaining layers are places"),
            PlaceRecord {
                name: get_string(input)?,
                alt_names: get_strings(input)?,
                place_type: get_string(input)?,
                source: decode_source(input)?,
                geometry: decode_geometry(input)?,
            },
        ),
    })
}

fn encode_address(out: &mut Vec<u8>, address: &AddressComponents) {
    put_str(out, &address.number);
    for value in [
        &address.street,
        &address.place,
        &address.unit,
        &address.locality,
        &address.region,
        &address.postcode,
        &address.country,
    ] {
        put_opt_str(out, value.as_deref());
    }
    put_strs(out, &address.areas);
}

fn decode_address(input: &mut &[u8]) -> Result<AddressComponents> {
    let number = get_string(input)?;
    let mut fields = [None, None, None, None, None, None, None];
    for field in &mut fields {
        *field = get_opt_string(input)?;
    }
    let [street, place, unit, locality, region, postcode, country] = fields;
    Ok(AddressComponents {
        areas: get_strings(input)?,
        number,
        street,
        place,
        unit,
        locality,
        region,
        postcode,
        country,
    })
}

fn encode_precision(out: &mut Vec<u8>, precision: LocationPrecision) {
    out.push(match precision {
        LocationPrecision::Point => 0,
        LocationPrecision::Centroid => 1,
    });
}

fn decode_precision(input: &mut &[u8]) -> Result<LocationPrecision> {
    Ok(match get_u8(input)? {
        0 => LocationPrecision::Point,
        _ => LocationPrecision::Centroid,
    })
}

fn encode_source(out: &mut Vec<u8>, source: &SourceProvenance) {
    out.push(match source.object_type {
        ObjectType::Node => 0,
        ObjectType::Way => 1,
        ObjectType::Relation => 2,
        ObjectType::Row => 3,
    });
    put_i64(out, source.object_id);
    if source.object_type == ObjectType::Row {
        put_str(out, &source.dataset);
    }
}

fn decode_source(input: &mut &[u8]) -> Result<SourceProvenance> {
    let object_type = match get_u8(input)? {
        0 => ObjectType::Node,
        1 => ObjectType::Way,
        2 => ObjectType::Relation,
        3 => {
            let row = get_i64(input)?;
            return Ok(SourceProvenance::imported(get_string(input)?, row));
        }
        _ => return Err(Error::Decode("unknown spilled object type")),
    };
    Ok(SourceProvenance::osm(object_type, get_i64(input)?))
}

/// Points keep full precision; they are quantized once, by the record store.
fn encode_point(out: &mut Vec<u8>, point: [f64; 2]) {
    out.extend_from_slice(&point[0].to_bits().to_le_bytes());
    out.extend_from_slice(&point[1].to_bits().to_le_bytes());
}

fn decode_point(input: &mut &[u8]) -> Result<[f64; 2]> {
    let mut point = [0.0; 2];
    for value in &mut point {
        let (bytes, rest) = input
            .split_at_checked(8)
            .ok_or(Error::Decode("spilled point is truncated"))?;
        *value = f64::from_bits(u64::from_le_bytes(bytes.try_into().expect("8 bytes")));
        *input = rest;
    }
    Ok(point)
}

/// Line vertices are stored as 1e-7 degree deltas, the precision the record
/// store keeps, without its range limit: a spilled line decodes to the values
/// the store would quantize anyway, so an out-of-range vertex fails the build
/// the same way whether or not it was spilled. Non-finite vertices keep their
/// raw bits for the same reason.
fn encode_geometry(out: &mut Vec<u8>, geometry: &Geometry) {
    match &geometry.value {
        GeometryValue::LineString { coordinates } => {
            let finite = coordinates
                .iter()
                .all(|position| position[0].is_finite() && position[1].is_finite());
            if !finite {
                out.push(2);
                put_u64(out, coordinates.len() as u64);
                for position in coordinates {
                    encode_point(out, [position[0], position[1]]);
                }
                return;
            }
            out.push(1);
            put_u64(out, coordinates.len() as u64);
            let mut previous = (0i64, 0i64);
            for position in coordinates {
                let lon = scale(position[0]);
                let lat = scale(position[1]);
                put_i64(out, lon.wrapping_sub(previous.0));
                put_i64(out, lat.wrapping_sub(previous.1));
                previous = (lon, lat);
            }
        }
        GeometryValue::Point { coordinates } => {
            out.push(0);
            encode_point(out, [coordinates[0], coordinates[1]]);
        }
        GeometryValue::MultiLineString { coordinates } => {
            out.push(3);
            put_u64(out, coordinates.len() as u64);
            for part in coordinates {
                encode_geometry(
                    out,
                    &Geometry::new(GeometryValue::LineString {
                        coordinates: part.clone(),
                    }),
                );
            }
        }
        _ => unreachable!("builder records are points or lines"),
    }
}

fn scale(value: f64) -> i64 {
    (value * COORDINATE_SCALE).round() as i64
}

fn decode_geometry(input: &mut &[u8]) -> Result<Geometry> {
    let line = |coordinates| Ok(Geometry::new(GeometryValue::LineString { coordinates }));
    match get_u8(input)? {
        0 => {
            let [lon, lat] = decode_point(input)?;
            Ok(point_geometry(lon, lat))
        }
        1 => {
            let count = usize::try_from(get_u64(input)?)?;
            let mut coordinates = Vec::with_capacity(count.min(input.len()));
            let mut previous = (0i64, 0i64);
            for _ in 0..count {
                previous.0 = previous.0.wrapping_add(get_i64(input)?);
                previous.1 = previous.1.wrapping_add(get_i64(input)?);
                coordinates.push(
                    vec![
                        previous.0 as f64 / COORDINATE_SCALE,
                        previous.1 as f64 / COORDINATE_SCALE,
                    ]
                    .into(),
                );
            }
            line(coordinates)
        }
        2 => {
            let count = usize::try_from(get_u64(input)?)?;
            let mut coordinates = Vec::with_capacity(count.min(input.len()));
            for _ in 0..count {
                coordinates.push(decode_point(input)?.to_vec().into());
            }
            line(coordinates)
        }
        3 => {
            let count = usize::try_from(get_u64(input)?)?;
            let mut parts = Vec::with_capacity(count.min(input.len()));
            for _ in 0..count {
                match decode_geometry(input)?.value {
                    GeometryValue::LineString { coordinates } => parts.push(coordinates),
                    _ => return Err(Error::Decode("spilled line part is not a line")),
                }
            }
            Ok(Geometry::new(GeometryValue::MultiLineString {
                coordinates: parts,
            }))
        }
        _ => Err(Error::Decode("unknown spilled geometry")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::PlaceLayer;

    fn round_trip<T: Spill + std::fmt::Debug + PartialEq>(value: &T) {
        let mut out = Vec::new();
        value.encode(&mut out);
        let mut input = out.as_slice();
        assert_eq!(&T::decode(&mut input).expect("decode"), value);
        assert!(input.is_empty());
    }

    #[test]
    fn out_of_range_line_coordinates_are_not_spilled_as_zero() {
        let record = Record::Street(StreetRecord {
            alt_names: Vec::new(),
            name: "Edge Road".into(),
            locality: None,
            geometry: Geometry::new(GeometryValue::LineString {
                coordinates: vec![vec![-79.0, 43.0].into(), vec![300.0, 43.1].into()],
            }),
            representative_point: [-79.0, 43.05],
            source: SourceProvenance::osm(ObjectType::Way, 13),
        });
        let mut out = Vec::new();
        PendingRecord {
            key: 0,
            seq: 0,
            origin: None,
            body: record.clone().into(),
        }
        .encode(&mut out);
        let decoded = PendingRecord::decode(&mut out.as_slice()).expect("decode");
        assert_eq!(
            decoded.body,
            record.into(),
            "spilling must not change coordinates"
        );

        let mut infinite = Record::Street(StreetRecord {
            alt_names: Vec::new(),
            name: "Edge Road".into(),
            locality: None,
            geometry: Geometry::new(GeometryValue::Point {
                coordinates: vec![0.0, 0.0].into(),
            }),
            representative_point: [-79.0, 43.05],
            source: SourceProvenance::osm(ObjectType::Way, 13),
        });
        if let Record::Street(street) = &mut infinite {
            street.geometry = Geometry::new(GeometryValue::LineString {
                coordinates: vec![vec![-79.0, 43.0].into(), vec![f64::INFINITY, 43.1].into()],
            });
        }
        let mut out = Vec::new();
        PendingRecord {
            key: 0,
            seq: 0,
            origin: None,
            body: infinite.clone().into(),
        }
        .encode(&mut out);
        let decoded = PendingRecord::decode(&mut out.as_slice()).expect("decode");
        assert_eq!(decoded.body, infinite.into());
    }

    #[test]
    fn round_trips_pending_records() {
        let line = Geometry::new(GeometryValue::LineString {
            coordinates: vec![vec![-79.0, 43.0].into(), vec![-79.0000001, 43.1].into()],
        });
        let records = [
            Record::Street(StreetRecord {
                alt_names: Vec::new(),
                name: "King Street".into(),
                locality: None,
                geometry: line.clone(),
                representative_point: [-79.0, 43.05],
                source: SourceProvenance::osm(ObjectType::Way, 13),
            }),
            // The ways of one street, merged.
            Record::Street(StreetRecord {
                alt_names: vec!["Te Huarahi o King".into()],
                name: "King Street".into(),
                locality: Some("Toronto".into()),
                geometry: Geometry::new(GeometryValue::MultiLineString {
                    coordinates: vec![
                        vec![vec![-79.0, 43.0].into(), vec![-79.0, 43.1].into()],
                        vec![vec![-79.0, 43.1].into(), vec![-79.1, 43.1].into()],
                    ],
                }),
                representative_point: [-79.0, 43.05],
                source: SourceProvenance::osm(ObjectType::Way, 16),
            }),
            Record::Interpolation(InterpolationRecord {
                address: InterpolationAddressComponents {
                    street: Some("King Street".into()),
                    place: None,
                    locality: None,
                    region: None,
                    postcode: None,
                    country: Some("CA".into()),
                },
                interpolation: InterpolationRange {
                    kind: "even".into(),
                    start: 2,
                    end: 10,
                    step: 2,
                },
                anchor_node_ids: [1, 2],
                geometry: line,
                representative_point: [-79.0, 43.05],
                source: SourceProvenance::osm(ObjectType::Way, 14),
            }),
            Record::Postcode(PostcodeRecord {
                postcode: "M5V".into(),
                country: CountryCode::parse("CA"),
                geometry: point_geometry(-79.0, 43.0),
                source: DerivedSourceProvenance::osm_address_records(3),
            }),
            Record::Place(
                PlaceLayer::Neighbourhood,
                PlaceRecord {
                    alt_names: Vec::new(),
                    name: "Annex".into(),
                    place_type: "suburb".into(),
                    geometry: point_geometry(-79.4, 43.67),
                    source: SourceProvenance::osm(ObjectType::Node, 15),
                },
            ),
            Record::Poi(PoiRecord {
                alt_names: Vec::new(),
                name: "Tim Hortons".into(),
                category: "amenity:cafe".into(),
                address: Some(AddressComponents {
                    areas: Vec::new(),
                    number: "123".into(),
                    street: Some("King Street West".into()),
                    place: None,
                    unit: None,
                    locality: None,
                    region: None,
                    postcode: Some("M5V 1A1".into()),
                    country: None,
                }),
                geometry: point_geometry(-79.38, 43.65),
                location_precision: LocationPrecision::Centroid,
                source: SourceProvenance::osm(ObjectType::Way, 16),
                prominence: Default::default(),
            }),
            Record::Poi(PoiRecord {
                alt_names: Vec::new(),
                name: "Riverdale Farm".into(),
                category: "tourism:attraction".into(),
                address: None,
                geometry: point_geometry(-79.36, 43.67),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, 17),
                prominence: Default::default(),
            }),
        ];
        for (seq, record) in records.into_iter().enumerate() {
            round_trip(&PendingRecord {
                key: 99,
                seq: seq as u64,
                origin: match seq {
                    0 => None,
                    1 => Some(ContextOrigin::Boundary(4)),
                    _ => Some(ContextOrigin::DerivedCountry("CA".into())),
                },
                body: record.into(),
            });
        }
    }

    /// A million addresses shaped like OpenAddresses rows: street, number,
    /// sometimes a unit, a city, a region, a postcode most of the time,
    /// 7-decimal coordinates, one dataset.
    fn synthetic_rows(count: u64) -> Vec<AddressRecord> {
        const WORDS: [&str; 24] = [
            "Maple",
            "Oak",
            "Cedar",
            "Pine",
            "Elm",
            "Washington",
            "Lincoln",
            "Jefferson",
            "Highland",
            "Sunset",
            "Lakeview",
            "Park",
            "Hill",
            "River",
            "Spring",
            "Valley",
            "Church",
            "Main",
            "Mill",
            "Forest",
            "Meadow",
            "Willow",
            "Franklin",
            "Madison",
        ];
        const TYPES: [&str; 8] = [
            "Street",
            "Avenue",
            "Road",
            "Drive",
            "Lane",
            "Court",
            "Boulevard",
            "Way",
        ];
        const CITIES: [&str; 8] = [
            "Springfield",
            "Riverside",
            "Fairview",
            "Georgetown",
            "Salem",
            "Madison",
            "Clinton",
            "Franklin",
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        (0..count)
            .map(|row| {
                // Rows come street by street, as a county's file lists them.
                let street_id = row / 40;
                let street = format!(
                    "{} {}",
                    WORDS[(street_id % 24) as usize],
                    TYPES[((street_id / 24) % 8) as usize]
                );
                let city = CITIES[((street_id / 192) % 8) as usize];
                let random = next();
                let number = (row % 40) * 2 + 1 + (street_id % 50) * 100;
                let unit = (random % 10 == 0).then(|| format!("{}", random % 20 + 1));
                let postcode = (random % 10 < 7).then(|| format!("{:05}", 90000 + street_id % 900));
                let lon = -118.0 - (street_id % 1000) as f64 * 0.001 - (row % 40) as f64 * 0.00011;
                let lat = 34.0 + (street_id / 1000) as f64 * 0.001 + (random % 1000) as f64 * 1e-7;
                let parse = |value: f64| format!("{value:.7}").parse::<f64>().expect("number");
                AddressRecord {
                    disputed: false,
                    street_postcodes: Vec::new(),
                    address: AddressComponents {
                        areas: Vec::new(),
                        number: number.to_string(),
                        street: Some(street),
                        place: None,
                        unit,
                        locality: Some(city.to_string()),
                        region: Some("CA".to_string()),
                        postcode,
                        country: None,
                    },
                    geometry: point_geometry(parse(lon), parse(lat)),
                    location_precision: LocationPrecision::Point,
                    source: SourceProvenance::imported("us/ca/los_angeles", row as i64 + 1),
                }
            })
            .collect()
    }

    /// Bytes an item takes in a run file: its encoding and the length
    /// prefix the spool writes before it.
    fn run_bytes<T: Spill>(item: &T) -> usize {
        let mut out = Vec::new();
        item.encode(&mut out);
        let mut prefix = Vec::new();
        put_u64(&mut prefix, out.len() as u64);
        out.len() + prefix.len()
    }

    fn address_record(
        number: &str,
        street: Option<&str>,
        point: [f64; 2],
        source: SourceProvenance,
    ) -> AddressRecord {
        AddressRecord {
            disputed: false,
            street_postcodes: Vec::new(),
            address: AddressComponents {
                areas: Vec::new(),
                number: number.into(),
                street: street.map(str::to_string),
                place: None,
                unit: None,
                locality: None,
                region: None,
                postcode: None,
                country: None,
            },
            geometry: point_geometry(point[0], point[1]),
            location_precision: LocationPrecision::Point,
            source,
        }
    }

    /// Addresses whose points take every form a point is stored in, and
    /// whose keys hold every component, long and absent ones too.
    fn varied_addresses() -> Vec<AddressRecord> {
        let osm_node = |e7: i32| 1e-9 * (i64::from(e7) * 100) as f64;
        let long = "Avenue ".repeat(40);
        let mut full = address_record(
            "10/1367",
            Some("King Street"),
            [-79.38321234567, 43.653],
            SourceProvenance::imported("openaddresses:ca", 12),
        );
        full.address = AddressComponents {
            areas: vec!["Annex".into(), "Seaton Village".into()],
            number: "10/1367".into(),
            street: Some("King Street".into()),
            place: Some("Rural Route".into()),
            unit: Some("4".into()),
            locality: Some("Toronto".into()),
            region: Some("ON".into()),
            postcode: Some("M5V 1A1".into()),
            country: Some("CA".into()),
        };
        full.location_precision = LocationPrecision::Centroid;
        vec![
            full,
            address_record(
                "12",
                Some("King Street"),
                [osm_node(-793_832_123), osm_node(436_530_001)],
                SourceProvenance::osm(ObjectType::Node, 5),
            ),
            address_record(
                "14",
                Some(&long),
                [176.2450500, -38.1386],
                SourceProvenance::osm(ObjectType::Way, 7),
            ),
            address_record(
                "1",
                None,
                [-0.0, 0.1 + 0.2],
                SourceProvenance::osm(ObjectType::Relation, 9),
            ),
            address_record(
                "7",
                Some("Tui Road"),
                [179.9999999, -89.9999999],
                SourceProvenance::imported("openaddresses:nz", 3),
            ),
        ]
    }

    #[test]
    fn a_pending_address_is_its_record_and_round_trips() {
        let mut datasets = Datasets::default();
        let mut previous: Option<PendingAddress> = None;
        for (seq, record) in varied_addresses().into_iter().enumerate() {
            let dataset = match record.source.object_type {
                ObjectType::Row => datasets.id(&record.source.dataset),
                _ => 0,
            };
            let pending = PendingAddress::new(record.clone(), seq as u64, dataset).expect("new");
            assert_eq!(pending.record(&datasets).expect("record"), record);
            round_trip(&pending);
            let mut out = Vec::new();
            pending.encode_after(previous.as_ref(), &mut out);
            let mut input = out.as_slice();
            assert_eq!(
                PendingAddress::decode_after(previous.as_ref(), &mut input).expect("decode"),
                pending
            );
            assert!(input.is_empty());
            let house = pending.house(seq as u64 * 3);
            round_trip(&house);
            previous = Some(pending);
        }
        assert_eq!(datasets.name(1).expect("name"), "openaddresses:nz");
    }

    /// The street order key as it was written before it was made compact:
    /// cells as i64, and each text after a presence byte and a four-byte
    /// length.
    fn wide_order(address: &AddressComponents, [lon, lat]: [f64; 2]) -> Vec<u8> {
        let text = |order: &mut Vec<u8>, value: Option<&str>| match value {
            None => order.push(0),
            Some(value) => {
                order.push(1);
                order.extend_from_slice(&(value.len() as u32).to_be_bytes());
                order.extend_from_slice(value.as_bytes());
            }
        };
        let mut order = Vec::new();
        let cell = |degrees: f64| (degrees / ADDRESS_GROUP_CELL_DEGREES).floor() as i64;
        order.extend_from_slice(&cell(lat).to_be_bytes());
        order.extend_from_slice(&cell(lon).to_be_bytes());
        text(
            &mut order,
            address
                .subject()
                .and_then(crate::text_index::street_search_text)
                .as_deref(),
        );
        order.push(u8::from(address.postcode.is_none()));
        for field in [
            &address.street,
            &address.place,
            &address.locality,
            &address.region,
            &address.postcode,
            &address.country,
        ] {
            text(&mut order, field.as_deref());
        }
        order.extend_from_slice(&(address.areas.len() as u32).to_be_bytes());
        for area in &address.areas {
            text(&mut order, Some(area));
        }
        let base = crate::text_index::base_house_number(&address.number).unwrap_or(u32::MAX);
        order.extend_from_slice(&base.to_be_bytes());
        text(&mut order, Some(&address.number));
        text(&mut order, address.unit.as_deref());
        order
    }

    #[test]
    fn the_compact_street_order_sorts_as_the_wide_one_did() {
        let mut records = varied_addresses();
        let long = "Road ".repeat(60);
        for (index, (number, street)) in [
            ("2", "King Street"),
            ("2", "King St"),
            ("2A", "King Street"),
            ("10", "King Street"),
            ("10", long.as_str()),
            ("10", "Kin"),
        ]
        .into_iter()
        .enumerate()
        {
            for point in [[-79.4, 43.6], [79.4, -43.6], [-0.01, 0.01], [0.0, 0.0]] {
                let mut record = address_record(
                    number,
                    Some(street),
                    point,
                    SourceProvenance::osm(ObjectType::Node, index as i64),
                );
                if index % 2 == 0 {
                    record.address.postcode = Some("M5V".into());
                }
                if index == 3 {
                    record.address.unit = Some("B".repeat(300));
                }
                records.push(record);
            }
        }
        let pending = records
            .iter()
            .enumerate()
            .map(|(seq, record)| PendingAddress::new(record.clone(), seq as u64, 0).expect("new"))
            .collect::<Vec<_>>();
        let mut compact = (0..pending.len()).collect::<Vec<_>>();
        compact.sort_by(|left, right| pending[*left].cmp(&pending[*right]));
        let mut wide = (0..records.len()).collect::<Vec<_>>();
        wide.sort_by_key(|index| {
            let record = &records[*index];
            (
                wide_order(
                    &record.address,
                    crate::util::geo::point_lon_lat(&record.geometry).expect("point"),
                ),
                *index,
            )
        });
        assert_eq!(compact, wide);
    }

    #[test]
    fn round_trips_a_placed_address() {
        for (postcodes, disputed) in [(None, false), (Some(0), true), (Some(70_000), false)] {
            round_trip(&PendingRecord {
                key: u64::MAX,
                seq: 7,
                origin: None,
                body: Body::Address(PlacedAddress {
                    at: 1 << 40,
                    postcodes,
                    disputed,
                }),
            });
        }
        let postcodes = Postcodes(vec!["M5V 1A1".into(), "M5V 2B2".into()]);
        round_trip(&postcodes);
    }

    /// Run-file bytes per address of each sort and of the address book,
    /// over `rows` taken in their order.
    fn bytes_per_address(rows: Vec<AddressRecord>) -> String {
        let count = rows.len() as f64;
        let mut pending = rows
            .into_iter()
            .enumerate()
            .map(|(seq, row)| PendingAddress::new(row, seq as u64, 0).expect("pending"))
            .collect::<Vec<_>>();
        let street = pending.iter().map(run_bytes).sum::<usize>();
        let resident = pending.iter().map(Spill::resident_bytes).sum::<usize>();
        pending.sort();
        let mut book = 0;
        for chunk in pending.chunks(64) {
            let mut out = Vec::new();
            put_u64(&mut out, chunk.len() as u64);
            let mut previous = None;
            for address in chunk {
                address.encode_after(previous, &mut out);
                previous = Some(address);
            }
            let mut prefix = Vec::new();
            put_u64(&mut prefix, out.len() as u64);
            // The index keeps an offset per chunk.
            book += out.len() + prefix.len() + 8;
        }
        let house = pending
            .iter()
            .enumerate()
            .map(|(at, address)| run_bytes(&address.house(at as u64)))
            .sum::<usize>();
        let placed = pending
            .iter()
            .enumerate()
            .map(|(at, address)| {
                let [lon, lat] = address.point;
                run_bytes(&PendingRecord {
                    key: crate::util::hilbert::hilbert_key(
                        crate::records::quantize(lon).expect("lon"),
                        crate::records::quantize(lat).expect("lat"),
                    ),
                    seq: at as u64 + 50_000_000,
                    origin: None,
                    body: Body::Address(PlacedAddress {
                        at: at as u64,
                        postcodes: address
                            .fields()
                            .postcode
                            .is_none()
                            .then_some(at as u64 / 40),
                        disputed: false,
                    }),
                })
            })
            .sum::<usize>();
        let per = |bytes: usize| bytes as f64 / count;
        format!(
            "street {:.1} (resident {:.1}) book {:.1} house {:.1} record {:.1}",
            per(street),
            per(resident),
            per(book),
            per(house),
            per(placed)
        )
    }

    #[test]
    #[ignore = "prints bytes per address on each sort"]
    fn bytes_per_synthetic_address() {
        println!("{}", bytes_per_address(synthetic_rows(1_000_000)));
    }
}
