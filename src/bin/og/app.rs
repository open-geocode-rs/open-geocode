//! Screen state and key handling. Packs are read from the og home, builds
//! and batch runs are real jobs on their own threads, and the query shell
//! asks the open Pack on a thread of its own.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use open_geocode::{
    alloc,
    builder::{BuildPlan, plan_build},
    resources,
};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    text::Line,
};

use crate::{
    ask::{Asker, PastedRow, Question, Reply},
    batch::{BatchFile, BatchSession, Run},
    engine::{self, Answer, Engine},
    format,
    home::{self, BuildRecord, Estimate, Home, PackInfo},
    job::{self, BuildInput, BuildJob, BuildRequest, Built},
    net,
    regions::{self, Region},
    ui,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Screen {
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
pub(crate) enum Mode {
    Search,
    Reverse,
    /// Many addresses at once, pasted as CSV.
    Batch,
}

impl Mode {
    pub(crate) fn name(self) -> &'static str {
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

pub(crate) const DONE_ITEMS: [&str; 3] = ["Open query shell", "Serve now", "Quit"];
/// Rows of the region list, so the box keeps one height while browsing.
pub(crate) const REGION_ROWS: usize = 13;
/// Where `og serve` and "Serve now" listen.
pub(crate) const SERVE_ADDRESS: &str = "127.0.0.1:8080";

/// One row of the region list.
pub(crate) struct RegionRow {
    pub region: &'static Region,
    /// Matched character positions of the name, for highlighting.
    pub positions: Vec<usize>,
    /// The regions above it, shown in search results.
    pub trail: Option<String>,
}

/// What the server says about the chosen region's extract right now.
#[derive(Default)]
pub(crate) struct Live {
    pub remote: Option<net::Remote>,
    /// When the newest extract's data is from.
    pub data_at: Option<i64>,
    pub error: Option<String>,
    pub done: bool,
}

/// The extract on disk for the chosen region, and when its data is from.
pub(crate) struct OnDisk {
    pub path: PathBuf,
    pub bytes: u64,
    pub data_at: Option<i64>,
}

pub(crate) struct App {
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
    /// The Pack the query shell is using.
    pub engine: Option<Arc<Engine>>,
    /// Asks `engine` off the interface thread.
    asker: Option<Asker>,
    pub active_name: String,
    pub active_dir: PathBuf,
    pub region_query: String,
    pub region_cursor: usize,
    pub region_scroll: usize,
    /// The regions browsed into, from a continent down. Empty at the top.
    pub region_path: Vec<usize>,
    /// The region the cursor last settled on: what a build covers when
    /// nothing is ticked.
    pub region: &'static Region,
    /// The regions ticked with space, by tree index, in the order they were
    /// ticked. Empty means "just the one under the cursor", so the one-region
    /// flow needs no ticking at all.
    pub picked: Vec<usize>,
    /// The name typed for a Pack of several regions, empty until one is.
    pub pack_name: String,
    /// Download the extract even though one is on disk: an update.
    pub force_download: bool,
    pub on_disk: Option<OnDisk>,
    pub live: Arc<Mutex<Live>>,
    /// What the build of the chosen region would run with and need, as the
    /// builder plans it.
    pub plan: Option<Result<BuildPlan, String>>,
    /// Build despite the shortfalls the plan finds.
    pub force: bool,
    /// Add the OpenAddresses points of the countries the Pack covers.
    pub openaddresses: bool,
    pub confirm_cursor: usize,
    pub job: Option<BuildJob>,
    /// Steps of the running job already printed into history.
    pub printed: usize,
    /// Warnings of the running job already printed into history.
    warned: usize,
    pub built: Option<Built>,
    pub build_error: Option<String>,
    pub show_logs: bool,
    /// What og holds on the heap, and the disk free where the Pack is
    /// written, sampled while a job runs.
    pub heap: Option<u64>,
    pub free_disk: Option<u64>,
    sampled: Instant,
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
    /// Whether the batch text changed since it was last read.
    batch_dirty: bool,
    pub batch_rows: Vec<PastedRow>,
    pub batch_note: String,
    pub batch_millis: f64,
    /// The file being geocoded, once one is opened.
    pub batch: Option<BatchSession>,
    /// The columns of the preview asked for last.
    preview_for: Option<Vec<bool>>,
    pub batch_button: usize,
    /// A file named in the batch tab, waiting to be continued with or dropped.
    pub batch_candidate: Option<BatchFile>,
    /// 0 is Continue, 1 is Cancel.
    pub batch_choice: usize,
    pub batch_error: Option<String>,
    /// One clipboard for the session: on Linux what is copied lives only as
    /// long as the clipboard that copied it.
    clipboard: Option<arboard::Clipboard>,
    /// Jobs asked to stop that may still be cleaning up. Dropping one waits
    /// for it, so they are kept until they finish or og exits.
    pub stopping_builds: Vec<BuildJob>,
    pub stopping_runs: Vec<Run>,
}

impl App {
    pub(crate) fn new(home: Home) -> Self {
        let home = Arc::new(home);
        let packs = home.packs();
        let history = home.history();
        let region = packs
            .first()
            .and_then(|pack| pack.regions.first().copied())
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
            engine: None,
            asker: None,
            active_name: String::new(),
            active_dir: PathBuf::new(),
            region_query: String::new(),
            region_cursor: 0,
            picked: Vec::new(),
            pack_name: String::new(),
            region_scroll: 0,
            region_path: Vec::new(),
            region,
            force_download: false,
            on_disk: None,
            live: Arc::new(Mutex::new(Live::default())),
            plan: None,
            force: false,
            openaddresses: false,
            confirm_cursor: 0,
            job: None,
            printed: 0,
            warned: 0,
            built: None,
            build_error: None,
            show_logs: false,
            heap: None,
            free_disk: None,
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
            batch_dirty: false,
            batch_rows: Vec::new(),
            batch_note: String::new(),
            batch_millis: 0.0,
            batch: None,
            preview_for: None,
            batch_button: 0,
            batch_candidate: None,
            batch_choice: 0,
            batch_error: None,
            clipboard: None,
            stopping_builds: Vec::new(),
            stopping_runs: Vec::new(),
        };
        app.check_updates();
        app
    }

    /// Asks each Pack's publisher, in the background, when its newest data
    /// is from.
    fn check_updates(&self) {
        for pack in &self.packs {
            // A Pack of several regions is only as fresh as its oldest
            // input, so it is checked against every publisher it came from.
            let urls: Vec<String> = pack
                .regions
                .iter()
                .filter_map(|region| region.state_url())
                .collect();
            if urls.is_empty() {
                continue;
            }
            let (updates, dir) = (Arc::clone(&self.updates), pack.dir.clone());
            thread::spawn(move || {
                // The newest each publisher has, taken together: the Pack is
                // behind when any one of its regions has newer data.
                if let Some(at) = urls
                    .iter()
                    .filter_map(|url| net::state_timestamp(url).ok())
                    .max()
                {
                    updates.lock().expect("updates lock").insert(dir, at);
                }
            });
        }
    }

    /// When newer data than a Pack's is published, the time it is from.
    pub(crate) fn update_for(&self, pack: &PackInfo) -> Option<i64> {
        let newest = *self.updates.lock().expect("updates lock").get(&pack.dir)?;
        (newest > pack.data_at?).then_some(newest)
    }

    /// Where the build writes. One region names its own Pack; several go
    /// under the name typed for them, falling back to their slugs joined.
    pub(crate) fn pack_dir(&self) -> PathBuf {
        let regions = self.build_regions();
        match regions.as_slice() {
            [region] => self.home.pack_dir(region),
            _ if !self.pack_name.trim().is_empty() => {
                self.home.named_pack_dir(self.pack_name.trim())
            }
            many => self.home.named_pack_dir(
                &many
                    .iter()
                    .map(|region| region.slug())
                    .collect::<Vec<_>>()
                    .join("-"),
            ),
        }
    }

    /// What the build reads, with the extracts it must fetch first. The one
    /// place that decides it, so the confirm screen and the build itself
    /// never disagree.
    pub(crate) fn build_inputs(&self) -> Vec<BuildInput> {
        self.build_regions()
            .into_iter()
            .map(|region| BuildInput {
                extract: self.home.extract_path(region),
                download: self.force_download || self.home.extract(region).is_none(),
                region,
            })
            .collect()
    }

    /// Whether the build downloads the extract first.
    pub(crate) fn downloads(&self) -> bool {
        self.force_download || self.on_disk.is_none()
    }

    /// The addresses OpenAddresses adds, at most, when it is turned on.
    pub(crate) fn imported_rows(&self) -> u64 {
        self.openaddresses_data()
            .filter(|_| self.openaddresses)
            .map_or(0, |data| data.rows)
    }

    /// The OpenAddresses data the build's regions hold, as the catalog counts
    /// it; `None` where it has none.
    pub(crate) fn openaddresses_data(&self) -> Option<regions::OpenAddresses> {
        self.build_regions()
            .iter()
            .filter_map(|region| region.openaddresses)
            .reduce(|total, data| regions::OpenAddresses {
                rows: total.rows + data.rows,
                bytes: total.bytes + data.bytes,
            })
    }

    /// Bytes to download: the live size when the server gave one.
    pub(crate) fn download_bytes(&self) -> u64 {
        let live = self.live.lock().expect("live lock");
        live.remote
            .and_then(|remote| remote.bytes)
            .unwrap_or(self.region.osm.bytes)
    }

    /// The extracts the build reads, in bytes. One region is measured from
    /// the copy on disk or the size its publisher states; several are
    /// measured from the catalog, which is what the picker showed.
    pub(crate) fn input_bytes(&self) -> u64 {
        let regions = self.build_regions();
        if regions.len() > 1 {
            return regions.iter().map(|region| region.osm.bytes).sum();
        }
        match (&self.on_disk, self.downloads()) {
            (Some(extract), false) => extract.bytes,
            _ => self.download_bytes(),
        }
    }

    /// How long the build takes, scaled from this machine's own builds.
    pub(crate) fn estimate(&self) -> Option<Estimate> {
        home::estimate(
            &self.history,
            self.input_bytes(),
            self.openaddresses && self.openaddresses_data().is_some(),
        )
    }

    pub(crate) fn estimate_for(&self, region: &Region) -> Option<Estimate> {
        home::estimate(&self.history, region.osm.bytes, false)
    }

    /// Plans the build of the chosen region as the builder will: the memory
    /// budget, the threads, and what memory and disk it needs. Forced, so
    /// the plan comes back with its shortfalls instead of failing on them.
    fn replan(&mut self) {
        let pack = self.pack_dir();
        self.plan = Some(
            plan_build(
                self.input_bytes(),
                self.imported_rows(),
                &pack,
                &pack,
                None,
                true,
            )
            .map_err(|error| format!("{error:#}")),
        );
    }

    /// What the build lacks: the plan's shortfalls, and room for the
    /// download where the extract is kept.
    pub(crate) fn shortfalls(&self) -> Vec<String> {
        let Some(Ok(plan)) = &self.plan else {
            return Vec::new();
        };
        let mut shortfalls = plan.shortfalls.clone();
        if self.downloads() {
            let extract = self.home.extract_path(self.region);
            let (what, mut needed) = ("the download", self.download_bytes());
            let what = if resources::same_volume(&extract, &self.pack_dir()) {
                needed += plan.scratch_disk_needed_bytes + plan.output_disk_needed_bytes;
                "the download and the build"
            } else {
                what
            };
            if let Some(free) = resources::free_disk(&extract)
                && needed > free
            {
                let dir = extract.parent().unwrap_or(&extract);
                shortfalls.push(format!(
                    "disk at {}: {what} need about {}, {} free",
                    dir.display(),
                    format::bytes(needed),
                    format::bytes(free)
                ));
            }
        }
        shortfalls
    }

    /// Whether the build can start: it was planned, and fits or is forced.
    pub(crate) fn can_build(&self) -> bool {
        matches!(self.plan, Some(Ok(_))) && (self.force || self.shortfalls().is_empty())
    }

    /// The equivalent non-interactive command for this build.
    pub(crate) fn command(&self) -> String {
        let mut command = format!("og build --region {}", self.region.id);
        if self.force_download {
            command.push_str(" --download");
        }
        if self.force {
            command.push_str(" --force");
        }
        if self.openaddresses && self.openaddresses_data().is_some() {
            command.push_str(" --openaddresses");
        }
        command
    }

    /// The region whose children the list shows.
    pub(crate) fn region_here(&self) -> &'static Region {
        let tree = regions::tree();
        self.region_path
            .last()
            .map_or_else(|| tree.root(), |index| tree.at(*index))
    }

    /// The rows of the region list. Browsing shows the region itself first,
    /// as the first row, then its children. A search shows every region whose
    /// name or alias matches, with the regions above it.
    pub(crate) fn region_rows(&self) -> Vec<RegionRow> {
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
    pub(crate) fn region_browsing(&self) -> bool {
        self.region_query.trim().is_empty()
    }

    /// Rows the list can show from the current scroll: browsing at the top
    /// spends one on the line under the region itself.
    pub(crate) fn region_capacity(&self) -> usize {
        if self.region_browsing() && self.region_scroll == 0 {
            REGION_ROWS - 1
        } else {
            REGION_ROWS
        }
    }

    /// The regions a build would cover: the ticked ones, or, when none are
    /// ticked, the one under the cursor. Always at least one.
    pub(crate) fn build_regions(&self) -> Vec<&'static Region> {
        let tree = regions::tree();
        if self.picked.is_empty() {
            return vec![self.region];
        }
        self.picked.iter().map(|index| tree.at(*index)).collect()
    }

    /// Whether this region is ticked.
    pub(crate) fn is_picked(&self, region: &Region) -> bool {
        self.picked.contains(&region.index)
    }

    /// Ticks or unticks the region under the cursor. Picking a region that
    /// contains, or lies in, one already ticked replaces it: an extract and
    /// the extract it is part of would index the same places twice, and the
    /// wider one already covers the narrower.
    ///
    /// Returns what to tell the person when a pick displaced others.
    pub(crate) fn toggle_picked(&mut self) -> Option<String> {
        let tree = regions::tree();
        let region = self
            .region_rows()
            .get(self.region_cursor)
            .map(|row| row.region)?;
        let index = region.index;
        if let Some(at) = self.picked.iter().position(|picked| *picked == index) {
            self.picked.remove(at);
            return None;
        }
        // Everything already ticked that this region contains or lies in.
        let overlapping: Vec<usize> = self
            .picked
            .iter()
            .copied()
            .filter(|picked| regions::overlaps(tree, region, tree.at(*picked)))
            .collect();
        self.picked.retain(|picked| !overlapping.contains(picked));
        self.picked.push(index);
        self.region = region;
        match overlapping.as_slice() {
            [] => None,
            picked => {
                let names = picked
                    .iter()
                    .map(|index| tree.at(*index).name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(format!(
                    "{} covers {names}, so {names} came off the list.",
                    region.name
                ))
            }
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
        self.picked.clear();
        self.pack_name.clear();
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
    /// and the build planned now, what the server has is asked in the
    /// background.
    fn open_confirm(&mut self) {
        self.on_disk = self.home.extract(self.region).map(|extract| OnDisk {
            data_at: open_geocode::builder::replication_timestamp(&extract.path)
                .ok()
                .flatten(),
            path: extract.path,
            bytes: extract.bytes,
        });
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
        self.force = false;
        self.replan();
        self.confirm_cursor = 0;
        self.screen = Screen::Confirm;
    }

    pub(crate) fn flash_text(&self) -> Option<&str> {
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

    pub(crate) fn tick(&mut self) {
        if self.job.is_some() && self.sampled.elapsed() >= Duration::from_secs(1) {
            self.heap = alloc::live_bytes().map(|bytes| bytes as u64);
            self.free_disk = resources::free_disk(&self.pack_dir());
            self.sampled = Instant::now();
        }
        // A stopped job that has cleaned up is let go of.
        self.stopping_builds.retain(|job| !job.is_finished());
        self.stopping_runs.retain(|run| !run.is_finished());
        self.tick_build();
        self.tick_batch();
        if self.batch_dirty {
            self.refresh_batch();
        }
        self.take_reply();
    }

    fn tick_build(&mut self) {
        let Some(job) = &self.job else {
            return;
        };
        // Steps that finished and warnings given since the last tick go
        // into history, in order.
        let inner = ui::content_width(self.width);
        let mut lines = Vec::new();
        {
            let warnings = job.progress.warnings.lock().expect("warnings lock");
            for warning in &warnings[self.warned..] {
                lines.extend(ui::warning_line(warning, inner));
            }
            self.warned = warnings.len();
        }
        {
            let steps = job.progress.steps.lock().expect("steps lock");
            while let Some(step) = steps.get(self.printed).filter(|step| step.secs.is_some()) {
                lines.extend(ui::step_done(step, inner));
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

    /// Takes the open Pack's answer to the newest question, once it is there.
    fn take_reply(&mut self) {
        let Some(reply) = self.asker.as_mut().and_then(Asker::take) else {
            return;
        };
        match reply {
            Reply::Search { answer, reading } => {
                self.reading = reading;
                self.show_answer(answer);
            }
            Reply::Reverse(answer) => {
                self.reading.clear();
                self.show_answer(answer);
            }
            Reply::Pasted { rows, note } => {
                // What the engine took for every row, also those answered
                // before.
                self.batch_millis = rows
                    .iter()
                    .filter_map(|row| row.answer.as_ref())
                    .map(|answer| answer.millis)
                    .sum();
                self.batch_rows = rows;
                self.batch_note = note;
                self.hit_cursor = self.hit_cursor.min(self.batch_rows.len().saturating_sub(1));
            }
            Reply::Addresses(hits) => {
                if let (Some(session), Some(used)) = (&mut self.batch, self.preview_for.take()) {
                    session.previews.insert(used, hits);
                }
            }
        }
    }

    fn show_answer(&mut self, answer: Result<Answer, String>) {
        self.hit_cursor = 0;
        match answer {
            Ok(answer) => {
                self.answer = Some(answer);
                self.query_error = None;
            }
            Err(error) => {
                self.answer = None;
                self.query_error = Some(error);
            }
        }
    }

    /// Whether the open Pack is still answering the newest question.
    pub(crate) fn asking(&self) -> bool {
        self.asker.as_ref().is_some_and(Asker::busy)
    }

    pub(crate) fn on_key(&mut self, key: KeyEvent) {
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
                // Updating rebuilds the Pack from every region it covers:
                // one Pack is one build, so its regions are re-picked whole.
                let covered = pack.regions.clone();
                let pack_name = pack.name.clone();
                match (covered.split_first(), self.update_for(pack)) {
                    (Some((first, rest)), Some(_)) => {
                        self.region = first;
                        self.region_path = regions::tree().ancestors(first);
                        self.picked = if rest.is_empty() {
                            Vec::new()
                        } else {
                            covered.iter().map(|region| region.index).collect()
                        };
                        self.pack_name = if rest.is_empty() {
                            String::new()
                        } else {
                            pack_name
                        };
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
                let engine = Arc::new(engine);
                self.asker = Some(Asker::new(Arc::clone(&engine)));
                self.engine = Some(engine);
                // Rows answered by another Pack are asked again.
                self.batch_dirty = true;
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
            // Space ticks a region for a Pack covering several. It comes
            // before the search box takes the key, which costs nothing: a
            // search never starts with a space.
            KeyCode::Char(' ') => {
                if let Some(displaced) = self.toggle_picked() {
                    self.flash(displaced);
                }
            }
            KeyCode::Enter => {
                // Ticked regions build together, whatever the cursor is on,
                // so enter on a parent does not open it into its children.
                if !self.picked.is_empty() {
                    self.region_query.clear();
                    self.open_confirm();
                    return;
                }
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
                    self.picked.clear();
                    self.pack_name.clear();
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
            KeyCode::Char('d') if self.on_disk.is_some() => {
                self.force_download ^= true;
                self.replan();
            }
            KeyCode::Char('f') => self.force ^= true,
            KeyCode::Char('o') if self.openaddresses_data().is_some() => {
                self.openaddresses ^= true;
                self.replan();
            }
            KeyCode::Enter if self.confirm_cursor == 1 => self.screen = Screen::Region,
            KeyCode::Enter if !self.can_build() => {
                self.flash(
                    "Too little memory or disk for this build: free some, or f to build anyway.",
                );
            }
            KeyCode::Enter => self.start_build(),
            KeyCode::Esc => self.screen = Screen::Region,
            _ => {}
        }
    }

    fn start_build(&mut self) {
        let pack = self.pack_dir();
        // Each region reads its own extract, and downloads it when it is
        // missing — or when this is an update, which refetches them all.
        let inputs = self.build_inputs();

        // A stopped build still removing its unfinished Pack generation, or
        // holding the download, would race this one.
        if self.stopping_builds.iter().any(|job| {
            inputs
                .iter()
                .any(|input| job.touches(&pack, &input.extract))
        }) {
            self.flash(format!(
                "The last build of {} is still stopping; start again in a moment.",
                self.region.name
            ));
            return;
        }
        let mut lines = ui::choices(self);
        lines.extend(ui::build_header(self));
        self.push_flow(lines);
        self.built = None;
        self.build_error = None;
        self.printed = 0;
        self.warned = 0;
        self.heap = alloc::live_bytes().map(|bytes| bytes as u64);
        self.free_disk = resources::free_disk(&pack);
        self.job = Some(BuildJob::start(
            BuildRequest {
                inputs,
                pack,
                memory_budget_bytes: None,
                scratch_budget_bytes: None,
                force: self.force,
                openaddresses: self.openaddresses && self.openaddresses_data().is_some(),
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
                    self.stopping_builds.push(job);
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
                        self.asker = None;
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
        self.hit_cursor = 0;
        self.set_mode(mode);
        self.screen = Screen::Query;
    }

    /// Switches the query shell's tab, and asks what the tab shows.
    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        if mode == Mode::Batch {
            self.batch_dirty = true;
        } else {
            self.refresh_query();
        }
    }

    /// Asks the Pack again after the query changed. The answer shown stays
    /// until the new one is there.
    fn refresh_query(&mut self) {
        let Some(asker) = &mut self.asker else {
            return;
        };
        let question = match self.mode {
            Mode::Search if !self.query.trim().is_empty() => {
                Some(Question::Search(self.query.clone()))
            }
            Mode::Reverse => {
                engine::point(&self.query).map(|(lat, lon)| Question::Reverse { lat, lon })
            }
            Mode::Search | Mode::Batch => None,
        };
        match question {
            Some(question) => asker.ask(question),
            None => {
                asker.forget();
                self.answer = None;
                self.reading.clear();
                self.query_error = None;
                self.hit_cursor = 0;
            }
        }
    }

    pub(crate) fn hits(&self) -> &[engine::Hit] {
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
                self.query.clear();
                self.set_mode(if self.mode == Mode::Reverse {
                    Mode::Search
                } else {
                    Mode::Reverse
                });
            }
            KeyCode::Tab => self.set_mode(self.mode.next()),
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
            KeyCode::Tab => self.set_mode(self.mode.next()),
            KeyCode::Char('r') if ctrl => {
                self.query.clear();
                self.set_mode(Mode::Reverse);
            }
            KeyCode::Char('u') if ctrl => self.edit_batch(String::clear),
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
            KeyCode::Char('j') if ctrl => self.edit_batch(|text| text.push('\n')),
            KeyCode::Enter => self.edit_batch(|text| text.push('\n')),
            KeyCode::Up if rows > 0 => self.hit_cursor = (self.hit_cursor + rows - 1) % rows,
            KeyCode::Down if rows > 0 => self.hit_cursor = (self.hit_cursor + 1) % rows,
            KeyCode::Esc => {
                if self.batch_text.is_empty() {
                    self.screen = Screen::Home;
                } else {
                    self.edit_batch(String::clear);
                }
            }
            KeyCode::Backspace => self.edit_batch(|text| {
                text.pop();
            }),
            KeyCode::Char(c) if !ctrl => self.edit_batch(|text| text.push(c)),
            _ => {}
        }
    }

    /// Changes the batch text. It is read again once per frame, not once
    /// per key: a paste sent as keys is thousands of them.
    fn edit_batch(&mut self, edit: impl FnOnce(&mut String)) {
        edit(&mut self.batch_text);
        self.batch_dirty = true;
    }

    /// Forgets the file waiting in the batch tab and clears the box.
    fn drop_batch_file(&mut self) {
        self.batch_candidate = None;
        self.batch_choice = 0;
        self.edit_batch(String::clear);
    }

    /// Reads the batch text again. A single line that names a file makes
    /// that file the candidate to continue with; anything else is read as
    /// rows, and the open Pack is asked for every address in it.
    fn refresh_batch(&mut self) {
        self.batch_dirty = false;
        let typed = self.batch_text.trim().trim_matches('"');
        if !typed.is_empty() && !typed.contains('\n') {
            let path = Path::new(typed);
            if self
                .batch_candidate
                .as_ref()
                .is_some_and(|file| file.path == path)
            {
                return;
            }
            if let Some(file) = BatchFile::from_path(path) {
                self.batch_candidate = Some(file);
                self.batch_choice = 0;
                self.batch_rows.clear();
                self.batch_note.clear();
                if let Some(asker) = &mut self.asker {
                    asker.forget();
                }
                return;
            }
        }
        self.batch_candidate = None;
        let Some(asker) = &mut self.asker else {
            return;
        };
        if self.batch_text.trim().is_empty() {
            asker.forget();
            self.batch_rows.clear();
            self.batch_note.clear();
            self.batch_millis = 0.0;
            self.hit_cursor = 0;
        } else {
            asker.ask(Question::Pasted(self.batch_text.clone()));
        }
    }

    /// Text pasted into the terminal as one piece.
    pub(crate) fn on_paste(&mut self, text: String) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        match self.screen {
            Screen::Query if self.mode == Mode::Batch => {
                self.edit_batch(|batch| batch.push_str(&text));
            }
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
        self.ask_preview();
    }

    /// Asks what a run would answer for the sample rows with the columns
    /// picked now, unless that is known.
    fn ask_preview(&mut self) {
        let (Some(session), Some(asker)) = (&self.batch, &mut self.asker) else {
            return;
        };
        if session.previews.contains_key(&session.used) {
            return;
        }
        let addresses = session
            .file
            .sample()
            .iter()
            .map(|row| session.query(row))
            .collect();
        asker.ask(Question::Addresses(addresses));
        self.preview_for = Some(session.used.clone());
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
                self.ask_preview();
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
                self.stopping_runs.push(run);
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
    pub(crate) fn json_text(&self) -> String {
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
            KeyCode::Char('y') => self.copy_json(),
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Query,
            _ => {}
        }
    }

    fn copy_json(&mut self) {
        let text = self.json_text();
        let lines = text.lines().count();
        if self.clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(board) => self.clipboard = Some(board),
                Err(error) => {
                    self.flash(format!("Could not copy: {error}"));
                    return;
                }
            }
        }
        let Some(board) = &mut self.clipboard else {
            return;
        };
        match board.set_text(text) {
            Ok(()) => self.flash(format!("Copied {lines} lines of GeoJSON.")),
            Err(error) => self.flash(format!("Could not copy: {error}")),
        }
    }
}

/// Matches a search against a region: every word of the query must occur in
/// the name or in the regions above it, so `ont canada` finds Ontario. Words
/// that start a word of the name rank first, then names that start with the
/// query, then shorter names. Returns the matched positions in the name, by
/// char.
fn find(query: &str, name: &str, trail: &str) -> Option<(Vec<usize>, i64)> {
    // Each lowercase char with the place of the char of the name it comes
    // from: lowercasing can make two chars of one (`İ`).
    let (lower, origins): (Vec<char>, Vec<usize>) = name
        .chars()
        .enumerate()
        .flat_map(|(at, c)| c.to_lowercase().map(move |lower| (lower, at)))
        .unzip();
    let trail = trail.to_lowercase();
    let mut positions = Vec::new();
    let mut score = 0;
    for word in query.to_lowercase().split_whitespace() {
        let word: Vec<char> = word.chars().collect();
        if let Some((at, word_start)) = locate(&lower, &word) {
            score += if word_start { 100 } else { 40 };
            if at == 0 {
                score += 50;
            }
            positions.extend(&origins[at..at + word.len()]);
        } else if trail.contains(&word.iter().collect::<String>()) {
            score += 10;
        } else {
            return None;
        }
    }
    positions.sort_unstable();
    positions.dedup();
    Some((positions, score * 1_000 - name.chars().count() as i64))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_the_chars_of_the_name_that_matched() {
        // `İ` lowercases to two chars; the match after it is still placed
        // on the name's own chars.
        let (positions, _) = find("stan", "İstanbul", "").expect("a match");
        assert_eq!(positions, [1, 2, 3, 4]);
        let name: Vec<char> = "İstanbul".chars().collect();
        assert_eq!(
            positions.iter().map(|at| name[*at]).collect::<String>(),
            "stan"
        );
    }
}
