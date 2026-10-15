//! A look-only preview of the interactive `og` interface. Regions, build
//! stages and query results are canned (`mock`), so the screens can be judged
//! before they are wired to the builder and the searcher.
//!
//! The interface draws in an inline viewport below the prompt, not the full
//! screen: finished steps are printed above it and stay in terminal history.

mod app;
mod mock;
mod ui;

use std::{io, time::Duration};

use ratatui::{
    DefaultTerminal, Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    crossterm::event::{self, Event, KeyEventKind},
    widgets::{Paragraph, Widget},
};

use app::App;

fn main() -> io::Result<()> {
    let mut app = App::new();
    let mut height = ui::height(&app);
    let mut terminal = ratatui::init_with_options(TerminalOptions {
        viewport: Viewport::Inline(height),
    });
    let result = run(&mut terminal, &mut app, &mut height);
    let flushed = print_scrollback(&mut terminal, &mut app);
    let cleared = terminal.clear();
    ratatui::restore();
    for line in &app.farewell {
        println!("  {line}");
    }
    result.and(flushed).and(cleared)
}

fn run(terminal: &mut DefaultTerminal, app: &mut App, height: &mut u16) -> io::Result<()> {
    while !app.quit {
        print_scrollback(terminal, app)?;
        let wanted = ui::height(app);
        if wanted != *height {
            // An inline viewport has a fixed height: clear it and open a new
            // one at the same place.
            terminal.clear()?;
            *terminal = Terminal::with_options(
                CrosstermBackend::new(io::stdout()),
                TerminalOptions {
                    viewport: Viewport::Inline(wanted),
                },
            )?;
            *height = wanted;
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
    Ok(())
}

/// Prints the lines waiting for terminal history above the viewport.
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
        let mut terminal = Terminal::new(TestBackend::new(84, ui::height(app))).unwrap();
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
    fn every_screen_fits() {
        let mut app = App::new();
        show(&app, "home");
        press(&mut app, &[KeyCode::Enter]);
        show(&app, "region");
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
