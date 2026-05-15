use indicatif::{ProgressBar, ProgressStyle};

pub(crate) fn byte_progress_bar(len: u64, message: &'static str) -> ProgressBar {
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
    let progress = ProgressBar::new(len);
    progress.set_style(
        ProgressStyle::with_template(
            "{msg:32} [{bar:40.cyan/blue}] {percent:>3}% {pos}/{len} elapsed {elapsed_precise}",
        )
        .expect("valid item progress template")
        .progress_chars("=> "),
    );
    progress.set_message(message);
    progress
}
