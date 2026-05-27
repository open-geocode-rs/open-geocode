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
        #[arg(long, default_value = "127.0.0.1:5173")]
        bind: SocketAddr,
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
        } => inspect_pack(pack, row, id, layer, limit, rejections),
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
        Commands::Serve { pack, demo, bind } => serve(ServeOptions { pack, demo, bind }).await,
    }
}

fn inspect_pack(
    pack: PathBuf,
    row: Option<RecordId>,
    id: Option<String>,
    layer: Option<String>,
    limit: usize,
    rejections: bool,
) -> Result<()> {
    let reader = PackReader::open(pack)?;
    let output = if rejections {
        serde_json::to_value(reader.rejections(limit)?)?
    } else if let Some(row) = row {
        serde_json::to_value(reader.read_record(row)?)?
    } else if let Some(id) = id {
        let Some(record) = reader.record_by_source_id(&id)? else {
            bail!("record not found: {id}");
        };
        serde_json::to_value(record)?
    } else if let Some(layer) = layer {
        serde_json::to_value(reader.records_by_layer(&layer, limit)?)?
    } else {
        serde_json::to_value(reader.manifest())?
    };

    write_json(output)
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
