//! A look-only preview of the interactive `og` interface. Regions, build
//! stages and query results are canned (`mock`), so the screens can be judged
//! before they are wired to the builder and the searcher.
//!
//! The interface draws in an inline viewport below the prompt, not the full
//! screen: finished steps are printed above it and stay in terminal history.

mod app;
mod batch;
mod mock;
mod regions;
mod ui;

use std::{
    io::{self, IsTerminal},
    time::Duration,
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

fn main() -> io::Result<()> {
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
    // `og --empty` previews the screens for a machine with no Packs.
    let empty = std::env::args().any(|arg| arg == "--empty");
    let mut app = App::new(empty);
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
    result.and(left)
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

#[cfg(test)]
mod preview {
    use ratatui::{
        Terminal,
        backend::TestBackend,
        crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    };

    use super::{App, ui};
    use crate::app::Screen;

    fn press(app: &mut App, keys: &[KeyCode]) {
        for key in keys {
            app.on_key(KeyEvent::new(*key, KeyModifiers::NONE));
        }
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, &[KeyCode::Char(c)]);
        }
    }

    fn show(app: &App, name: &str) {
        let mut terminal = Terminal::new(TestBackend::new(84, ui::HEIGHT)).unwrap();
        terminal.draw(|frame| ui::draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        println!("=== {name}");
        for y in 0..buffer.area.height {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            println!("{}", row.trim_end());
        }
    }

    #[test]
    fn batch_mode() {
        let mut app = App::new(false);
        press(&mut app, &[KeyCode::Char('g')]);
        show(&app, "batch empty");
        app.on_paste(
            "address,id\n100 queen st w toronto,1\n24 sussex dr ottawa,2\n1 yonge st toronto,3\nnowhere road,4\n"
                .to_string(),
        );
        show(&app, "batch pasted");
        app.on_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        show(&app, "batch geojson");
    }

    fn text(line: &ratatui::text::Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn build_lines() {
        let mut app = App::new(false);
        press(&mut app, &[KeyCode::Char('b')]);
        press(&mut app, &[KeyCode::Down, KeyCode::Enter]);
        app.screen = Screen::Confirm;
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.screen == Screen::Build);
        let build = app.build.as_mut().expect("a build is running");
        build.current = 5;
        let build = app.build.as_ref().expect("a build is running");
        println!("=== build history");
        for line in &app.scrollback {
            println!("{}", text(line));
        }
        for index in 0..5 {
            for line in ui::stage_done(build, index, 82) {
                println!("{}", text(&line));
            }
        }
        show(&app, "build running");
        let build = app.build.as_mut().expect("a build is running");
        build.current = 8;
        show(&app, "build no total");
    }

    #[test]
    fn batch_flow() {
        use std::time::{Duration, Instant};

        let mut app = App::new(false);
        press(&mut app, &[KeyCode::Char('g')]);
        app.on_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert!(app.batch_candidate.is_some());
        show(&app, "batch file card");
        press(&mut app, &[KeyCode::Right]);
        show(&app, "batch file card, cancel chosen");
        press(&mut app, &[KeyCode::Left, KeyCode::Enter]);
        assert!(app.screen == Screen::BatchSetup);
        show(&app, "batch setup");
        press(&mut app, &[KeyCode::Right, KeyCode::Right]);
        press(&mut app, &[KeyCode::Down]);
        show(&app, "batch setup, region role changed");
        press(&mut app, &[KeyCode::Up]);
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.screen == Screen::BatchRun);
        if let Some(build) = app.build.as_mut() {
            build.current = 1;
            build.stage_started = Instant::now() - Duration::from_secs(4);
        }
        show(&app, "batch running");
        for _ in 0..4 {
            if let Some(build) = app.build.as_mut() {
                build.stage_started = Instant::now() - Duration::from_secs(60);
            }
            app.tick();
        }
        assert!(app.screen == Screen::BatchDone);
        println!("=== batch history");
        for line in &app.scrollback {
            println!("{}", text(line));
        }
        show(&app, "batch done");
    }

    #[test]
    fn batch_file_path() {
        let path = std::env::temp_dir().join("og-preview-batch.csv");
        std::fs::write(
            &path,
            "Name,Street Address,Town,State
A,100 queen st w,Toronto,ON

B,24 sussex dr,Ottawa,ON
",
        )
        .expect("the temp file is written");
        let mut app = App::new(false);
        press(&mut app, &[KeyCode::Char('g')]);
        app.on_paste(path.display().to_string());
        assert!(app.batch_candidate.is_some());
        press(&mut app, &[KeyCode::Enter]);
        std::fs::remove_file(&path).ok();
        assert!(app.screen == Screen::BatchSetup);
        let session = app.batch.as_ref().expect("a file is open");
        assert_eq!(session.file.total_rows, 2);
        assert!(session.file.header_found);
        assert_eq!(session.query(0), "100 queen st w, Toronto, ON");
        show(&app, "batch setup from a file");
    }

    #[test]
    fn leaving_a_flow_erases_its_history() {
        use std::time::{Duration, Instant};

        let mut app = App::new(false);
        press(&mut app, &[KeyCode::Char('b')]);
        press(&mut app, &[KeyCode::Down, KeyCode::Enter]);
        app.screen = Screen::Confirm;
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.screen == Screen::Build);
        assert!(app.flow_lines > 0);
        for _ in 0..12 {
            if let Some(build) = app.build.as_mut() {
                build.stage_started = Instant::now() - Duration::from_secs(60);
            }
            app.tick();
        }
        assert!(app.screen == Screen::Done);
        let printed = app.flow_lines;
        assert!(printed > 6, "the stages are in history");
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(app.erase, Some(printed));
        assert_eq!(app.flow_lines, 0);
    }

    #[test]
    fn home_without_packs() {
        let app = App::new(true);
        show(&app, "home without packs");
    }

    #[test]
    fn every_screen_fits() {
        let mut app = App::new(false);
        show(&app, "home");
        press(&mut app, &[KeyCode::Down, KeyCode::Down, KeyCode::Down]);
        show(&app, "home build row");
        press(&mut app, &[KeyCode::Enter]);
        show(&app, "region");
        press(&mut app, &[KeyCode::Down; 7]);
        press(&mut app, &[KeyCode::Enter]);
        show(&app, "region north america");
        press(&mut app, &[KeyCode::Down, KeyCode::Enter]);
        show(&app, "region canada");
        press(&mut app, &[KeyCode::PageDown]);
        show(&app, "region canada scrolled");
        press(&mut app, &[KeyCode::Left, KeyCode::Left]);
        typed(&mut app, "onta");
        show(&app, "region onta");
        press(&mut app, &[KeyCode::Enter, KeyCode::Char(' ')]);
        show(&app, "sources");
        press(&mut app, &[KeyCode::Enter]);
        show(&app, "confirm");
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.screen == Screen::Build);
        show(&app, "build");
        if let Some(build) = &mut app.build {
            build.current = 3;
        }
        app.show_logs = true;
        show(&app, "build join with logs");
        app.build = None;
        app.screen = Screen::Done;
        app.built_secs = 588.0;
        show(&app, "done");
        press(&mut app, &[KeyCode::Enter]);
        typed(&mut app, "100 queen st w toronto");
        show(&app, "query");
        press(&mut app, &[KeyCode::Enter]);
        show(&app, "json");
        press(&mut app, &[KeyCode::Esc]);
        app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        typed(&mut app, "43.6535, -79.3839");
        show(&app, "reverse");
    }
}
