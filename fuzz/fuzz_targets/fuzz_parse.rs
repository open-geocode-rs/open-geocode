//! Parses any query the HTTP API accepts and fails on a panic or a slow parse.
//!
//! The parser's dictionaries and weights are trained into each Pack, so the
//! target builds one small Pack once and parses against it.
//!
//! Run it with `--sanitizer none`: under AddressSanitizer a parse is about
//! five times slower, and the time limit stops meaning anything.

#![no_main]

use std::{
    path::PathBuf,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use libfuzzer_sys::fuzz_target;
use open_geocode::{
    builder::{
        BuildObserver, BuildOsmOptions, BuildStage, build_osm_pack,
        osm::test_pbf::{TestElement, node, relation, way, write_pbf},
    },
    parse::lattice::LatticeTagger,
};

/// `MAX_QUERY_CHARS` in `src/http/bounds.rs`: longer queries never reach the parser.
const MAX_QUERY_CHARS: usize = 256;

/// 50 ms in a release build, the most a parse may take before it ties up a
/// server thread. Coverage instrumentation makes a parse about three times
/// slower: a 256-character query timed 52 ms here and 19 ms in release.
const SLOW: Duration = Duration::from_millis(150);

/// A parse over [`SLOW`] is timed again this many times, and fails only if
/// every run is slow, so a runner's scheduling hiccup is not a failure.
const RETRIES: usize = 3;

static TAGGER: LazyLock<LatticeTagger> = LazyLock::new(fixture);

fuzz_target!(|query: &str| {
    if query.chars().count() > MAX_QUERY_CHARS {
        return;
    }
    let tagger = &*TAGGER;
    let time = || {
        let start = Instant::now();
        let readings = tagger.parse(query).len();
        (start.elapsed(), readings)
    };
    let (mut took, readings) = time();
    for _ in 0..RETRIES {
        if took <= SLOW {
            return;
        }
        took = took.min(time().0);
    }
    assert!(
        took <= SLOW,
        "parse took {took:?}, over {SLOW:?}, for {query:?} ({readings} readings)"
    );
});

/// The builder prints its stages to the terminal by default, once per
/// fuzzer process.
#[derive(Debug)]
struct Quiet;

impl BuildObserver for Quiet {
    fn started(&self, _: BuildStage, _: Option<u64>) {}
    fn advanced(&self, _: u64) {}
    fn finished(&self, _: BuildStage) {}
    fn warned(&self, _: &str) {}
}

fn fixture() -> LatticeTagger {
    let dir = std::env::temp_dir().join(format!("og-fuzz-parse-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("fixture directory");
    let input = dir.join("fixture.osm.pbf");
    let pack: PathBuf = dir.join("pack");
    write_pbf(&input, &elements());
    build_osm_pack(&BuildOsmOptions {
        inputs: vec![input],
        pack: pack.clone(),
        observer: Arc::new(Quiet),
        ..BuildOsmOptions::default()
    })
    .expect("fixture Pack");
    let tagger = LatticeTagger::open(&pack).expect("fixture tagger");
    let _ = std::fs::remove_dir_all(&dir);
    tagger
}

fn address(
    id: i64,
    lat: f64,
    lon: f64,
    number: &'static str,
    street: &'static str,
    city: &'static str,
    postcode: &'static str,
) -> TestElement {
    node(
        id,
        lat,
        lon,
        &[
            ("addr:housenumber", number),
            ("addr:street", street),
            ("addr:city", city),
            ("addr:postcode", postcode),
        ],
    )
}

/// A few places in Latin, accented and Japanese script, so the Pack's
/// dictionaries hold streets, localities, regions and postcodes in each.
fn elements() -> Vec<Vec<TestElement>> {
    vec![
        vec![
            address(
                1,
                43.6000,
                -79.4000,
                "670",
                "King Street West",
                "Toronto",
                "M5V 1M9",
            ),
            address(
                2,
                43.6001,
                -79.4001,
                "12½",
                "Rue d'Éole",
                "Montréal",
                "H2X 1Y4",
            ),
            address(
                3,
                44.3000,
                -78.3200,
                "1",
                "George Street",
                "Peterborough",
                "K9H 2H7",
            ),
            address(4, 48.1400, 11.5800, "8", "Parkstraße", "München", "80331"),
            address(
                5,
                35.6800,
                139.7600,
                "1-1",
                "内堀通り",
                "千代田区",
                "100-0001",
            ),
            node(
                6,
                43.6700,
                -79.4000,
                &[("place", "neighbourhood"), ("name", "Annex")],
            ),
            node(20, 43.6100, -79.4200, &[]),
            node(21, 43.6100, -79.4100, &[]),
            node(1011, 43.0, -80.0, &[]),
            node(1012, 43.0, -79.0, &[]),
            node(1013, 44.0, -79.0, &[]),
            node(1014, 44.0, -80.0, &[]),
            node(1021, 35.0, 139.0, &[]),
            node(1022, 35.0, 140.0, &[]),
            node(1023, 36.0, 140.0, &[]),
            node(1024, 36.0, 139.0, &[]),
        ],
        vec![
            way(
                600,
                &[20, 21],
                &[("highway", "residential"), ("name", "Queen Street East")],
            ),
            way(601, &[1011, 1012, 1013, 1014, 1011], &[]),
            way(602, &[1021, 1022, 1023, 1024, 1021], &[]),
        ],
        vec![
            relation(
                900,
                &[(601, "outer")],
                &[
                    ("boundary", "administrative"),
                    ("admin_level", "4"),
                    ("name", "Ontario"),
                    ("ISO3166-2", "CA-ON"),
                ],
            ),
            relation(
                901,
                &[(602, "outer")],
                &[
                    ("boundary", "administrative"),
                    ("admin_level", "4"),
                    ("name", "東京都"),
                    ("name:en", "Tokyo"),
                    ("ISO3166-2", "JP-13"),
                ],
            ),
        ],
    ]
}
