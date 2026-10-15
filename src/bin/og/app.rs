//! Screen state and key handling. Every action ends in canned data from
//! `mock`; nothing is built, downloaded or searched for real.

use std::time::{Duration, Instant};

use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    text::Line,
};

use crate::{
    mock::{self, Region, Stage},
    ui,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Region,
    Sources,
    Confirm,
    Build,
    Done,
    Query,
    Json,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Search,
    Autocomplete,
    Reverse,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Autocomplete => "autocomplete",
            Self::Reverse => "reverse",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Search => Self::Autocomplete,
            Self::Autocomplete => Self::Reverse,
            Self::Reverse => Self::Search,
        }
    }
}

pub const HOME_ITEMS: [(&str, &str); 4] = [
    ("Build a Pack", "download a region and index it"),
    ("Query a Pack", "try searches and see the GeoJSON"),
    ("Serve a Pack", "start the HTTP API"),
    ("Docs", "open the guide in your browser"),
];

pub const DONE_ITEMS: [&str; 3] = ["Open query shell", "Serve now", "Quit"];
pub const MEMORY_STEPS_GB: [u32; 5] = [2, 4, 6, 8, 12];
/// Rows of the region list, so the box keeps one height while filtering.
pub const REGION_ROWS: usize = 7;

pub struct Build {
    pub stages: Vec<Stage>,
    pub current: usize,
    pub stage_started: Instant,
}

impl Build {
    pub fn stage(&self) -> &Stage {
        &self.stages[self.current]
    }

    /// The current stage's progress, 0 to 1.
    pub fn progress(&self) -> f64 {
        (self.stage_started.elapsed().as_secs_f64() / self.stage().play_secs).min(1.0)
    }

    pub fn overall(&self) -> f64 {
        let total: f64 = self.stages.iter().map(|stage| stage.play_secs).sum();
        let done: f64 = self.stages[..self.current]
            .iter()
            .map(|stage| stage.play_secs)
            .sum();
        (done + self.progress() * self.stage().play_secs) / total
    }

    /// Elapsed time as a real build would show it.
    pub fn elapsed(&self) -> f64 {
        let done: f64 = self.stages[..self.current]
            .iter()
            .map(|stage| stage.real_secs)
            .sum();
        done + self.progress() * self.stage().real_secs
    }

    pub fn remaining(&self) -> f64 {
        let total: f64 = self.stages.iter().map(|stage| stage.real_secs).sum();
        total - self.elapsed()
    }

    /// `3/8` for builder stages; downloads carry no number.
    pub fn number(&self, index: usize) -> Option<String> {
        let stage = &self.stages[index];
        if !stage.numbered {
            return None;
        }
        let total = self.stages.iter().filter(|stage| stage.numbered).count();
        let position = self.stages[..=index]
            .iter()
            .filter(|stage| stage.numbered)
            .count();
        Some(format!("{position}/{total}"))
    }
}

pub struct App {
    pub screen: Screen,
    pub quit: bool,
    /// Lines to print above the live area: they stay in terminal history.
    pub scrollback: Vec<Line<'static>>,
    /// Plain lines printed after the interface closes.
    pub farewell: Vec<String>,
    pub flash: Option<(String, Instant)>,
    pub started: Instant,
    pub home_cursor: usize,
    pub region_query: String,
    pub region_cursor: usize,
    pub region: &'static Region,
    pub sources: [bool; 3],
    pub source_cursor: usize,
    pub memory_step: usize,
    pub confirm_cursor: usize,
    pub build: Option<Build>,
    pub built_secs: f64,
    pub show_logs: bool,
    pub done_cursor: usize,
    pub query: String,
    pub mode: Mode,
    pub hit_cursor: usize,
    pub json_scroll: usize,
    pub json_all: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            screen: Screen::Home,
            quit: false,
            scrollback: ui::logo(),
            farewell: Vec::new(),
            flash: None,
            started: Instant::now(),
            home_cursor: 0,
            region_query: String::new(),
            region_cursor: 0,
            region: &mock::REGIONS[1],
            sources: [true, false, false],
            source_cursor: 1,
            memory_step: 2,
            confirm_cursor: 0,
            build: None,
            built_secs: 0.0,
            show_logs: false,
            done_cursor: 0,
            query: String::new(),
            mode: Mode::Search,
            hit_cursor: 0,
            json_scroll: 0,
            json_all: false,
        }
    }

    pub fn memory_gb(&self) -> u32 {
        MEMORY_STEPS_GB[self.memory_step]
    }

    pub fn pack_path(&self) -> String {
        format!("./packs/{}", self.region.slug())
    }

    pub fn download_mb(&self) -> f64 {
        let region = if self.region.local.is_some() {
            0.0
        } else {
            self.region.size_mb
        };
        let extra: f64 = mock::SOURCES
            .iter()
            .zip(self.sources)
            .filter(|(_, on)| *on)
            .map(|(source, _)| source.download_mb)
            .sum();
        region + extra
    }

    /// Disk the build needs: the downloads plus scratch and the Pack.
    pub fn disk_gb(&self) -> f64 {
        (self.region.size_mb * 5.5 + self.download_mb()) / 1_000.0
    }

    pub fn fits_disk(&self) -> bool {
        self.disk_gb() <= mock::FREE_DISK_GB
    }

    pub fn build_secs(&self) -> f64 {
        let extra =
            if self.sources[1] { 60.0 } else { 0.0 } + if self.sources[2] { 90.0 } else { 0.0 };
        self.region.build_secs() + extra
    }

    /// The equivalent non-interactive command for this build.
    pub fn command(&self) -> String {
        let mut command = format!("og build --region {}", self.region.arg());
        for (source, on) in mock::SOURCES.iter().zip(self.sources).skip(1) {
            if on {
                command.push_str(&format!(" --with {}", source.arg));
            }
        }
        command.push_str(&format!(" --memory {}G", self.memory_gb()));
        command
    }

    /// Regions matching the search, with the matched character positions.
    /// The world is not among them: it stays pinned below the list.
    pub fn region_matches(&self) -> Vec<(&'static Region, Vec<usize>)> {
        let mut matches: Vec<(&'static Region, Vec<usize>, i64)> = mock::REGIONS
            .iter()
            .filter_map(|region| {
                let (positions, score) = fuzzy(&self.region_query, region.name)?;
                Some((region, positions, score))
            })
            .collect();
        matches.sort_by_key(|(_, _, score)| std::cmp::Reverse(*score));
        matches
            .into_iter()
            .map(|(region, positions, _)| (region, positions))
            .collect()
    }

    pub fn hits(&self) -> Vec<(&'static mock::Record, f64)> {
        match self.mode {
            Mode::Search | Mode::Autocomplete => mock::search(&self.query),
            Mode::Reverse => mock::point(&self.query)
                .map(|(lat, lon)| mock::reverse(lat, lon))
                .unwrap_or_default(),
        }
    }

    pub fn flash_text(&self) -> Option<&str> {
        self.flash
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(3))
            .map(|(text, _)| text.as_str())
    }

    fn flash(&mut self, text: impl Into<String>) {
        self.flash = Some((text.into(), Instant::now()));
    }

    pub fn tick(&mut self) {
        let Some(build) = &mut self.build else {
            return;
        };
        if build.progress() < 1.0 {
            return;
        }
        let line = ui::stage_done(build, build.current);
        self.scrollback.push(line);
        if build.current + 1 < build.stages.len() {
            build.current += 1;
            build.stage_started = Instant::now();
        } else {
            self.built_secs = build.elapsed();
            self.build = None;
            self.screen = Screen::Done;
            self.done_cursor = 0;
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.farewell.push("Cancelled.".to_string());
            self.quit = true;
            return;
        }
        match self.screen {
            Screen::Home => self.home_key(key),
            Screen::Region => self.region_key(key),
            Screen::Sources => self.sources_key(key),
            Screen::Confirm => self.confirm_key(key),
            Screen::Build => self.build_key(key),
            Screen::Done => self.done_key(key),
            Screen::Query => self.query_key(key),
            Screen::Json => self.json_key(key),
        }
    }

    fn home_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.home_cursor = (self.home_cursor + HOME_ITEMS.len() - 1) % HOME_ITEMS.len();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.home_cursor = (self.home_cursor + 1) % HOME_ITEMS.len();
            }
            KeyCode::Enter => match self.home_cursor {
                0 => self.screen = Screen::Region,
                1 => self.open_query(),
                2 => self.flash("Serving is not wired in this preview."),
                _ => self.flash("Docs are not wired in this preview."),
            },
            KeyCode::Esc | KeyCode::Char('q') => self.quit = true,
            _ => {}
        }
    }

    fn region_key(&mut self, key: KeyEvent) {
        let rows = self.region_matches().len() + 1;
        match key.code {
            KeyCode::Up => self.region_cursor = (self.region_cursor + rows - 1) % rows,
            KeyCode::Down => self.region_cursor = (self.region_cursor + 1) % rows,
            KeyCode::Enter => {
                let matches = self.region_matches();
                self.region = matches
                    .get(self.region_cursor)
                    .map_or(&mock::WORLD, |(region, _)| *region);
                self.screen = Screen::Sources;
            }
            KeyCode::Esc => {
                if self.region_query.is_empty() {
                    self.screen = Screen::Home;
                } else {
                    self.region_query.clear();
                    self.region_cursor = 0;
                }
            }
            KeyCode::Backspace => {
                self.region_query.pop();
                self.region_cursor = 0;
            }
            KeyCode::Char(c) => {
                self.region_query.push(c);
                self.region_cursor = 0;
            }
            _ => {}
        }
    }

    fn sources_key(&mut self, key: KeyEvent) {
        let rows = mock::SOURCES.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.source_cursor = (self.source_cursor + rows - 1) % rows;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.source_cursor = (self.source_cursor + 1) % rows;
            }
            KeyCode::Char(' ') => {
                if self.source_cursor == 0 {
                    self.flash("OpenStreetMap is the base of every Pack.");
                } else {
                    self.sources[self.source_cursor] ^= true;
                }
            }
            KeyCode::Enter => {
                self.confirm_cursor = 0;
                self.screen = Screen::Confirm;
            }
            KeyCode::Esc => self.screen = Screen::Region,
            _ => {}
        }
    }

    fn confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => self.confirm_cursor ^= 1,
            KeyCode::Char('m') => self.memory_step = (self.memory_step + 1) % MEMORY_STEPS_GB.len(),
            KeyCode::Enter if self.confirm_cursor == 1 => self.screen = Screen::Home,
            KeyCode::Enter if !self.fits_disk() => self.flash(format!(
                "Not enough free disk: {} needs about {:.0} GB. Pick a smaller region.",
                self.region.name,
                self.disk_gb()
            )),
            KeyCode::Enter => self.start_build(),
            KeyCode::Esc => self.screen = Screen::Sources,
            _ => {}
        }
    }

    fn start_build(&mut self) {
        self.scrollback.extend(ui::choices(self));
        self.build = Some(Build {
            stages: mock::stages(self.region, &self.sources),
            current: 0,
            stage_started: Instant::now(),
        });
        self.show_logs = false;
        self.screen = Screen::Build;
    }

    fn build_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('l') => self.show_logs ^= true,
            KeyCode::Esc => {
                if let Some(build) = self.build.take() {
                    self.scrollback.push(ui::cancelled(&build));
                }
                self.screen = Screen::Confirm;
            }
            _ => {}
        }
    }

    fn done_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Left => {
                self.done_cursor = (self.done_cursor + DONE_ITEMS.len() - 1) % DONE_ITEMS.len()
            }
            KeyCode::Right | KeyCode::Tab => {
                self.done_cursor = (self.done_cursor + 1) % DONE_ITEMS.len()
            }
            KeyCode::Enter => match self.done_cursor {
                0 => {
                    self.scrollback.extend(ui::done_summary(self));
                    self.open_query();
                }
                1 => self.flash("Serving is not wired in this preview."),
                _ => {
                    self.scrollback.extend(ui::done_summary(self));
                    self.quit = true;
                }
            },
            KeyCode::Esc | KeyCode::Char('q') => {
                self.scrollback.extend(ui::done_summary(self));
                self.quit = true;
            }
            _ => {}
        }
    }

    fn open_query(&mut self) {
        self.query.clear();
        self.mode = Mode::Search;
        self.hit_cursor = 0;
        self.screen = Screen::Query;
    }

    fn query_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let hits = self.hits().len();
        match key.code {
            KeyCode::Char('r') if ctrl => {
                self.mode = if self.mode == Mode::Reverse {
                    Mode::Search
                } else {
                    Mode::Reverse
                };
                self.query.clear();
                self.hit_cursor = 0;
            }
            KeyCode::Tab => {
                self.mode = self.mode.next();
                self.hit_cursor = 0;
            }
            KeyCode::Up if hits > 0 => self.hit_cursor = (self.hit_cursor + hits - 1) % hits,
            KeyCode::Down if hits > 0 => self.hit_cursor = (self.hit_cursor + 1) % hits,
            KeyCode::Enter if hits > 0 => {
                self.json_scroll = 0;
                self.json_all = false;
                self.screen = Screen::Json;
            }
            KeyCode::Esc => {
                if self.query.is_empty() {
                    self.quit = true;
                } else {
                    self.query.clear();
                    self.hit_cursor = 0;
                }
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.hit_cursor = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                self.query.push(c);
                self.hit_cursor = 0;
            }
            _ => {}
        }
    }

    fn json_key(&mut self, key: KeyEvent) {
        let last = ui::json_lines(self).len().saturating_sub(ui::JSON_ROWS);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.json_scroll = self.json_scroll.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.json_scroll = (self.json_scroll + 1).min(last)
            }
            KeyCode::PageUp => self.json_scroll = self.json_scroll.saturating_sub(10),
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.json_scroll = (self.json_scroll + 10).min(last);
            }
            KeyCode::Char('a') => {
                self.json_all ^= true;
                self.json_scroll = 0;
            }
            KeyCode::Char('y') => self.flash("Copy is not wired in this preview."),
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Query,
            _ => {}
        }
    }
}

/// Subsequence match, case-insensitive. Rewards runs of matched letters and
/// matches at word starts. An empty query matches everything in list order.
fn fuzzy(query: &str, text: &str) -> Option<(Vec<usize>, i64)> {
    let query: Vec<char> = query
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if query.is_empty() {
        return Some((Vec::new(), 0));
    }
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    let mut positions = Vec::with_capacity(query.len());
    let mut score = 0;
    let mut next = 0;
    for wanted in query {
        let found = (next..chars.len()).find(|&index| chars[index] == wanted)?;
        let word_start = found == 0 || !chars[found - 1].is_alphanumeric();
        let run = positions
            .last()
            .is_some_and(|&last: &usize| last + 1 == found);
        score += 1 + if word_start { 8 } else { 0 } + if run { 4 } else { 0 };
        positions.push(found);
        next = found + 1;
    }
    // Shorter names first among equal matches: "Canada" before its provinces.
    Some((positions, score * 100 - chars.len() as i64))
}
