use std::{
    io::IsTerminal,
    sync::OnceLock,
    time::{Duration, Instant},
};

use indicatif::{ProgressBar, ProgressStyle};

/// Write a line naming the stage that starts when stderr is no terminal, so a
/// build run from a script or a service logs where its time and memory go
/// instead of drawing nothing.
fn log_stage(message: &str) {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    if !std::io::stderr().is_terminal() {
        eprintln!("[{:>7.1}s] {message}", start.elapsed().as_secs_f64());
    }
}

pub(crate) fn byte_progress_bar(len: u64, message: &'static str) -> ProgressBar {
    log_stage(message);
    let progress = ProgressBar::new(len);
    progress.set_style(
        ProgressStyle::with_template(
            "{msg:32} [{bar:40.cyan/blue}] {percent:>3}% {bytes}/{total_bytes} {bytes_per_sec} elapsed {elapsed_precise}",
        )
        .expect("valid byte progress template")
        .progress_chars("=> "),
    );
    progress.set_message(message);
    progress
}

pub(crate) fn item_progress_bar(len: u64, message: &'static str) -> ProgressBar {
    log_stage(message);
    let progress = ProgressBar::new(len);
    progress.set_style(
        ProgressStyle::with_template(
            "{msg:32} [{bar:40.cyan/blue}] {percent:>3}% {pos}/{len} {per_sec} elapsed {elapsed_precise}",
        )
        .expect("valid item progress template")
        .progress_chars("=> "),
    );
    progress.set_message(message);
    progress
}

pub(crate) fn stage_progress(message: &'static str) -> ProgressBar {
    log_stage(message);
    let progress = ProgressBar::new_spinner();
    progress.set_style(
        ProgressStyle::with_template("{msg:32} {spinner} elapsed {elapsed_precise}")
            .expect("valid stage progress template"),
    );
    progress.set_message(message);
    progress.enable_steady_tick(Duration::from_millis(120));
    progress
}
