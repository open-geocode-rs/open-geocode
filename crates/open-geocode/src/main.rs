use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use open_geocode::normalize_osm::{NormalizeOsmOptions, normalize_osm};

#[derive(Debug, Parser)]
#[command(name = "open-geocode")]
#[command(about = "Build and serve lightweight geocoding data packs")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Normalize explicit OSM address records from a regional .osm.pbf extract.
    NormalizeOsm {
        /// Input regional .osm.pbf extract.
        #[arg(long)]
        input: PathBuf,

        /// Output normalized AddressRecord NDJSON.
        #[arg(long)]
        output: PathBuf,

        /// Output import report JSON.
        #[arg(long)]
        report: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::NormalizeOsm {
            input,
            output,
            report,
        } => normalize_osm(NormalizeOsmOptions {
            input,
            output,
            report,
        }),
    }
}
