//! A look-only preview of the interactive `og` interface. Regions, build
//! stages and query results are canned (`mock`), so the screens can be judged
//! before they are wired to the builder and the searcher.
//!
//! The interface draws in an inline viewport below the prompt, not the full
//! screen: finished steps are printed above it and stay in terminal history.

mod app;
mod mock;
mod regions;
mod ui;

use std::{
    io::{self, IsTerminal},
    time::Duration,
};

use ratatui::{
    DefaultTerminal, TerminalOptions, Viewport,
    crossterm::{
        event::{self, Event, KeyEventKind},
        terminal,
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
    let result = run(&mut terminal, &mut app, inline);
    let cleared = terminal.clear();
    ratatui::restore();
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
    result.and(cleared)
}

fn run(terminal: &mut DefaultTerminal, app: &mut App, inline: bool) -> io::Result<()> {
    while !app.quit {
        if inline {
            print_scrollback(terminal, app)?;
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(Duration::from_millis(40))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.on_key(key);
        }
        app.tick();
    }
    if inline {
        print_scrollback(terminal, app)?;
    }
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
