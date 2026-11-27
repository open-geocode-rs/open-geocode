mod app;
mod ask;
mod batch;
mod engine;
mod format;
mod home;
mod job;
mod net;
mod regions;
#[cfg(test)]
#[path = "../../test_support.rs"]
mod test_support;
mod ui;

use std::{
    io::{self, IsTerminal},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use open_geocode::{
    alloc::CountingAlloc,
    builder::Leftover,
    limits::Limits,
    resources,
    runtime::{DEFAULT_SHUTDOWN_TIMEOUT_SECS, ServeOptions, serve},
};
use ratatui::{
    DefaultTerminal, Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    crossterm::{
        cursor::{MoveTo, Show},
        event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind},
        execute,
        terminal::{self, Clear, ClearType},
    },
    widgets::{Paragraph, Widget},
};

use app::App;
use home::Home;
use job::{BuildInput, BuildJob, BuildRequest};
use regions::Region;

#[global_allocator]
static ALLOCATOR: CountingAlloc<mimalloc::MiMalloc> = CountingAlloc::new(mimalloc::MiMalloc);

#[derive(Parser)]
#[command(
    name = "og",
    version,
    about = "Build and search geocoding Packs from open data"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Download a region's OpenStreetMap extract and build a Pack from it.
    Build {
        /// The region's catalog id, for example `canada/ontario`. Repeat it
        /// to build one Pack covering several regions, served by one process.
        #[arg(long = "region", required = true)]
        regions: Vec<String>,

        /// What to call a Pack of several regions. Defaults to their ids
        /// joined; ignored when building one region.
        #[arg(long)]
        name: Option<String>,

        /// Memory the build may hold, in MiB, beside a fixed 512 MiB and
        /// 64 MiB per worker thread, all free beyond what the process already
        /// holds outside its heap. Sized to what this machine can give when
        /// not set.
        #[arg(long)]
        memory_budget_mb: Option<u64>,

        /// The most scratch disk the build may hold at once, in MiB. It
        /// compresses its spill harder as it nears the cap and stops early
        /// when it cannot fit. The scratch volume's free space caps it
        /// either way.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=1 << 30))]
        scratch_budget_mb: Option<u64>,

        /// Build even when the pre-flight check finds too little memory or
        /// disk.
        #[arg(long)]
        force: bool,

        /// Go on with a build of this Pack that stopped from its last
        /// checkpoint; start over when there is none.
        #[arg(long, conflicts_with = "fresh")]
        resume: bool,

        /// Clear what a build of this Pack that stopped left, and start
        /// over.
        #[arg(long)]
        fresh: bool,

        /// Download the extract again even when one is on disk.
        #[arg(long)]
        download: bool,

        /// Add the OpenAddresses points of the countries the Pack covers,
        /// leaving out the ones OSM already has. Needs an OpenAddresses API
        /// token in OA_TOKEN.
        #[arg(long)]
        openaddresses: bool,

        /// Add openly licensed Overture Maps places. Needs duckdb on PATH.
        #[arg(long)]
        overture: bool,

        /// Read reviewed Overture place locations for this release.
        #[arg(long, value_name = "JSON", requires = "overture")]
        overture_verified: Option<PathBuf>,

        /// Enrich U.S. results with TIGER address ranges and Census codes.
        #[arg(long)]
        tiger: bool,
    },
    /// Serve a Pack's HTTP API.
    Serve {
        /// A Pack in the og home, by name (`canada-ontario`), or a path.
        pack: String,

        /// Address and port to listen on.
        #[arg(long, default_value = app::SERVE_ADDRESS)]
        bind: SocketAddr,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = Home::locate()?;
    // A build is a long run no user seems to wait on; it should not be
    // moved to efficiency cores, nor slow what the user works in meanwhile.
    resources::opt_out_of_power_throttling();
    resources::yield_to_interactive_programs();
    match cli.command {
        Some(Command::Build {
            regions,
            name,
            memory_budget_mb,
            scratch_budget_mb,
            force,
            resume,
            fresh,
            download,
            openaddresses,
            overture,
            overture_verified,
            tiger,
        }) => {
            let memory_budget_bytes = memory_budget_mb
                .map(|mb| usize::try_from(mb << 20))
                .transpose()
                .context("--memory-budget-mb is larger than this machine can address")?;
            build(
                home,
                &regions,
                name.as_deref(),
                Flags {
                    memory_budget_bytes,
                    scratch_budget_bytes: scratch_budget_mb.map(|mb| mb << 20),
                    force,
                    leftover: match (resume, fresh) {
                        (true, _) => Leftover::Resume,
                        (_, true) => Leftover::Clear,
                        _ => Leftover::Refuse,
                    },
                    download,
                    openaddresses,
                    overture,
                    overture_verified,
                    tiger,
                },
            )
        }
        Some(Command::Serve { pack, bind }) => {
            let dir = home.packs_dir().join(&pack);
            let pack = if dir.is_dir() {
                dir
            } else {
                PathBuf::from(pack)
            };
            serve_pack(pack, bind)
        }
        None => interactive(home),
    }
}

fn serve_pack(pack: PathBuf, bind: SocketAddr) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(serve_options(pack, bind)))
        .map_err(Into::into)
}

fn serve_options(pack: PathBuf, bind: SocketAddr) -> ServeOptions {
    ServeOptions {
        pack,
        demo: None,
        bind,
        basemap: None,
        job_dir: None,
        limits: Limits::DEFAULT,
        hook_hosts: Vec::new(),
        public_url: None,
        shutdown_timeout: Duration::from_secs(DEFAULT_SHUTDOWN_TIMEOUT_SECS),
    }
}

struct Flags {
    memory_budget_bytes: Option<usize>,
    scratch_budget_bytes: Option<u64>,
    force: bool,
    leftover: Leftover,
    download: bool,
    openaddresses: bool,
    overture: bool,
    overture_verified: Option<PathBuf>,
    tiger: bool,
}

fn build(home: Home, ids: &[String], name: Option<&str>, flags: Flags) -> Result<()> {
    let Flags {
        memory_budget_bytes,
        scratch_budget_bytes,
        force,
        leftover,
        download,
        openaddresses,
        overture,
        overture_verified,
        tiger,
    } = flags;
    let tree = regions::tree();
    let mut picked: Vec<&'static Region> = Vec::new();
    for id in ids {
        let Some(region) = tree.get(id) else {
            bail!("no region {id} in the catalog; ids look like canada/ontario");
        };
        if let Some(other) = picked
            .iter()
            .find(|other| regions::overlaps(tree, other, region))
        {
            if other.index == region.index {
                bail!("region {id} was given twice");
            }
            bail!(
                "{} and {} overlap: one contains the other, so build the wider one alone",
                other.id,
                region.id
            );
        }
        picked.push(region);
    }
    let inputs: Vec<BuildInput> = picked
        .iter()
        .map(|region| {
            let extract = home.extract_path(region);
            BuildInput {
                region,
                download: download || !extract.is_file(),
                extract,
            }
        })
        .collect();
    let pack = match (picked.as_slice(), name) {
        ([region], None) => home.pack_dir(region),
        (_, Some(name)) => home.named_pack_dir(name),
        (many, None) => home.named_pack_dir(
            &many
                .iter()
                .map(|region| region.slug())
                .collect::<Vec<_>>()
                .join("-"),
        ),
    };
    let request = BuildRequest {
        inputs,
        pack,
        memory_budget_bytes,
        scratch_budget_bytes,
        force,
        leftover,
        openaddresses,
        overture,
        overture_verified,
        tiger,
    };
    let pack = request.pack.clone();
    let mut job = BuildJob::start(request, Arc::new(home));
    let interrupted = job.cancel_flag();
    signal_hook::flag::register_conditional_shutdown(
        signal_hook::consts::SIGINT,
        130,
        Arc::clone(&interrupted),
    )
    .context("failed to handle Ctrl-C")?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, interrupted)
        .context("failed to handle Ctrl-C")?;
    let (mut printed, mut warned) = (0, 0);
    loop {
        {
            let warnings = job.progress.warnings.lock().expect("warnings lock");
            for warning in &warnings[warned..] {
                eprintln!("warning: {warning}");
            }
            warned = warnings.len();
        }
        {
            let steps = job.progress.steps.lock().expect("steps lock");
            while let Some(step) = steps.get(printed).filter(|step| step.secs.is_some()) {
                eprintln!(
                    "{:<10} {:<18} {:>8}",
                    step.did,
                    step.object,
                    format::duration(step.secs.unwrap_or_default())
                );
                printed += 1;
            }
        }
        if let Some(outcome) = job.take() {
            let built = match outcome {
                Err(error) if job::cancelled(&error) => {
                    eprintln!("Cancelled; the unfinished Pack was removed.");
                    std::process::exit(130);
                }
                outcome => outcome?,
            };
            println!(
                "Pack ready: {} · {} records · {} · built in {}",
                pack.display(),
                format::thousands(built.pack.records),
                format::bytes(built.pack.bytes),
                format::duration(built.build_secs)
            );
            eprint!("{}", built.costs);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn interactive(home: Home) -> Result<()> {
    if !io::stdout().is_terminal() {
        eprintln!("og opens an interactive interface and needs a terminal.");
        eprintln!("Run `og --help` for the commands that work in scripts.");
        return Ok(());
    }
    let inline = terminal::size()?.1 > ui::HEIGHT;
    let viewport = if inline {
        Viewport::Inline(ui::HEIGHT)
    } else {
        Viewport::Fullscreen
    };
    let mut app = App::new(home);
    let mut terminal = ratatui::init_with_options(TerminalOptions { viewport });
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    let result = run(&mut terminal, &mut app, inline);
    let left = leave(&mut terminal, inline);
    for line in &app.scrollback {
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        println!("{text}");
    }
    for line in &app.farewell {
        println!("  {line}");
    }
    if app.job.is_some() || !app.stopping_builds.is_empty() || !app.stopping_runs.is_empty() {
        println!("  Stopping, and removing what was not finished…");
    }
    result.and(left)?;
    if let Some(pack) = app.serve.take() {
        let bind = app::SERVE_ADDRESS.parse().context("the serve address")?;
        drop(app);
        return serve_pack(pack, bind);
    }
    Ok(())
}

fn leave(terminal: &mut DefaultTerminal, inline: bool) -> io::Result<()> {
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    let erased = if inline {
        let top = terminal.get_frame().area().y;
        execute!(
            io::stdout(),
            MoveTo(0, top),
            Clear(ClearType::FromCursorDown),
            Show
        )
    } else {
        Ok(())
    };
    ratatui::restore();
    erased
}

fn run(terminal: &mut DefaultTerminal, app: &mut App, inline: bool) -> io::Result<()> {
    while !app.quit {
        if inline {
            print_scrollback(terminal, app)?;
        }
        if let Some(lines) = app.erase.take() {
            if inline {
                erase_above(terminal, lines)?;
            } else {
                let keep = app.scrollback.len().saturating_sub(lines);
                app.scrollback.truncate(keep);
            }
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(Duration::from_millis(40))? {
            loop {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                    Event::Paste(text) => app.on_paste(&text),
                    _ => {}
                }
                if app.quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        app.width = terminal.size()?.width;
        app.tick();
    }
    if inline {
        print_scrollback(terminal, app)?;
    }
    Ok(())
}

fn erase_above(terminal: &mut DefaultTerminal, lines: usize) -> io::Result<()> {
    let top = terminal.get_frame().area().y;
    let from = top.saturating_sub(u16::try_from(lines).unwrap_or(u16::MAX));
    execute!(
        io::stdout(),
        MoveTo(0, from),
        Clear(ClearType::FromCursorDown)
    )?;
    *terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(ui::HEIGHT),
        },
    )?;
    Ok(())
}

fn print_scrollback(terminal: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    if app.scrollback.is_empty() {
        return Ok(());
    }
    let lines = std::mem::take(&mut app.scrollback);
    terminal.insert_before(lines.len() as u16, |buffer| {
        Paragraph::new(lines).render(buffer.area, buffer);
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{App, Home};
    use crate::test_support::TempDir;

    fn write_pack(dir: &Path) {
        use open_geocode::pack::Writer;
        use open_geocode::record::{
            AddressComponents, AddressRecord, LocationPrecision, ObjectType, PlaceLayer,
            PlaceRecord, Record, SourceProvenance, point_geometry,
        };
        let mut writer = Writer::create(dir).unwrap();
        for (name, id, lon, lat) in [
            ("Andorra la Vella", 1, 1.5218, 42.5063),
            ("Escaldes", 2, 1.5341, 42.5097),
        ] {
            let town = PlaceRecord {
                alt_names: Vec::new(),
                name: name.to_string(),
                place_type: "town".to_string(),
                geometry: point_geometry(lon, lat),
                source: SourceProvenance::osm(ObjectType::Node, id),
                prominence: Default::default(),
                wikidata: None,
            };
            writer
                .write(&Record::Place(PlaceLayer::Locality, town), None)
                .unwrap();
        }
        let house = AddressRecord {
            building: None,
            access: Vec::new(),
            disputed: false,
            street_postcodes: Vec::new(),
            address: AddressComponents {
                block_number_len: None,
                areas: Vec::new(),
                number: "70".to_string(),
                street: Some("Carrer Prat de la Creu".to_string()),
                place: None,
                unit: None,
                locality: Some("Andorra la Vella".to_string()),
                region: None,
                postcode: None,
                country: None,
            },
            geometry: point_geometry(1.5235, 42.5078),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, 3),
        };
        writer.write(&house.into(), None).unwrap();
        writer.finish().unwrap();
    }

    #[test]
    fn the_shell_answers_what_the_api_answers() {
        let dir = TempDir::new("og-shell");
        let pack = dir.join("pack");
        write_pack(&pack);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let bind = listener.local_addr().unwrap();
        let options = open_geocode::runtime::ServeOptions {
            job_dir: Some(dir.join("jobs")),
            ..super::serve_options(pack.clone(), bind)
        };
        let (stopped_tx, stopped) = std::sync::mpsc::channel();
        runtime.spawn(async move {
            let _ = stopped_tx.send(open_geocode::runtime::serve_on(listener, options).await);
        });
        let engine = crate::engine::Engine::open(&pack).unwrap();
        let get = |path: &str| -> serde_json::Value {
            let text = ureq::get(&format!("http://{bind}{path}"))
                .call()
                .unwrap_or_else(|error| {
                    panic!(
                        "GET {path} failed with {error}, and the server ended with {:?}",
                        stopped.recv_timeout(std::time::Duration::from_secs(1))
                    )
                })
                .body_mut()
                .read_to_string()
                .unwrap();
            serde_json::from_str(&text).unwrap()
        };
        let json = |text: String| -> serde_json::Value { serde_json::from_str(&text).unwrap() };
        for query in [
            "andorra la vella",
            "carrer prat de la creu 70",
            "escaldes",
            "x",
        ] {
            let shell = engine.search(query).unwrap();
            let q = query.replace(' ', "%20");
            let mut api = get(&format!("/geocode?q={q}"));
            if api["features"].as_array().unwrap().is_empty() {
                api = get(&format!("/places?q={q}"));
            }
            assert_eq!(json(shell.collection.json(None)), api, "search {query}");
            assert!(!shell.hits.is_empty() || api["features"].as_array().unwrap().is_empty());
            if !shell.hits.is_empty() {
                assert_eq!(
                    json(shell.collection.json(Some(0)))["features"][0],
                    api["features"][0]
                );
            }
        }
        for (lat, lon) in [(42.5063, 1.5218), (42.55, 1.6), (0.0, 0.0)] {
            let shell = engine.reverse(lat, lon).unwrap();
            let api = get(&format!("/reverse?lat={lat}&lon={lon}"));
            assert_eq!(
                json(shell.collection.json(None)),
                api,
                "reverse {lat},{lon}"
            );
        }
        let address = "carrer prat de la creu 70, andorra la vella";
        std::fs::write(dir.join("in.csv"), format!("address\n\"{address}\"\n")).unwrap();
        open_geocode::batch::run_batch_geocode(&open_geocode::batch::GeocodeOptions {
            pack: Some(pack),
            input: dir.join("in.csv"),
            output: dir.join("out.csv"),
            address_field_groups: vec![vec![open_geocode::batch::Column::At(0)]],
            ..Default::default()
        })
        .unwrap();
        let written = std::fs::read_to_string(dir.join("out.csv")).unwrap();
        let hit = engine.geocode_address(address).unwrap().hits.remove(0);
        assert!(
            written.contains(&format!("{:.7},{:.7}", hit.lat, hit.lon)),
            "{written}"
        );
    }

    fn generations(pack: &Path) -> usize {
        std::fs::read_dir(pack.join("generations"))
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    #[test]
    fn quitting_mid_build_cleans_up() {
        let dir = TempDir::new("og-quit");
        let home = Home {
            root: dir.join("home"),
        };
        let region = crate::regions::tree().get("monaco").unwrap();
        let pack = dir.join("pack");
        let mut app = App::new(Home {
            root: home.root.clone(),
        });
        let request = crate::job::BuildRequest {
            inputs: vec![crate::job::BuildInput {
                region,
                extract: Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("fixtures/smoke/monaco.osm.pbf"),
                download: false,
            }],
            pack: pack.clone(),
            memory_budget_bytes: None,
            scratch_budget_bytes: None,
            force: false,
            leftover: open_geocode::builder::Leftover::Refuse,
            openaddresses: false,
            overture: false,
            overture_verified: None,
            tiger: false,
        };
        let progress = crate::job::Progress::of(&request);
        let build_held_at_first_stage = progress.steps.lock().unwrap();
        let mut job = crate::job::BuildJob::spawn(
            request,
            std::sync::Arc::new(home),
            std::sync::Arc::clone(&progress),
        );
        while generations(&pack) == 0 {
            if job.is_finished() {
                let ended = job.take().map(|built| built.map(|_| ()));
                panic!("the build ended before it made a generation: {ended:?}");
            }
            std::thread::yield_now();
        }
        app.job = Some(job);
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
        drop(build_held_at_first_stage);
        drop(app);
        let stopped = open_geocode::builder::resume::find(&pack).unwrap();
        assert!(
            stopped
                .iter()
                .all(open_geocode::builder::resume::Found::resumable)
                && generations(&pack) == stopped.len(),
            "an unfinished generation was left in {}",
            pack.display()
        );
    }
}
