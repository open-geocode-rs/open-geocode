//! `og build`'s lines on stderr: a line per finished step, and the running
//! step's line, redrawn in place on a terminal and printed at an interval
//! anywhere else, so a log gets a few lines and not every redraw.

use std::{
    io::{self, BufWriter, IsTerminal, Write},
    time::Duration,
};

use ratatui::crossterm::{
    cursor::MoveToColumn,
    queue,
    style::Print,
    terminal::{self, Clear, ClearType},
};

use crate::{
    format,
    job::{Readout, Step},
    ui::{SIZE_WIDTH, SPEED_WIDTH, TIME_WIDTH, VERB_WIDTH},
};

/// How often a log gets the running step's line.
const LOG_EVERY: Duration = Duration::from_secs(10);

/// `Downloading OpenStreetMap      52%  49.8 GB/95.7 GB    48 MB/s  15m50s`.
/// A step with no measure has its time passed where its finished line has
/// its time, so finishing changes only the verb.
pub(crate) fn running_line(step: &Step, readout: &Readout, name_width: usize) -> String {
    let head = format!("{:<VERB_WIDTH$} {:<name_width$}", step.doing, step.object);
    let line = match readout.ratio {
        Some(_) => format!(
            "{head}  {}  {:>SIZE_WIDTH$}{:>SPEED_WIDTH$}{:>TIME_WIDTH$}",
            readout.percent(),
            readout.size,
            readout.speed,
            readout.time
        ),
        None => format!("{head} {:>TIME_WIDTH$}", readout.time),
    };
    line.trim_end().to_string()
}

/// `Downloaded  OpenStreetMap         40s`.
pub(crate) fn finished_line(step: &Step, name_width: usize) -> String {
    format!(
        "{:<VERB_WIDTH$} {:<name_width$} {:>TIME_WIDTH$}",
        step.did,
        step.object,
        format::duration(step.secs.unwrap_or_default())
    )
}

/// Where the running step's line goes.
pub(crate) struct Status {
    terminal: bool,
    /// The line drawn in place now, on a terminal.
    shown: Option<String>,
    /// The step the log last got a line of, and how many intervals in.
    logged: Option<(usize, u64)>,
}

impl Status {
    pub(crate) fn new() -> Self {
        Self {
            terminal: io::stderr().is_terminal(),
            shown: None,
            logged: None,
        }
    }

    /// Prints a line that stays, above the running one.
    pub(crate) fn print(&mut self, line: &str) {
        self.clear();
        eprintln!("{line}");
    }

    /// Shows step `index`'s running line, `elapsed` into the step.
    pub(crate) fn update(&mut self, index: usize, elapsed: Duration, line: String) {
        if !self.terminal {
            if let Some(logged) = log_due(self.logged, index, elapsed) {
                self.logged = Some(logged);
                eprintln!("{line}");
            }
            return;
        }
        // A terminal that does not say its width reports zero.
        let line = match terminal::size() {
            Ok((columns, _)) if columns > 0 => fit(line, usize::from(columns)),
            _ => line,
        };
        if self.shown.as_ref() == Some(&line) {
            return;
        }
        // Drawn over the old line, then the rest of the row erased, in one
        // write: the line never blinks out, and a shorter one leaves nothing
        // of a longer one behind.
        let _ = draw(|out| {
            queue!(
                out,
                MoveToColumn(0),
                Print(&line),
                Clear(ClearType::UntilNewLine)
            )
        });
        self.shown = Some(line);
    }

    /// Erases the running line, so what is printed next starts on a clean
    /// line.
    pub(crate) fn clear(&mut self) {
        if self.shown.take().is_some() {
            let _ = draw(|out| queue!(out, MoveToColumn(0), Clear(ClearType::UntilNewLine)));
        }
    }
}

impl Drop for Status {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Sends what `commands` queue to stderr at once.
fn draw(commands: impl FnOnce(&mut BufWriter<io::StderrLock>) -> io::Result<()>) -> io::Result<()> {
    let mut out = BufWriter::new(io::stderr().lock());
    commands(&mut out)?;
    out.flush()
}

/// Cuts a line to fit a terminal `columns` wide, dropping whole columns
/// from the right while the start still fits. A line that wrapped would
/// leave its first row behind, as a carriage return goes back only to the
/// start of the last; the last column stays free, as some terminals wrap
/// on writing it.
fn fit(line: String, columns: usize) -> String {
    let room = columns.saturating_sub(1);
    if line.chars().count() <= room {
        return line;
    }
    let cut: String = line.chars().take(room).collect();
    // Columns are set apart by two spaces or more; words within one by one.
    if line.chars().nth(room) == Some(' ') {
        return cut.trim_end().to_string();
    }
    match cut.rfind("  ") {
        Some(gap) => cut[..gap].trim_end().to_string(),
        None => cut,
    }
}

/// The log's new mark when step `index`'s line is due, `elapsed` into it:
/// once each whole interval, none for a step that ends within its first.
fn log_due(logged: Option<(usize, u64)>, index: usize, elapsed: Duration) -> Option<(usize, u64)> {
    let marks = elapsed.as_secs() / LOG_EVERY.as_secs();
    let before = match logged {
        Some((step, marks)) if step == index => marks,
        _ => 0,
    };
    (marks > before).then_some((index, marks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::Unit;

    fn step(unit: Unit) -> Step {
        Step {
            did: "Downloaded",
            doing: "Downloading",
            object: "OpenStreetMap".to_string(),
            explain: ["", ""],
            unit,
            total: None,
            started: None,
            secs: None,
        }
    }

    fn readout(ratio: Option<f64>, size: &str, speed: &str, time: &str) -> Readout {
        Readout {
            ratio,
            size: size.to_string(),
            speed: speed.to_string(),
            time: time.to_string(),
        }
    }

    #[test]
    fn running_line_has_percent_size_speed_and_time_left() {
        let line = running_line(
            &step(Unit::Bytes),
            &readout(Some(0.52), "49.8 GB/95.7 GB", "48 MB/s", "15m50s"),
            16,
        );
        assert_eq!(
            line,
            "Downloading OpenStreetMap      52%  49.8 GB/95.7 GB    48 MB/s  15m50s"
        );
    }

    #[test]
    fn running_line_before_a_rate_ends_at_its_size() {
        let line = running_line(
            &step(Unit::Bytes),
            &readout(Some(0.0), "0 B/95.7 GB", "", ""),
            16,
        );
        assert_eq!(line, "Downloading OpenStreetMap       0%      0 B/95.7 GB");
    }

    #[test]
    fn unmeasured_step_keeps_its_time_where_the_finished_line_has_it() {
        let mut step = step(Unit::None);
        (step.did, step.doing, step.object) = ("Built", "Building", "Pack indexes".to_string());
        let running = running_line(&step, &readout(None, "", "", "12s"), 16);
        step.secs = Some(14.0);
        let finished = finished_line(&step, 16);
        assert_eq!(running, "Building    Pack indexes          12s");
        assert_eq!(finished, "Built       Pack indexes          14s");
    }

    #[test]
    fn finished_line_lines_up_with_the_running_one() {
        let mut step = step(Unit::Bytes);
        step.secs = Some(40.0);
        assert_eq!(
            finished_line(&step, 16),
            "Downloaded  OpenStreetMap         40s"
        );
        // A wider name pads every line to it.
        assert_eq!(
            finished_line(&step, 20),
            "Downloaded  OpenStreetMap             40s"
        );
    }

    #[test]
    fn fit_drops_whole_columns_and_keeps_the_last_column_free() {
        let line = "Downloading OpenStreetMap      5%    196 KB/3.5 MB   162 KB/s     20s";
        assert_eq!(fit(line.to_string(), 80), line);
        assert_eq!(fit(line.to_string(), 70), line);
        assert_eq!(
            fit(line.to_string(), 69),
            "Downloading OpenStreetMap      5%    196 KB/3.5 MB   162 KB/s"
        );
        assert_eq!(
            fit(line.to_string(), 51),
            "Downloading OpenStreetMap      5%    196 KB/3.5 MB"
        );
        assert_eq!(
            fit(line.to_string(), 40),
            "Downloading OpenStreetMap      5%"
        );
        // A start too long for any cut at a gap is cut where it ends.
        assert_eq!(fit(line.to_string(), 20), "Downloading OpenStr");
        assert_eq!(fit("Île de France".to_string(), 4), "Île");
        assert_eq!(fit(line.to_string(), 0), "");
    }

    #[test]
    fn log_gets_a_line_each_whole_interval_of_a_step() {
        let secs = Duration::from_secs;
        assert_eq!(log_due(None, 0, secs(0)), None);
        assert_eq!(log_due(None, 0, secs(9)), None);
        assert_eq!(log_due(None, 0, secs(10)), Some((0, 1)));
        assert_eq!(log_due(Some((0, 1)), 0, secs(19)), None);
        assert_eq!(log_due(Some((0, 1)), 0, secs(20)), Some((0, 2)));
        // A late look prints once, not once per missed interval.
        assert_eq!(log_due(Some((0, 2)), 0, secs(55)), Some((0, 5)));
        // A new step counts from its own start.
        assert_eq!(log_due(Some((0, 5)), 1, secs(3)), None);
        assert_eq!(log_due(Some((0, 5)), 1, secs(10)), Some((1, 1)));
    }
}
