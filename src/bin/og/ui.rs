//! Drawing. Each screen is a short stack of lines inside the inline viewport;
//! finished steps leave the viewport as plain lines in terminal history.

use std::borrow::Cow;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph},
};

use crate::{
    app::{self, App, Build, Mode, RegionRow, Screen},
    batch::{self, BatchSession, Role},
    mock::{self, Record, Stage},
    regions,
};

const ACCENT: Color = Color::Rgb(92, 184, 255);
const DIM: Color = Color::Rgb(132, 132, 148);
const FAINT: Color = Color::Rgb(76, 76, 92);
const OK: Color = Color::Rgb(108, 212, 140);
const WARN: Color = Color::Rgb(236, 190, 96);
const ERR: Color = Color::Rgb(238, 106, 106);
const PINK: Color = Color::Rgb(226, 136, 214);
const VIOLET: Color = Color::Rgb(160, 140, 255);

/// Gradient ends of the stage bar and the overall bar.
const BAR: [(u8, u8, u8); 2] = [(70, 130, 255), (92, 220, 255)];
/// Full bars of finished stages, muted so the running one stands out.
const DONE_BAR: Color = Color::Rgb(74, 140, 104);
/// The shimmer on the running stage's title sweeps from the first colour to
/// the second.
const SHIMMER: [(u8, u8, u8); 2] = [(112, 150, 205), (240, 248, 255)];

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Rows of JSON the GeoJSON box shows at once.
pub const JSON_ROWS: usize = 14;

fn span(text: impl Into<Cow<'static, str>>, color: Color) -> Span<'static> {
    Span::styled(text, Style::new().fg(color))
}

fn plain(text: impl Into<Cow<'static, str>>) -> Span<'static> {
    Span::raw(text)
}

fn bold(text: impl Into<Cow<'static, str>>) -> Span<'static> {
    Span::styled(text, Style::new().add_modifier(Modifier::BOLD))
}

fn strong(text: impl Into<Cow<'static, str>>, color: Color) -> Span<'static> {
    Span::styled(text, Style::new().fg(color).add_modifier(Modifier::BOLD))
}

fn panel(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(FAINT))
        .title(Line::from(vec![
            plain(" "),
            strong(title.to_string(), ACCENT),
            plain(" "),
        ]))
}

/// Text width in terminal cells; everything drawn here is one cell a char.
fn width(text: &str) -> usize {
    text.chars().count()
}

fn pad(text: &str, cells: usize) -> String {
    format!("{text}{}", " ".repeat(cells.saturating_sub(width(text))))
}

fn spinner(app: &App) -> &'static str {
    SPINNER[(app.started.elapsed().as_millis() / 80) as usize % SPINNER.len()]
}

/// The header printed once on start. It stays in terminal history.
pub fn logo() -> Vec<Line<'static>> {
    let o = Style::new().fg(DIM);
    let g = Style::new().add_modifier(Modifier::BOLD);
    vec![
        Line::raw(""),
        Line::from(vec![
            plain("  "),
            Span::styled("█▀▀█ ", o),
            Span::styled("█▀▀▀", g),
        ]),
        Line::from(vec![
            plain("  "),
            Span::styled("█  █ ", o),
            Span::styled("█ ▀█", g),
            plain("   "),
            bold("open-geocode"),
            span(" 0.1.0", DIM),
        ]),
        Line::from(vec![
            plain("  "),
            Span::styled("▀▀▀▀ ", o),
            Span::styled("▀▀▀▀", g),
            plain("   "),
            span(
                "Fast, lightweight, self-hosted geocoding in pure Rust.",
                DIM,
            ),
        ]),
        Line::raw(""),
        Line::from(vec![plain("  Build a Pack from open data.")]),
        Line::from(vec![plain(
            "  Forward, reverse, autocomplete and batch geocoding, all on your machine.",
        )]),
        Line::raw(""),
    ]
}

fn choice(label: &str, value: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![plain("   "), span(pad(label, 10), DIM)];
    spans.extend(value);
    Line::from(spans)
}

/// The wizard's answers so far, one line each.
fn answers(app: &App, count: usize) -> Vec<Line<'static>> {
    let mut region = vec![
        plain(app.region.label.clone()),
        span(format!("  {}", mock::size(app.region.size_mb())), DIM),
    ];
    if app.region.local().is_some() {
        region.push(span("  on disk", OK));
    }
    let sources: Vec<&str> = mock::SOURCES
        .iter()
        .zip(app.sources)
        .filter(|(_, on)| *on)
        .map(|(source, _)| source.name)
        .collect();
    let lines = vec![
        choice("Region", region),
        choice("Sources", vec![plain(sources.join(" + "))]),
        choice(
            "Output",
            vec![
                plain(app.pack_path()),
                span(format!("  memory {} GB", app.memory_gb()), DIM),
            ],
        ),
    ];
    lines.into_iter().take(count).collect()
}

/// What a started build leaves in terminal history.
pub fn choices(app: &App) -> Vec<Line<'static>> {
    // History lines start at the terminal's edge; the live window is one
    // column in.
    answers(app, 3)
        .into_iter()
        .map(|line| {
            let mut spans = vec![plain(" ")];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// Width of the content: the terminal minus its margins, at most 96.
pub fn content_width(terminal: u16) -> usize {
    terminal.saturating_sub(2).min(96) as usize
}

/// The line that opens a build in terminal history.
pub fn build_header(app: &App) -> Vec<Line<'static>> {
    vec![
        Line::raw(""),
        Line::from(vec![
            plain("    "),
            bold(format!("Building {}", app.region.label)),
        ]),
        Line::raw(""),
    ]
}

/// A finished stage as it goes into terminal history, with a blank line
/// after it so the stages do not run together.
pub fn stage_done(build: &Build, index: usize, inner: usize) -> Vec<Line<'static>> {
    vec![finished_line(&build.stages[index], inner), Line::raw("")]
}

pub fn done_summary(app: &App) -> Vec<Line<'static>> {
    vec![
        Line::raw(""),
        Line::from(vec![
            plain("    "),
            bold("Pack ready"),
            span(
                format!(
                    "  {} · {} · built in {}",
                    app.pack_path(),
                    mock::PACK.size,
                    mock::duration(app.built_secs)
                ),
                DIM,
            ),
        ]),
        Line::from(vec![
            span("    Build it again  ", DIM),
            span(app.command(), ACCENT),
        ]),
        Line::raw(""),
    ]
}

/// Rows of the window every screen is drawn into. The window never changes
/// size: a short screen leaves blank rows, and the key hints stay on the last
/// row.
pub const HEIGHT: u16 = 20;

pub fn draw(frame: &mut Frame, app: &App) {
    let full = frame.area();
    // The last two rows belong to the rule and the key hints.
    let area = Rect {
        x: full.x + 1,
        width: full.width.saturating_sub(2).min(96),
        height: full.height.saturating_sub(2),
        ..full
    };
    match app.screen {
        Screen::Home => home(frame, area, app),
        Screen::Region => region(frame, area, app),
        Screen::Sources => sources(frame, area, app),
        Screen::Confirm => confirm(frame, area, app),
        Screen::Build => build(frame, area, app),
        Screen::Done => done(frame, area, app),
        Screen::Query => query(frame, area, app),
        Screen::Json => json(frame, area, app),
        Screen::BatchSetup => batch_setup(frame, area, app),
        Screen::BatchRun => batch_run(frame, area, app),
        Screen::BatchDone => batch_done(frame, area, app),
    }
}

fn footer(frame: &mut Frame, app: &App, hints: &[(&str, &str)]) {
    let line = match app.flash_text() {
        Some(text) => Line::from(vec![span(" ! ", WARN), span(text.to_string(), WARN)]),
        None => {
            let mut spans = vec![plain(" ")];
            for (index, (key, action)) in hints.iter().enumerate() {
                if index > 0 {
                    spans.push(span(" · ", FAINT));
                }
                spans.push(plain(key.to_string()));
                spans.push(span(format!(" {action}"), DIM));
            }
            Line::from(spans)
        }
    };
    let full = frame.area();
    let hints = Rect {
        x: full.x + 1,
        y: full.y + full.height.saturating_sub(1),
        width: full.width.saturating_sub(2).min(96),
        height: 1.min(full.height),
    };
    let rule = Rect {
        y: hints.y.saturating_sub(1),
        ..hints
    };
    frame.render_widget(
        Paragraph::new(span("─".repeat(hints.width as usize), FAINT)),
        rule,
    );
    frame.render_widget(Paragraph::new(line), hints);
}

fn home(frame: &mut Frame, area: Rect, app: &App) {
    let on_build = app.home_cursor == app.packs.len();
    if app.packs.is_empty() {
        let lines = vec![
            Line::from(bold("   Your Packs")),
            Line::raw(""),
            Line::from(plain("   No Packs yet.")),
            Line::from(span(
                "   A Pack is the data file og searches, built from open data.",
                DIM,
            )),
            Line::raw(""),
            Line::from(vec![span(" ❯ ", ACCENT), bold("Build a Pack")]),
        ];
        frame.render_widget(Paragraph::new(lines), area);
        footer(frame, app, &[("enter", "build a Pack"), ("q", "quit")]);
        return;
    }
    let name_width = app
        .packs
        .iter()
        .map(|pack| width(pack.name))
        .max()
        .unwrap_or(0)
        .clamp(10, 20);
    let [left, _, right] = Layout::horizontal([
        Constraint::Length(name_width as u16 + 23),
        Constraint::Length(4),
        Constraint::Min(0),
    ])
    .areas(area);

    let mut list = vec![Line::from(bold("   Your Packs")), Line::raw("")];
    for (index, pack) in app.packs.iter().enumerate() {
        list.push(pack_row(pack, index == app.home_cursor, name_width));
    }
    list.push(Line::raw(""));
    list.push(Line::from(vec![
        if on_build {
            span(" ❯ ", ACCENT)
        } else {
            plain("   ")
        },
        span("+ ", ACCENT),
        if on_build {
            bold("Build a Pack")
        } else {
            plain("Build a Pack")
        },
    ]));
    frame.render_widget(Paragraph::new(list), left);

    let detail = if on_build {
        vec![
            Line::from(bold("Build a Pack")),
            Line::raw(""),
            Line::from(plain("Pick a region and extra sources.")),
            Line::from(plain("og downloads the open data and")),
            Line::from(plain("builds the Pack on this machine.")),
        ]
    } else {
        pack_detail(&app.packs[app.home_cursor])
    };
    frame.render_widget(Paragraph::new(detail), right);

    let hints: Vec<(&str, &str)> = if on_build {
        vec![("↑↓", "move"), ("enter", "build a Pack"), ("q", "quit")]
    } else {
        let mut hints = vec![
            ("↑↓", "move"),
            ("enter", "search"),
            ("v", "serve"),
            ("g", "batch geocode"),
        ];
        if app.packs[app.home_cursor].update.is_some() {
            hints.push(("u", "update"));
        }
        hints.push(("b", "build"));
        hints.push(("q", "quit"));
        hints
    };
    footer(frame, app, &hints);
}

fn pack_row(pack: &mock::SamplePack, selected: bool, name_width: usize) -> Line<'static> {
    let shown = if width(pack.name) > name_width {
        let keep: String = pack.name.chars().take(name_width - 1).collect();
        format!("{keep}…")
    } else {
        pack.name.to_string()
    };
    let name = if selected {
        bold(pad(&shown, name_width))
    } else {
        plain(pad(&shown, name_width))
    };
    Line::from(vec![
        if selected {
            span(" ❯ ", ACCENT)
        } else {
            plain("   ")
        },
        name,
        span(format!("{:>8}", pack.size), DIM),
        span(format!("  {:>7}", pack.built_ago), DIM),
        if pack.update.is_some() {
            span("  ↑", ACCENT)
        } else {
            plain("   ")
        },
    ])
}

fn detail_line(label: &str, value: String) -> Line<'static> {
    Line::from(vec![span(pad(label, 10), DIM), plain(value)])
}

fn pack_detail(pack: &mock::SamplePack) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(bold(pack.name)),
        Line::raw(""),
        detail_line("Region", pack.region.to_string()),
        detail_line("Sources", pack.sources.to_string()),
        detail_line("Records", pack.records.to_string()),
        detail_line("Built", format!("{} · {}", pack.built_long, pack.built_on)),
        detail_line("Path", pack.path.to_string()),
        Line::raw(""),
    ];
    if let Some(date) = pack.update {
        lines.push(Line::from(span("↑ Update available", ACCENT)));
        lines.push(Line::from(span(format!("  Newer data from {date}"), DIM)));
    }
    lines
}

fn input_line(text: &str, placeholder: &str) -> Line<'static> {
    let mut spans = vec![span(" ❯ ", ACCENT), plain(text.to_string())];
    spans.push(span("▌", ACCENT));
    if text.is_empty() {
        spans.push(span(format!(" {placeholder}"), FAINT));
    }
    Line::from(spans)
}

fn region(frame: &mut Frame, area: Rect, app: &App) {
    let tree = regions::tree();
    let rows = app.region_rows();
    let browsing = app.region_browsing();
    let inner = area.width.saturating_sub(2) as usize;

    // The title is the way down the tree: the levels above dim, this one bold.
    let mut names = vec!["Region".to_string()];
    if browsing {
        names.extend(
            app.region_path
                .iter()
                .map(|index| tree.at(*index).name.clone()),
        );
    } else {
        names.push("search".to_string());
    }
    let mut title = vec![plain("   ")];
    for (index, name) in names.iter().enumerate() {
        if index > 0 {
            title.push(span(" › ", FAINT));
        }
        title.push(if index + 1 == names.len() {
            bold(name.clone())
        } else {
            span(name.clone(), DIM)
        });
    }
    let mut search = vec![
        plain("   "),
        plain(app.region_query.clone()),
        span("▌", ACCENT),
    ];
    if app.region_query.is_empty() {
        search.push(span(
            format!(" type to search {} regions", tree.searchable().count()),
            FAINT,
        ));
    }
    let mut lines = vec![
        Line::from(title),
        Line::raw(""),
        Line::from(search),
        Line::raw(""),
    ];
    if rows.is_empty() {
        lines.push(Line::from(span(
            format!("   No region matches \"{}\".", app.region_query.trim()),
            DIM,
        )));
    }
    let mut room = app::REGION_ROWS;
    let mut next = app.region_scroll;
    while room > 0 && next < rows.len() {
        let all = browsing && next == 0;
        lines.push(region_row(
            &rows[next],
            next == app.region_cursor,
            all,
            inner,
        ));
        room -= 1;
        // A line under the region itself sets it apart from the regions below.
        if all && rows.len() > 1 && room > 0 {
            lines.push(Line::from(span(
                format!("   {}", "─".repeat(inner.saturating_sub(6))),
                FAINT,
            )));
            room -= 1;
        }
        next += 1;
    }
    while lines.len() < 4 + app::REGION_ROWS {
        lines.push(Line::raw(""));
    }
    // Under the list: how many rows are out of sight, and where the cursor is.
    let more = rows.len().saturating_sub(next);
    let left = if more > 0 {
        format!("   ↓ {more} more")
    } else {
        String::new()
    };
    let right = if rows.is_empty() {
        String::new()
    } else {
        format!("{} of {}", app.region_cursor + 1, rows.len())
    };
    lines.push(Line::from(vec![
        span(pad(&left, inner.saturating_sub(width(&right))), DIM),
        span(right, DIM),
    ]));
    frame.render_widget(Paragraph::new(lines), area);
    let hints: &[(&str, &str)] = if browsing {
        &[
            ("↑↓", "move"),
            ("enter", "open or select"),
            ("←→", "up or open"),
            ("esc", "back"),
        ]
    } else {
        &[
            ("↑↓", "move"),
            ("enter", "open or select"),
            ("esc", "clear search"),
        ]
    };
    footer(frame, app, hints);
}

/// One line of the region list: the name, what is around it, and what the
/// build would cost.
fn region_row(row: &RegionRow, selected: bool, all: bool, inner: usize) -> Line<'static> {
    let region = row.region;
    let base = if selected {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let hit = Style::new().fg(ACCENT).add_modifier(Modifier::BOLD);
    let name = match (all, region.is_world()) {
        (true, true) => region.name.clone(),
        (true, false) => region.label.clone(),
        (false, _) => region.name.clone(),
    };
    let mut spans = vec![if selected {
        span(" ❯ ", ACCENT)
    } else {
        plain("   ")
    }];
    for (index, c) in name.chars().enumerate() {
        let style = if row.positions.contains(&index) {
            hit
        } else {
            base
        };
        spans.push(Span::styled(c.to_string(), style));
    }
    let right = format!(
        "{:>8}  {:>9} ",
        mock::size(region.size_mb()),
        mock::about(region.build_secs())
    );
    let opens = if all || region.children.is_empty() {
        String::new()
    } else {
        format!("  {} {} ›", region.children.len(), region.unit())
    };
    let disk = if region.local().is_some() {
        "  ● on disk"
    } else {
        ""
    };
    let used = 3 + width(&name) + width(&opens) + width(disk) + width(&right);
    let mut room = inner.saturating_sub(used);
    if let Some(trail) = row.trail.as_deref().filter(|trail| !trail.is_empty())
        && room >= 6
    {
        let fitted: String = if width(trail) + 2 <= room {
            trail.to_string()
        } else {
            let keep: String = trail.chars().take(room - 3).collect();
            format!("{keep}…")
        };
        room -= width(&fitted) + 2;
        spans.push(span(format!("  {fitted}"), DIM));
    }
    spans.push(span(opens, DIM));
    spans.push(span(disk, OK));
    spans.push(plain(" ".repeat(room)));
    spans.push(span(right, DIM));
    Line::from(spans)
}

fn sources(frame: &mut Frame, area: Rect, app: &App) {
    let inner = area.width.saturating_sub(2) as usize;
    let mut lines = answers(app, 1);
    lines.push(Line::raw(""));
    lines.push(Line::from(bold("   Sources")));
    lines.push(Line::raw(""));
    lines.extend(mock::SOURCES.iter().enumerate().map(|(index, source)| {
        let selected = index == app.source_cursor;
        let on = app.sources[index];
        let mark = match (index, on) {
            (0, _) => span("[✓] ", DIM),
            (_, true) => span("[✓] ", ACCENT),
            (_, false) => span("[ ] ", DIM),
        };
        let name = if selected {
            bold(pad(source.name, 18))
        } else {
            plain(pad(source.name, 18))
        };
        let right = if index == 0 {
            "required ".to_string()
        } else {
            format!("+{} ", mock::size(source.download_mb))
        };
        let used = 3 + 4 + 18 + width(source.detail);
        Line::from(vec![
            if selected {
                span(" ❯ ", ACCENT)
            } else {
                plain("   ")
            },
            mark,
            name,
            span(source.detail, DIM),
            plain(" ".repeat(inner.saturating_sub(used + width(&right)))),
            span(right, FAINT),
        ])
    }));
    frame.render_widget(Paragraph::new(lines), area);
    footer(
        frame,
        app,
        &[
            ("↑↓", "move"),
            ("space", "toggle"),
            ("enter", "continue"),
            ("esc", "back"),
        ],
    );
}

fn field(label: &str, value: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![plain("   "), span(pad(label, 13), DIM)];
    spans.extend(value);
    Line::from(spans)
}

fn button(label: &str, selected: bool, enabled: bool) -> Span<'static> {
    let text = format!(" {label} ");
    match (selected, enabled) {
        (true, true) => Span::styled(
            text,
            Style::new()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        (true, false) => Span::styled(text, Style::new().fg(Color::Black).bg(FAINT)),
        (false, true) => span(text, DIM),
        (false, false) => span(text, FAINT),
    }
}

fn confirm(frame: &mut Frame, area: Rect, app: &App) {
    let download = match (app.download_mb(), app.region.local()) {
        (0.0, Some(path)) => vec![plain("nothing"), span(format!("  using {path}"), DIM)],
        (mb, _) => vec![
            plain(mock::size(mb)),
            span("  from download.geofabrik.de", DIM),
        ],
    };
    let mut disk = vec![
        plain(format!("~ {:.1} GB", app.disk_gb())),
        span(format!("   {:.0} GB free", mock::FREE_DISK_GB), DIM),
    ];
    if !app.fits_disk() {
        disk.push(span("   not enough", ERR));
    }
    let mut lines = answers(app, 2);
    lines.extend([
        Line::raw(""),
        Line::from(bold("   Ready to build")),
        Line::raw(""),
        field("Output", vec![plain(app.pack_path())]),
        field(
            "Memory",
            vec![
                plain(format!("{} GB", app.memory_gb())),
                span(format!(" of {} GB RAM", mock::RAM_GB), DIM),
                span("      m change", FAINT),
            ],
        ),
        field(
            "Scratch",
            vec![span(format!("{}/.scratch", app.pack_path()), DIM)],
        ),
        Line::raw(""),
        field("Download", download),
        field("Disk needed", disk),
        field("Time", vec![plain(mock::about(app.build_secs()))]),
        Line::raw(""),
        Line::from(vec![
            plain("  "),
            button("Start build", app.confirm_cursor == 0, app.fits_disk()),
            plain("  "),
            button("Back", app.confirm_cursor == 1, true),
        ]),
    ]);
    frame.render_widget(Paragraph::new(lines), area);
    footer(
        frame,
        app,
        &[
            ("←→", "choose"),
            ("m", "memory"),
            ("enter", "confirm"),
            ("esc", "back"),
        ],
    );
}

fn lerp(ends: [(u8, u8, u8); 2], t: f64) -> Color {
    let mix = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
    Color::Rgb(
        mix(ends[0].0, ends[1].0),
        mix(ends[0].1, ends[1].1),
        mix(ends[0].2, ends[1].2),
    )
}

// A stage line is laid out as Ollama lays out a layer, with cargo's verb in
// front: the verb, what it works on, the percent, a bar, then size, speed and
// time in columns of fixed width, so nothing moves as the numbers change.
const VERB_WIDTH: usize = 11;
const NAME_WIDTH: usize = 16;
const SIZE_WIDTH: usize = 13;
const SPEED_WIDTH: usize = 9;
const TIME_WIDTH: usize = 8;
/// Everything on a line but the bar: the gutter, verb, name, percent and the
/// caps and gap around the bar, then the three right-hand columns.
const LINE_FIXED: usize =
    3 + VERB_WIDTH + 1 + NAME_WIDTH + 2 + 5 + 2 + 2 + SIZE_WIDTH + SPEED_WIDTH + TIME_WIDTH;

/// Cells of the bar: what the width leaves after the other columns.
fn bar_cells(inner: usize) -> usize {
    inner.saturating_sub(LINE_FIXED).clamp(8, 24)
}

/// A bar between thin caps. The filled part runs along a gradient, or is one
/// colour when `solid` is given; its last cell is cut in eighths.
fn bar_spans(cells: usize, ratio: f64, solid: Option<Color>) -> Vec<Span<'static>> {
    const EIGHTHS: [&str; 7] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let exact = ratio.clamp(0.0, 1.0) * cells as f64;
    let full = exact.floor() as usize;
    let part = ((exact - full as f64) * 8.0) as usize;
    let mut spans = vec![span("▕", FAINT)];
    for index in 0..cells {
        let color = solid.unwrap_or_else(|| lerp(BAR, index as f64 / cells.max(1) as f64));
        spans.push(if index < full {
            span("█", color)
        } else if index == full && part > 0 {
            span(EIGHTHS[part - 1], color)
        } else {
            span("░", FAINT)
        });
    }
    spans.push(span("▏", FAINT));
    spans
}

/// A title whose letters brighten in a band that sweeps along it.
fn shimmer(text: &str, millis: u128) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let head = ((millis / 70) as usize % (chars.len() + 10)) as f64 - 5.0;
    chars
        .iter()
        .enumerate()
        .map(|(index, c)| {
            let near = (1.0 - (index as f64 - head).abs() / 3.5).max(0.0);
            Span::styled(
                c.to_string(),
                Style::new()
                    .fg(lerp(SHIMMER, near))
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect()
}

fn finished_line(stage: &Stage, inner: usize) -> Line<'static> {
    let cells = bar_cells(inner);
    // History lines start at the terminal's edge; the live window sits one
    // column in, so the line carries that margin to stay in line with it.
    let mut spans = vec![
        plain("    "),
        Span::styled(
            pad(stage.did, VERB_WIDTH),
            Style::new().fg(DONE_BAR).add_modifier(Modifier::BOLD),
        ),
        plain(" "),
        plain(pad(&cut(&stage.object, NAME_WIDTH), NAME_WIDTH)),
        plain("  "),
    ];
    if stage.total.is_some() {
        spans.push(plain("100% "));
        spans.extend(bar_spans(cells, 1.0, Some(DONE_BAR)));
        spans.push(span(
            format!("  {:>SIZE_WIDTH$}", cut(&stage.summary, SIZE_WIDTH)),
            DIM,
        ));
        spans.push(plain(" ".repeat(SPEED_WIDTH)));
    } else {
        // A stage with no total has no bar: its result stands where the bar was.
        spans.push(plain("     "));
        spans.push(span(pad(&cut(&stage.summary, cells + 2), cells + 2), DIM));
        spans.push(plain(" ".repeat(2 + SIZE_WIDTH + SPEED_WIDTH)));
    }
    spans.push(span(
        format!("{:>TIME_WIDTH$}", mock::duration(stage.real_secs)),
        DIM,
    ));
    Line::from(spans)
}

fn running_line(app: &App, build: &Build, inner: usize) -> Line<'static> {
    let stage = build.stage();
    let progress = build.progress();
    let cells = bar_cells(inner);
    // The verb and what it works on shimmer as one title.
    let title = format!(
        "{} {}",
        pad(stage.doing, VERB_WIDTH),
        pad(&cut(&stage.object, NAME_WIDTH), NAME_WIDTH)
    );
    let mut spans = vec![plain(" "), span(spinner(app), ACCENT), plain(" ")];
    spans.extend(shimmer(&title, app.started.elapsed().as_millis()));
    spans.push(plain("  "));
    match stage.total {
        Some(total) => {
            let megabytes = stage.unit == "MB";
            let (done, whole) = if megabytes {
                (mock::size(total * progress), mock::size(total))
            } else {
                (mock::count(total * progress), mock::count(total))
            };
            let rate = if megabytes {
                format!("{:.1} MB/s", total / stage.real_secs)
            } else {
                format!("{}/s", mock::count(total / stage.real_secs))
            };
            spans.push(plain(format!("{:>3}% ", (progress * 100.0).floor())));
            spans.extend(bar_spans(cells, progress, None));
            spans.push(plain(format!(
                "  {:>SIZE_WIDTH$}",
                format!("{done}/{whole}")
            )));
            spans.push(span(format!("{rate:>SPEED_WIDTH$}"), DIM));
            spans.push(span(
                format!(
                    "{:>TIME_WIDTH$}",
                    mock::duration((1.0 - progress) * stage.real_secs)
                ),
                DIM,
            ));
        }
        None => {
            // No total, so no percent and no bar: only time passing.
            spans.push(plain("     "));
            spans.push(span(pad("working…", cells + 2), DIM));
            spans.push(plain(" ".repeat(2 + SIZE_WIDTH + SPEED_WIDTH)));
            spans.push(span(
                format!(
                    "{:>TIME_WIDTH$}",
                    mock::duration(progress * stage.real_secs)
                ),
                DIM,
            ));
        }
    }
    Line::from(spans)
}

fn build(frame: &mut Frame, area: Rect, app: &App) {
    let Some(build) = &app.build else {
        return;
    };
    let inner = area.width as usize;
    let overall = build.overall();
    let left = mock::about(build.remaining());
    let mut lines = vec![
        running_line(app, build, inner),
        Line::raw(""),
        Line::from(span(
            format!(
                "   Step {} of {}  ·  {} elapsed  ·  about {} left",
                build.current + 1,
                build.stages.len(),
                mock::duration(build.elapsed()),
                left.trim_start_matches("~ "),
            ),
            DIM,
        )),
    ];
    if app.show_logs {
        let stage = build.stage();
        let budget = app.memory_gb() as f64;
        let memory = (1.4 + overall * 4.0).min(budget * 0.85);
        lines.push(Line::raw(""));
        lines.push(Line::from(span(format!("   {}", stage.explain[0]), DIM)));
        lines.push(Line::from(span(format!("   {}", stage.explain[1]), DIM)));
        lines.push(Line::raw(""));
        lines.push(Line::from(span(
            format!("   memory {memory:.1} / {budget:.1} GB"),
            DIM,
        )));
        lines.push(Line::raw(""));
        lines.push(Line::from(span(
            format!("   log {}", "─".repeat(inner.saturating_sub(7))),
            FAINT,
        )));
        let end = ((overall * mock::LOGS.len() as f64).ceil() as usize).clamp(1, mock::LOGS.len());
        let start = end.saturating_sub(3);
        for (level, text) in &mock::LOGS[start..end] {
            let color = if *level == "warn" { WARN } else { DIM };
            lines.push(Line::from(vec![
                span(format!("   {level:<5} "), color),
                span(text.to_string(), DIM),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
    let details = if app.show_logs {
        "hide details"
    } else {
        "details"
    };
    footer(
        frame,
        app,
        &[("l", details), ("esc", "cancel, keeps downloads")],
    );
}

fn done(frame: &mut Frame, area: Rect, app: &App) {
    let stat = |label: &str, value: &str| {
        vec![bold(value.to_string()), span(format!(" {label}    "), DIM)]
    };
    let mut stats = vec![plain("   ")];
    stats.extend(stat("places", mock::PACK.places));
    stats.extend(stat("streets", mock::PACK.streets));
    stats.extend(stat("addresses", mock::PACK.addresses));
    stats.extend(stat("POIs", mock::PACK.pois));
    let step = |label: &str, command: String| {
        Line::from(vec![
            plain("   "),
            plain(pad(label, 17)),
            span(command, ACCENT),
        ])
    };
    let more =
        |text: String, color: Color| Line::from(vec![plain(" ".repeat(20)), span(text, color)]);
    // The command is long: break it after the region, as a shell would.
    let command = app.command();
    let (head, rest) = command
        .match_indices(" --")
        .nth(1)
        .map_or((command.as_str(), ""), |(at, _)| command.split_at(at));
    let mut buttons = vec![plain("  ")];
    for (index, label) in app::DONE_ITEMS.iter().enumerate() {
        buttons.push(button(label, index == app.done_cursor, true));
        buttons.push(plain("  "));
    }
    let lines = vec![
        Line::raw(""),
        Line::from(vec![
            plain("   "),
            bold("Pack ready"),
            span(
                format!(
                    "   {} · {} · built in {}",
                    app.pack_path(),
                    mock::PACK.size,
                    mock::duration(app.built_secs)
                ),
                DIM,
            ),
        ]),
        Line::raw(""),
        Line::from(stats),
        Line::raw(""),
        Line::from(bold("   Next")),
        Line::raw(""),
        step("Search it", format!("og query {}", app.pack_path())),
        step(
            "Serve the API",
            format!("og serve --pack {}", app.pack_path()),
        ),
        more("then GET http://127.0.0.1:8080/search?q=…".to_string(), DIM),
        step("Build it again", format!("{head} \\")),
        more(rest.trim_start().to_string(), ACCENT),
        Line::raw(""),
        Line::from(buttons),
    ];
    frame.render_widget(Paragraph::new(lines), area);
    footer(
        frame,
        app,
        &[("←→", "choose"), ("enter", "select"), ("q", "quit")],
    );
}

fn badge() -> Span<'static> {
    Span::styled(
        " og query ",
        Style::new()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
    )
}

fn layer_color(layer: &str) -> Color {
    match layer {
        "address" => VIOLET,
        "poi" => PINK,
        "street" => ACCENT,
        _ => OK,
    }
}

fn label_color(label: &str) -> Color {
    match label {
        "house_number" => WARN,
        "street" => ACCENT,
        "locality" => OK,
        "region" => OK,
        "postcode" => PINK,
        _ => VIOLET,
    }
}

fn query(frame: &mut Frame, area: Rect, app: &App) {
    let [header, input, parsed, _, results, _, _] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            badge(),
            span(
                format!("  {} · {} records", app.active_name, app.active_records),
                DIM,
            ),
        ])),
        header,
    );
    let mut tabs = Vec::new();
    for mode in [Mode::Search, Mode::Reverse, Mode::Batch] {
        tabs.push(if mode == app.mode {
            Span::styled(
                format!(" {} ", mode.name()),
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
            )
        } else {
            span(format!(" {} ", mode.name()), FAINT)
        });
    }
    frame.render_widget(Paragraph::new(Line::from(tabs).right_aligned()), header);

    let placeholder = match app.mode {
        Mode::Search => "try: 100 queen st w tor",
        Mode::Reverse => "lat, lon   try: 43.6535, -79.3839",
        Mode::Batch => "paste a CSV, one address per row, or type a file path",
    };
    let typed = if app.mode == Mode::Batch {
        batch_input(app)
    } else {
        app.query.clone()
    };
    frame.render_widget(
        Paragraph::new(input_line(&typed, placeholder)).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(FAINT)),
        ),
        input,
    );

    let parsed_line = match app.mode {
        Mode::Batch if app.batch_rows.is_empty() => Line::raw(""),
        Mode::Batch => {
            let matched = app
                .batch_rows
                .iter()
                .filter(|row| row.hit.is_some())
                .count();
            Line::from(vec![
                span("  ", DIM),
                bold(app.batch_rows.len().to_string()),
                span(format!(" rows  ·  {}  ·  ", app.batch_note), DIM),
                span(
                    format!("{matched} matched"),
                    if matched == app.batch_rows.len() {
                        OK
                    } else {
                        DIM
                    },
                ),
            ])
        }
        Mode::Reverse => match mock::point(&app.query) {
            Some((lat, lon)) => Line::from(vec![
                span("  point   ", DIM),
                span("lat ", DIM),
                bold(format!("{lat:.5}")),
                span("  lon ", DIM),
                bold(format!("{lon:.5}")),
            ]),
            None if app.query.is_empty() => Line::raw(""),
            None => Line::from(span("  waiting for lat, lon", WARN)),
        },
        _ => {
            let parts = mock::parse(&app.query);
            let mut spans = vec![span("  parsed  ", DIM)];
            for (index, (text, label)) in parts.iter().enumerate() {
                if index > 0 {
                    spans.push(span("  ·  ", FAINT));
                }
                spans.push(bold(text.clone()));
                spans.push(span(format!(" {label}"), label_color(label)));
            }
            if parts.is_empty() {
                Line::raw("")
            } else {
                Line::from(spans)
            }
        }
    };
    frame.render_widget(Paragraph::new(parsed_line), parsed);
    if !app.query.is_empty() || !app.batch_rows.is_empty() && app.mode == Mode::Batch {
        let millis = match app.mode {
            Mode::Batch => 2 + app.batch_rows.len() / 40,
            _ => 3 + app.query.len() % 5,
        };
        frame.render_widget(
            Paragraph::new(Line::from(span(format!("{millis} ms "), DIM)).right_aligned()),
            parsed,
        );
    }

    let hits = app.hits();
    let lines: Vec<Line> = if app.mode == Mode::Batch && app.batch_candidate.is_some() {
        batch_card(app)
    } else if app.mode == Mode::Batch {
        batch_table(app, results)
    } else if app.query.is_empty() {
        vec![
            Line::from(span(
                "  Results follow each key you type. Enter shows the picked result as GeoJSON.",
                DIM,
            )),
            Line::from(span(
                "  Tab switches between search, reverse and batch.",
                DIM,
            )),
        ]
    } else if hits.is_empty() {
        vec![Line::from(span("  No match in this Pack.", DIM))]
    } else {
        hits.iter()
            .enumerate()
            .map(|(index, (record, value))| {
                hit_row(record, *value, app.mode, index == app.hit_cursor)
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), results);

    if app.mode == Mode::Batch && app.batch_candidate.is_some() {
        footer(
            frame,
            app,
            &[("←→", "choose"), ("enter", "select"), ("esc", "cancel")],
        );
        return;
    }
    if app.mode == Mode::Batch {
        let esc = if app.batch_text.is_empty() {
            "back"
        } else {
            "clear"
        };
        footer(
            frame,
            app,
            &[
                ("↑↓", "pick"),
                ("ctrl+g", "GeoJSON"),
                ("ctrl+e", "example"),
                ("ctrl+u", "clear"),
                ("tab", "mode"),
                ("esc", esc),
            ],
        );
        return;
    }
    let esc = if app.query.is_empty() {
        "back"
    } else {
        "clear"
    };
    footer(
        frame,
        app,
        &[
            ("↑↓", "pick"),
            ("enter", "GeoJSON"),
            ("tab", "mode"),
            ("ctrl+r", "reverse"),
            ("esc", esc),
        ],
    );
}

/// What the batch box shows: the text typed, or how much was pasted.
fn batch_input(app: &App) -> String {
    if app.batch_text.contains('\n') {
        let lines = app
            .batch_text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        format!("{lines} lines pasted")
    } else {
        app.batch_text.clone()
    }
}

/// Cuts text to a number of cells, ending it with an ellipsis.
fn cut(text: &str, cells: usize) -> String {
    if width(text) > cells {
        let keep: String = text.chars().take(cells.saturating_sub(1)).collect();
        format!("{keep}…")
    } else {
        text.to_string()
    }
}

/// The pasted addresses with the answer for each.
fn batch_table(app: &App, area: Rect) -> Vec<Line<'static>> {
    let rows = &app.batch_rows;
    if rows.is_empty() {
        return vec![
            Line::from(span(
                "  Paste a CSV with one address per row, or type the path of a file.",
                DIM,
            )),
            Line::from(span(
                "  A column named address is used; without one, each row is one address.",
                DIM,
            )),
            Line::from(span(
                "  Paste a file path to continue with that file, or press ctrl+e for a 10,000 address example.",
                DIM,
            )),
            Line::raw(""),
            Line::from(span("  address", FAINT)),
            Line::from(span("  100 queen st w toronto", FAINT)),
            Line::from(span("  24 sussex dr ottawa", FAINT)),
        ];
    }
    let inner = area.width as usize;
    let address_width = 26.min(inner / 3);
    let match_width = inner.saturating_sub(3 + 4 + address_width + 2 + 2 + 2 + 5 + 1);
    let capacity = (area.height as usize).saturating_sub(1).max(1);
    let start = (app.hit_cursor + 1).saturating_sub(capacity);
    let mut lines = vec![Line::from(span(
        format!(
            "   {:>3}  {}  {}  {:>5}",
            "#",
            pad("Address", address_width),
            pad("Match", match_width + 2),
            "Score"
        ),
        FAINT,
    ))];
    for (index, row) in rows.iter().enumerate().skip(start).take(capacity) {
        let selected = index == app.hit_cursor;
        let address = pad(&cut(&row.input, address_width), address_width);
        let mut spans = vec![
            if selected {
                span(" ❯ ", ACCENT)
            } else {
                plain("   ")
            },
            span(format!("{:>3}  ", index + 1), DIM),
            if selected {
                bold(address)
            } else {
                plain(address)
            },
            plain("  "),
        ];
        match row.hit {
            Some((record, value)) => {
                spans.push(span("✓ ", OK));
                spans.push(plain(pad(&cut(record.label, match_width), match_width)));
                spans.push(span(format!("  {value:>5.2}"), DIM));
            }
            None => spans.push(span("– no match", DIM)),
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn hit_row(record: &Record, value: f64, mode: Mode, selected: bool) -> Line<'static> {
    let measure = match mode {
        Mode::Reverse if value >= 1_000.0 => span(format!("{:>5.1} km", value / 1_000.0), DIM),
        Mode::Reverse => span(format!("{:>6.0} m", value), DIM),
        _ => {
            let color = if value >= 0.8 {
                OK
            } else if value >= 0.55 {
                WARN
            } else {
                DIM
            };
            span(format!("{value:.2}"), color)
        }
    };
    Line::from(vec![
        if selected {
            span(" ❯ ", ACCENT)
        } else {
            plain("   ")
        },
        measure,
        plain("  "),
        span(pad(record.layer, 9), layer_color(record.layer)),
        if selected {
            bold(record.label)
        } else {
            plain(record.label)
        },
    ])
}

/// A JSON value with its keys in contract order.
enum Json {
    Str(String),
    Num(String),
    Obj(Vec<(&'static str, Json)>),
    Arr(Vec<Json>),
}

fn string(text: &str) -> Json {
    Json::Str(text.to_string())
}

fn area(id: &str, name: &str) -> Json {
    Json::Obj(vec![("id", string(id)), ("name", string(name))])
}

fn feature(record: &Record, value: f64, mode: Mode, row: usize) -> Json {
    let mut address = Vec::new();
    if let Some(number) = record.housenumber {
        address.push(("housenumber", string(number)));
    }
    if let Some(street) = record.street {
        address.push(("street", string(street)));
    }
    if let Some(postcode) = record.postcode {
        address.push(("postcode", string(postcode)));
    }
    let (kind, id) = record.id.rsplit_once(':').unwrap_or(("node", "0"));
    let kind = kind.rsplit(':').next().unwrap_or(kind);
    let mut properties = vec![
        ("id", string(record.id)),
        (
            "record_id",
            Json::Num((4_812_330 + row * 7_919).to_string()),
        ),
        ("layer", string(record.layer)),
        ("accuracy", string(record.accuracy)),
        ("name", string(record.name)),
        ("label", string(record.label)),
    ];
    if let Some(category) = record.category {
        properties.push(("category", string(category)));
    }
    if !address.is_empty() {
        properties.push(("address", Json::Obj(address)));
    }
    let mut context = Vec::new();
    if record.layer != "locality" {
        context.push(("locality", area("osm:relation:324211", record.locality)));
    }
    context.push(("region", area("osm:relation:68841", "Ontario")));
    context.push(("country", area("osm:relation:1428125", "Canada")));
    properties.push(("context", Json::Obj(context)));
    properties.push((
        "source",
        Json::Obj(vec![
            ("dataset", string("osm")),
            ("object_type", string(kind)),
            ("object_id", Json::Num(id.to_string())),
        ]),
    ));
    match mode {
        Mode::Reverse => properties.push(("distance_m", Json::Num(format!("{value:.1}")))),
        _ => properties.push(("score", Json::Num(format!("{value:.2}")))),
    }
    Json::Obj(vec![
        ("type", string("Feature")),
        (
            "geometry",
            Json::Obj(vec![
                ("type", string("Point")),
                (
                    "coordinates",
                    Json::Arr(vec![
                        Json::Num(format!("{:.7}", record.lon)),
                        Json::Num(format!("{:.7}", record.lat)),
                    ]),
                ),
            ]),
        ),
        ("properties", Json::Obj(properties)),
    ])
}

/// The answer as the API would return it, one styled line per JSON line.
pub fn json_lines(app: &App) -> Vec<Line<'static>> {
    let hits = app.hits();
    let picked: Vec<(usize, (&Record, f64))> = if app.mode == Mode::Batch {
        app.batch_rows
            .iter()
            .enumerate()
            .filter_map(|(row, batch)| batch.hit.map(|hit| (row, hit)))
            .filter(|(row, _)| app.json_all || *row == app.hit_cursor)
            .collect()
    } else if app.json_all {
        hits.iter().copied().enumerate().collect()
    } else {
        hits.iter()
            .copied()
            .enumerate()
            .skip(app.hit_cursor)
            .take(1)
            .collect()
    };
    let geocoding = match (app.mode, mock::point(&app.query)) {
        (Mode::Reverse, Some((lat, lon))) => vec![
            ("version", string("1")),
            (
                "point",
                Json::Obj(vec![
                    ("lon", Json::Num(format!("{lon}"))),
                    ("lat", Json::Num(format!("{lat}"))),
                ]),
            ),
        ],
        (Mode::Batch, _) => vec![
            ("version", string("1")),
            (
                "query",
                string(&format!("batch of {} rows", app.batch_rows.len())),
            ),
        ],
        _ => vec![
            ("version", string("1")),
            ("query", string(app.query.trim())),
        ],
    };
    let mut geocoding = geocoding;
    geocoding.push(("attribution", string("© OpenStreetMap contributors, ODbL")));
    let collection = Json::Obj(vec![
        ("type", string("FeatureCollection")),
        ("geocoding", Json::Obj(geocoding)),
        (
            "features",
            Json::Arr(
                picked
                    .into_iter()
                    .map(|(row, (record, value))| feature(record, value, app.mode, row))
                    .collect(),
            ),
        ),
    ]);
    let mut lines = Vec::new();
    emit(&collection, 0, None, false, &mut lines);
    lines
}

fn emit(value: &Json, indent: usize, key: Option<&str>, comma: bool, out: &mut Vec<Line<'static>>) {
    let mut head = vec![plain(" ".repeat(indent + 1))];
    if let Some(key) = key {
        head.push(span(format!("\"{key}\""), ACCENT));
        head.push(span(": ", FAINT));
    }
    let tail = || span(if comma { "," } else { "" }, FAINT);
    match value {
        Json::Str(text) => {
            let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
            head.push(span(format!("\"{escaped}\""), OK));
            head.push(tail());
            out.push(Line::from(head));
        }
        Json::Num(number) => {
            head.push(span(number.clone(), WARN));
            head.push(tail());
            out.push(Line::from(head));
        }
        Json::Arr(items) if items.iter().all(|item| matches!(item, Json::Num(_))) => {
            head.push(span("[", FAINT));
            for (index, item) in items.iter().enumerate() {
                if let Json::Num(number) = item {
                    if index > 0 {
                        head.push(span(", ", FAINT));
                    }
                    head.push(span(number.clone(), WARN));
                }
            }
            head.push(span("]", FAINT));
            head.push(tail());
            out.push(Line::from(head));
        }
        Json::Arr(items) => {
            head.push(span("[", FAINT));
            out.push(Line::from(head));
            for (index, item) in items.iter().enumerate() {
                emit(item, indent + 2, None, index + 1 < items.len(), out);
            }
            out.push(Line::from(vec![
                plain(" ".repeat(indent + 1)),
                span("]", FAINT),
                tail(),
            ]));
        }
        Json::Obj(fields) => {
            head.push(span("{", FAINT));
            out.push(Line::from(head));
            for (index, (name, item)) in fields.iter().enumerate() {
                emit(item, indent + 2, Some(name), index + 1 < fields.len(), out);
            }
            out.push(Line::from(vec![
                plain(" ".repeat(indent + 1)),
                span("}", FAINT),
                tail(),
            ]));
        }
    }
}

fn json(frame: &mut Frame, area: Rect, app: &App) {
    let [header, panel_area, _] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(JSON_ROWS as u16 + 2),
        Constraint::Length(1),
    ])
    .areas(area);
    let hits = app.hits();
    let batch = app.mode == Mode::Batch;
    let what = if app.json_all {
        format!("  all {} results", hits.len())
    } else if batch {
        format!("  row {} of {}", app.hit_cursor + 1, app.batch_rows.len())
    } else {
        format!("  result {} of {}", app.hit_cursor + 1, hits.len())
    };
    let shown = if batch {
        app.batch_rows.get(app.hit_cursor).and_then(|row| row.hit)
    } else {
        hits.get(app.hit_cursor).copied()
    };
    let label = shown
        .filter(|_| !app.json_all)
        .map(|(record, _)| format!(" · {}", record.label))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(Line::from(vec![badge(), plain(what), span(label, DIM)])),
        header,
    );

    let lines = json_lines(app);
    let scroll = app.json_scroll.min(lines.len().saturating_sub(JSON_ROWS));
    let more = lines.len().saturating_sub(scroll + JSON_ROWS);
    let corner = if more > 0 {
        span(format!(" ↓ {more} more lines "), DIM)
    } else {
        span(" end ", FAINT)
    };
    let shown: Vec<Line> = lines.into_iter().skip(scroll).take(JSON_ROWS).collect();
    let block = panel("GeoJSON").title_bottom(Line::from(corner).right_aligned());
    frame.render_widget(Paragraph::new(shown).block(block), panel_area);
    let all = if app.json_all {
        "one result"
    } else {
        "all results"
    };
    footer(
        frame,
        app,
        &[("↑↓", "scroll"), ("a", all), ("y", "copy"), ("esc", "back")],
    );
}

/// The line that opens a batch run in terminal history.
pub fn batch_header(app: &App) -> Vec<Line<'static>> {
    let name = app
        .batch
        .as_ref()
        .map(|session| session.file.name.clone())
        .unwrap_or_default();
    vec![
        Line::raw(""),
        Line::from(vec![
            plain("    "),
            bold("Batch geocode"),
            span(format!("  ·  {name}"), DIM),
        ]),
        Line::raw(""),
    ]
}

/// How a column's role reads above it; the selected column's is in brackets.
fn role_span(role: Role, selected: bool, cells: usize) -> Span<'static> {
    let label = if selected {
        format!("[{}]", role.label())
    } else {
        role.label().to_string()
    };
    let text = pad(&label, cells);
    match (role, selected) {
        (Role::Skip, true) => Span::styled(text, Style::new().fg(DIM).add_modifier(Modifier::BOLD)),
        (Role::Skip, false) => span(text, FAINT),
        (_, true) => Span::styled(text, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
        (_, false) => span(text, ACCENT),
    }
}

/// Which columns of a table fit in `avail` cells, keeping `selected` in view.
fn visible_columns(widths: &[usize], selected: usize, avail: usize) -> std::ops::Range<usize> {
    let mut start = 0;
    loop {
        let mut end = start;
        let mut used = 0;
        while end < widths.len() && used + widths[end] <= avail {
            used += widths[end];
            end += 1;
        }
        if end == start {
            end = (start + 1).min(widths.len());
        }
        if selected < end || end >= widths.len() || start + 1 >= widths.len() {
            return start..end;
        }
        start += 1;
    }
}

// The result columns of the batch screens: the query, what it matched, the
// coordinates, and one more figure (a score, or the time the query took).
const MATCH_WIDTH: usize = 20;
const LAT_WIDTH: usize = 9;
const LON_WIDTH: usize = 10;
const LAST_WIDTH: usize = 7;

fn query_width(inner: usize) -> usize {
    inner
        .saturating_sub(3 + 2 + MATCH_WIDTH + 2 + LAT_WIDTH + 2 + LON_WIDTH + 2 + LAST_WIDTH)
        .clamp(14, 30)
}

fn result_header(inner: usize, title: &str, last: &str) -> Line<'static> {
    Line::from(span(
        format!(
            "   {}  {}  {:>LAT_WIDTH$}  {:>LON_WIDTH$}  {last:>LAST_WIDTH$}",
            pad(title, query_width(inner)),
            " ".repeat(MATCH_WIDTH),
            "lat",
            "lon",
        ),
        DIM,
    ))
}

fn result_line(
    inner: usize,
    query: &str,
    hit: Option<(&Record, f64)>,
    last: Span<'static>,
) -> Line<'static> {
    let query_cells = query_width(inner);
    let shown = if query.is_empty() {
        "(no address)"
    } else {
        query
    };
    let mut spans = vec![
        plain("   "),
        if query.is_empty() {
            span(pad(&cut(shown, query_cells), query_cells), FAINT)
        } else {
            plain(pad(&cut(shown, query_cells), query_cells))
        },
        plain("  "),
    ];
    match hit {
        Some((record, _)) => {
            spans.push(plain(pad(&cut(record.name, MATCH_WIDTH), MATCH_WIDTH)));
            spans.push(plain(format!("  {:>LAT_WIDTH$.5}", record.lat)));
            spans.push(plain(format!("  {:>LON_WIDTH$.5}", record.lon)));
        }
        None => {
            spans.push(span(pad("no match", MATCH_WIDTH), DIM));
            spans.push(span(format!("  {:>LAT_WIDTH$}", "–"), DIM));
            spans.push(span(format!("  {:>LON_WIDTH$}", "–"), DIM));
        }
    }
    spans.push(plain("  "));
    spans.push(last);
    Line::from(spans)
}

/// `10,000 rows · 4.2 MB · header found · 14 blank rows skipped`.
fn file_facts(file: &batch::BatchFile) -> String {
    let mut facts = format!(
        "{} rows · {} · {}",
        mock::thousands(file.total_rows),
        mock::size(file.bytes as f64 / 1e6),
        if file.header_found {
            "header found"
        } else {
            "no header row"
        },
    );
    if file.blank_skipped > 0 {
        facts.push_str(&format!(
            " · {} blank {} skipped",
            file.blank_skipped,
            if file.blank_skipped == 1 {
                "row"
            } else {
                "rows"
            }
        ));
    }
    facts
}

/// The file named in the batch tab, with a choice to continue or drop it.
fn batch_card(app: &App) -> Vec<Line<'static>> {
    let Some(file) = &app.batch_candidate else {
        return Vec::new();
    };
    vec![
        Line::from(vec![plain("   "), bold(file.name.clone())]),
        Line::from(span(format!("   {}", file_facts(file)), DIM)),
        Line::raw(""),
        Line::from(vec![
            plain("  "),
            button("Continue", app.batch_choice == 0, true),
            plain("  "),
            button("Cancel", app.batch_choice == 1, true),
        ]),
    ]
}

fn batch_setup(frame: &mut Frame, area: Rect, app: &App) {
    let Some(session) = &app.batch else {
        return;
    };
    let file = &session.file;
    let inner = area.width as usize;
    let shown = 3.min(file.total_rows);
    let sample: Vec<Vec<String>> = (0..shown).map(|index| file.row(index)).collect();

    let facts = file_facts(file);

    // The columns, each with the role it will play, and the first rows.
    let widths: Vec<usize> = file
        .headers
        .iter()
        .enumerate()
        .map(|(column, header)| {
            let cells = sample
                .iter()
                .map(|row| width(&row[column]))
                .max()
                .unwrap_or(0);
            let role = width(session.roles[column].label()) + 2;
            (width(header).max(cells).max(role) + 2).clamp(9, 18)
        })
        .collect();
    let window = visible_columns(&widths, session.column, inner.saturating_sub(5));
    let more = window.end < widths.len();
    let mut roles = vec![plain("   ")];
    let mut headers = vec![plain("   ")];
    let mut rows: Vec<Vec<Span<'static>>> = sample.iter().map(|_| vec![plain("   ")]).collect();
    for column in window {
        let cells = widths[column];
        roles.push(role_span(
            session.roles[column],
            column == session.column,
            cells,
        ));
        headers.push(span(
            pad(&cut(&file.headers[column], cells - 2), cells),
            DIM,
        ));
        for (row, line) in sample.iter().zip(rows.iter_mut()) {
            line.push(plain(pad(&cut(&row[column], cells - 2), cells)));
        }
    }
    if more {
        roles.push(span("…", FAINT));
    }

    let mut lines = vec![
        Line::from(vec![
            plain("   "),
            bold("Batch geocode"),
            span(format!("  ·  {}", app.active_name), DIM),
        ]),
        Line::raw(""),
        Line::from(vec![
            plain("   "),
            bold(file.name.clone()),
            span(format!("    {facts}"), DIM),
        ]),
        Line::raw(""),
        Line::from(roles),
        Line::from(headers),
    ];
    lines.extend(rows.into_iter().map(Line::from));
    lines.push(Line::raw(""));
    lines.push(result_header(inner, "First rows geocoded", "score"));
    for index in 0..shown {
        let hit = session.lookup(index);
        let score = match hit {
            Some((_, score)) => span(format!("{score:>LAST_WIDTH$.2}"), DIM),
            None => span(format!("{:>LAST_WIDTH$}", "–"), DIM),
        };
        lines.push(result_line(inner, &session.query(index), hit, score));
    }
    let mut note = session.estimate();
    let missing = session.without_address();
    if missing > 0 {
        note.push_str(&format!(
            "  ·  {} rows have no address and will be skipped",
            mock::thousands(missing)
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(span(format!("   {note}"), DIM)));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        plain("   "),
        button("Start", true, true),
        plain("  "),
        button("Back", false, true),
    ]));
    frame.render_widget(Paragraph::new(lines), area);
    footer(
        frame,
        app,
        &[
            ("←→", "column"),
            ("↑↓", "role"),
            ("enter", "start"),
            ("esc", "back"),
        ],
    );
}

/// The newest queries of a run, a few at a time: the engine does thousands a
/// second, so the list shows a sample that changes about seven times a second.
fn latest_queries(
    session: &BatchSession,
    build: &Build,
    done: usize,
    inner: usize,
) -> Vec<Line<'static>> {
    let stage = build.stage();
    let total = session.file.total_rows;
    let (p50, p95, p99) = batch::latency_stats(done);
    let matched = (done as f64 * session.share).round() as usize;
    let left = format!("   p50 {p50:.1} ms  ·  p95 {p95:.1} ms  ·  p99 {p99:.1} ms");
    let share = if done > 0 {
        matched as f64 / done as f64 * 100.0
    } else {
        0.0
    };
    let right = format!(
        "{share:.1}% matched  ·  {} with no match",
        mock::thousands(done - matched)
    );
    let mut lines = vec![
        Line::from(vec![
            span(pad(&left, inner.saturating_sub(width(&right))), DIM),
            span(right, DIM),
        ]),
        Line::raw(""),
        result_header(inner, "Latest", "time"),
    ];
    let per_tick = (total as f64 / (stage.play_secs * 1000.0 / 150.0)).max(1.0) as usize;
    let tick = (build.stage_started.elapsed().as_millis() / 150) as usize;
    let anchor = (tick * per_tick).min(done);
    for back in 0..8 {
        if anchor < back + 1 {
            break;
        }
        let index = anchor - 1 - back;
        let took = mock::latency_ms(index);
        let color = if took > 3.0 { ACCENT } else { DIM };
        lines.push(result_line(
            inner,
            &session.query(index),
            session.lookup(index),
            span(format!("{:>LAST_WIDTH$}", format!("{took:.1} ms")), color),
        ));
    }
    lines
}

fn batch_run(frame: &mut Frame, area: Rect, app: &App) {
    let (Some(build), Some(session)) = (&app.build, &app.batch) else {
        return;
    };
    let inner = area.width as usize;
    let mut lines = vec![running_line(app, build, inner), Line::raw("")];
    if build.stage().doing == "Geocoding" {
        let done = (session.file.total_rows as f64 * build.progress()) as usize;
        lines.extend(latest_queries(session, build, done, inner));
    }
    frame.render_widget(Paragraph::new(lines), area);
    footer(frame, app, &[("esc", "cancel")]);
}

/// The first rows of the output file: the file's own columns, then lat and
/// lon, which the run adds.
fn output_table(session: &BatchSession, inner: usize) -> Vec<Line<'static>> {
    let file = &session.file;
    let shown = 3.min(file.total_rows);
    let sample: Vec<Vec<String>> = (0..shown).map(|index| file.row(index)).collect();
    let widths: Vec<usize> = file
        .headers
        .iter()
        .enumerate()
        .map(|(column, header)| {
            let cells = sample
                .iter()
                .map(|row| width(&row[column]))
                .max()
                .unwrap_or(0);
            (width(header).max(cells) + 2).clamp(6, 18)
        })
        .collect();
    let avail = inner.saturating_sub(3 + LAT_WIDTH + 1 + 2 + LON_WIDTH + 1 + 2);
    let mut used = 0;
    let mut count = 0;
    for cells in &widths {
        if used + cells > avail && count > 0 {
            break;
        }
        used += cells;
        count += 1;
    }
    let coordinate = Style::new().fg(ACCENT);
    let mut header = vec![plain("   ")];
    for (cells, name) in widths.iter().zip(&file.headers).take(count) {
        header.push(span(pad(&cut(name, cells - 2), *cells), DIM));
    }
    header.push(Span::styled(
        format!("{:>w$}", "lat", w = LAT_WIDTH + 1),
        coordinate,
    ));
    header.push(Span::styled(
        format!("  {:>w$}", "lon", w = LON_WIDTH + 1),
        coordinate,
    ));
    let mut lines = vec![Line::from(header)];
    for (index, row) in sample.iter().enumerate() {
        let mut spans = vec![plain("   ")];
        for (cells, cell) in widths.iter().zip(row).take(count) {
            spans.push(plain(pad(&cut(cell, cells - 2), *cells)));
        }
        match session.lookup(index) {
            Some((record, _)) => {
                spans.push(plain(format!("{:>w$.5}", record.lat, w = LAT_WIDTH + 1)));
                spans.push(plain(format!("  {:>w$.5}", record.lon, w = LON_WIDTH + 1)));
            }
            None => spans.push(plain("")),
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn batch_done(frame: &mut Frame, area: Rect, app: &App) {
    let Some(session) = &app.batch else {
        return;
    };
    let Some(summary) = &session.summary else {
        return;
    };
    let inner = area.width as usize;
    let rows = session.file.total_rows;
    let missed = rows - summary.matched;
    let percent = |count: usize| count as f64 / rows.max(1) as f64 * 100.0;
    let stem = session.file.stem().to_string();

    let mut accuracy = vec![plain("   ")];
    for (name, count) in &summary.by_accuracy {
        accuracy.push(span(format!("{name} "), DIM));
        accuracy.push(bold(mock::thousands(*count)));
        accuracy.push(plain("    "));
    }
    let output = format!("{stem}.geocoded.csv");
    let size = format!(
        "{} rows · {}",
        mock::thousands(rows),
        mock::size(summary.output_bytes as f64 / 1e6)
    );
    let mut lines = vec![
        Line::raw(""),
        Line::from(vec![
            plain("   "),
            bold(mock::thousands(summary.matched)),
            span(
                format!(" matched  ·  {:.1}%", percent(summary.matched)),
                DIM,
            ),
            plain("          "),
            bold(mock::thousands(missed)),
            span(format!(" with no match  ·  {:.1}%", percent(missed)), DIM),
        ]),
        Line::from(accuracy),
        Line::from(span(
            format!(
                "   p50 {:.1} ms  ·  p95 {:.1} ms  ·  p99 {:.1} ms  ·  slowest {:.1} ms  ·  {:.1}s in all",
                summary.p50, summary.p95, summary.p99, summary.slowest, summary.seconds
            ),
            DIM,
        )),
        Line::raw(""),
        Line::from(vec![
            plain("   "),
            bold(output.clone()),
            span(
                format!(
                    "{}{size}",
                    " ".repeat(inner.saturating_sub(3 + width(&output) + width(&size)))
                ),
                DIM,
            ),
        ]),
    ];
    lines.extend(output_table(session, inner));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        span("   Also written   ", DIM),
        plain(format!("{stem}.audit.csv")),
        span(
            format!(" · {}", mock::size(summary.audit_bytes as f64 / 1e6)),
            DIM,
        ),
    ]));
    lines.push(Line::raw(""));
    let mut buttons = vec![plain("  ")];
    for (index, label) in ["Review no matches", "Search in the shell", "Done"]
        .iter()
        .enumerate()
    {
        buttons.push(button(label, index == app.batch_button, true));
        buttons.push(plain("  "));
    }
    lines.push(Line::from(buttons));
    frame.render_widget(Paragraph::new(lines), area);
    footer(
        frame,
        app,
        &[("←→", "choose"), ("enter", "select"), ("q", "home")],
    );
}
