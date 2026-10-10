use std::{fs::File, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde_json::Value;

use open_geocode::{
    batch::{BatchGeocodeOptions, CoordinateJoinOptions, parse_field_groups, run_batch_geocode},
    bench::{PackBenchmarkOptions, benchmark_pack},
    builder::{BuildOsmOptions, DEFAULT_MEMORY_BUDGET_BYTES, build_osm_pack},
    pack::{PackReader, RecordId},
    reverse::{PackReverseGeocoder, ReverseGeocodeOptions},
    route::{RouteOptions, route},
    runtime::{ServeOptions, serve},
    search::{PackTextSearcher, TextSearchOptions},
};

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

        /// Memory for build sort buffers, in MiB (64 to 1048576). Larger inputs
        /// spill to scratch files on disk instead of using more memory.
        #[arg(
            long,
            default_value_t = (DEFAULT_MEMORY_BUDGET_BYTES >> 20) as u64,
            value_parser = clap::value_parser!(u64).range(MIN_MEMORY_BUDGET_MB..=MAX_MEMORY_BUDGET_MB),
        )]
        memory_budget_mb: u64,

        /// Directory for scratch files. Defaults to inside the Pack directory.
        #[arg(long)]
        scratch_dir: Option<PathBuf>,
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
    /// Serve one HTTP entry point in front of one Runtime per country Pack.
    ///
    /// Requests with `country=XX` go to that country's Runtime; /search and /autocomplete
    /// without it go to every Runtime and are merged by score.
    Route {
        /// A Runtime as COUNTRY=URL, e.g. NZ=http://127.0.0.1:8081. Repeat per country.
        #[arg(long = "worker", required = true)]
        workers: Vec<String>,

        /// Address and port to bind.
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: SocketAddr,
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
        } => {
            let report = build_osm_pack(BuildOsmOptions {
                input,
                pack,
                memory_budget_bytes: usize::try_from(memory_budget_mb << 20)
                    .context("--memory-budget-mb is larger than this machine can address")?,
                scratch_dir,
            })?;
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
        Commands::BenchPack {
            pack,
            queries,
            iterations,
            warmup,
            output,
        } => bench_pack(pack, queries, iterations, warmup, output),
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
        Commands::Route { workers, bind } => {
            let workers = workers
                .into_iter()
                .map(|w| match w.split_once('=') {
                    Some((country, url)) => Ok((country.to_string(), url.to_string())),
                    None => bail!("--worker takes COUNTRY=URL, got {w:?}"),
                })
                .collect::<Result<Vec<_>>>()?;
            route(RouteOptions { workers, bind }).await
        }
    }
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

fn write_json_to_path(value: &Value, output: PathBuf) -> Result<()> {
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(&output)?;
    serde_json::to_writer_pretty(file, value)?;
    Ok(())
}

fn search_pack(pack: PathBuf, query: String, layer: Option<String>, limit: usize) -> Result<()> {
    let searcher = PackTextSearcher::open(pack)?;
    let hits = searcher.search(TextSearchOptions {
        query,
        limit,
        layer,
    })?;
    write_json(serde_json::to_value(hits)?)
}

fn reverse_pack(pack: PathBuf, lon: f64, lat: f64) -> Result<()> {
    let geocoder = PackReverseGeocoder::open(pack)?;
    let response = geocoder.reverse(ReverseGeocodeOptions { lon, lat })?;
    write_json(serde_json::to_value(response)?)
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

    fn build_budget(value: &str) -> Result<usize, clap::Error> {
        let cli = Cli::try_parse_from([
            "open-geocode",
            "build",
            "--input",
            "in.osm.pbf",
            "--pack",
            "pack",
            "--memory-budget-mb",
            value,
        ])?;
        match cli.command {
            Commands::Build {
                memory_budget_mb, ..
            } => Ok(memory_budget_mb as usize),
            _ => unreachable!("parsed a build command"),
        }
    }

    #[test]
    fn memory_budget_must_be_usable() {
        assert!(build_budget("0").is_err());
        assert!(build_budget("63").is_err());
        assert!(build_budget("99999999999999999").is_err());
        assert_eq!(build_budget("64").expect("minimum"), 64);
        assert_eq!(build_budget("16384").expect("planet"), 16_384);
    }
}
