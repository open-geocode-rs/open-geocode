use std::{
    fs::File,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde_json::Value;

use open_geocode::{
    alloc::CountingAlloc,
    api,
    batch::{Column, CoordinateJoinOptions, GeocodeOptions, parse_field_groups, run_batch_geocode},
    bench::{
        BenchmarkFixture, PackBenchmarkOptions, PoiFixtureOptions, benchmark_pack,
        sample_poi_fixture,
    },
    builder::{
        BuildOsmOptions, Leftover,
        addresses::{AddressFile, AddressFiles, AddressSource, AddressSources},
        build_planned_osm_pack, plan_osm_build,
    },
    calibration,
    geocode_bench::{GeocodeBenchmarkOptions, benchmark_geocode},
    openaddresses::OpenAddresses,
    pack::{Reader, RecordId},
    parse::{
        bench::{
            ParseBenchOptions, TaggerKind, align_predictions, mapping::Scheme, normalize_fixtures,
            run_parse_bench, write_queries,
        },
        lattice::LatticeTagger,
    },
    resources,
    reverse::{self, PackReverseGeocoder},
    runtime::{ServeOptions, serve},
    search::{TextSearchOptions, TextSearcher},
};

/// mimalloc, counted. The Windows process heap serializes a many-threaded
/// build on its lock once the heap has churned (LLVM measured a ThinLTO link
/// 16 times slower on it than on mimalloc); mimalloc's heaps are per thread.
/// The count lets the build's memory pool see the heap it does not track.
#[global_allocator]
static ALLOCATOR: CountingAlloc<mimalloc::MiMalloc> = CountingAlloc::new(mimalloc::MiMalloc);

#[derive(Debug, Parser)]
#[command(name = "open-geocode")]
#[command(about = "Build and serve lightweight geocoding data packs")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Build, inspect and query a Pack.
    #[command(subcommand)]
    Pack(PackCommand),

    /// Geocode in bulk, and score how well it does.
    #[command(subcommand)]
    Geocode(GeocodeCommand),

    /// Label address queries, and score the parser.
    #[command(subcommand)]
    Parse(ParseCommand),

    /// Build the fixtures the benchmarks read.
    #[command(subcommand)]
    Fixture(FixtureCommand),

    /// Serve the Runtime HTTP API and static demo files.
    Serve {
        /// Pack directory or Pack file.
        #[arg(long)]
        pack: PathBuf,

        /// Static demo directory to serve.
        #[arg(long, default_value = "demo")]
        demo: PathBuf,

        /// Address and port to bind.
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: SocketAddr,

        /// Basemap PMTiles archive to serve at /basemap.pmtiles. Skipped if the
        /// file is absent, so the demo still runs without a local basemap.
        #[arg(long, default_value = "data/ontario.pmtiles")]
        basemap: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum PackCommand {
    /// Build a Pack from an OSM .pbf extract, from a city up to the planet.
    Build {
        /// Input .osm.pbf file, sorted by type and id (the standard layout).
        /// Repeat it to build one Pack covering several extracts; they
        /// should be from the same date, as an older copy of a shared
        /// boundary would otherwise win over a newer one.
        #[arg(long = "input", required = true)]
        inputs: Vec<PathBuf>,

        /// Output Pack directory. The new build is published atomically.
        #[arg(long)]
        pack: PathBuf,

        /// Memory for the build's buffers, in MiB (64 to 1048576). Larger
        /// inputs spill to scratch files on disk instead of using more memory.
        /// Defaults to half of the memory this process can use, less what the
        /// build holds outside its buffers.
        #[arg(
            long,
            value_parser = clap::value_parser!(u64).range(MIN_MEMORY_BUDGET_MB..=MAX_MEMORY_BUDGET_MB),
        )]
        memory_budget_mb: Option<u64>,

        /// Directory for scratch files. Defaults to inside the Pack directory.
        #[arg(long)]
        scratch_dir: Option<PathBuf>,

        /// The most scratch disk the build may hold at once, in MiB. Near
        /// the cap the build compresses its spill harder and merges older
        /// runs to free space; past it, it stops early and says what is
        /// live and what would fit. The scratch volume's free space caps it
        /// either way.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=1 << 30))]
        scratch_budget_mb: Option<u64>,

        /// Build even when the pre-flight check finds too little memory or
        /// disk for the input.
        #[arg(long)]
        force: bool,

        /// Go on with a build of this Pack that stopped (failed, was
        /// cancelled, ran out of disk or lost power) from its last
        /// checkpoint, when it was a build of the same inputs and options.
        /// Starts over when there is none.
        #[arg(long, conflicts_with = "fresh")]
        resume: bool,

        /// Clear what a build of this Pack that stopped left, and start
        /// over. Without --resume or --fresh, a build refuses to start
        /// beside one and says what it left.
        #[arg(long)]
        fresh: bool,

        /// Add the OpenAddresses points of the countries the Pack covers,
        /// leaving out the ones OSM already has. Downloading needs an
        /// OpenAddresses API token in OA_TOKEN; the files are kept in
        /// <PACK>/openaddresses for the next build.
        #[arg(long)]
        openaddresses: bool,

        /// Add the address points of a file: OpenAddresses GeoJSON, one
        /// feature per line, or CSV with LON, LAT, NUMBER and STREET columns,
        /// gzipped or not. `DATASET=PATH` names the dataset its records
        /// come from; a bare path is named by its file name. Repeat it to add
        /// several.
        #[arg(long = "addresses", value_name = "[DATASET=]PATH")]
        address_files: Vec<String>,
    },

    /// Inspect Pack records as readable JSON.
    Inspect {
        /// Pack directory or Pack file.
        #[arg(long)]
        pack: PathBuf,

        /// Read one Pack record by numeric row id.
        #[arg(long)]
        row: Option<RecordId>,

        /// Read one Pack record by source id, for example osm:node:123.
        #[arg(long)]
        id: Option<String>,

        /// List records from one layer.
        #[arg(long)]
        layer: Option<String>,

        /// Number of records or rejections to print. Use 0 for no limit.
        #[arg(long, default_value_t = 20)]
        limit: usize,

        /// Print rejected evidence instead of accepted records.
        #[arg(long)]
        rejections: bool,

        /// Include Boundary-Derived Context for --row or --id.
        #[arg(long)]
        context: bool,
    },

    /// Check a Pack file against its section checksums, for example after
    /// copying or downloading it.
    Verify {
        /// Pack directory or Pack file.
        #[arg(long)]
        pack: PathBuf,
    },

    /// Search a Pack text index and hydrate matching records.
    Search {
        /// Pack directory or Pack file.
        #[arg(long)]
        pack: PathBuf,

        /// Text query to search.
        #[arg(long)]
        query: String,

        /// Restrict hits to one record layer.
        #[arg(long)]
        layer: Option<String>,

        /// Number of search hits to print. Use 0 for the default.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },

    /// Reverse geocode one coordinate from a Pack spatial index.
    Reverse {
        /// Pack directory or Pack file.
        #[arg(long)]
        pack: PathBuf,

        /// Longitude in WGS84 decimal degrees.
        #[arg(long)]
        lon: f64,

        /// Latitude in WGS84 decimal degrees.
        #[arg(long)]
        lat: f64,
    },

    /// Benchmark Pack size, open time, and query latency.
    Bench {
        /// Pack directory or Pack file.
        #[arg(long)]
        pack: PathBuf,

        /// Optional JSON fixture with search, autocomplete, and reverse cases.
        #[arg(long)]
        queries: Option<PathBuf>,

        /// Measured runs per query case.
        #[arg(long, default_value_t = 5)]
        iterations: usize,

        /// Warmup runs per query case, excluded from latency stats.
        #[arg(long, default_value_t = 1)]
        warmup: usize,

        /// Optional output path for the JSON benchmark report.
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum GeocodeCommand {
    /// Geocode CSV rows into lat/lon columns using a Pack text index.
    Batch(Box<BatchGeocodeArgs>),

    /// Score forward geocoding against authoritative coordinates, in frozen
    /// distance buckets plus geohash agreement. Separate from `pack bench`
    /// because that matches object ids and this measures distance.
    Bench {
        /// Pack directory or Pack file.
        #[arg(long)]
        pack: PathBuf,

        /// JSON fixture of cases carrying expect_lat/expect_lon.
        #[arg(long)]
        queries: PathBuf,

        /// Candidates inspected per query. At least 5, for hit@5.
        #[arg(long, default_value_t = 10)]
        limit: usize,

        /// Optional output path for the JSON report.
        #[arg(long)]
        output: Option<PathBuf>,
    },

    /// Fit the Pack's answer confidence to held-out address points: each is
    /// written as messy queries, searched, and its answers judged right or
    /// wrong. The model is written beside the Pack file, replacing any
    /// before it, and every answer from then on carries a `confidence`.
    Calibrate {
        /// Pack directory or Pack file.
        #[arg(long)]
        pack: PathBuf,

        /// Held-out address points, in the layouts `pack build --addresses`
        /// reads, never ones the Pack was built from. `DATASET=PATH` names
        /// the dataset; a bare path is named by its file name. Repeat it to
        /// add several.
        #[arg(long = "addresses", value_name = "[DATASET=]PATH", required = true)]
        address_files: Vec<String>,

        /// Rows drawn from each file.
        #[arg(long, default_value_t = 1000)]
        rows: usize,

        /// Messy queries written of each row.
        #[arg(long, default_value_t = 3)]
        variants: usize,

        /// Seed of the draws; the same Pack, rows and seed fit the same model.
        #[arg(long, default_value_t = 1)]
        seed: u64,

        /// How far from its row's point, in metres, an answer may lie and
        /// still be right.
        #[arg(long, default_value_t = 100.0)]
        radius_m: f64,
    },
}

#[derive(Debug, Subcommand)]
enum ParseCommand {
    /// Label the tokens of an address query with the Pack's parser, and print
    /// its best labellings.
    Query {
        /// Pack directory or Pack file.
        #[arg(long)]
        pack: PathBuf,

        /// Address query to label.
        query: String,
    },

    /// Score address parsing per token against the gold rows, as-is,
    /// stripped of commas and messy, beside stored baseline predictions for
    /// the same rows.
    Bench {
        /// Gold directory, <cc>.tsv per country.
        #[arg(long, default_value = "fixtures/parse/gold")]
        gold: PathBuf,

        /// Root of stored baseline predictions, <baseline>/<cc>.<variant>.tsv.
        #[arg(long, default_value = "fixtures/parse/predictions")]
        predictions: PathBuf,

        /// Countries to score, comma-separated; default every gold file.
        #[arg(long, value_delimiter = ',')]
        countries: Vec<String>,

        /// Pack directory or Pack file, for taggers that read one.
        #[arg(long)]
        pack: Option<PathBuf>,

        /// og tagger to score. Repeat to compare several.
        #[arg(long = "tagger", value_enum, default_values_t = [TaggerKind::Legacy])]
        taggers: Vec<TaggerKind>,

        /// Baseline to score. Repeat for several; default every one stored.
        #[arg(long = "baseline")]
        baselines: Vec<String>,

        /// List this many of the first tagger's worst rows per variant.
        #[arg(long, default_value_t = 0)]
        show_misses: usize,

        /// Also score every system apart on the rows whose locality, region
        /// and street names the Pack's dictionaries all hold, and on the
        /// others. Needs --pack.
        #[arg(long)]
        coverage: bool,

        /// Also write the full results as JSON to this path.
        #[arg(long)]
        json: Option<PathBuf>,
    },

    /// Build `parse bench` fixtures: normalize source rows into gold rows,
    /// export each variant's queries for a baseline, and align its answers
    /// back to the gold tokens.
    #[command(subcommand)]
    Fixture(ParseFixtureCommand),
}

#[derive(Debug, Subcommand)]
enum FixtureCommand {
    /// Sample a seeded answer key of named POIs for `pack bench`: each case
    /// queries "<name>, <locality>" and expects that POI.
    SamplePoi {
        /// Pack directory or Pack file.
        #[arg(long)]
        pack: PathBuf,

        /// Number of POIs to draw.
        #[arg(long, default_value_t = 500)]
        count: usize,

        /// Seed of the draw; the same Pack and seed give the same fixture.
        #[arg(long, default_value_t = 1)]
        seed: u64,

        /// Output path for the fixture JSON.
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ParseFixtureCommand {
    /// Map and normalize raw `lang<TAB>country<TAB>token/label ...` rows,
    /// <input>/<cc>.tsv, into gold rows, <output>/<cc>.tsv.
    Normalize {
        /// Label scheme of the raw rows.
        #[arg(long, value_enum, default_value_t = Scheme::Libpostal)]
        scheme: Scheme,

        /// Directory of raw <cc>.tsv files.
        #[arg(long)]
        input: PathBuf,

        /// Countries to normalize, comma-separated; default every raw file.
        #[arg(long, value_delimiter = ',')]
        countries: Vec<String>,

        /// Gold directory to write <cc>.tsv into.
        #[arg(long, default_value = "fixtures/parse/gold")]
        output: PathBuf,
    },

    /// Write each gold row's query in every variant, one per line, as
    /// <cc>.as-is.txt, <cc>.stripped.txt and <cc>.messy.txt.
    Queries {
        #[arg(long, default_value = "fixtures/parse/gold")]
        gold: PathBuf,

        /// Countries to export, comma-separated; default every gold file.
        #[arg(long, value_delimiter = ',')]
        countries: Vec<String>,

        #[arg(long)]
        output: PathBuf,
    },

    /// Align a baseline's <cc>.<variant>.jsonl answers to the gold tokens and
    /// store them as <cc>.<variant>.tsv predictions.
    Align {
        /// Label scheme of the answers.
        #[arg(long, value_enum, default_value_t = Scheme::Libpostal)]
        scheme: Scheme,

        #[arg(long, default_value = "fixtures/parse/gold")]
        gold: PathBuf,

        /// Countries to align, comma-separated; default every gold file.
        #[arg(long, value_delimiter = ',')]
        countries: Vec<String>,

        /// Directory of raw answers, <cc>.<variant>.jsonl.
        #[arg(long)]
        input: PathBuf,

        /// Prediction directory for this baseline, for example
        /// fixtures/parse/predictions/libpostal.
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Debug, Args)]
struct BatchGeocodeArgs {
    /// Pack directory or Pack file. Required with --address-fields.
    #[arg(long)]
    pack: Option<PathBuf>,

    /// Input CSV path.
    #[arg(long)]
    input: PathBuf,

    /// Clean output CSV path. Input columns are preserved and lat/lon are added or filled.
    #[arg(long)]
    output: PathBuf,

    /// Audit CSV path.
    #[arg(long)]
    audit: PathBuf,

    /// Comma-separated address columns to try as one query candidate. Repeat for fallbacks.
    #[arg(long = "address-fields")]
    address_fields: Vec<String>,

    /// Optional locality/city column used for engine context validation.
    #[arg(long)]
    locality_field: Option<String>,

    /// Optional region/province/state column used for engine context validation.
    #[arg(long)]
    region_field: Option<String>,

    /// Optional postal-code column used for engine context validation.
    #[arg(long)]
    postcode_field: Option<String>,

    /// Optional layer filter passed to the engine searcher.
    #[arg(long)]
    layer: Option<String>,

    /// ISO 3166-1 alpha-2 codes to answer from, comma-separated; default
    /// every country the Pack covers.
    #[arg(long = "country", value_delimiter = ',')]
    countries: Vec<String>,

    /// Number of engine candidates to inspect per query. Use 0 for the engine default.
    #[arg(long, default_value_t = 10)]
    limit: usize,

    /// Latitude output column name.
    #[arg(long, default_value = "lat")]
    lat_column: String,

    /// Longitude output column name.
    #[arg(long, default_value = "lon")]
    lon_column: String,

    /// Copy coordinates from another CSV instead of geocoding address fields.
    #[arg(long)]
    join_coordinates_from: Option<PathBuf>,

    /// Key column used with --join-coordinates-from.
    #[arg(long)]
    join_key: Option<String>,

    /// Latitude column in the joined CSV.
    #[arg(long, default_value = "lat")]
    join_lat_column: String,

    /// Longitude column in the joined CSV.
    #[arg(long, default_value = "lon")]
    join_lon_column: String,
}

/// Below this, sorters spill so often that the build drowns in tiny run files.
const MIN_MEMORY_BUDGET_MB: u64 = 64;
/// 1 TiB; keeps the conversion to bytes far from overflow.
const MAX_MEMORY_BUDGET_MB: u64 = 1 << 20;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Pack(PackCommand::Build {
            inputs,
            pack,
            memory_budget_mb,
            scratch_dir,
            scratch_budget_mb,
            force,
            resume,
            fresh,
            openaddresses,
            address_files,
        }) => {
            resources::opt_out_of_power_throttling();
            let addresses = address_sources(&pack, openaddresses, &address_files)?;
            let options = BuildOsmOptions {
                inputs,
                pack,
                addresses,
                memory_budget_bytes: memory_budget_mb
                    .map(|mb| usize::try_from(mb << 20))
                    .transpose()
                    .context("--memory-budget-mb is larger than this machine can address")?,
                scratch_dir,
                scratch_budget_bytes: scratch_budget_mb.map(|mb| mb << 20),
                force,
                leftover: match (resume, fresh) {
                    (true, _) => Leftover::Resume,
                    (_, true) => Leftover::Clear,
                    _ => Leftover::Refuse,
                },
                ..BuildOsmOptions::default()
            };
            let plan = plan_osm_build(&options)?;
            // One pool serves every stage of the build, sized by the plan
            // unless RAYON_NUM_THREADS sets it.
            if std::env::var_os("RAYON_NUM_THREADS").is_none() {
                rayon::ThreadPoolBuilder::new()
                    .num_threads(plan.threads)
                    .build_global()
                    .context("failed to start the build's worker threads")?;
            }
            let found = &plan.resources;
            eprintln!(
                "{} MiB memory budget ({}), {} threads; {} usable CPUs ({}), {} MiB memory available ({})",
                plan.memory_budget_bytes >> 20,
                if plan.memory_budget_derived {
                    "derived"
                } else {
                    "set"
                },
                rayon::current_num_threads(),
                found.usable_cpus,
                found.cpu_limit,
                found.available_memory_bytes >> 20,
                found.memory_limit,
            );
            let report = build_planned_osm_pack(options, plan)?;
            eprint!("{}", report.stage_table());
            write_json(serde_json::json!({
                "records": report.output.record_count,
                "rejections": report.output.rejection_count,
                "pack_bytes": report.output.pack_bytes,
                "scratch_spilled_bytes": report.scratch.spilled_bytes,
                "scratch_peak_bytes": report.scratch.peak_live_bytes,
                "seconds": report.phases.total_ms as f64 / 1000.0,
            }))
        }
        Commands::Pack(PackCommand::Inspect {
            pack,
            row,
            id,
            layer,
            limit,
            rejections,
            context,
        }) => inspect_pack(pack, row, id, layer, limit, rejections, context),
        Commands::Pack(PackCommand::Verify { pack }) => {
            let reader = Reader::open(pack)?;
            reader.verify()?;
            write_json(serde_json::json!({
                "path": reader.path().display().to_string(),
                "sections": reader.container().sections().len(),
                "bytes": reader.container().file_size(),
                "verified": true,
            }))
        }
        Commands::Pack(PackCommand::Search {
            pack,
            query,
            layer,
            limit,
        }) => search_pack(pack, query, layer, limit),
        Commands::Pack(PackCommand::Reverse { pack, lon, lat }) => reverse_pack(pack, lon, lat),
        Commands::Parse(ParseCommand::Query { pack, query }) => parse_query(pack, &query),
        Commands::Pack(PackCommand::Bench {
            pack,
            queries,
            iterations,
            warmup,
            output,
        }) => bench_pack(pack, queries, iterations, warmup, output),
        Commands::Geocode(GeocodeCommand::Bench {
            pack,
            queries,
            limit,
            output,
        }) => {
            let report = benchmark_geocode(GeocodeBenchmarkOptions {
                pack,
                fixture: queries,
                limit,
                output: None,
            })?;
            let value = serde_json::to_value(report)?;
            if let Some(output) = output {
                write_json_to_path(&value, output)
            } else {
                write_json(value)
            }
        }
        Commands::Geocode(GeocodeCommand::Calibrate {
            pack,
            address_files,
            rows,
            variants,
            seed,
            radius_m,
        }) => {
            let report = calibration::run(&calibration::Options {
                pack,
                files: address_files_of(&address_files)?,
                rows,
                variants,
                seed,
                radius_m,
            })?;
            write_json(serde_json::to_value(report)?)
        }
        Commands::Fixture(FixtureCommand::SamplePoi {
            pack,
            count,
            seed,
            output,
        }) => {
            let fixture = sample_poi_fixture(PoiFixtureOptions { pack, count, seed })?;
            let cases = fixture.search.len();
            write_fixture_to_path(&fixture, &output)?;
            write_json(serde_json::json!({
                "cases": cases,
                "output": output.display().to_string(),
            }))
        }
        Commands::Parse(ParseCommand::Bench {
            gold,
            predictions,
            countries,
            pack,
            taggers,
            baselines,
            show_misses,
            coverage,
            json,
        }) => {
            let report = run_parse_bench(&ParseBenchOptions {
                gold,
                predictions,
                countries,
                pack,
                taggers,
                baselines: (!baselines.is_empty()).then_some(baselines),
                show_misses,
                coverage,
            })?;
            if let Some(json) = json {
                write_json_to_path(&report, json)?;
            }
            print!("{}", report.to_markdown());
            Ok(())
        }
        Commands::Parse(ParseCommand::Fixture(command)) => parse_fixture(command),
        Commands::Geocode(GeocodeCommand::Batch(args)) => batch_geocode(*args),
        Commands::Serve {
            pack,
            demo,
            bind,
            basemap,
        } => {
            serve(ServeOptions {
                pack,
                demo: Some(demo),
                bind,
                basemap: Some(basemap),
            })
            .await?;
            Ok(())
        }
    }
}

/// What `--openaddresses` and `--addresses` add to a build, `None` for
/// neither.
fn address_sources(
    pack: &Path,
    openaddresses: bool,
    files: &[String],
) -> Result<Option<Arc<dyn AddressSource>>> {
    let mut sources: Vec<Arc<dyn AddressSource>> = Vec::new();
    if openaddresses {
        sources.push(Arc::new(OpenAddresses::from_env(
            pack.join("openaddresses"),
        )?));
    }
    if !files.is_empty() {
        sources.push(Arc::new(AddressFiles(address_files_of(files)?)));
    }
    Ok(match sources.len() {
        0 => None,
        1 => sources.pop(),
        _ => Some(Arc::new(AddressSources(sources))),
    })
}

/// The files `--addresses` names, each `[DATASET=]PATH`: a bare path is
/// named by its file name.
fn address_files_of(files: &[String]) -> Result<Vec<AddressFile>> {
    files
        .iter()
        .map(|file| {
            let (dataset, path) = match file.split_once('=') {
                Some((dataset, path)) if !dataset.is_empty() => {
                    (dataset.to_string(), PathBuf::from(path))
                }
                _ => {
                    let path = PathBuf::from(file);
                    let name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .and_then(|name| name.split('.').next())
                        .filter(|name| !name.is_empty())
                        .with_context(|| format!("--addresses {file} names no file"))?
                        .to_string();
                    (name, path)
                }
            };
            Ok(AddressFile { dataset, path })
        })
        .collect()
}

fn parse_fixture(command: ParseFixtureCommand) -> Result<()> {
    let files = match command {
        ParseFixtureCommand::Normalize {
            scheme,
            input,
            countries,
            output,
        } => normalize_fixtures(scheme, &input, &countries, &output)?
            .into_iter()
            .map(|(path, rows, dropped)| {
                serde_json::json!({ "path": path, "rows": rows, "dropped": dropped })
            })
            .collect::<Vec<_>>(),
        ParseFixtureCommand::Queries {
            gold,
            countries,
            output,
        } => write_queries(&gold, &countries, &output)?
            .into_iter()
            .map(|(path, rows)| serde_json::json!({ "path": path, "rows": rows }))
            .collect(),
        ParseFixtureCommand::Align {
            scheme,
            gold,
            countries,
            input,
            output,
        } => align_predictions(scheme, &gold, &countries, &input, &output)?
            .into_iter()
            .map(|(path, rows, failed)| {
                serde_json::json!({ "path": path, "rows": rows, "failed": failed })
            })
            .collect(),
    };
    write_json(serde_json::json!({ "files": files }))
}

fn inspect_pack(
    pack: PathBuf,
    row: Option<RecordId>,
    id: Option<String>,
    layer: Option<String>,
    limit: usize,
    rejections: bool,
    include_context: bool,
) -> Result<()> {
    let reader = Reader::open(pack)?;
    let output = if rejections {
        serde_json::to_value(reader.rejections(limit)?)?
    } else if let Some(row) = row {
        inspect_record_json(&reader, row, include_context)?
    } else if let Some(id) = id {
        inspect_record_by_source_id_json(&reader, &id, include_context)?
    } else if let Some(layer) = layer {
        serde_json::to_value(reader.records_json_by_layer(&layer, limit)?)?
    } else {
        serde_json::json!({
            "path": reader.path().display().to_string(),
            "manifest": reader.manifest(),
            "sections": reader.section_sizes(),
        })
    };

    write_json(output)
}

fn inspect_record_by_source_id_json(
    reader: &Reader,
    source_id: &str,
    include_context: bool,
) -> Result<Value> {
    match reader.find_by_source_id(source_id)? {
        Some(record_id) => inspect_record_json(reader, record_id, include_context),
        None => bail!("record not found: {source_id}"),
    }
}

fn inspect_record_json(
    reader: &Reader,
    record_id: RecordId,
    include_context: bool,
) -> Result<Value> {
    let mut value = reader.record_json(record_id)?;
    if !include_context {
        return Ok(value);
    }

    let boundary_context = boundary_context_json(reader, record_id)?;
    let Some(object) = value.as_object_mut() else {
        bail!("record JSON must be an object");
    };
    object.insert("boundary_context".to_string(), boundary_context);
    Ok(value)
}

fn boundary_context_json(reader: &Reader, record_id: RecordId) -> Result<Value> {
    let Some(context) = reader.boundary_context(record_id)? else {
        return Ok(serde_json::json!(null));
    };
    let mut object = serde_json::Map::new();
    object.insert("flags".to_string(), serde_json::json!(context.flags));

    {
        let tuple = context.admin_context;
        for (key, value) in [
            ("country", tuple.country_record_id()),
            ("region", tuple.region_record_id()),
            ("district", tuple.district_record_id()),
            ("locality", tuple.locality_record_id()),
            ("neighbourhood", tuple.neighbourhood_record_id()),
            ("place", tuple.place_record_id()),
        ] {
            if let Some(parent_id) = value
                && let Some(record) = reader.context_record(parent_id)?
            {
                object.insert(
                    key.to_string(),
                    serde_json::json!({
                        "record_id": parent_id,
                        "id": record.id,
                        "label": record.label,
                        "name": record.name,
                        "layer": record.layer,
                    }),
                );
            }
        }
    }

    Ok(Value::Object(object))
}

fn write_json(value: Value) -> Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer_pretty(&mut lock, &value)?;
    use std::io::Write;
    writeln!(lock)?;
    Ok(())
}

/// A query fixture as one compact JSON object per case. Pretty-printing 500
/// cases spans 4,500 lines; a line per case stays greppable and keeps a
/// regenerated answer key to a readable diff.
fn write_fixture_to_path(fixture: &BenchmarkFixture, output: &Path) -> Result<()> {
    create_output_dir(output)?;
    let body = fixture
        .search
        .iter()
        .map(|case| Ok(format!("    {}", serde_json::to_string(case)?)))
        .collect::<Result<Vec<_>>>()?
        .join(",\n");
    std::fs::write(output, format!("{{\n  \"search\": [\n{body}\n  ]\n}}\n"))?;
    Ok(())
}

fn create_output_dir(output: &Path) -> Result<()> {
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn write_json_to_path(value: &impl serde::Serialize, output: PathBuf) -> Result<()> {
    create_output_dir(&output)?;
    let file = File::create(&output)?;
    serde_json::to_writer_pretty(file, value)?;
    Ok(())
}

/// Each token with its label, then the n-best labellings with their scores.
fn parse_query(pack: PathBuf, query: &str) -> Result<()> {
    let tagger = LatticeTagger::open(&pack)?;
    let parses = tagger.parse(query);
    let Some(best) = parses.first() else {
        bail!("the query has no words to label");
    };
    let width = best
        .tokens
        .iter()
        .map(|tagged| tagged.token.chars().count())
        .max()
        .unwrap_or_default();
    for tagged in &best.tokens {
        println!("{:width$}  {}", tagged.token, tagged.label.name());
    }
    println!();
    for (rank, parse) in parses.iter().enumerate() {
        let country = parse
            .country
            .map_or("--".to_string(), |code| code.to_string());
        let labelled = parse
            .pieces
            .iter()
            .map(|tagged| format!("{}/{}", tagged.token, tagged.label.name()))
            .collect::<Vec<_>>()
            .join(" ");
        println!("{}. {:8.3} {country}  {labelled}", rank + 1, parse.score);
    }
    Ok(())
}

/// The `/search` answer for `query`, as the Runtime gives it.
fn search_pack(pack: PathBuf, query: String, layer: Option<String>, limit: usize) -> Result<()> {
    let pack = Arc::new(Reader::open(pack)?);
    let searcher = TextSearcher::from_pack(Arc::clone(&pack))?;
    let hits = searcher.search(TextSearchOptions {
        query: query.clone(),
        limit,
        layer,
        countries: Vec::new(),
        focus: None,
        bbox: None,
    })?;
    write_json(serde_json::to_value(api::forward_collection(
        &pack, query, hits,
    )?)?)
}

/// The `/reverse` answer for a point, as the Runtime gives it.
fn reverse_pack(pack: PathBuf, lon: f64, lat: f64) -> Result<()> {
    let pack = Arc::new(Reader::open(pack)?);
    let geocoder = PackReverseGeocoder::from_pack(Arc::clone(&pack))?;
    let response = geocoder.reverse(reverse::GeocodeOptions { lon, lat })?;
    write_json(serde_json::to_value(api::reverse_collection(
        &pack, &response,
    )?)?)
}

fn bench_pack(
    pack: PathBuf,
    queries: Option<PathBuf>,
    iterations: usize,
    warmup: usize,
    output: Option<PathBuf>,
) -> Result<()> {
    let report = benchmark_pack(PackBenchmarkOptions {
        pack,
        queries,
        iterations,
        warmup,
    })?;
    let value = serde_json::to_value(report)?;
    if let Some(output) = output {
        write_json_to_path(&value, output)
    } else {
        write_json(value)
    }
}

fn batch_geocode(args: BatchGeocodeArgs) -> Result<()> {
    let BatchGeocodeArgs {
        pack,
        input,
        output,
        audit,
        address_fields,
        locality_field,
        region_field,
        postcode_field,
        layer,
        countries,
        limit,
        lat_column,
        lon_column,
        join_coordinates_from,
        join_key,
        join_lat_column,
        join_lon_column,
    } = args;
    let address_field_groups = parse_field_groups(&address_fields)?;
    let join = if let Some(path) = join_coordinates_from {
        let key_column = join_key.context("--join-key is required with --join-coordinates-from")?;
        Some(CoordinateJoinOptions {
            path,
            key_column,
            lat_column: join_lat_column,
            lon_column: join_lon_column,
        })
    } else {
        None
    };
    let report = run_batch_geocode(GeocodeOptions {
        pack,
        input,
        output: output.clone(),
        audit: Some(audit.clone()),
        address_field_groups,
        locality_field: locality_field.map(Column::Named),
        region_field: region_field.map(Column::Named),
        postcode_field: postcode_field.map(Column::Named),
        layer,
        countries,
        limit,
        lat_column,
        lon_column,
        join,
        ..GeocodeOptions::default()
    })?;
    write_json(serde_json::json!({
        "rows": report.rows,
        "resolved": report.resolved,
        "output": output.display().to_string(),
        "audit": audit.display().to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn build_budget(value: Option<&str>) -> Result<Option<u64>, clap::Error> {
        let mut args = vec![
            "open-geocode",
            "pack",
            "build",
            "--input",
            "in.osm.pbf",
            "--pack",
            "pack",
        ];
        if let Some(value) = value {
            args.extend(["--memory-budget-mb", value]);
        }
        match Cli::try_parse_from(args)?.command {
            Commands::Pack(PackCommand::Build {
                memory_budget_mb, ..
            }) => Ok(memory_budget_mb),
            _ => unreachable!("parsed a build command"),
        }
    }

    #[test]
    fn memory_budget_must_be_usable() {
        assert!(build_budget(Some("0")).is_err());
        assert!(build_budget(Some("63")).is_err());
        assert!(build_budget(Some("99999999999999999")).is_err());
        assert_eq!(build_budget(Some("64")).expect("minimum"), Some(64));
        assert_eq!(build_budget(Some("16384")).expect("planet"), Some(16_384));
        assert_eq!(build_budget(None).expect("derived"), None);
    }
}
