//! The timezone of an area or a point, from timezone boundary polygons
//! derived from OpenStreetMap (<https://github.com/evansiroky/timezone-boundary-builder>,
//! ODbL). The file holds one GeoJSON feature per zone, its `tzid` an IANA
//! zone name and its geometry a Polygon or MultiPolygon.
//!
//! The file is read a feature at a time and each zone goes to a shape file
//! as the areas do (see [`super::containment`]), so the heap holds the
//! largest zone while it is read and then only names and boxes.
//!
//! A zone is answered only where the file names exactly one. It draws a few
//! disputed regions under two zones, and there neither is a fact.

use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
    sync::Arc,
};

use geo::Rect;
use rstar::{
    AABB, RTree,
    primitives::{GeomWithData, Rectangle},
};
use serde::{
    Deserialize,
    de::{DeserializeSeed, Error as _, IgnoredAny, MapAccess, SeqAccess, Visitor},
};

use crate::{
    error::{Error, Result},
    extsort::Scratch,
};

use super::containment::{
    ShapeRef, ShapeWriter, Shapes, covers, encode_rings, fixed, interior_point, near_outline,
    outline_within, ring_extents, ring_vertices,
};

/// How far apart, in degrees, the file and OpenStreetMap may draw one
/// border. The file writes six decimals of a degree and OpenStreetMap
/// seven, so a border they share is two lines up to this far apart, and an
/// area ending at it would otherwise seem to reach into the next zone.
const RESOLUTION: f64 = 1e-6;
const SHAPES_LABEL: &str = "timezone-shapes";

#[derive(Debug)]
struct Zone {
    /// Its name, by its index in [`Timezones::names`].
    name: u16,
    shape: ShapeRef,
}

/// The box around a zone, with its index in [`Timezones::zones`].
type ZoneBox = GeomWithData<Rectangle<[f64; 2]>, u32>;
/// The box around one ring of a zone, with the zone and the ring.
type RingBox = GeomWithData<Rectangle<[f64; 2]>, (u32, u32)>;

#[derive(Debug)]
pub(crate) struct Timezones {
    names: Vec<String>,
    zones: Vec<Zone>,
    boxes: RTree<ZoneBox>,
    rings: RTree<RingBox>,
    shapes: Shapes,
}

impl Timezones {
    pub(crate) fn load(path: &Path, scratch: &Arc<Scratch>) -> Result<Self> {
        let unusable = |problem: String| {
            Error::BadInput(format!(
                "timezone file {} is not usable ({problem}). It must be the GeoJSON of \
                 timezone-boundary-builder, one feature per zone, each a Polygon or a \
                 MultiPolygon with a `tzid` property",
                path.display()
            ))
        };
        let file = File::open(path).map_err(|source| Error::IoPath {
            action: "failed to open",
            path: path.to_path_buf(),
            source,
        })?;
        let mut reader = BufReader::with_capacity(1 << 20, file);
        if reader.fill_buf()?.starts_with(b"PK") {
            return Err(unusable(
                "it is a zip archive, so unpack it and give the file inside".to_string(),
            ));
        }
        let mut loading = Loading {
            names: Vec::new(),
            zones: Vec::new(),
            boxes: Vec::new(),
            rings: Vec::new(),
            shapes: ShapeWriter::create(scratch, SHAPES_LABEL)?,
            block: Vec::new(),
            failed: None,
        };
        let mut decoder = serde_json::Deserializer::from_reader(reader);
        let read = Collection(&mut loading).deserialize(&mut decoder);
        if let Some(error) = loading.failed.take() {
            return Err(error);
        }
        read.map_err(|error| unusable(error.to_string()))?;
        decoder.end().map_err(|error| unusable(error.to_string()))?;
        if loading.zones.is_empty() {
            return Err(unusable("it holds no zone".to_string()));
        }
        Ok(Self {
            names: loading.names,
            zones: loading.zones,
            boxes: RTree::bulk_load(loading.boxes),
            rings: RTree::bulk_load(loading.rings),
            shapes: loading.shapes.finish()?,
        })
    }

    /// The zone names. A zone is stored as its index here.
    pub(crate) fn names(&self) -> &[String] {
        &self.names
    }

    /// The zone the point lies in. None outside every zone, and where two
    /// zones cover it.
    pub(crate) fn at(&self, [lon, lat]: [f64; 2]) -> Option<u16> {
        let mut found = None;
        for zone in self.boxes.locate_all_at_point([lon, lat]) {
            let zone = &self.zones[zone.data as usize];
            if !self.shapes.covers(zone.shape, lon, lat) {
                continue;
            }
            if found.is_some_and(|held| held != zone.name) {
                return None;
            }
            found = Some(zone.name);
        }
        found
    }

    /// The one zone the area of `block` lies wholly in, `bounds` being the
    /// box around it. None for an area that reaches out of every zone by
    /// more than [`RESOLUTION`], and for one that lies wholly in two.
    pub(crate) fn of_area(&self, block: &[u8], bounds: Rect<f64>) -> Option<u16> {
        let [lon, lat] = interior_point(block)?;
        let reach = AABB::from_corners(
            [bounds.min().x - RESOLUTION, bounds.min().y - RESOLUTION],
            [bounds.max().x + RESOLUTION, bounds.max().y + RESOLUTION],
        );
        let mut held = None;
        for found in self.boxes.locate_all_at_point([lon, lat]) {
            let zone = &self.zones[found.data as usize];
            let shape = self.shapes.block(zone.shape)?;
            // Inside it, its outline never out of it, and no hole or end of
            // the zone inside the area.
            let holds = covers(shape, lon, lat)
                && outline_within(block, shape, RESOLUTION)
                && !self.rings.locate_in_envelope(reach).any(|ring| {
                    ring.data.0 == found.data && ring_inside(shape, ring.data.1, block)
                });
            if !holds {
                continue;
            }
            if held.is_some_and(|held| held != zone.name) {
                return None;
            }
            held = Some(zone.name);
        }
        held
    }
}

/// Whether ring `ring` of `zone` lies in the area of `block`, off the area's
/// outline. A ring that runs along the outline is the same border drawn by
/// the other source, so the ring is judged by its first vertex away from it.
fn ring_inside(zone: &[u8], ring: u32, block: &[u8]) -> bool {
    ring_vertices(zone, ring as usize)
        .find(|vertex| !near_outline(block, *vertex, RESOLUTION))
        .is_some_and(|[lon, lat]| covers(block, lon, lat))
}

/// The zones as they are read.
struct Loading {
    names: Vec<String>,
    zones: Vec<Zone>,
    boxes: Vec<ZoneBox>,
    rings: Vec<RingBox>,
    shapes: ShapeWriter,
    block: Vec<u8>,
    /// What stopped the read that is not the file's fault.
    failed: Option<Error>,
}

impl Loading {
    /// `Err` says what is wrong with the feature.
    fn add(&mut self, feature: Feature) -> std::result::Result<(), String> {
        let tzid = feature.properties.tzid;
        if tzid.is_empty() {
            return Err("a feature's `tzid` is empty".to_string());
        }
        let polygons = match (feature.geometry.kind.as_str(), feature.geometry.coordinates) {
            ("Polygon", Nest::Polygon(rings)) => vec![rings],
            ("MultiPolygon", Nest::Multi(polygons)) => polygons,
            ("Polygon" | "MultiPolygon", _) => {
                return Err(format!(
                    "the coordinates of zone {tzid} are not nested as those of a {} are",
                    feature.geometry.kind
                ));
            }
            (kind, _) => {
                return Err(format!(
                    "zone {tzid} is a {kind}, not a Polygon or a MultiPolygon"
                ));
            }
        };
        let mut coords = Vec::new();
        let mut ring_ends = Vec::new();
        for ring in polygons.iter().flatten() {
            if ring.len() < 4 || ring.first() != ring.last() {
                return Err(format!(
                    "zone {tzid} has a ring of {} points that does not close",
                    ring.len()
                ));
            }
            for &[lon, lat] in ring {
                if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
                    return Err(format!(
                        "zone {tzid} has the point [{lon}, {lat}], which is not a longitude \
                         and a latitude"
                    ));
                }
                coords.push([fixed(lon), fixed(lat)]);
            }
            ring_ends.push(
                u32::try_from(coords.len())
                    .map_err(|_| format!("zone {tzid} has more points than a shape holds"))?,
            );
        }
        if coords.is_empty() {
            return Err(format!("zone {tzid} has no ring"));
        }
        let name = match self.names.iter().position(|known| *known == tzid) {
            Some(name) => name,
            None => {
                self.names.push(tzid);
                self.names.len() - 1
            }
        };
        let name = u16::try_from(name).map_err(|_| "it names too many zones".to_string())?;
        self.block.clear();
        encode_rings(&coords, &ring_ends, &mut self.block);
        let shape = match self.shapes.write(&self.block) {
            Ok(shape) => shape,
            Err(error) => {
                self.failed = Some(error);
                return Err("the build stopped".to_string());
            }
        };
        let zone = u32::try_from(self.zones.len()).map_err(|_| "it holds too many features")?;
        let rectangle = |[west, south, east, north]: [f64; 4]| {
            Rectangle::from_corners([west, south], [east, north])
        };
        let mut around = None::<[f64; 4]>;
        for (ring, extent) in ring_extents(&self.block).into_iter().enumerate() {
            let Some(extent) = extent else { continue };
            self.rings
                .push(GeomWithData::new(rectangle(extent), (zone, ring as u32)));
            around = Some(around.map_or(extent, |held| {
                [
                    held[0].min(extent[0]),
                    held[1].min(extent[1]),
                    held[2].max(extent[2]),
                    held[3].max(extent[3]),
                ]
            }));
        }
        if let Some(around) = around {
            self.boxes.push(GeomWithData::new(rectangle(around), zone));
        }
        self.zones.push(Zone { name, shape });
        Ok(())
    }
}

#[derive(Deserialize)]
struct Feature {
    properties: Properties,
    geometry: Geometry,
}

#[derive(Deserialize)]
struct Properties {
    tzid: String,
}

#[derive(Deserialize)]
struct Geometry {
    #[serde(rename = "type")]
    kind: String,
    coordinates: Nest,
}

/// A GeoJSON coordinate array, as deep as it is nested. A geometry's `type`
/// may come after its coordinates, so the depth is read off the array.
enum Nest {
    Position([f64; 2]),
    Ring(Vec<[f64; 2]>),
    Polygon(Vec<Vec<[f64; 2]>>),
    Multi(Vec<Vec<Vec<[f64; 2]>>>),
}

/// One element of a coordinate array, a number or an array a level down.
enum Element {
    Number(f64),
    Nest(Nest),
}

impl<'de> Deserialize<'de> for Element {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Elements;
        impl<'de> Visitor<'de> for Elements {
            type Value = Element;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a number or an array of coordinates")
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Element, E> {
                Ok(Element::Number(value))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Element, E> {
                Ok(Element::Number(value as f64))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Element, E> {
                Ok(Element::Number(value as f64))
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Element, A::Error> {
                let uneven = || A::Error::custom("a coordinate array mixes depths");
                let Some(first) = seq.next_element::<Element>()? else {
                    return Err(A::Error::custom("a coordinate array is empty"));
                };
                // Each arm reads the rest of the array at its first
                // element's depth.
                macro_rules! rest {
                    ($first:expr, $depth:path) => {{
                        let mut all = vec![$first];
                        while let Some(next) = seq.next_element::<Element>()? {
                            match next {
                                Element::Nest($depth(next)) => all.push(next),
                                _ => return Err(uneven()),
                            }
                        }
                        all
                    }};
                }
                Ok(Element::Nest(match first {
                    Element::Number(lon) => {
                        let Some(Element::Number(lat)) = seq.next_element::<Element>()? else {
                            return Err(A::Error::custom("a position is not two numbers"));
                        };
                        // A position may carry an altitude, which no zone has.
                        while seq.next_element::<IgnoredAny>()?.is_some() {}
                        Nest::Position([lon, lat])
                    }
                    Element::Nest(Nest::Position(first)) => {
                        Nest::Ring(rest!(first, Nest::Position))
                    }
                    Element::Nest(Nest::Ring(first)) => Nest::Polygon(rest!(first, Nest::Ring)),
                    Element::Nest(Nest::Polygon(first)) => Nest::Multi(rest!(first, Nest::Polygon)),
                    Element::Nest(Nest::Multi(_)) => {
                        return Err(A::Error::custom(
                            "a coordinate array is nested deeper than a MultiPolygon",
                        ));
                    }
                }))
            }
        }
        deserializer.deserialize_any(Elements)
    }
}

impl<'de> Deserialize<'de> for Nest {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        match Element::deserialize(deserializer)? {
            Element::Nest(nest) => Ok(nest),
            Element::Number(_) => Err(D::Error::custom("coordinates are a number, not an array")),
        }
    }
}

/// The feature collection, each feature handed on as it is read.
struct Collection<'a>(&'a mut Loading);

impl<'de> DeserializeSeed<'de> for Collection<'_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Collection<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a GeoJSON feature collection")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        let mut found = false;
        while let Some(key) = map.next_key::<String>()? {
            if key == "features" {
                map.next_value_seed(Features(&mut *self.0))?;
                found = true;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        if found {
            Ok(())
        } else {
            Err(A::Error::custom("it has no `features`"))
        }
    }
}

struct Features<'a>(&'a mut Loading);

impl<'de> DeserializeSeed<'de> for Features<'_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<(), D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Features<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an array of features")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        while let Some(feature) = seq.next_element::<Feature>()? {
            self.0.add(feature).map_err(A::Error::custom)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use geo::{BoundingRect, LineString, MultiPolygon, Polygon};

    use super::super::containment::encode_shape;
    use super::*;

    /// A directory removed when the test ends, however it ends.
    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("og-timezones-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("directory");
            Self(dir)
        }

        fn load(&self, geojson: &str) -> Result<Timezones> {
            let path = self.0.join("zones.json");
            std::fs::write(&path, geojson).expect("file");
            Timezones::load(
                &path,
                &Scratch::create(self.0.join("scratch")).expect("scratch"),
            )
        }

        /// What loading `geojson` fails with, less the path and the advice
        /// every such error carries.
        fn problem(&self, geojson: &str) -> String {
            let error = self.load(geojson).expect_err("unusable").to_string();
            let path = self.0.join("zones.json");
            let start = format!("timezone file {} is not usable (", path.display());
            let end = "). It must be the GeoJSON of timezone-boundary-builder, one feature per \
                       zone, each a Polygon or a MultiPolygon with a `tzid` property";
            let found = error.find(&start).expect("names the file") + start.len();
            error[found..]
                .strip_suffix(end)
                .unwrap_or_else(|| panic!("no advice in {error}"))
                .to_string()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ring(west: f64, south: f64, east: f64, north: f64) -> String {
        format!(
            "[[{west},{south}],[{east},{south}],[{east},{north}],[{west},{north}],[{west},{south}]]"
        )
    }

    fn feature(tzid: &str, kind: &str, coordinates: &str) -> String {
        format!(
            r#"{{"type":"Feature","properties":{{"tzid":"{tzid}"}},"geometry":{{"type":"{kind}","coordinates":{coordinates}}}}}"#
        )
    }

    fn collection(features: &[String]) -> String {
        format!(
            r#"{{"type":"FeatureCollection","features":[{}]}}"#,
            features.join(",")
        )
    }

    /// West with a hole, East beside it in two parts, and Shared over the
    /// eastern half of East.
    fn zones() -> String {
        collection(&[
            feature(
                "Test/West",
                "Polygon",
                &format!(
                    "[{},{}]",
                    ring(0.0, 0.0, 10.0, 10.0),
                    ring(2.0, 2.0, 3.0, 3.0)
                ),
            ),
            feature(
                "Test/East",
                "MultiPolygon",
                &format!(
                    "[[{}],[{}]]",
                    ring(10.0, 0.0, 20.0, 10.0),
                    ring(30.0, 0.0, 31.0, 1.0)
                ),
            ),
            feature(
                "Test/Shared",
                "Polygon",
                &format!("[{}]", ring(15.0, 0.0, 20.0, 10.0)),
            ),
        ])
    }

    fn area(west: f64, south: f64, east: f64, north: f64) -> (Vec<u8>, Rect<f64>) {
        let geometry = MultiPolygon::new(vec![Polygon::new(
            LineString::from(vec![
                (west, south),
                (east, south),
                (east, north),
                (west, north),
                (west, south),
            ]),
            Vec::new(),
        )]);
        let mut block = Vec::new();
        encode_shape(&geometry, &mut block);
        (block, geometry.bounding_rect().expect("a box"))
    }

    #[test]
    fn a_point_has_the_one_zone_that_covers_it() {
        let dir = Dir::new();
        let zones = dir.load(&zones()).expect("zones");
        assert_eq!(zones.names(), ["Test/West", "Test/East", "Test/Shared"]);
        assert_eq!(zones.at([5.0, 5.0]), Some(0));
        assert_eq!(zones.at([12.0, 5.0]), Some(1));
        assert_eq!(zones.at([30.5, 0.5]), Some(1), "the second part of East");
        assert_eq!(zones.at([2.5, 2.5]), None, "the hole of West");
        assert_eq!(zones.at([17.0, 5.0]), None, "East and Shared");
        assert_eq!(zones.at([25.0, 5.0]), None, "between the parts of East");
        assert_eq!(zones.at([5.0, 50.0]), None);
    }

    #[test]
    fn an_area_has_the_one_zone_it_lies_wholly_in() {
        let dir = Dir::new();
        let zones = dir.load(&zones()).expect("zones");
        let zone = |west: f64, south: f64, east: f64, north: f64| {
            let (block, bounds) = area(west, south, east, north);
            zones.of_area(&block, bounds)
        };
        assert_eq!(zone(4.0, 4.0, 6.0, 6.0), Some(0));
        assert_eq!(zone(11.0, 4.0, 14.0, 6.0), Some(1));
        assert_eq!(zone(8.0, 4.0, 12.0, 6.0), None, "in West and East");
        assert_eq!(zone(1.0, 1.0, 4.0, 4.0), None, "around the hole of West");
        assert_eq!(zone(2.2, 2.2, 2.8, 2.8), None, "in the hole of West");
        assert_eq!(
            zone(16.0, 4.0, 18.0, 6.0),
            None,
            "wholly in East and Shared"
        );
        assert_eq!(zone(12.0, 4.0, 18.0, 6.0), Some(1), "Shared holds a part");
        assert_eq!(zone(18.0, 4.0, 22.0, 6.0), None, "out of every zone");
        assert_eq!(zone(40.0, 4.0, 42.0, 6.0), None);
        assert_eq!(
            zone(10.000_000_4, -0.000_000_4, 20.000_000_4, 10.000_000_4),
            Some(1)
        );
        assert_eq!(
            zone(9.999_999_6, 0.000_000_4, 14.0, 9.999_999_6),
            Some(1),
            "a sliver of it over the border"
        );
        assert_eq!(zone(9.999_99, 0.0, 14.0, 10.0), None, "more than a sliver");
    }

    #[test]
    fn a_file_that_is_not_the_zones_is_refused_by_name_and_fault() {
        let dir = Dir::new();
        let square = format!("[{}]", ring(0.0, 0.0, 1.0, 1.0));
        for (geojson, problem) in [
            ("not json".to_string(), "expected ident at line 1 column 2"),
            (
                r#"{"type":"FeatureCollection"}"#.to_string(),
                "it has no `features` at line 1 column 28",
            ),
            (collection(&[]), "it holds no zone"),
            (
                r#"{"features":[{"type":"Feature","properties":{},"geometry":null}]}"#.to_string(),
                "missing field `tzid` at line 1 column 46",
            ),
            (
                collection(&[feature("", "Polygon", &square)]),
                "a feature's `tzid` is empty at line 1 column 162",
            ),
            (
                collection(&[feature("Test/Point", "Point", "[1,2]")]),
                "zone Test/Point is a Point, not a Polygon or a MultiPolygon at line 1 column 142",
            ),
            (
                collection(&[feature("Test/Flat", "MultiPolygon", &square)]),
                "the coordinates of zone Test/Flat are not nested as those of a MultiPolygon are \
                 at line 1 column 176",
            ),
            (
                collection(&[feature(
                    "Test/Open",
                    "Polygon",
                    "[[[0,0],[1,0],[1,1],[0,1]]]",
                )]),
                "zone Test/Open has a ring of 4 points that does not close at line 1 column 165",
            ),
            (
                collection(&[feature(
                    "Test/Far",
                    "Polygon",
                    "[[[0,0],[181,0],[181,1],[0,0]]]",
                )]),
                "zone Test/Far has the point [181, 0], which is not a longitude and a latitude \
                 at line 1 column 168",
            ),
            (
                collection(&[feature("Test/Mixed", "Polygon", "[[[0,0],[1,0],1,[0,0]]]")]),
                "a coordinate array mixes depths at line 1 column 153",
            ),
            (
                collection(&[feature("Test/One", "Polygon", "[[[0],[1,0],[1,1],[0,0]]]")]),
                "a position is not two numbers at line 1 column 139",
            ),
            (
                "PK\u{3}\u{4}zipped".to_string(),
                "it is a zip archive, so unpack it and give the file inside",
            ),
        ] {
            assert_eq!(dir.problem(&geojson), problem, "{geojson}");
        }
        let missing = dir.0.join("none.json");
        let error = Timezones::load(
            &missing,
            &Scratch::create(dir.0.join("scratch")).expect("scratch"),
        )
        .expect_err("no file");
        assert!(
            matches!(&error, Error::IoPath { action: "failed to open", path, .. } if *path == missing),
            "{error}"
        );
    }

    #[test]
    fn a_position_may_carry_an_altitude_and_a_type_may_follow_its_coordinates() {
        let dir = Dir::new();
        let zones = dir
            .load(
                r#"{"features":[{"geometry":{"coordinates":[[[0,0,5],[4,0,5],[4,4,5.5],[0,4,5],[0,0,5]]],"type":"Polygon"},"properties":{"tzid":"Test/High","other":[1,{"a":2}]},"id":7}],"type":"FeatureCollection"}"#,
            )
            .expect("zones");
        assert_eq!(zones.names(), ["Test/High"]);
        assert_eq!(zones.at([2.0, 2.0]), Some(0));
        assert_eq!(zones.at([5.0, 2.0]), None);
    }
}
