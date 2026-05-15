use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use open_geocode::builder::{BuildOsmRecordsOptions, build_osm_records};

#[derive(Debug, Parser)]
#[command(name = "open-geocode")]
#[command(about = "Build and serve lightweight geocoding data packs")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Build normalized records from a regional OSM .pbf extract.
    #[command(name = "build-osm-records", alias = "normalize-osm")]
    BuildOsmRecords {
        /// Input regional .osm.pbf extract.
        #[arg(long)]
        input: PathBuf,

        /// Output normalized record NDJSON.
        #[arg(long)]
        output: PathBuf,

        /// Output builder report JSON.
        #[arg(long)]
        report: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::BuildOsmRecords {
            input,
            output,
            report,
        } => build_osm_records(BuildOsmRecordsOptions {
            input,
            output,
            report,
        }),
    }
}
