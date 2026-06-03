use std::{fs::File, net::SocketAddr, path::PathBuf};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use serde_json::Value;

use open_geocode::{
    bench::{PackBenchmarkOptions, benchmark_pack},
    builder::{BuildOsmOptions, build_osm_pack},
    pack::{PackReader, RecordId},
    reverse::{PackReverseGeocoder, ReverseGeocodeOptions},
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
    /// Build a binary Pack from a regional OSM .pbf extract.
    Build {
        /// Input regional .osm.pbf extract.
        #[arg(long)]
        input: PathBuf,

        /// Output binary Pack directory.
        #[arg(long)]
        pack: PathBuf,
    },

    /// Inspect binary Pack records as readable JSON.
    #[command(name = "inspect-pack")]
    InspectPack {
        /// Binary Pack directory.
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

        /// Include materialized Boundary-Derived Context for --row or --id.
        #[arg(long)]
        context: bool,
    },

    /// Search a Pack text index and hydrate matching records.
    #[command(name = "search-pack")]
    SearchPack {
        /// Binary Pack directory.
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
        /// Binary Pack directory.
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
        /// Binary Pack directory.
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

    /// Serve the Runtime HTTP API and static demo files.
    Serve {
        /// Binary Pack directory.
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Build { input, pack } => build_osm_pack(BuildOsmOptions { input, pack }),
        Commands::InspectPack {
            pack,
            row,
            id,
            layer,
            limit,
            rejections,
            context,
        } => inspect_pack(pack, row, id, layer, limit, rejections, context),
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
        serde_json::to_value(reader.manifest())?
    };

    write_json(output)
}

fn inspect_record_by_source_id_json(
    reader: &PackReader,
    source_id: &str,
    include_context: bool,
) -> Result<Value> {
    for record_id in 0..reader.manifest().record_count {
        let summary = reader.record_summary(record_id)?;
        if summary.id == source_id {
            return inspect_record_json(reader, record_id, include_context);
        }
    }
    bail!("record not found: {source_id}")
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
    object.insert(
        "assignment_method".to_string(),
        serde_json::json!(context.assignment_method),
    );
    object.insert("flags".to_string(), serde_json::json!(context.flags));

    if let Some(tuple) = context.admin_context {
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

    if let Some(postcode_record_id) = context.postcode_record_id
        && let Some(record) = reader.context_record(postcode_record_id)?
    {
        object.insert(
            "postcode".to_string(),
            serde_json::json!({
                "record_id": postcode_record_id,
                "id": record.id,
                "label": record.label,
                "name": record.name,
                "layer": record.layer,
                "postcode": record.postcode,
            }),
        );
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
