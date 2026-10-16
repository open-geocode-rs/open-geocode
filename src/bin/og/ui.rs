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
    mock::{self, Record},
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
const BAR_OVERALL: [(u8, u8, u8); 2] = [(96, 96, 120), (160, 160, 184)];

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
    let mut spans = vec![span("  ✓ ", OK), span(pad(label, 11), DIM)];
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
    answers(app, 3)
}

pub fn stage_done(build: &Build, index: usize) -> Line<'static> {
    let stage = &build.stages[index];
    let title = match build.number(index) {
        Some(number) => format!("{number} {}", stage.title),
        None => stage.title.clone(),
    };
    Line::from(vec![
        span("  ✓ ", OK),
        plain(pad(&title, 36)),
        span(format!("{:>7}", mock::duration(stage.real_secs)), DIM),
        plain("   "),
        span(stage.summary.clone(), DIM),
    ])
}

pub fn cancelled(build: &Build) -> Line<'static> {
    Line::from(vec![
        span("  ✗ ", ERR),
        plain(format!("Build cancelled during {}.", build.stage().title)),
        span(" Downloaded files are kept.", DIM),
    ])
}

pub fn done_summary(app: &App) -> Vec<Line<'static>> {
    vec![
        Line::raw(""),
        Line::from(vec![
            span("  ● ", OK),
            bold("Pack ready"),
            plain(format!("  {}", app.pack_path())),
            span(
                format!(
                    "  {}  built in {}",
                    mock::PACK.size,
                    mock::duration(app.built_secs)
                ),
                DIM,
            ),
        ]),
        Line::from(vec![
            span("    rebuild with  ", DIM),
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
    // The last row belongs to the key hints.
    let area = Rect {
        x: full.x + 1,
        width: full.width.saturating_sub(2).min(96),
        height: full.height.saturating_sub(1),
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
    let area = Rect {
        x: full.x + 1,
        y: full.y + full.height.saturating_sub(1),
        width: full.width.saturating_sub(2).min(96),
        height: 1.min(full.height),
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn home(frame: &mut Frame, area: Rect, app: &App) {
    let [_, menu, packs, _, _] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(6),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let lines: Vec<Line> = app::HOME_ITEMS
        .iter()
        .enumerate()
        .map(|(index, (label, detail))| {
            if index == app.home_cursor {
                Line::from(vec![
                    span(" ❯ ", ACCENT),
                    strong(pad(label, 18), ACCENT),
                    span(detail.to_string(), DIM),
                ])
            } else {
                Line::from(vec![
                    plain("   "),
                    plain(pad(label, 18)),
                    span(detail.to_string(), DIM),
                ])
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(panel("What next?")), menu);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            span("  Packs found  ", DIM),
            plain("./packs/ontario"),
            span("  1.3 GB · built 2 days ago", DIM),
        ])),
        packs,
    );
    footer(
        frame,
        app,
        &[("↑↓", "move"), ("enter", "select"), ("q", "quit")],
    );
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
    let mut lines = vec![
        input_line(
            &app.region_query,
            &format!("type to search {} regions", tree.searchable().count()),
        ),
        Line::raw(""),
    ];
    if rows.is_empty() {
        lines.push(Line::from(span(
            format!("  No region matches \"{}\".", app.region_query.trim()),
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
                format!("  {}", "─".repeat(inner.saturating_sub(4))),
                FAINT,
            )));
            room -= 1;
        }
        next += 1;
    }

    let mut trail = vec!["Region".to_string()];
    if browsing {
        trail.extend(
            app.region_path
                .iter()
                .map(|index| tree.at(*index).name.clone()),
        );
    } else {
        trail.push("search".to_string());
    }
    let more = rows.len().saturating_sub(next);
    let mut block = panel(&trail.join(" › "));
    if more > 0 {
        block =
            block.title_bottom(Line::from(span(format!(" ↓ {more} more "), DIM)).left_aligned());
    }
    if !rows.is_empty() {
        block = block.title_bottom(
            Line::from(span(
                format!(" {} of {} ", app.region_cursor + 1, rows.len()),
                DIM,
            ))
            .right_aligned(),
        );
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
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
    let [done, list, _, _] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(5),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(answers(app, 1)), done);
    let inner = list.width.saturating_sub(2) as usize;
    let lines: Vec<Line> = mock::SOURCES
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let selected = index == app.source_cursor;
            let on = app.sources[index];
            let mark = match (index, on) {
                (0, _) => span("● ", DIM),
                (_, true) => span("● ", ACCENT),
                (_, false) => span("○ ", DIM),
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
            let used = 5 + 18 + width(source.detail);
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
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(panel("Sources")), list);
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
    let mut spans = vec![span(format!("  {}", pad(label, 14)), DIM)];
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
    let [done, panel_area, _, _] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(11),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(answers(app, 2)), done);
    let download = match (app.download_mb(), app.region.local()) {
        (0.0, Some(path)) => vec![plain("nothing"), span(format!("  using {path}"), DIM)],
        (mb, _) => vec![
            plain(mock::size(mb)),
            span("  from download.geofabrik.de", DIM),
        ],
    };
    let disk_mark = if app.fits_disk() {
        span("  ✓", OK)
    } else {
        span("  ✗ not enough", ERR)
    };
    let lines = vec![
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
        field(
            "Disk needed",
            vec![
                plain(format!("~ {:.1} GB", app.disk_gb())),
                span(format!("   free {:.0} GB", mock::FREE_DISK_GB), DIM),
                disk_mark,
            ],
        ),
        field("Time", vec![plain(mock::about(app.build_secs()))]),
        Line::raw(""),
        Line::from(vec![
            plain("  "),
            button("Start build", app.confirm_cursor == 0, app.fits_disk()),
            plain("  "),
            button("Cancel", app.confirm_cursor == 1, true),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(panel("Ready to build")),
        panel_area,
    );
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

/// A thin bar that fills left to right; its colour runs along a gradient.
fn bar(cells: usize, ratio: f64, ends: [(u8, u8, u8); 2]) -> Vec<Span<'static>> {
    let filled = ratio.clamp(0.0, 1.0) * cells as f64;
    let full = filled.floor() as usize;
    let half = full < cells && filled - full as f64 >= 0.5;
    let mut spans = Vec::with_capacity(cells);
    for index in 0..cells {
        let color = lerp(ends, index as f64 / cells.max(1) as f64);
        if index < full {
            spans.push(span("━", color));
        } else if index == full && half {
            spans.push(span("╸", color));
        } else {
            spans.push(span("━", FAINT));
        }
    }
    spans
}

/// A band that sweeps back and forth, for stages with no total.
fn sweep(cells: usize, app: &App) -> Vec<Span<'static>> {
    let band = 10.min(cells);
    let travel = (cells - band).max(1);
    let step = (app.started.elapsed().as_millis() / 40) as usize % (travel * 2);
    let start = if step < travel {
        step
    } else {
        travel * 2 - step
    };
    (0..cells)
        .map(|index| {
            if (start..start + band).contains(&index) {
                span("━", lerp(BAR, (index - start) as f64 / band as f64))
            } else {
                span("━", FAINT)
            }
        })
        .collect()
}

fn build(frame: &mut Frame, area: Rect, app: &App) {
    let Some(build) = &app.build else {
        return;
    };
    let box_rows = if app.show_logs { 17 } else { 12 };
    let [panel_area, _, _] = Layout::vertical([
        Constraint::Length(box_rows),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let stage = build.stage();
    let progress = build.progress();
    let inner = panel_area.width.saturating_sub(2) as usize;

    let mut title_line = vec![plain("  "), span(spinner(app), ACCENT), plain(" ")];
    title_line.push(bold(stage.title.clone()));
    let mut lines = vec![
        Line::raw(""),
        Line::from(title_line),
        Line::from(vec![plain(format!("    {}", stage.explain[0]))]),
        Line::from(vec![span(format!("    {}", stage.explain[1]), DIM)]),
        Line::raw(""),
    ];

    let mut stage_bar = vec![plain("    ")];
    match stage.total {
        Some(total) => {
            let right = format!(
                "  {:>3}%   {} / {} {}",
                (progress * 100.0).floor(),
                mock::count(total * progress),
                mock::count(total),
                stage.unit
            );
            let cells = inner.saturating_sub(4 + width(&right) + 2).max(10);
            stage_bar.extend(bar(cells, progress, BAR));
            stage_bar.push(plain(right));
        }
        None => {
            let right = "  working".to_string();
            let cells = inner.saturating_sub(4 + 30).max(10);
            stage_bar.extend(sweep(cells, app));
            stage_bar.push(span(right, DIM));
        }
    }
    lines.push(Line::from(stage_bar));
    lines.push(Line::raw(""));

    let overall = build.overall();
    let right = format!(
        "  {:>3}%   {} left",
        (overall * 100.0).floor(),
        mock::about(build.remaining())
    );
    let cells = inner.saturating_sub(4 + 9 + width(&right) + 2).max(10);
    let mut overall_line = vec![span("    overall  ", DIM)];
    overall_line.extend(bar(cells, overall, BAR_OVERALL));
    overall_line.push(span(right, DIM));
    lines.push(Line::from(overall_line));

    let stats = if stage.unit == "MB" {
        vec![
            span("    speed ", DIM),
            plain(format!(
                "{:.1} MB/s",
                stage.total.unwrap_or(0.0) / stage.real_secs
            )),
            span("    from ", DIM),
            plain("download.geofabrik.de"),
        ]
    } else {
        let budget = app.memory_gb() as f64;
        let memory = (1.4 + overall * 4.0).min(budget * 0.85);
        let spill = (overall * 2.2 - 0.6).max(0.0);
        let mut stats = vec![
            span("    memory ", DIM),
            plain(format!("{memory:.1} / {budget:.1} GB")),
            span("    spill ", DIM),
            plain(format!("{spill:.1} GB")),
        ];
        if let Some(total) = stage.total {
            stats.push(span("    rate ", DIM));
            stats.push(plain(format!(
                "{} {}/s",
                mock::count(total / stage.real_secs),
                stage.unit
            )));
        }
        stats
    };
    lines.push(Line::from(stats));

    if app.show_logs {
        lines.push(Line::raw(""));
        lines.push(Line::from(span(
            format!("    log {}", "─".repeat(inner.saturating_sub(12))),
            FAINT,
        )));
        let end = ((overall * mock::LOGS.len() as f64).ceil() as usize).clamp(1, mock::LOGS.len());
        let start = end.saturating_sub(3);
        for (level, text) in &mock::LOGS[start..end] {
            let color = if *level == "warn" { WARN } else { DIM };
            lines.push(Line::from(vec![
                span(format!("    {level:<5} "), color),
                span(text.to_string(), DIM),
            ]));
        }
    }

    let position = build
        .number(build.current)
        .unwrap_or_else(|| "download".to_string());
    let corner = Line::from(span(
        format!(" {position} · {} ", mock::duration(build.elapsed())),
        DIM,
    ))
    .right_aligned();
    let block = panel(&format!("Building {}", app.region.label)).title_top(corner);
    frame.render_widget(Paragraph::new(lines).block(block), panel_area);
    let logs = if app.show_logs { "hide logs" } else { "logs" };
    footer(
        frame,
        app,
        &[("l", logs), ("esc", "cancel, keeps downloads")],
    );
}

fn done(frame: &mut Frame, area: Rect, app: &App) {
    let [body, _, _] = Layout::vertical([
        Constraint::Length(13),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let stat =
        |label: &str, value: &str| vec![bold(value.to_string()), span(format!(" {label}   "), DIM)];
    let mut stats = vec![plain("    ")];
    stats.extend(stat("places", mock::PACK.places));
    stats.extend(stat("streets", mock::PACK.streets));
    stats.extend(stat("addresses", mock::PACK.addresses));
    stats.extend(stat("POIs", mock::PACK.pois));
    let step = |number: &str, label: &str, command: String| {
        Line::from(vec![
            span(format!("    {number}  "), DIM),
            plain(pad(label, 18)),
            span(command, ACCENT),
        ])
    };
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
            span("  ● ", OK),
            bold("Pack ready"),
            plain(format!("   {}", app.pack_path())),
            span(
                format!(
                    "   {}   built in {}",
                    mock::PACK.size,
                    mock::duration(app.built_secs)
                ),
                DIM,
            ),
        ]),
        Line::raw(""),
        Line::from(stats),
        Line::raw(""),
        Line::from(bold("  Next steps")),
        step("1", "Try it now", format!("og query {}", app.pack_path())),
        step(
            "2",
            "Serve the API",
            format!("og serve --pack {}", app.pack_path()),
        ),
        Line::from(vec![
            plain(format!("       {}", " ".repeat(18))),
            span("then GET http://127.0.0.1:8080/search?q=…", DIM),
        ]),
        step("3", "Same build again", format!("{head} \\")),
        Line::from(vec![
            plain(format!("       {}", " ".repeat(18))),
            span(format!("  {}", rest.trim_start()), ACCENT),
        ]),
        Line::raw(""),
        Line::from(buttons),
    ];
    frame.render_widget(Paragraph::new(lines), body);
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
        Constraint::Length(6),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            badge(),
            span(
                format!("  {} · {} records", app.region.slug(), mock::PACK.records),
                DIM,
            ),
        ])),
        header,
    );
    let mut tabs = Vec::new();
    for mode in [Mode::Search, Mode::Autocomplete, Mode::Reverse] {
        tabs.push(if mode == app.mode {
            Span::styled(
                format!(" {} ", mode.name()),
                Style::new()
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )
        } else {
            span(format!(" {} ", mode.name()), FAINT)
        });
    }
    frame.render_widget(Paragraph::new(Line::from(tabs).right_aligned()), header);

    let placeholder = match app.mode {
        Mode::Search => "try: 100 queen st w toronto",
        Mode::Autocomplete => "type a few letters: cn tow",
        Mode::Reverse => "lat, lon   try: 43.6535, -79.3839",
    };
    frame.render_widget(
        Paragraph::new(input_line(&app.query, placeholder)).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(FAINT)),
        ),
        input,
    );

    let parsed_line = match app.mode {
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
    if !app.query.is_empty() {
        let millis = match app.mode {
            Mode::Autocomplete => 1 + app.query.len() % 3,
            _ => 3 + app.query.len() % 5,
        };
        frame.render_widget(
            Paragraph::new(Line::from(span(format!("{millis} ms "), DIM)).right_aligned()),
            parsed,
        );
    }

    let hits = app.hits();
    let lines: Vec<Line> = if app.query.is_empty() {
        vec![
            Line::from(span(
                "  Results follow each key you type. Enter shows the picked result as GeoJSON.",
                DIM,
            )),
            Line::from(span(
                "  Tab switches between search, autocomplete and reverse.",
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

    let esc = if app.query.is_empty() {
        "quit"
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
    let picked: Vec<(usize, &(&Record, f64))> = if app.json_all {
        hits.iter().enumerate().collect()
    } else {
        hits.iter()
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
                    .map(|(row, (record, value))| feature(record, *value, app.mode, row))
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
    let what = if app.json_all {
        format!("  all {} results", hits.len())
    } else {
        format!("  result {} of {}", app.hit_cursor + 1, hits.len())
    };
    let label = hits
        .get(app.hit_cursor)
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
