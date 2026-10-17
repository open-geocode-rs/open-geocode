//! `og`: the interactive interface to open-geocode, and the commands it
//! names for scripts. Run alone it opens the interface; `og build` and
//! `og serve` do the same work without it.
//!
//! The interface draws in an inline viewport below the prompt, not the full
//! screen: finished steps are printed above it and stay in terminal history.

mod app;
mod ask;
mod batch;
mod engine;
mod format;
mod home;
mod job;
mod net;
mod regions;
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
    resources,
    runtime::{ServeOptions, serve},
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
use job::{BuildJob, BuildRequest};

/// mimalloc, counted, as the `open-geocode` binary has it: builds run on
/// it, and the count lets a build's memory pool see the heap it does not
/// track.
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
        /// The region's catalog id, for example `canada/ontario`.
        #[arg(long)]
        region: String,

        /// Memory for the build's buffers, in MiB. Sized to what this
        /// machine can give when not set.
        #[arg(long)]
        memory_budget_mb: Option<u64>,

        /// Build even when the pre-flight check finds too little memory or
        /// disk.
        #[arg(long)]
        force: bool,

        /// Download the extract again even when one is on disk.
        #[arg(long)]
        download: bool,
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
    // moved to efficiency cores.
    resources::opt_out_of_power_throttling();
    match cli.command {
        Some(Command::Build {
            region,
            memory_budget_mb,
            force,
            download,
        }) => {
            let memory_budget_bytes = memory_budget_mb
                .map(|mb| usize::try_from(mb << 20))
                .transpose()
                .context("--memory-budget-mb is larger than this machine can address")?;
            build(home, &region, memory_budget_bytes, force, download)
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
        .block_on(serve(ServeOptions {
            pack,
            demo: None,
            bind,
            basemap: None,
        }))
        .map_err(Into::into)
}

/// `og build`: the build the interface runs, with one line per step. Ctrl-C
/// stops it and removes the unfinished Pack generation; a second one quits
/// at once.
fn build(
    home: Home,
    id: &str,
    memory_budget_bytes: Option<usize>,
    force: bool,
    download: bool,
) -> Result<()> {
    let Some(region) = regions::tree().get(id) else {
        bail!("no region {id} in the catalog; ids look like canada/ontario");
    };
    let extract = home.extract_path(region);
    let request = BuildRequest {
        region,
        download: download || !extract.is_file(),
        extract,
        pack: home.pack_dir(region),
        memory_budget_bytes,
        force,
    };
    let pack = request.pack.clone();
    let mut job = BuildJob::start(request, Arc::new(home));
    let interrupted = job.cancel_flag();
    // The second handler quits only once the first has set the flag.
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
    // One window of fixed height, drawn below the prompt so finished steps
    // stay in terminal history. A terminal too short for it gets the whole
    // screen instead, as atuin does.
    let inline = terminal::size()?.1 > ui::HEIGHT;
    let viewport = if inline {
        Viewport::Inline(ui::HEIGHT)
    } else {
        Viewport::Fullscreen
    };
    let mut app = App::new(home);
    let mut terminal = ratatui::init_with_options(TerminalOptions { viewport });
    // A terminal that supports it marks pasted text, so a pasted CSV arrives
    // in one piece. The others send it as keys.
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    let result = run(&mut terminal, &mut app, inline);
    let left = leave(&mut terminal, inline);
    // Without scrollback, what the window would have pushed up is printed
    // as plain lines once the screen is back.
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
        // The interface and its Pack are closed before the server opens it.
        drop(app);
        return serve_pack(pack, bind);
    }
    Ok(())
}

/// Gives the terminal back. The window is erased from its first row down, so
/// the shell prompt returns where the window began and nothing is left to
/// clear by hand.
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
                // Not printed yet: take them back.
                let keep = app.scrollback.len().saturating_sub(lines);
                app.scrollback.truncate(keep);
            }
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(Duration::from_millis(40))? {
            // Take everything waiting before drawing again: a paste sent as
            // keys is thousands of events.
            loop {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                    Event::Paste(text) => app.on_paste(text),
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

/// Erases the lines just above the window and opens the window again where
/// they began, so a finished flow leaves nothing stacked above the next
/// screen.
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

/// Prints the lines waiting for terminal history above the window.
fn print_scrollback(terminal: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    if app.scrollback.is_empty() {
        return Ok(());
    }
    let lines = std::mem::take(&mut app.scrollback);
    terminal.insert_before(lines.len() as u16, |buffer| {
        Paragraph::new(lines).render(buffer.area, buffer);
    })
}

/// Checks the interface against a Pack in `OG_HOME`. Both tests are
/// `#[ignore]`d: they need that Pack and bind a port.
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{App, Home};

    /// The query shell's GeoJSON is the HTTP API's answer, value for value.
    #[test]
    #[ignore = "needs the Andorra Pack in OG_HOME"]
    fn the_shell_answers_what_the_api_answers() {
        let home = Home::locate().unwrap();
        let pack = home.packs_dir().join("andorra");
        let bind: std::net::SocketAddr = "127.0.0.1:8098".parse().unwrap();
        let served = pack.clone();
        std::thread::spawn(move || super::serve_pack(served, bind));
        std::thread::sleep(Duration::from_secs(2));
        let engine = crate::engine::Engine::open(&pack).unwrap();
        let get = |path: &str| -> serde_json::Value {
            let text = ureq::get(&format!("http://{bind}{path}"))
                .call()
                .unwrap()
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
            let api = get(&format!("/search?q={}", query.replace(' ', "%20")));
            assert_eq!(json(shell.collection.json(None)), api, "search {query}");
            assert!(!shell.hits.is_empty() || api["features"].as_array().unwrap().is_empty());
            // One feature picked from the list is that feature of the answer.
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
        // A pasted row is answered as a row of a file is.
        let dir = std::env::temp_dir().join(format!("og-pasted-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let address = "carrer prat de la creu 70, andorra la vella";
        std::fs::write(dir.join("in.csv"), format!("address\n\"{address}\"\n")).unwrap();
        open_geocode::batch::run_batch_geocode(open_geocode::batch::GeocodeOptions {
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Quitting during a build leaves no unfinished Pack generation behind.
    #[test]
    #[ignore = "needs the Andorra extract in OG_HOME"]
    fn quitting_mid_build_cleans_up() {
        let home = Home::locate().unwrap();
        let region = crate::regions::tree().get("andorra").unwrap();
        let pack = home.packs_dir().join("andorra-stopped");
        let mut app = App::new(Home::locate().unwrap());
        app.job = Some(crate::job::BuildJob::start(
            crate::job::BuildRequest {
                region,
                extract: home.extract_path(region),
                download: false,
                pack: pack.clone(),
                memory_budget_bytes: None,
                force: false,
            },
            std::sync::Arc::new(home),
        ));
        std::thread::sleep(Duration::from_millis(300));
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
        drop(app);
        let left = std::fs::read_dir(pack.join("generations"))
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(
            left,
            0,
            "an unfinished generation was left in {}",
            pack.display()
        );
        let _ = std::fs::remove_dir_all(&pack);
    }
}
