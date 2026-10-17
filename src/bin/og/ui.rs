//! Drawing. Each screen is a short stack of lines inside the inline viewport;
//! finished steps leave the viewport as plain lines in terminal history.

use std::borrow::Cow;

use open_geocode::{builder::BuildPlan, record::Layer};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph},
};

use crate::{
    app::{self, App, Mode, RegionRow, Screen},
    batch,
    engine::{self, Hit, Measure},
    format,
    home::PackInfo,
    job::{Step, Unit},
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
pub(crate) const JSON_ROWS: usize = 14;

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

/// Cuts text to a number of cells, ending it with an ellipsis.
fn cut(text: &str, cells: usize) -> String {
    if width(text) > cells {
        let keep: String = text.chars().take(cells.saturating_sub(1)).collect();
        format!("{keep}…")
    } else {
        text.to_string()
    }
}

fn spinner(app: &App) -> &'static str {
    SPINNER[(app.started.elapsed().as_millis() / 80) as usize % SPINNER.len()]
}

/// The header printed once on start. It stays in terminal history.
pub(crate) fn logo() -> Vec<Line<'static>> {
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
            span(concat!(" ", env!("CARGO_PKG_VERSION")), DIM),
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
        span(format!("  {}", format::bytes(app.input_bytes())), DIM),
    ];
    if !app.downloads() {
        region.push(span("  on disk", OK));
    }
    let mut output = vec![plain(app.pack_dir().display().to_string())];
    if let Some(Ok(plan)) = &app.plan {
        output.push(span(
            format!(
                "  memory budget {}",
                format::bytes(plan.memory_budget_bytes)
            ),
            DIM,
        ));
    }
    let lines = vec![
        choice("Region", region),
        choice(
            "Sources",
            vec![plain(
                regions::dataset_name(app.region.dataset()).to_string(),
            )],
        ),
        choice("Output", output),
    ];
    lines.into_iter().take(count).collect()
}

/// What a started build leaves in terminal history.
pub(crate) fn choices(app: &App) -> Vec<Line<'static>> {
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
pub(crate) fn content_width(terminal: u16) -> usize {
    terminal.saturating_sub(2).min(96) as usize
}

/// The line that opens a build in terminal history.
pub(crate) fn build_header(app: &App) -> Vec<Line<'static>> {
    vec![
        Line::raw(""),
        Line::from(vec![
            plain("    "),
            bold(format!("Building {}", app.region.label)),
        ]),
        Line::raw(""),
    ]
}

/// A finished step as it goes into terminal history, with a blank line
/// after it so the steps do not run together.
pub(crate) fn step_done(step: &Step, inner: usize) -> Vec<Line<'static>> {
    vec![finished_line(step, inner), Line::raw("")]
}

/// A build's warning as it goes into terminal history.
pub(crate) fn warning_line(warning: &str, inner: usize) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            plain("    "),
            span(cut(&format!("! {warning}"), inner.saturating_sub(4)), WARN),
        ]),
        Line::raw(""),
    ]
}

pub(crate) fn done_summary(app: &App) -> Vec<Line<'static>> {
    let Some(built) = &app.built else {
        return Vec::new();
    };
    vec![
        Line::raw(""),
        Line::from(vec![
            plain("    "),
            bold("Pack ready"),
            span(
                format!(
                    "  {} · {} · built in {}",
                    built.pack.dir.display(),
                    format::bytes(built.pack.bytes),
                    format::duration(built.build_secs)
                ),
                DIM,
            ),
        ]),
        Line::from(vec![
            span("    Serve it        ", DIM),
            span(format!("og serve {}", app.region.slug()), ACCENT),
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
pub(crate) const HEIGHT: u16 = 20;

pub(crate) fn draw(frame: &mut Frame, app: &App) {
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
            Line::from(span(
                format!("   Packs are kept in {}", app.home.packs_dir().display()),
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
        .map(|pack| width(&pack.name))
        .max()
        .unwrap_or(0)
        .clamp(10, 20);
    let [left, _, right] = Layout::horizontal([
        Constraint::Length(name_width as u16 + 23),
        Constraint::Length(4),
        Constraint::Min(0),
    ])
    .areas(area);

    let now = format::now();
    let mut list = vec![Line::from(bold("   Your Packs")), Line::raw("")];
    for (index, pack) in app.packs.iter().enumerate() {
        list.push(pack_row(
            pack,
            index == app.home_cursor,
            name_width,
            app.update_for(pack).is_some(),
            now,
        ));
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
            Line::from(plain("Pick a region of the world.")),
            Line::from(plain("og downloads its OpenStreetMap")),
            Line::from(plain("data and builds the Pack on this")),
            Line::from(plain("machine.")),
        ]
    } else {
        let pack = &app.packs[app.home_cursor];
        pack_detail(pack, app.update_for(pack), now)
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
        if app.update_for(&app.packs[app.home_cursor]).is_some() {
            hints.push(("u", "update"));
        }
        hints.push(("b", "build"));
        hints.push(("q", "quit"));
        hints
    };
    footer(frame, app, &hints);
}

fn pack_row(
    pack: &PackInfo,
    selected: bool,
    name_width: usize,
    update: bool,
    now: i64,
) -> Line<'static> {
    let shown = cut(&pack.name, name_width);
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
        span(format!("{:>8}", format::bytes(pack.bytes)), DIM),
        span(format!("  {:>7}", format::ago(pack.built_at, now)), DIM),
        if update {
            span("  ↑", ACCENT)
        } else {
            plain("   ")
        },
    ])
}

fn detail_line(label: &str, value: String) -> Line<'static> {
    Line::from(vec![span(pad(label, 10), DIM), plain(value)])
}

fn pack_detail(pack: &PackInfo, update: Option<i64>, now: i64) -> Vec<Line<'static>> {
    let mut built = format!(
        "{} · {}",
        format::ago(pack.built_at, now),
        format::date(pack.built_at)
    );
    if let Some(secs) = pack.build_secs {
        built.push_str(&format!(" · in {}", format::duration(secs)));
    }
    let mut lines = vec![
        Line::from(bold(pack.name.clone())),
        Line::raw(""),
        detail_line(
            "Region",
            pack.region.map_or_else(
                || "not from the catalog".to_string(),
                |region| region.label.clone(),
            ),
        ),
        detail_line(
            "Sources",
            if pack.datasets.is_empty() {
                "unknown".to_string()
            } else {
                pack.datasets
                    .iter()
                    .map(|id| regions::dataset_name(id))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        ),
        detail_line(
            "Data",
            pack.data_at.map_or_else(
                || "date unknown".to_string(),
                |at| format!("from {}", format::date(at)),
            ),
        ),
        detail_line("Records", format::thousands(pack.records)),
        detail_line("Built", built),
        detail_line("Path", pack.dir.display().to_string()),
        Line::raw(""),
    ];
    if let Some(at) = update {
        lines.push(Line::from(span("↑ Update available", ACCENT)));
        lines.push(Line::from(span(
            format!("  Newer data from {}", format::date(at)),
            DIM,
        )));
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
            app,
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

/// One line of the region list: the name, what is around it, the size of
/// its extract, and how long its build takes when this machine has built
/// before.
fn region_row(
    app: &App,
    row: &RegionRow,
    selected: bool,
    all: bool,
    inner: usize,
) -> Line<'static> {
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
    let time = app
        .estimate_for(region)
        .map(|estimate| format::about(estimate.secs))
        .unwrap_or_default();
    let right = format!("{:>8}  {:>9} ", format::bytes(region.osm.bytes), time);
    let opens = if all || region.children.is_empty() {
        String::new()
    } else {
        format!("  {} {} ›", region.children.len(), region.unit())
    };
    let disk = if app.home.extract(region).is_some() {
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
    let (newest, live_done, live_error) = {
        let live = app.live.lock().expect("live lock");
        (live.data_at, live.done, live.error.clone())
    };
    let host = app
        .region
        .osm
        .url
        .split('/')
        .nth(2)
        .unwrap_or_default()
        .to_string();
    let mut download = match (&app.on_disk, app.downloads()) {
        (Some(extract), false) => {
            let mut value = vec![
                plain("nothing"),
                span(format!("  using {}", extract.path.display()), DIM),
            ];
            if let Some(at) = extract.data_at {
                value.push(span(format!("  data from {}", format::date(at)), DIM));
            }
            value
        }
        _ => {
            let mut value = vec![
                plain(format::bytes(app.download_bytes())),
                span(format!("  from {host}"), DIM),
            ];
            match (newest, live_done) {
                (Some(at), _) => {
                    value.push(span(format!("  data from {}", format::date(at)), DIM));
                }
                (None, false) => value.push(span("  checking…", FAINT)),
                (None, true) => {}
            }
            value
        }
    };
    // An extract on disk with newer data published: say so, and how to take it.
    if let (Some(extract), Some(at)) = (&app.on_disk, newest)
        && !app.downloads()
        && extract.data_at.is_some_and(|own| at > own)
    {
        download.push(span(
            format!("  newer: {}  d download it", format::date(at)),
            ACCENT,
        ));
    }
    if app.downloads() && app.on_disk.is_some() {
        download.push(span("  replaces the one on disk  d keep it", DIM));
    }
    let estimate = app.estimate();
    let time = match &estimate {
        Some(estimate) => vec![
            plain(format::about(estimate.secs)),
            span(
                format!(
                    "   your {} build took {}",
                    region_name(&estimate.from.region),
                    format::duration(estimate.from.build_secs)
                ),
                FAINT,
            ),
        ],
        None => vec![span(
            "no estimate: no build of a similar size has finished on this machine",
            DIM,
        )],
    };
    let mut lines = answers(app, 2);
    lines.extend([
        Line::raw(""),
        Line::from(bold("   Ready to build")),
        Line::raw(""),
        field("Output", vec![plain(app.pack_dir().display().to_string())]),
    ]);
    let shortfalls = app.shortfalls();
    match &app.plan {
        Some(Ok(plan)) => lines.extend(plan_lines(plan)),
        Some(Err(error)) => lines.push(field(
            "Plan",
            vec![span(
                cut(error, area.width.saturating_sub(17) as usize),
                ERR,
            )],
        )),
        None => {}
    }
    lines.extend([field("Download", download), field("Time", time)]);
    if let Some(error) = live_error {
        lines.push(field(
            "",
            vec![span(cut(&format!("server: {error}"), 70), WARN)],
        ));
    }
    let room = area.width.saturating_sub(19) as usize;
    if let Some(Ok(plan)) = &app.plan {
        for warning in &plan.warnings {
            lines.push(field(
                "",
                vec![span(cut(&format!("! {warning}"), room), WARN)],
            ));
        }
    }
    for shortfall in &shortfalls {
        lines.push(field(
            "",
            vec![span(cut(&format!("✗ {shortfall}"), room), ERR)],
        ));
    }
    if !shortfalls.is_empty() {
        lines.push(field(
            "",
            vec![if app.force {
                span("forced: builds anyway, and may run out    f undo", WARN)
            } else {
                span("f build anyway", FAINT)
            }],
        ));
    }
    lines.extend([
        Line::raw(""),
        Line::from(vec![
            plain("  "),
            button("Start build", app.confirm_cursor == 0, app.can_build()),
            plain("  "),
            button("Back", app.confirm_cursor == 1, true),
        ]),
    ]);
    frame.render_widget(Paragraph::new(lines), area);
    let mut hints = vec![("←→", "choose")];
    if app.on_disk.is_some() {
        hints.push(("d", "download"));
    }
    if !shortfalls.is_empty() {
        hints.push(("f", "force"));
    }
    hints.extend([("enter", "confirm"), ("esc", "back")]);
    footer(frame, app, &hints);
}

/// What the build plan sizes and needs: the memory budget and threads it
/// chose from what the machine has, and the memory and disk the input
/// needs against what is free.
fn plan_lines(plan: &BuildPlan) -> Vec<Line<'static>> {
    let found = &plan.resources;
    let free = |free: Option<u64>| {
        free.map_or_else(
            || "free unknown".to_string(),
            |free| format!("{} free", format::bytes(free)),
        )
    };
    vec![
        field(
            "Memory",
            vec![
                plain(format!(
                    "{} budget",
                    format::bytes(plan.memory_budget_bytes)
                )),
                span(
                    format!(
                        "{} · {} threads · {} of {} available",
                        if plan.memory_budget_derived {
                            ", sized to this machine"
                        } else {
                            ""
                        },
                        plan.threads,
                        format::bytes(found.available_memory_bytes),
                        format::bytes(found.total_memory_bytes)
                    ),
                    DIM,
                ),
            ],
        ),
        field(
            "Needs",
            vec![
                plain(format!(
                    "about {} memory",
                    format::bytes(plan.memory_needed_bytes)
                )),
                span(
                    format!(
                        " · {} scratch, {} · {} Pack, {}",
                        format::bytes(plan.scratch_disk_needed_bytes),
                        free(plan.scratch_disk_free_bytes),
                        format::bytes(plan.output_disk_needed_bytes),
                        free(plan.output_disk_free_bytes)
                    ),
                    DIM,
                ),
            ],
        ),
    ]
}

/// A region's name from its id, for lines that name a past build.
fn region_name(id: &str) -> String {
    regions::tree()
        .get(id)
        .map_or_else(|| id.to_string(), |region| region.name.clone())
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
const SIZE_WIDTH: usize = 15;
const SPEED_WIDTH: usize = 11;
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

/// An amount of a step's work: `752 MB`, `48.2M ways`.
fn amount(unit: Unit, value: u64) -> String {
    match unit {
        Unit::Bytes => format::bytes(value),
        Unit::Count(_) => format::count(value),
        Unit::None => String::new(),
    }
}

fn finished_line(step: &Step, inner: usize) -> Line<'static> {
    let cells = bar_cells(inner);
    // History lines start at the terminal's edge; the live window sits one
    // column in, so the line carries that margin to stay in line with it.
    let mut spans = vec![
        plain("    "),
        Span::styled(
            pad(step.did, VERB_WIDTH),
            Style::new().fg(DONE_BAR).add_modifier(Modifier::BOLD),
        ),
        plain(" "),
        plain(pad(&cut(&step.object, NAME_WIDTH), NAME_WIDTH)),
        plain("  "),
    ];
    match (step.unit, step.total) {
        (Unit::None, _) | (_, None) => {
            // A step with no measure has no bar: only its time is known.
            spans.push(plain(
                " ".repeat(5 + cells + 2 + 2 + SIZE_WIDTH + SPEED_WIDTH),
            ));
        }
        (unit, Some(total)) => {
            let summary = match unit {
                Unit::Count(name) => format!("{} {name}", amount(unit, total)),
                _ => amount(unit, total),
            };
            spans.push(plain("100% "));
            spans.extend(bar_spans(cells, 1.0, Some(DONE_BAR)));
            spans.push(span(
                format!("  {:>SIZE_WIDTH$}", cut(&summary, SIZE_WIDTH)),
                DIM,
            ));
            spans.push(plain(" ".repeat(SPEED_WIDTH)));
        }
    }
    spans.push(span(
        format!(
            "{:>TIME_WIDTH$}",
            format::duration(step.secs.unwrap_or_default())
        ),
        DIM,
    ));
    Line::from(spans)
}

/// The running step: what it does, how far it is, and the rate it goes at,
/// all measured.
fn running_line(app: &App, step: &Step, done: u64, inner: usize) -> Line<'static> {
    let cells = bar_cells(inner);
    let elapsed = step.started.map_or(0.0, |at| at.elapsed().as_secs_f64());
    // The verb and what it works on shimmer as one title.
    let title = format!(
        "{} {}",
        pad(step.doing, VERB_WIDTH),
        pad(&cut(&step.object, NAME_WIDTH), NAME_WIDTH)
    );
    let mut spans = vec![plain(" "), span(spinner(app), ACCENT), plain(" ")];
    spans.extend(shimmer(&title, app.started.elapsed().as_millis()));
    spans.push(plain("  "));
    match (step.unit, step.total) {
        (unit, Some(total)) if unit != Unit::None && total > 0 => {
            let ratio = (done as f64 / total as f64).min(1.0);
            let rate = done as f64 / elapsed.max(0.001);
            let speed = if elapsed < 1.0 || done == 0 {
                String::new()
            } else {
                match unit {
                    Unit::Bytes => format!("{}/s", format::bytes(rate as u64)),
                    _ => format!("{}/s", format::count(rate as u64)),
                }
            };
            let left = if elapsed < 1.0 || done == 0 {
                String::new()
            } else {
                format::duration(total.saturating_sub(done) as f64 / rate)
            };
            spans.push(plain(format!("{:>3}% ", (ratio * 100.0).floor())));
            spans.extend(bar_spans(cells, ratio, None));
            spans.push(plain(format!(
                "  {:>SIZE_WIDTH$}",
                cut(
                    &format!("{}/{}", amount(unit, done), amount(unit, total)),
                    SIZE_WIDTH
                )
            )));
            spans.push(span(format!("{speed:>SPEED_WIDTH$}"), DIM));
            spans.push(span(format!("{left:>TIME_WIDTH$}"), DIM));
        }
        _ => {
            // No measure, so no percent and no bar: only time passing.
            spans.push(plain("     "));
            spans.push(span(pad("working…", cells + 2), DIM));
            spans.push(plain(" ".repeat(2 + SIZE_WIDTH + SPEED_WIDTH)));
            spans.push(span(
                format!("{:>TIME_WIDTH$}", format::duration(elapsed)),
                DIM,
            ));
        }
    }
    Line::from(spans)
}

fn build(frame: &mut Frame, area: Rect, app: &App) {
    let inner = area.width as usize;
    if let Some(error) = &app.build_error {
        let mut lines = vec![
            Line::from(vec![plain("   "), strong("Build failed", ERR)]),
            Line::raw(""),
        ];
        let room = inner.saturating_sub(6).max(20);
        for part in error.split(": ") {
            for chunk in part.chars().collect::<Vec<_>>().chunks(room) {
                lines.push(Line::from(span(
                    format!("   {}", chunk.iter().collect::<String>()),
                    DIM,
                )));
            }
        }
        frame.render_widget(Paragraph::new(lines), area);
        footer(frame, app, &[("esc", "back")]);
        return;
    }
    let Some(job) = &app.job else {
        return;
    };
    let progress = &job.progress;
    let current = progress.current();
    let done = progress.done();
    let steps = progress.steps.lock().expect("steps lock");
    let Some(step) = steps.get(current) else {
        return;
    };
    let mut status = format!(
        "   Step {} of {}  ·  {} elapsed",
        current + 1,
        steps.len(),
        format::duration(progress.started.elapsed().as_secs_f64()),
    );
    // What is left, from this machine's own builds, once the build runs.
    let build_started = steps
        .iter()
        .rev()
        .take(open_geocode::builder::BuildStage::ALL.len())
        .filter_map(|step| step.started)
        .min();
    if let (Some(estimate), Some(started)) = (app.estimate(), build_started) {
        let left = estimate.secs - started.elapsed().as_secs_f64();
        if left > 0.0 {
            status.push_str(&format!(
                "  ·  {} left by your {} build",
                format::about(left),
                region_name(&estimate.from.region)
            ));
        }
    }
    let mut lines = vec![
        running_line(app, step, done, inner),
        Line::raw(""),
        Line::from(span(status, DIM)),
    ];
    if app.show_logs {
        lines.push(Line::raw(""));
        lines.push(Line::from(span(format!("   {}", step.explain[0]), DIM)));
        lines.push(Line::from(span(format!("   {}", step.explain[1]), DIM)));
        lines.push(Line::raw(""));
        let mut facts = Vec::new();
        if let Some(plan) = progress.plan.lock().expect("plan lock").as_ref() {
            facts.push(format!(
                "memory budget {}",
                format::bytes(plan.memory_budget_bytes)
            ));
            facts.push(format!("{} threads", plan.threads));
        }
        if let Some(heap) = app.heap {
            facts.push(format!("og holds {}", format::bytes(heap)));
        }
        if let Some(free) = app.free_disk {
            facts.push(format!("{} disk free", format::bytes(free)));
        }
        lines.push(Line::from(span(format!("   {}", facts.join("  ·  ")), DIM)));
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
    let Some(built) = &app.built else {
        return;
    };
    let pack = &built.pack;
    let stat = |label: &str, value: u64| {
        vec![
            bold(format::thousands(value)),
            span(format!(" {label}    "), DIM),
        ]
    };
    let mut stats = vec![plain("   ")];
    stats.extend(stat(
        "places",
        pack.records_in(|layer| layer.place_layer().is_some()),
    ));
    stats.extend(stat(
        "streets",
        pack.records_in(|layer| layer == Layer::Street),
    ));
    stats.extend(stat(
        "addresses",
        pack.records_in(|layer| layer == Layer::Address),
    ));
    stats.extend(stat("POIs", pack.records_in(|layer| layer == Layer::Poi)));
    let step = |label: &str, command: String| {
        Line::from(vec![
            plain("   "),
            plain(pad(label, 17)),
            span(command, ACCENT),
        ])
    };
    let more =
        |text: String, color: Color| Line::from(vec![plain(" ".repeat(20)), span(text, color)]);
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
                    "   {} · {} records · built in {}",
                    format::bytes(pack.bytes),
                    format::thousands(pack.records),
                    format::duration(built.build_secs)
                ),
                DIM,
            ),
        ]),
        Line::from(span(format!("   {}", pack.dir.display()), FAINT)),
        Line::raw(""),
        Line::from(stats),
        Line::raw(""),
        Line::from(bold("   Next")),
        Line::raw(""),
        step("Serve the API", format!("og serve {}", app.region.slug())),
        more(
            format!("then GET http://{}/search?q=…", app::SERVE_ADDRESS),
            DIM,
        ),
        step("Build it again", app.command()),
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
        "address" | "interpolation" => VIOLET,
        "poi" => PINK,
        "street" => ACCENT,
        _ => OK,
    }
}

fn label_color(label: &str) -> Color {
    match label {
        "house_number" | "unit" => WARN,
        "street" => ACCENT,
        "locality" | "region" | "country" => OK,
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

    let records = app.engine.as_ref().map_or(0, |engine| engine.records());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            badge(),
            span(
                format!(
                    "  {} · {} records",
                    app.active_name,
                    format::thousands(records)
                ),
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
        Mode::Search => "an address, a street, a place or a POI",
        Mode::Reverse => "lat, lon",
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
                .filter(|row| row.hit().is_some())
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
        Mode::Reverse => match engine::point(&app.query) {
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
        Mode::Search => {
            let mut spans = vec![span("  parsed  ", DIM)];
            for (index, (text, label)) in app.reading.iter().enumerate() {
                if index > 0 {
                    spans.push(span("  ·  ", FAINT));
                }
                spans.push(bold(text.clone()));
                spans.push(span(format!(" {label}"), label_color(label)));
            }
            if app.reading.is_empty() {
                Line::raw("")
            } else {
                Line::from(spans)
            }
        }
    };
    frame.render_widget(Paragraph::new(parsed_line), parsed);
    let millis = match app.mode {
        Mode::Batch if !app.batch_rows.is_empty() => Some(app.batch_millis),
        Mode::Batch => None,
        _ => app.answer.as_ref().map(|answer| answer.millis),
    };
    // While the Pack is still answering, a spinner stands where its time
    // will be.
    let took = if app.asking() {
        Some(span(format!("{} ", spinner(app)), ACCENT))
    } else {
        millis.map(|millis| span(format!("{} ", milliseconds(millis)), DIM))
    };
    if let Some(took) = took {
        frame.render_widget(Paragraph::new(Line::from(took).right_aligned()), parsed);
    }

    let hits = app.hits();
    let lines: Vec<Line> = if app.mode == Mode::Batch && app.batch_candidate.is_some() {
        batch_card(app)
    } else if app.mode == Mode::Batch {
        batch_table(app, results)
    } else if let Some(error) = &app.query_error {
        vec![Line::from(span(format!("  {error}"), ERR))]
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
            .map(|(index, hit)| hit_row(hit, index == app.hit_cursor))
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

/// `0.8 ms`, `12 ms`.
fn milliseconds(millis: f64) -> String {
    if millis < 10.0 {
        format!("{millis:.1} ms")
    } else {
        format!("{millis:.0} ms")
    }
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
                "  A file path opens that file, to geocode all of it into a new CSV.",
                DIM,
            )),
        ];
    }
    let inner = area.width as usize;
    let address_width = 26.min(inner / 3);
    let match_width = inner.saturating_sub(3 + 4 + address_width + 2 + 2 + 2 + 6 + 1);
    let capacity = (area.height as usize).saturating_sub(1).max(1);
    let start = (app.hit_cursor + 1).saturating_sub(capacity);
    let mut lines = vec![Line::from(span(
        format!(
            "   {:>3}  {}  {}  {:>6}",
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
        match row.hit() {
            Some(hit) => {
                spans.push(span("✓ ", OK));
                spans.push(plain(pad(&cut(&hit.label, match_width), match_width)));
                spans.push(span(format!("  {}", measure(hit.measure)), DIM));
            }
            None => spans.push(span("– no match", DIM)),
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Search's score or reverse's distance, six cells wide.
fn measure(measure: Measure) -> String {
    match measure {
        Measure::Score(score) => format!("{score:>6.1}"),
        Measure::Distance(Some(metres)) if metres >= 1_000.0 => {
            format!("{:>4.1}km", metres / 1_000.0)
        }
        Measure::Distance(Some(metres)) => format!("{:>5.0}m", metres),
        Measure::Distance(None) => format!("{:>6}", "area"),
    }
}

fn hit_row(hit: &Hit, selected: bool) -> Line<'static> {
    Line::from(vec![
        if selected {
            span(" ❯ ", ACCENT)
        } else {
            plain("   ")
        },
        span(measure(hit.measure), DIM),
        plain("  "),
        span(pad(&hit.layer, 13), layer_color(&hit.layer)),
        if selected {
            bold(hit.label.clone())
        } else {
            plain(hit.label.clone())
        },
    ])
}

/// The answer as the API returns it, one styled line per JSON line.
pub(crate) fn json_lines(app: &App) -> Vec<Line<'static>> {
    app.json_text().lines().map(json_line).collect()
}

/// Colours one line of pretty JSON: keys, strings, numbers and punctuation.
fn json_line(line: &str) -> Line<'static> {
    let indent = line.len() - line.trim_start().len();
    let mut spans = vec![plain(" ".repeat(indent + 1))];
    let mut rest = line.trim_start();
    while !rest.is_empty() {
        if let Some(body) = rest.strip_prefix('"') {
            // A string runs to the next quote that is not escaped.
            let mut end = 0;
            let mut escaped = false;
            for (at, c) in body.char_indices() {
                match c {
                    '\\' if !escaped => escaped = true,
                    '"' if !escaped => {
                        end = at;
                        break;
                    }
                    _ => escaped = false,
                }
            }
            let text = format!("\"{}\"", &body[..end]);
            rest = &body[end + 1..];
            if let Some(after) = rest.strip_prefix(": ") {
                spans.push(span(text, ACCENT));
                spans.push(span(": ", FAINT));
                rest = after;
            } else {
                spans.push(span(text, OK));
            }
        } else {
            let end = rest
                .find([',', ']', '}', '[', '{', '"'])
                .unwrap_or(rest.len());
            if end == 0 {
                spans.push(span(rest[..1].to_string(), FAINT));
                rest = &rest[1..];
            } else {
                spans.push(span(rest[..end].trim().to_string(), WARN));
                rest = &rest[end..];
            }
        }
    }
    Line::from(spans)
}

fn json(frame: &mut Frame, area: Rect, app: &App) {
    let [header, panel_area, _] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(JSON_ROWS as u16 + 2),
        Constraint::Length(1),
    ])
    .areas(area);
    let batch = app.mode == Mode::Batch;
    let hits = app.hits();
    let (what, label) = if batch {
        let row = app.batch_rows.get(app.hit_cursor);
        (
            format!("  row {} of {}", app.hit_cursor + 1, app.batch_rows.len()),
            row.and_then(|row| row.hit())
                .map(|hit| format!(" · {}", hit.label))
                .unwrap_or_default(),
        )
    } else if app.json_all {
        (format!("  all {} results", hits.len()), String::new())
    } else {
        (
            format!("  result {} of {}", app.hit_cursor + 1, hits.len()),
            hits.get(app.hit_cursor)
                .map(|hit| format!(" · {}", hit.label))
                .unwrap_or_default(),
        )
    };
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
    let mut hints = vec![("↑↓", "scroll")];
    if !batch {
        hints.push((
            "a",
            if app.json_all {
                "one result"
            } else {
                "all results"
            },
        ));
    }
    hints.extend([("y", "copy"), ("esc", "back")]);
    footer(frame, app, &hints);
}

/// The line that opens a batch run in terminal history.
pub(crate) fn batch_header(app: &App) -> Vec<Line<'static>> {
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

/// Whether a column is in the address, as it reads above the column; the
/// selected column's mark is in brackets.
fn use_span(used: bool, selected: bool, cells: usize) -> Span<'static> {
    let mark = if used { "✓ address" } else { "· skip" };
    let label = if selected {
        format!("[{mark}]")
    } else {
        mark.to_string()
    };
    let text = pad(&label, cells);
    match (used, selected) {
        (false, true) => Span::styled(text, Style::new().fg(DIM).add_modifier(Modifier::BOLD)),
        (false, false) => span(text, FAINT),
        (true, true) => Span::styled(text, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
        (true, false) => span(text, ACCENT),
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
const LAST_WIDTH: usize = 8;

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
    hit: Option<(&str, f64, f64)>,
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
        Some((name, lat, lon)) => {
            spans.push(plain(pad(&cut(name, MATCH_WIDTH), MATCH_WIDTH)));
            spans.push(plain(format!("  {lat:>LAT_WIDTH$.5}")));
            spans.push(plain(format!("  {lon:>LON_WIDTH$.5}")));
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

/// `10,000 rows · 4.2 MB · header found`, the rows marked as a count so
/// far until the whole file is counted.
fn file_facts(file: &batch::BatchFile) -> String {
    let rows = if file.counted() {
        format!("{} rows", format::thousands(file.rows()))
    } else {
        format!("counting rows… {}", format::thousands(file.rows()))
    };
    format!(
        "{rows} · {} · {}",
        format::bytes(file.bytes),
        if file.header_found {
            "header found"
        } else {
            "no header row"
        },
    )
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
    let sample = file.sample();

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
            (width(header).max(cells).max(11) + 2).clamp(13, 20)
        })
        .collect();
    let window = visible_columns(&widths, session.column, inner.saturating_sub(5));
    let more = window.end < widths.len();
    let mut roles = vec![plain("   ")];
    let mut headers = vec![plain("   ")];
    let mut rows: Vec<Vec<Span<'static>>> = sample.iter().map(|_| vec![plain("   ")]).collect();
    for column in window {
        let cells = widths[column];
        roles.push(use_span(
            session.used[column],
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
    let preview = session.previews.get(&session.used);
    for (index, row) in sample.iter().enumerate() {
        let Some(hit) = preview.map(|hits| hits.get(index).cloned().flatten()) else {
            lines.push(Line::from(span(
                format!("   {}", cut(&session.query(row), inner.saturating_sub(4))),
                FAINT,
            )));
            continue;
        };
        let last = match &hit {
            Some(hit) => span(format!("{:>LAST_WIDTH$}", measure(hit.measure)), DIM),
            None => span(format!("{:>LAST_WIDTH$}", "–"), DIM),
        };
        lines.push(result_line(
            inner,
            &session.query(row),
            hit.as_ref()
                .map(|hit| (hit.name.as_str(), hit.lat, hit.lon)),
            last,
        ));
    }
    lines.push(Line::raw(""));
    let note = match &app.batch_error {
        Some(error) => span(format!("   {}", cut(error, inner.saturating_sub(4))), ERR),
        None => span(
            format!(
                "   Writes {} next to the file, with coordinates added.",
                session
                    .output
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            ),
            DIM,
        ),
    };
    lines.push(Line::from(note));
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
            ("space", "use or skip"),
            ("enter", "start"),
            ("esc", "back"),
        ],
    );
}

fn batch_run(frame: &mut Frame, area: Rect, app: &App) {
    let Some(session) = &app.batch else {
        return;
    };
    let Some(run) = &session.run else {
        return;
    };
    let inner = area.width as usize;
    let stats = &run.stats;
    let (done, matched) = stats.counts();
    let step = Step {
        did: "Geocoded",
        doing: "Geocoding",
        object: session.file.stem().to_string(),
        explain: ["", ""],
        unit: Unit::Count("rows"),
        total: session.file.counted().then(|| session.file.rows()),
        started: Some(run.started),
        secs: None,
    };
    let latency = |q: f64| stats.latencies.quantile(q);
    let left = format!(
        "   p50 {:.1} ms  ·  p95 {:.1} ms  ·  p99 {:.1} ms",
        latency(0.50),
        latency(0.95),
        latency(0.99)
    );
    let share = if done > 0 {
        matched as f64 / done as f64 * 100.0
    } else {
        0.0
    };
    let right = format!(
        "{share:.1}% matched  ·  {} with no match",
        format::thousands(done.saturating_sub(matched))
    );
    let mut lines = vec![
        running_line(app, &step, done, inner),
        Line::raw(""),
        Line::from(vec![
            span(pad(&left, inner.saturating_sub(width(&right))), DIM),
            span(right, DIM),
        ]),
        Line::raw(""),
        result_header(inner, "Latest", "time"),
    ];
    for latest in stats.latest.lock().expect("latest lock").iter() {
        let color = if latest.millis > 3.0 { ACCENT } else { DIM };
        lines.push(result_line(
            inner,
            &latest.query,
            latest
                .hit
                .as_ref()
                .map(|(name, lat, lon)| (name.as_str(), *lat, *lon)),
            span(
                format!("{:>LAST_WIDTH$}", milliseconds(latest.millis)),
                color,
            ),
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
    footer(frame, app, &[("esc", "cancel")]);
}

/// The first rows of the output file as it was written.
fn output_table(head: &[Vec<String>], inner: usize) -> Vec<Line<'static>> {
    let Some((header, rows)) = head.split_first() else {
        return Vec::new();
    };
    let widths: Vec<usize> = header
        .iter()
        .enumerate()
        .map(|(column, name)| {
            let cells = rows
                .iter()
                .map(|row| row.get(column).map_or(0, |cell| width(cell)))
                .max()
                .unwrap_or(0);
            (width(name).max(cells) + 2).clamp(6, 18)
        })
        .collect();
    // The lat and lon the run added are the last two columns: keep them in
    // view and drop columns before them that do not fit.
    let tail = widths.len().saturating_sub(2);
    let mut avail = inner.saturating_sub(3 + widths[tail..].iter().sum::<usize>());
    let mut shown = Vec::new();
    for (column, cells) in widths.iter().enumerate().take(tail) {
        if *cells > avail {
            break;
        }
        avail -= cells;
        shown.push(column);
    }
    shown.extend(tail..widths.len());
    let line = |row: &[String], style: fn(String) -> Span<'static>, coordinate: Style| {
        let mut spans = vec![plain("   ")];
        for &column in &shown {
            let cell = row.get(column).map_or("", String::as_str);
            let text = pad(&cut(cell, widths[column] - 2), widths[column]);
            spans.push(if column >= tail {
                Span::styled(text, coordinate)
            } else {
                style(text)
            });
        }
        Line::from(spans)
    };
    let mut lines = vec![line(
        header,
        |text| span(text, DIM),
        Style::new().fg(ACCENT),
    )];
    for row in rows {
        lines.push(line(row, plain, Style::new()));
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
    let rows = summary.rows;
    let missed = rows.saturating_sub(summary.matched);
    let percent = |count: u64| count as f64 / rows.max(1) as f64 * 100.0;

    let mut accuracy = vec![plain("   ")];
    for (name, count) in &summary.by_accuracy {
        accuracy.push(span(format!("{name} "), DIM));
        accuracy.push(bold(format::thousands(*count)));
        accuracy.push(plain("    "));
    }
    let output = summary
        .output
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let size = format!(
        "{} rows · {}",
        format::thousands(rows),
        format::bytes(summary.output_bytes)
    );
    let mut lines = vec![
        Line::raw(""),
        Line::from(vec![
            plain("   "),
            bold(format::thousands(summary.matched)),
            span(
                format!(" matched  ·  {:.1}%", percent(summary.matched)),
                DIM,
            ),
            plain("          "),
            bold(format::thousands(missed)),
            span(format!(" with no match  ·  {:.1}%", percent(missed)), DIM),
        ]),
        Line::from(accuracy),
        Line::from(span(
            format!(
                "   p50 {:.1} ms  ·  p95 {:.1} ms  ·  p99 {:.1} ms  ·  slowest {:.1} ms  ·  {} in all",
                summary.p50,
                summary.p95,
                summary.p99,
                summary.slowest,
                format::duration(summary.seconds)
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
    lines.extend(output_table(&summary.output_head, inner));
    lines.push(Line::raw(""));
    let mut buttons = vec![plain("  ")];
    for (index, label) in ["Search in the shell", "Done"].iter().enumerate() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn colours_json_without_losing_text() {
        for line in [
            r#"  "label": "100 Queen Street West, Toronto","#,
            r#"  "coordinates": ["#,
            r#"    -79.3839347,"#,
            r#"  "escaped": "a \"quoted\" word","#,
            r#"}"#,
        ] {
            assert_eq!(text(&json_line(line)).trim(), line.trim(), "{line}");
        }
    }
}
