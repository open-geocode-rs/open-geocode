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
    batch::{BatchGeocodeOptions, CoordinateJoinOptions, parse_field_groups, run_batch_geocode},
    bench::{
        BenchmarkFixture, PackBenchmarkOptions, PoiFixtureOptions, benchmark_pack,
        sample_poi_fixture,
    },
    builder::{BuildOsmOptions, build_planned_osm_pack, plan_osm_build},
    geocode_bench::{GeocodeBenchmarkOptions, benchmark_geocode},
    pack::{PackReader, RecordId},
    parse::{
        bench::{
            ParseBenchOptions, TaggerKind, align_predictions, mapping::Scheme, normalize_fixtures,
            run_parse_bench, write_queries,
        },
        lattice::LatticeTagger,
    },
    resources,
    reverse::{PackReverseGeocoder, ReverseGeocodeOptions},
    runtime::{ServeOptions, serve},
    search::{PackTextSearcher, TextSearchOptions},
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
    /// Build a Pack from an OSM .pbf extract, from a city up to the planet.
    Build {
        /// Input .osm.pbf file, sorted by type and id (the standard layout).
        #[arg(long)]
        input: PathBuf,

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

        /// Build even when the pre-flight check finds too little memory or
        /// disk for the input.
        #[arg(long)]
        force: bool,
    },

    /// Inspect Pack records as readable JSON.
    #[command(name = "inspect-pack")]
    InspectPack {
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
    #[command(name = "verify-pack")]
    VerifyPack {
        /// Pack directory or Pack file.
        #[arg(long)]
        pack: PathBuf,
    },

    /// Search a Pack text index and hydrate matching records.
    #[command(name = "search-pack")]
    SearchPack {
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

    /// Label the tokens of an address query with the Pack's parser, and print
    /// its best labellings.
    Parse {
        /// Pack directory or Pack file.
        #[arg(long)]
        pack: PathBuf,

        /// Address query to label.
        query: String,
    },

    /// Reverse geocode one coordinate from a Pack spatial index.
    #[command(name = "reverse-pack")]
    ReversePack {
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
    #[command(name = "bench-pack")]
    BenchPack {
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

    /// Score forward geocoding against authoritative coordinates, in frozen
    /// distance buckets plus geohash agreement. Separate from bench-pack
    /// because that matches object ids and this measures distance.
    #[command(name = "bench-geocode")]
    BenchGeocode {
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

    /// Sample a seeded answer key of named POIs for bench-pack: each case
    /// queries "<name>, <locality>" and expects that POI.
    #[command(name = "sample-poi-fixture")]
    SamplePoiFixture {
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

    /// Score address parsing per token against the gold rows, as-is,
    /// stripped of commas and messy, beside stored baseline predictions for
    /// the same rows.
    #[command(name = "parse-bench")]
    ParseBench {
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

    /// Build parse-bench fixtures: normalize source rows into gold rows,
    /// export each variant's queries for a baseline, and align its answers
    /// back to the gold tokens.
    #[command(name = "parse-fixture", subcommand)]
    ParseFixture(ParseFixtureCommand),

    /// Geocode CSV rows into lat/lon columns using a Pack text index.
    #[command(name = "batch-geocode")]
    BatchGeocode(Box<BatchGeocodeArgs>),

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
        Commands::Build {
            input,
            pack,
            memory_budget_mb,
            scratch_dir,
            force,
        } => {
            resources::opt_out_of_power_throttling();
            let options = BuildOsmOptions {
                input,
                pack,
                memory_budget_bytes: memory_budget_mb
                    .map(|mb| usize::try_from(mb << 20))
                    .transpose()
                    .context("--memory-budget-mb is larger than this machine can address")?,
                scratch_dir,
                force,
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
            write_json(serde_json::json!({
                "records": report.output.record_count,
                "rejections": report.output.rejection_count,
                "pack_bytes": report.output.pack_bytes,
                "scratch_spilled_bytes": report.scratch.spilled_bytes,
                "seconds": report.phases.total_ms as f64 / 1000.0,
            }))
        }
        Commands::InspectPack {
            pack,
            row,
            id,
            layer,
            limit,
            rejections,
            context,
        } => inspect_pack(pack, row, id, layer, limit, rejections, context),
        Commands::VerifyPack { pack } => {
            let reader = PackReader::open(pack)?;
            reader.verify()?;
            write_json(serde_json::json!({
                "path": reader.path().display().to_string(),
                "sections": reader.container().sections().len(),
                "bytes": reader.container().file_size(),
                "verified": true,
            }))
        }
        Commands::SearchPack {
            pack,
            query,
            layer,
            limit,
        } => search_pack(pack, query, layer, limit),
        Commands::ReversePack { pack, lon, lat } => reverse_pack(pack, lon, lat),
        Commands::Parse { pack, query } => parse_query(pack, &query),
        Commands::BenchPack {
            pack,
            queries,
            iterations,
            warmup,
            output,
        } => bench_pack(pack, queries, iterations, warmup, output),
        Commands::BenchGeocode {
            pack,
            queries,
            limit,
            output,
        } => {
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
        Commands::SamplePoiFixture {
            pack,
            count,
            seed,
            output,
        } => {
            let fixture = sample_poi_fixture(PoiFixtureOptions { pack, count, seed })?;
            let cases = fixture.search.len();
            write_fixture_to_path(&fixture, &output)?;
            write_json(serde_json::json!({
                "cases": cases,
                "output": output.display().to_string(),
            }))
        }
        Commands::ParseBench {
            gold,
            predictions,
            countries,
            pack,
            taggers,
            baselines,
            show_misses,
            coverage,
            json,
        } => {
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
        Commands::ParseFixture(command) => parse_fixture(command),
        Commands::BatchGeocode(args) => batch_geocode(*args),
        Commands::Serve {
            pack,
            demo,
            bind,
            basemap,
        } => {
            serve(ServeOptions {
                pack,
                demo,
                bind,
                basemap,
            })
            .await
        }
    }
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
    let reader = PackReader::open(pack)?;
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
    reader: &PackReader,
    source_id: &str,
    include_context: bool,
) -> Result<Value> {
    match reader.find_by_source_id(source_id)? {
        Some(record_id) => inspect_record_json(reader, record_id, include_context),
        None => bail!("record not found: {source_id}"),
    }
}

fn inspect_record_json(
    reader: &PackReader,
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

fn boundary_context_json(reader: &PackReader, record_id: RecordId) -> Result<Value> {
    let Some(context) = reader.boundary_context(record_id)? else {
        return Ok(serde_json::json!(null));
    };
    let mut object = serde_json::Map::new();
    object.insert("flags".to_string(), serde_json::json!(context.flags));

    {
        let tuple = context.admin_context;
        for (key, value) in [
            ("country", tuple.country_record_id),
            ("region", tuple.region_record_id),
            ("district", tuple.district_record_id),
            ("locality", tuple.locality_record_id),
            ("neighbourhood", tuple.neighbourhood_record_id),
            ("place", tuple.place_record_id),
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
    let pack = Arc::new(PackReader::open(pack)?);
    let searcher = PackTextSearcher::from_pack(Arc::clone(&pack))?;
    let hits = searcher.search(TextSearchOptions {
        query: query.clone(),
        limit,
        layer,
    })?;
    write_json(serde_json::to_value(api::forward_collection(
        &pack, query, hits,
    )?)?)
}

/// The `/reverse` answer for a point, as the Runtime gives it.
fn reverse_pack(pack: PathBuf, lon: f64, lat: f64) -> Result<()> {
    let pack = Arc::new(PackReader::open(pack)?);
    let geocoder = PackReverseGeocoder::from_pack(Arc::clone(&pack))?;
    let response = geocoder.reverse(ReverseGeocodeOptions { lon, lat })?;
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
    let report = run_batch_geocode(BatchGeocodeOptions {
        pack,
        input,
        output: output.clone(),
        audit: audit.clone(),
        address_field_groups,
        locality_field,
        region_field,
        postcode_field,
        layer,
        limit,
        lat_column,
        lon_column,
        join,
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
            Commands::Build {
                memory_budget_mb, ..
            } => Ok(memory_budget_mb),
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
