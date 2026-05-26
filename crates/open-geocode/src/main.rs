use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use serde_json::Value;

use open_geocode::{
    builder::{BuildOsmOptions, build_osm_pack},
    pack::{PackReader, RecordId},
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
}

fn main() -> Result<()> {
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
