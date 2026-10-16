//! Screen state and key handling. Packs are read from the og home, builds
//! and batch runs are real jobs on their own threads, and the query shell
//! asks the open Pack.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    text::Line,
};

use crate::{
    batch::{BatchFile, BatchSession},
    engine::{self, Answer, Engine},
    format,
    home::{self, BuildRecord, Estimate, Home, PackInfo},
    job::{self, BuildJob, BuildPlan, Built},
    machine::{self, ProcessMemory},
    net,
    regions::{self, Region},
    ui,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Region,
    Confirm,
    Build,
    Done,
    Query,
    Json,
    /// Choosing the columns of a file to geocode.
    BatchSetup,
    BatchRun,
    BatchDone,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Search,
    Reverse,
    /// Many addresses at once, pasted as CSV.
    Batch,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Reverse => "reverse",
            Self::Batch => "batch",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Search => Self::Reverse,
            Self::Reverse => Self::Batch,
            Self::Batch => Self::Search,
        }
    }
}

pub const DONE_ITEMS: [&str; 3] = ["Open query shell", "Serve now", "Quit"];
/// Memory budgets offered for a build, in GB, up to what the machine has
/// free.
const MEMORY_STEPS_GB: [u64; 8] = [1, 2, 4, 6, 8, 12, 16, 32];
/// Rows of the region list, so the box keeps one height while browsing.
pub const REGION_ROWS: usize = 13;
/// Where `og serve` and "Serve now" listen.
pub const SERVE_ADDRESS: &str = "127.0.0.1:8080";

/// One pasted address and the best answer for it.
pub struct BatchRow {
    pub input: String,
    pub answer: Option<Arc<Answer>>,
}

impl BatchRow {
    pub fn hit(&self) -> Option<&engine::Hit> {
        self.answer.as_ref().and_then(|answer| answer.hits.first())
    }
}

/// One row of the region list.
pub struct RegionRow {
    pub region: &'static Region,
    /// Matched character positions of the name, for highlighting.
    pub positions: Vec<usize>,
    /// The regions above it, shown in search results.
    pub trail: Option<String>,
}

/// What the server says about the chosen region's extract right now.
#[derive(Default)]
pub struct Live {
    pub remote: Option<net::Remote>,
    /// When the newest extract's data is from.
    pub data_at: Option<i64>,
    pub error: Option<String>,
    pub done: bool,
}

/// The extract on disk for the chosen region, and when its data is from.
pub struct OnDisk {
    pub path: PathBuf,
    pub bytes: u64,
    pub data_at: Option<i64>,
}

pub struct App {
    pub screen: Screen,
    pub quit: bool,
    /// Lines to print above the live area: they stay in terminal history.
    pub scrollback: Vec<Line<'static>>,
    /// Lines a build or a batch run has put into history, so they can be
    /// erased when the flow is left.
    pub flow_lines: usize,
    /// How many lines above the window to erase: asked for here, done by the
    /// loop that owns the terminal.
    pub erase: Option<usize>,
    /// Plain lines printed after the interface closes.
    pub farewell: Vec<String>,
    /// A Pack to serve once the interface closes.
    pub serve: Option<PathBuf>,
    pub flash: Option<(String, Instant)>,
    pub started: Instant,
    /// Terminal width, for lines printed into history.
    pub width: u16,
    pub home: Arc<Home>,
    pub home_cursor: usize,
    /// The Packs the home screen lists.
    pub packs: Vec<PackInfo>,
    /// When the newest data for each Pack's region is from, by Pack
    /// directory, as the servers answer.
    pub updates: Arc<Mutex<HashMap<PathBuf, i64>>>,
    pub history: Vec<BuildRecord>,
    pub memory: machine::Memory,
    /// The Pack the query shell is using.
    pub engine: Option<Engine>,
    pub active_name: String,
    pub active_dir: PathBuf,
    pub region_query: String,
    pub region_cursor: usize,
    pub region_scroll: usize,
    /// The regions browsed into, from a continent down. Empty at the top.
    pub region_path: Vec<usize>,
    pub region: &'static Region,
    /// Download the extract even though one is on disk: an update.
    pub force_download: bool,
    pub on_disk: Option<OnDisk>,
    pub live: Arc<Mutex<Live>>,
    pub free_disk: Option<u64>,
    pub memory_gb: u64,
    pub confirm_cursor: usize,
    pub job: Option<BuildJob>,
    /// Steps of the running job already printed into history.
    pub printed: usize,
    pub built: Option<Built>,
    pub build_error: Option<String>,
    pub show_logs: bool,
    pub process: ProcessMemory,
    /// What og holds in memory, sampled while a job runs.
    pub resident: Option<u64>,
    pub sampled: Instant,
    pub done_cursor: usize,
    pub query: String,
    pub mode: Mode,
    pub answer: Option<Answer>,
    pub reading: Vec<(String, &'static str)>,
    pub query_error: Option<String>,
    pub hit_cursor: usize,
    pub json_scroll: usize,
    pub json_all: bool,
    /// What was pasted or typed in batch mode: CSV text, or a file path.
    pub batch_text: String,
    pub batch_rows: Vec<BatchRow>,
    pub batch_note: String,
    /// Answers already found for pasted rows, by row text.
    batch_cache: HashMap<String, Option<Arc<Answer>>>,
    pub batch_millis: f64,
    /// The file being geocoded, once one is opened.
    pub batch: Option<BatchSession>,
    pub batch_button: usize,
    /// A file named in the batch tab, waiting to be continued with or dropped.
    pub batch_candidate: Option<BatchFile>,
    /// 0 is Continue, 1 is Cancel.
    pub batch_choice: usize,
    pub batch_error: Option<String>,
    /// Jobs asked to stop that may still be cleaning up. Dropping one waits
    /// for it, so they are kept until og exits.
    pub stopping: Vec<Box<dyn Send>>,
}

impl App {
    pub fn new(home: Home) -> Self {
        let home = Arc::new(home);
        let packs = home.packs();
        let history = home.history();
        let memory = machine::memory();
        let region = packs
            .first()
            .and_then(|pack| pack.region)
            .unwrap_or_else(|| regions::tree().root());
        let app = Self {
            screen: Screen::Home,
            quit: false,
            scrollback: ui::logo(),
            flow_lines: 0,
            erase: None,
            farewell: Vec::new(),
            serve: None,
            flash: None,
            started: Instant::now(),
            width: 96,
            home,
            home_cursor: 0,
            packs,
            updates: Arc::new(Mutex::new(HashMap::new())),
            history,
            memory_gb: default_memory_gb(&memory),
            memory,
            engine: None,
            active_name: String::new(),
            active_dir: PathBuf::new(),
            region_query: String::new(),
            region_cursor: 0,
            region_scroll: 0,
            region_path: Vec::new(),
            region,
            force_download: false,
            on_disk: None,
            live: Arc::new(Mutex::new(Live::default())),
            free_disk: None,
            confirm_cursor: 0,
            job: None,
            printed: 0,
            built: None,
            build_error: None,
            show_logs: false,
            process: ProcessMemory::new(),
            resident: None,
            sampled: Instant::now(),
            done_cursor: 0,
            query: String::new(),
            mode: Mode::Search,
            answer: None,
            reading: Vec::new(),
            query_error: None,
            hit_cursor: 0,
            json_scroll: 0,
            json_all: false,
            batch_text: String::new(),
            batch_rows: Vec::new(),
            batch_note: String::new(),
            batch_cache: HashMap::new(),
            batch_millis: 0.0,
            batch: None,
            batch_button: 0,
            batch_candidate: None,
            batch_choice: 0,
            batch_error: None,
            stopping: Vec::new(),
        };
        app.check_updates();
        app
    }

    /// Asks each Pack's publisher, in the background, when its newest data
    /// is from.
    fn check_updates(&self) {
        for pack in &self.packs {
            let Some(url) = pack.region.and_then(Region::state_url) else {
                continue;
            };
            let (updates, dir) = (Arc::clone(&self.updates), pack.dir.clone());
            thread::spawn(move || {
                if let Ok(at) = net::state_timestamp(&url) {
                    updates.lock().expect("updates lock").insert(dir, at);
                }
            });
        }
    }

    /// When newer data than a Pack's is published, the time it is from.
    pub fn update_for(&self, pack: &PackInfo) -> Option<i64> {
        let newest = *self.updates.lock().expect("updates lock").get(&pack.dir)?;
        (newest > pack.data_at?).then_some(newest)
    }

    /// The memory budgets this machine can offer.
    pub fn memory_steps(&self) -> Vec<u64> {
        memory_steps(&self.memory)
    }

    pub fn pack_dir(&self) -> PathBuf {
        self.home.pack_dir(self.region)
    }

    /// Whether the build downloads the extract first.
    pub fn downloads(&self) -> bool {
        self.force_download || self.on_disk.is_none()
    }

    /// Bytes to download: the live size when the server gave one.
    pub fn download_bytes(&self) -> u64 {
        let live = self.live.lock().expect("live lock");
        live.remote
            .and_then(|remote| remote.bytes)
            .unwrap_or(self.region.osm.bytes)
    }

    /// The extract the build reads, in bytes.
    pub fn input_bytes(&self) -> u64 {
        match (&self.on_disk, self.downloads()) {
            (Some(extract), false) => extract.bytes,
            _ => self.download_bytes(),
        }
    }

    /// The build of this region scaled from this machine's own builds.
    pub fn estimate(&self) -> Option<Estimate> {
        home::estimate(&self.history, self.input_bytes())
    }

    pub fn estimate_for(&self, region: &Region) -> Option<Estimate> {
        home::estimate(&self.history, region.osm.bytes)
    }

    /// Disk the build is known to need: the download, plus what builds on
    /// this machine have taken.
    pub fn disk_needed(&self) -> u64 {
        let download = if self.downloads() {
            self.download_bytes()
        } else {
            0
        };
        download + self.estimate().map_or(0, |estimate| estimate.disk_bytes)
    }

    pub fn fits_disk(&self) -> bool {
        self.free_disk.is_none_or(|free| self.disk_needed() <= free)
    }

    /// The equivalent non-interactive command for this build.
    pub fn command(&self) -> String {
        format!(
            "og build --region {} --memory {}",
            self.region.id, self.memory_gb
        )
    }

    /// The region whose children the list shows.
    pub fn region_here(&self) -> &'static Region {
        let tree = regions::tree();
        self.region_path
            .last()
            .map_or_else(|| tree.root(), |index| tree.at(*index))
    }

    /// The rows of the region list. Browsing shows the region itself first,
    /// as the first row, then its children. A search shows every region whose
    /// name or alias matches, with the regions above it.
    pub fn region_rows(&self) -> Vec<RegionRow> {
        let tree = regions::tree();
        if self.region_browsing() {
            let here = self.region_here();
            let mut rows = vec![RegionRow {
                region: here,
                positions: Vec::new(),
                trail: None,
            }];
            rows.extend(here.children.iter().map(|index| RegionRow {
                region: tree.at(*index),
                positions: Vec::new(),
                trail: None,
            }));
            return rows;
        }
        let mut matches: Vec<(RegionRow, i64)> = tree
            .searchable()
            .filter_map(|region| {
                let trail = tree.trail(region);
                let (positions, score) =
                    find(&self.region_query, &region.name, &trail).or_else(|| {
                        // An alias match ranks below a match of the name.
                        region
                            .aliases
                            .iter()
                            .filter_map(|alias| find(&self.region_query, alias, &trail))
                            .map(|(_, score)| (Vec::new(), score - 500_000))
                            .max_by_key(|(_, score)| *score)
                    })?;
                let row = RegionRow {
                    region,
                    positions,
                    trail: Some(trail),
                };
                Some((row, score))
            })
            .collect();
        matches.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
        matches.into_iter().map(|(row, _)| row).collect()
    }

    /// Whether the list shows the browsed region's own children.
    pub fn region_browsing(&self) -> bool {
        self.region_query.trim().is_empty()
    }

    /// Rows the list can show from the current scroll: browsing at the top
    /// spends one on the line under the region itself.
    pub fn region_capacity(&self) -> usize {
        if self.region_browsing() && self.region_scroll == 0 {
            REGION_ROWS - 1
        } else {
            REGION_ROWS
        }
    }

    /// Scrolls so the cursor row is on screen.
    fn region_follow(&mut self) {
        if self.region_cursor < self.region_scroll {
            self.region_scroll = self.region_cursor;
        } else if self.region_cursor >= self.region_scroll + self.region_capacity() {
            self.region_scroll = (self.region_cursor + 1).saturating_sub(REGION_ROWS).max(1);
        }
    }

    fn region_reset(&mut self, cursor: usize) {
        self.region_cursor = cursor;
        self.region_scroll = 0;
        self.region_follow();
    }

    /// Opens a region: the list shows what is below it.
    fn region_open(&mut self, region: &'static Region) {
        self.region_path = regions::tree().ancestors(region);
        self.region_path.push(region.index);
        self.region_query.clear();
        self.region_reset(0);
    }

    /// Goes up one level, leaving the cursor on the region just left.
    fn region_up(&mut self) {
        let Some(left) = self.region_path.pop() else {
            return;
        };
        let cursor = self
            .region_rows()
            .iter()
            .position(|row| row.region.index == left)
            .unwrap_or(0);
        self.region_reset(cursor);
    }

    /// Takes a region for the build and moves on to confirming it. The list
    /// stays on that region for when the user comes back.
    fn region_select(&mut self, region: &'static Region) {
        self.region = region;
        self.region_query.clear();
        self.region_path = regions::tree().ancestors(region);
        let cursor = self
            .region_rows()
            .iter()
            .position(|row| row.region.index == region.index)
            .unwrap_or(0);
        self.region_reset(cursor);
        self.force_download = false;
        self.open_confirm();
    }

    /// Opens the confirm step for `self.region`: what is on disk is read
    /// now, what the server has is asked in the background.
    fn open_confirm(&mut self) {
        self.on_disk = self.home.extract(self.region).map(|extract| OnDisk {
            data_at: open_geocode::builder::replication_timestamp(&extract.path)
                .ok()
                .flatten(),
            path: extract.path,
            bytes: extract.bytes,
        });
        self.free_disk = machine::free_disk(&self.pack_dir());
        self.memory = machine::memory();
        let steps = self.memory_steps();
        if !steps.contains(&self.memory_gb) {
            self.memory_gb = default_memory_gb(&self.memory);
        }
        let live = Arc::new(Mutex::new(Live::default()));
        self.live = Arc::clone(&live);
        let region = self.region;
        thread::spawn(move || {
            let remote = net::head(&region.osm.url);
            let data_at = region.state_url().map(|url| net::state_timestamp(&url));
            let mut live = live.lock().expect("live lock");
            match remote {
                Ok(remote) => live.remote = Some(remote),
                Err(error) => live.error = Some(format!("{error:#}")),
            }
            live.data_at = data_at.and_then(Result::ok);
            live.done = true;
        });
        self.confirm_cursor = 0;
        self.screen = Screen::Confirm;
    }

    pub fn flash_text(&self) -> Option<&str> {
        self.flash
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(4))
            .map(|(text, _)| text.as_str())
    }

    fn flash(&mut self, text: impl Into<String>) {
        self.flash = Some((text.into(), Instant::now()));
    }

    /// Puts lines of a flow into history, to be erased when it is left.
    fn push_flow(&mut self, lines: Vec<Line<'static>>) {
        self.flow_lines += lines.len();
        self.scrollback.extend(lines);
    }

    /// Asks for what the flow put into history to be erased, so the next
    /// screen starts where the flow did and not under its leftovers.
    fn erase_flow(&mut self) {
        if self.flow_lines > 0 {
            self.erase = Some(self.flow_lines);
            self.flow_lines = 0;
        }
    }

    pub fn tick(&mut self) {
        if self.job.is_some() && self.sampled.elapsed() >= Duration::from_secs(1) {
            self.resident = self.process.resident();
            self.free_disk = machine::free_disk(&self.pack_dir());
            self.sampled = Instant::now();
        }
        self.tick_build();
        self.tick_batch();
    }

    fn tick_build(&mut self) {
        let Some(job) = &self.job else {
            return;
        };
        // Steps that finished since the last tick go into history, in order.
        let mut lines = Vec::new();
        {
            let steps = job.progress.steps.lock().expect("steps lock");
            while let Some(step) = steps.get(self.printed).filter(|step| step.secs.is_some()) {
                lines.extend(ui::step_done(step, ui::content_width(self.width)));
                self.printed += 1;
            }
        }
        self.push_flow(lines);
        let Some(outcome) = self.job.as_mut().and_then(BuildJob::take) else {
            return;
        };
        self.job = None;
        match outcome {
            Ok(built) => {
                self.history = self.home.history();
                self.packs = self.home.packs();
                self.check_updates();
                self.built = Some(built);
                self.done_cursor = 0;
                self.screen = Screen::Done;
            }
            Err(error) if job::cancelled(&error) => {}
            Err(error) => self.build_error = Some(format!("{error:#}")),
        }
    }

    fn tick_batch(&mut self) {
        let Some(session) = &mut self.batch else {
            return;
        };
        let Some(run) = &mut session.run else {
            return;
        };
        let Some(outcome) = run.take() else {
            return;
        };
        match outcome {
            Ok(()) => {
                session.summarize();
                session.run = None;
                self.batch_button = 0;
                self.screen = Screen::BatchDone;
            }
            Err(error) if job::cancelled(&error) => session.run = None,
            Err(error) => {
                session.run = None;
                self.batch_error = Some(format!("{error:#}"));
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if let Some(job) = &self.job {
                job.cancel();
            }
            self.farewell.push("Cancelled.".to_string());
            self.quit = true;
            return;
        }
        match self.screen {
            Screen::Home => self.home_key(key),
            Screen::Region => self.region_key(key),
            Screen::Confirm => self.confirm_key(key),
            Screen::Build => self.build_key(key),
            Screen::Done => self.done_key(key),
            Screen::Query => self.query_key(key),
            Screen::Json => self.json_key(key),
            Screen::BatchSetup => self.batch_setup_key(key),
            Screen::BatchRun => self.batch_run_key(key),
            Screen::BatchDone => self.batch_done_key(key),
        }
    }

    fn home_key(&mut self, key: KeyEvent) {
        // The last row builds a new Pack; the rows above it are the Packs.
        let rows = self.packs.len() + 1;
        let on_build = self.home_cursor == self.packs.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.home_cursor = (self.home_cursor + rows - 1) % rows;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.home_cursor = (self.home_cursor + 1) % rows;
            }
            KeyCode::Char('b') => self.screen = Screen::Region,
            KeyCode::Enter if on_build => self.screen = Screen::Region,
            KeyCode::Enter => {
                let dir = self.packs[self.home_cursor].dir.clone();
                if self.activate(&dir) {
                    self.open_query(Mode::Search);
                }
            }
            KeyCode::Char('v') if !on_build => {
                self.serve = Some(self.packs[self.home_cursor].dir.clone());
                self.quit = true;
            }
            KeyCode::Char('g') if !on_build => {
                let dir = self.packs[self.home_cursor].dir.clone();
                if self.activate(&dir) {
                    self.open_query(Mode::Batch);
                }
            }
            KeyCode::Char('u') if !on_build => {
                let pack = &self.packs[self.home_cursor];
                match (pack.region, self.update_for(pack)) {
                    (Some(region), Some(_)) => {
                        self.region = region;
                        self.region_path = regions::tree().ancestors(region);
                        self.force_download = true;
                        self.open_confirm();
                    }
                    _ => {
                        let name = pack.name.clone();
                        self.flash(format!("{name} has the newest data its publisher has."));
                    }
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => self.quit = true,
            _ => {}
        }
    }

    /// Opens a Pack for the query shell. False, with the reason shown, when
    /// it cannot be opened.
    fn activate(&mut self, dir: &Path) -> bool {
        if self.engine.is_some() && self.active_dir == dir {
            return true;
        }
        match Engine::open(dir) {
            Ok(engine) => {
                self.active_name = self
                    .packs
                    .iter()
                    .find(|pack| pack.dir == dir)
                    .map_or_else(|| dir.display().to_string(), |pack| pack.name.clone());
                self.active_dir = dir.to_path_buf();
                self.engine = Some(engine);
                self.batch_cache.clear();
                true
            }
            Err(error) => {
                self.flash(format!("{error:#}"));
                false
            }
        }
    }

    fn region_key(&mut self, key: KeyEvent) {
        let rows = self.region_rows();
        let count = rows.len().max(1);
        match key.code {
            KeyCode::Up => {
                self.region_cursor = (self.region_cursor + count - 1) % count;
                self.region_follow();
            }
            KeyCode::Down => {
                self.region_cursor = (self.region_cursor + 1) % count;
                self.region_follow();
            }
            KeyCode::PageUp => {
                self.region_cursor = self.region_cursor.saturating_sub(REGION_ROWS);
                self.region_follow();
            }
            KeyCode::PageDown => {
                self.region_cursor = (self.region_cursor + REGION_ROWS).min(count - 1);
                self.region_follow();
            }
            KeyCode::Enter => {
                let Some(row) = rows.get(self.region_cursor) else {
                    return;
                };
                let all = self.region_browsing() && self.region_cursor == 0;
                if !all && !row.region.children.is_empty() {
                    self.region_open(row.region);
                } else {
                    self.region_select(row.region);
                }
            }
            KeyCode::Right => {
                let all = self.region_browsing() && self.region_cursor == 0;
                if let Some(row) = rows.get(self.region_cursor)
                    && !all
                    && !row.region.children.is_empty()
                {
                    self.region_open(row.region);
                }
            }
            KeyCode::Left if self.region_browsing() => self.region_up(),
            KeyCode::Esc => {
                if !self.region_browsing() {
                    self.region_query.clear();
                    self.region_reset(0);
                } else if self.region_path.is_empty() {
                    self.screen = Screen::Home;
                } else {
                    self.region_up();
                }
            }
            KeyCode::Backspace => {
                if self.region_query.pop().is_some() {
                    self.region_reset(0);
                } else {
                    self.region_up();
                }
            }
            KeyCode::Char(c) => {
                self.region_query.push(c);
                self.region_reset(0);
            }
            _ => {}
        }
    }

    fn confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => self.confirm_cursor ^= 1,
            KeyCode::Char('m') => {
                let steps = self.memory_steps();
                let at = steps
                    .iter()
                    .position(|gb| *gb == self.memory_gb)
                    .unwrap_or(0);
                self.memory_gb = steps[(at + 1) % steps.len()];
            }
            KeyCode::Char('d') if self.on_disk.is_some() => {
                self.force_download ^= true;
                self.free_disk = machine::free_disk(&self.pack_dir());
            }
            KeyCode::Enter if self.confirm_cursor == 1 => self.screen = Screen::Region,
            KeyCode::Enter if !self.fits_disk() => self.flash(format!(
                "Not enough free disk: {} needs {}, {} is free.",
                self.region.name,
                format::bytes(self.disk_needed()),
                format::bytes(self.free_disk.unwrap_or(0))
            )),
            KeyCode::Enter => self.start_build(),
            KeyCode::Esc => self.screen = Screen::Region,
            _ => {}
        }
    }

    fn start_build(&mut self) {
        let mut lines = ui::choices(self);
        lines.extend(ui::build_header(self));
        self.push_flow(lines);
        self.built = None;
        self.build_error = None;
        self.printed = 0;
        self.resident = self.process.resident();
        self.job = Some(BuildJob::start(
            BuildPlan {
                region: self.region,
                extract: self.home.extract_path(self.region),
                download: self.downloads(),
                pack: self.pack_dir(),
                memory_budget_bytes: self.memory_gb << 30,
            },
            Arc::clone(&self.home),
        ));
        self.show_logs = false;
        self.screen = Screen::Build;
    }

    fn build_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('l') => self.show_logs ^= true,
            KeyCode::Esc => {
                if let Some(job) = self.job.take() {
                    job.cancel();
                    self.stopping.push(Box::new(job));
                }
                self.build_error = None;
                self.erase_flow();
                self.open_confirm();
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
                    let dir = self.pack_dir();
                    self.erase_flow();
                    // The Pack may have been rebuilt where an older one was open.
                    if self.active_dir == dir {
                        self.engine = None;
                    }
                    if self.activate(&dir) {
                        self.open_query(Mode::Search);
                    }
                }
                1 => {
                    self.scrollback.extend(ui::done_summary(self));
                    self.serve = Some(self.pack_dir());
                    self.quit = true;
                }
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

    fn open_query(&mut self, mode: Mode) {
        self.query.clear();
        self.answer = None;
        self.reading.clear();
        self.query_error = None;
        self.mode = mode;
        self.hit_cursor = 0;
        self.screen = Screen::Query;
    }

    /// Asks the Pack again after the query changed.
    fn refresh_query(&mut self) {
        self.hit_cursor = 0;
        self.answer = None;
        self.reading.clear();
        self.query_error = None;
        let Some(engine) = &self.engine else {
            return;
        };
        if self.query.trim().is_empty() {
            return;
        }
        let answer = match self.mode {
            Mode::Search => {
                self.reading = engine.read(&self.query).parts;
                Some(engine.search_default(&self.query))
            }
            Mode::Reverse => engine::point(&self.query).map(|(lat, lon)| engine.reverse(lat, lon)),
            Mode::Batch => None,
        };
        match answer {
            Some(Ok(answer)) => self.answer = Some(answer),
            Some(Err(error)) => self.query_error = Some(format!("{error:#}")),
            None => {}
        }
    }

    pub fn hits(&self) -> &[engine::Hit] {
        self.answer
            .as_ref()
            .map_or(&[], |answer| answer.hits.as_slice())
    }

    fn query_key(&mut self, key: KeyEvent) {
        if self.mode == Mode::Batch {
            self.batch_key(key);
            return;
        }
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
                self.refresh_query();
            }
            KeyCode::Tab => {
                self.mode = self.mode.next();
                self.refresh_query();
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
                    self.screen = Screen::Home;
                } else {
                    self.query.clear();
                    self.refresh_query();
                }
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.refresh_query();
            }
            KeyCode::Char(c) if !ctrl => {
                self.query.push(c);
                self.refresh_query();
            }
            _ => {}
        }
    }

    /// Batch mode edits a block of text: a terminal that does not mark a
    /// paste sends it as keys, so Enter adds a line instead of submitting.
    fn batch_key(&mut self, key: KeyEvent) {
        // A file is waiting: choose between continuing with it and dropping it.
        if self.batch_candidate.is_some() {
            match key.code {
                KeyCode::Left | KeyCode::Right => {
                    self.batch_choice ^= 1;
                    return;
                }
                KeyCode::Enter if self.batch_choice == 0 => {
                    if let Some(file) = self.batch_candidate.take() {
                        self.open_batch_setup(file);
                    }
                    return;
                }
                KeyCode::Enter | KeyCode::Esc => {
                    self.drop_batch_file();
                    return;
                }
                _ => {}
            }
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let rows = self.batch_rows.len();
        match key.code {
            KeyCode::Tab => {
                self.mode = self.mode.next();
                self.refresh_query();
            }
            KeyCode::Char('r') if ctrl => {
                self.mode = Mode::Reverse;
                self.query.clear();
                self.refresh_query();
            }
            KeyCode::Char('u') if ctrl => {
                self.batch_text.clear();
                self.refresh_batch();
            }
            KeyCode::Char('g') if ctrl => {
                if self
                    .batch_rows
                    .get(self.hit_cursor)
                    .is_some_and(|row| row.hit().is_some())
                {
                    self.json_scroll = 0;
                    self.json_all = false;
                    self.screen = Screen::Json;
                } else {
                    self.flash("That row has no match to show.");
                }
            }
            KeyCode::Char('j') if ctrl => self.push_batch("\n"),
            KeyCode::Enter => self.push_batch("\n"),
            KeyCode::Up if rows > 0 => self.hit_cursor = (self.hit_cursor + rows - 1) % rows,
            KeyCode::Down if rows > 0 => self.hit_cursor = (self.hit_cursor + 1) % rows,
            KeyCode::Esc => {
                if self.batch_text.is_empty() {
                    self.screen = Screen::Home;
                } else {
                    self.batch_text.clear();
                    self.refresh_batch();
                }
            }
            KeyCode::Backspace => {
                self.batch_text.pop();
                self.refresh_batch();
            }
            KeyCode::Char(c) if !ctrl => self.push_batch(c.encode_utf8(&mut [0; 4])),
            _ => {}
        }
    }

    fn push_batch(&mut self, text: &str) {
        self.batch_text.push_str(text);
        self.refresh_batch();
    }

    /// Forgets the file waiting in the batch tab and clears the box.
    fn drop_batch_file(&mut self) {
        self.batch_candidate = None;
        self.batch_choice = 0;
        self.batch_text.clear();
        self.refresh_batch();
    }

    /// Reads the batch text again. A single line that names a CSV file makes
    /// that file the candidate to continue with; anything else is read as
    /// rows and every address in it is looked up. Rows already looked up are
    /// not asked again.
    fn refresh_batch(&mut self) {
        let typed = self.batch_text.trim().trim_matches('"');
        let file = (!typed.is_empty() && !typed.contains('\n'))
            .then(|| BatchFile::from_path(Path::new(typed)))
            .flatten();
        if let Some(file) = file {
            self.batch_candidate = Some(file);
            self.batch_choice = 0;
            self.batch_rows.clear();
            self.batch_note.clear();
            return;
        }
        self.batch_candidate = None;
        let (addresses, note) = parse_pasted(&self.batch_text);
        let mut rows = Vec::with_capacity(addresses.len());
        for input in addresses {
            let answer = match self.batch_cache.get(&input) {
                Some(answer) => answer.clone(),
                None => {
                    let answer = self
                        .engine
                        .as_ref()
                        .and_then(|engine| engine.search(&input, 1).ok())
                        .map(Arc::new);
                    self.batch_cache.insert(input.clone(), answer.clone());
                    answer
                }
            };
            rows.push(BatchRow { input, answer });
        }
        // What the engine took for every row, also those answered before.
        self.batch_millis = rows
            .iter()
            .filter_map(|row| row.answer.as_ref())
            .map(|answer| answer.millis)
            .sum();
        self.batch_rows = rows;
        self.batch_note = note;
        self.hit_cursor = self.hit_cursor.min(self.batch_rows.len().saturating_sub(1));
    }

    /// Text pasted into the terminal as one piece.
    pub fn on_paste(&mut self, text: String) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        match self.screen {
            Screen::Query if self.mode == Mode::Batch => self.push_batch(&text),
            Screen::Query => {
                self.query
                    .push_str(text.lines().next().unwrap_or("").trim());
                self.refresh_query();
            }
            Screen::Region => {
                self.region_query
                    .push_str(text.lines().next().unwrap_or("").trim());
                self.region_reset(0);
            }
            _ => {}
        }
    }

    fn open_batch_setup(&mut self, file: BatchFile) {
        self.batch = Some(BatchSession::new(file));
        self.batch_error = None;
        self.screen = Screen::BatchSetup;
    }

    fn batch_setup_key(&mut self, key: KeyEvent) {
        let Some(session) = &mut self.batch else {
            self.screen = Screen::Query;
            return;
        };
        let columns = session.used.len().max(1);
        match key.code {
            KeyCode::Left => session.column = (session.column + columns - 1) % columns,
            KeyCode::Right | KeyCode::Tab => session.column = (session.column + 1) % columns,
            KeyCode::Char(' ') | KeyCode::Up | KeyCode::Down => {
                if let Some(on) = session.used.get_mut(session.column) {
                    *on ^= true;
                }
            }
            KeyCode::Enter if !session.has_address() => {
                self.flash("Pick at least one column for the address.");
            }
            KeyCode::Enter => self.start_batch_run(),
            KeyCode::Esc => {
                self.batch_candidate = self.batch.take().map(|session| session.file);
                self.batch_choice = 0;
                self.screen = Screen::Query;
            }
            _ => {}
        }
    }

    fn start_batch_run(&mut self) {
        let Some(session) = &mut self.batch else {
            return;
        };
        self.batch_error = None;
        session.start(&self.active_dir);
        let lines = ui::batch_header(self);
        self.push_flow(lines);
        self.screen = Screen::BatchRun;
    }

    fn batch_run_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            if let Some(session) = &mut self.batch
                && let Some(run) = session.run.take()
            {
                run.cancel();
                self.stopping.push(Box::new(run));
            }
            self.batch_error = None;
            self.erase_flow();
            self.screen = Screen::BatchSetup;
        }
    }

    fn batch_done_key(&mut self, key: KeyEvent) {
        let buttons = 2;
        match key.code {
            KeyCode::Left => self.batch_button = (self.batch_button + buttons - 1) % buttons,
            KeyCode::Right | KeyCode::Tab => self.batch_button = (self.batch_button + 1) % buttons,
            KeyCode::Enter => match self.batch_button {
                0 => {
                    self.erase_flow();
                    self.open_query(Mode::Search);
                }
                _ => {
                    self.erase_flow();
                    self.screen = Screen::Home;
                }
            },
            KeyCode::Esc | KeyCode::Char('q') => {
                self.erase_flow();
                self.screen = Screen::Home;
            }
            _ => {}
        }
    }

    /// The JSON the GeoJSON screen shows.
    pub fn json_text(&self) -> String {
        if self.mode == Mode::Batch {
            return self
                .batch_rows
                .get(self.hit_cursor)
                .and_then(|row| row.answer.as_ref())
                .map(|answer| answer.collection.json(None))
                .unwrap_or_default();
        }
        let only = (!self.json_all).then_some(self.hit_cursor);
        self.answer
            .as_ref()
            .map(|answer| answer.collection.json(only))
            .unwrap_or_default()
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
            KeyCode::Char('a') if self.mode != Mode::Batch => {
                self.json_all ^= true;
                self.json_scroll = 0;
            }
            KeyCode::Char('y') => {
                let text = self.json_text();
                let lines = text.lines().count();
                match arboard::Clipboard::new().and_then(|mut board| board.set_text(text)) {
                    Ok(()) => self.flash(format!("Copied {lines} lines of GeoJSON.")),
                    Err(error) => self.flash(format!("Could not copy: {error}")),
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Query,
            _ => {}
        }
    }
}

fn memory_steps(memory: &machine::Memory) -> Vec<u64> {
    let available_gb = memory.available >> 30;
    let steps: Vec<u64> = MEMORY_STEPS_GB
        .into_iter()
        .filter(|gb| *gb <= available_gb.max(1))
        .collect();
    if steps.is_empty() { vec![1] } else { steps }
}

/// Half the memory free now, at the step below it.
fn default_memory_gb(memory: &machine::Memory) -> u64 {
    let half = (memory.available >> 30) / 2;
    memory_steps(memory)
        .into_iter()
        .filter(|gb| *gb <= half.max(1))
        .max()
        .unwrap_or(1)
}

/// Reads pasted CSV into one address per row. A header with a column named
/// like an address picks that column; without one every row is an address.
/// Returns the addresses and a short note on how they were read.
fn parse_pasted(text: &str) -> (Vec<String>, String) {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(open_geocode::batch::delimiter_of(text.as_bytes()))
        .from_reader(text.as_bytes());
    let records: Vec<csv::StringRecord> = reader.records().filter_map(Result::ok).collect();
    let column = records.first().and_then(|first| {
        first.iter().position(|field| {
            matches!(
                field.trim().to_lowercase().as_str(),
                "address" | "addr" | "full_address" | "query" | "q"
            )
        })
    });
    let (rows, note) = match column {
        Some(_) => (&records[1..], "address column"),
        None => (&records[..], "one address per row"),
    };
    let addresses = rows
        .iter()
        .map(|record| match column {
            Some(index) => record.get(index).unwrap_or("").trim().to_string(),
            None => record
                .iter()
                .map(str::trim)
                .filter(|field| !field.is_empty())
                .collect::<Vec<_>>()
                .join(", "),
        })
        .filter(|address| !address.is_empty())
        .collect();
    (addresses, note.to_string())
}

/// Matches a search against a region: every word of the query must occur in
/// the name or in the regions above it, so `ont canada` finds Ontario. Words
/// that start a word of the name rank first, then names that start with the
/// query, then shorter names. Returns the matched positions in the name.
fn find(query: &str, name: &str, trail: &str) -> Option<(Vec<usize>, i64)> {
    let name_chars: Vec<char> = name.to_lowercase().chars().collect();
    let trail = trail.to_lowercase();
    let mut positions = Vec::new();
    let mut score = 0;
    for word in query.to_lowercase().split_whitespace() {
        let word: Vec<char> = word.chars().collect();
        if let Some((at, word_start)) = locate(&name_chars, &word) {
            score += if word_start { 100 } else { 40 };
            if at == 0 {
                score += 50;
            }
            positions.extend(at..at + word.len());
        } else if trail.contains(&word.iter().collect::<String>()) {
            score += 10;
        } else {
            return None;
        }
    }
    positions.sort_unstable();
    positions.dedup();
    Some((positions, score * 1_000 - name_chars.len() as i64))
}

/// Where `needle` occurs in `text`, preferring the start of a word.
fn locate(text: &[char], needle: &[char]) -> Option<(usize, bool)> {
    let at = |start: usize| text[start..].starts_with(needle);
    let word_start = |start: usize| start == 0 || !text[start - 1].is_alphanumeric();
    (0..text.len())
        .find(|&start| at(start) && word_start(start))
        .map(|start| (start, true))
        .or_else(|| {
            (0..text.len())
                .find(|&start| at(start))
                .map(|start| (start, false))
        })
}
