//! What one server lets its clients ask of it: the capacity limits an
//! operator sets for their machine and their users, each a `serve` flag and an
//! `OG_*` environment variable.
//!
//! Bounds that guard parsing rather than capacity, such as the longest query,
//! are fixed in `http::bounds`.

use std::{
    num::{NonZeroU64, NonZeroUsize},
    time::Duration,
};

use crate::http::bounds::BATCH_ITEM_BYTES;

/// Bytes an average job row may take when the upload size follows the row
/// cap: an address with an id and a few columns beside it.
const JOB_ROW_BYTES: u64 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::Args)]
pub struct Limits {
    /// Longest one lookup may run, in milliseconds: a request, a batch item
    /// or a job row. Past it the lookup stops and answers 503 `timeout`.
    #[arg(long, env = "OG_LOOKUP_TIMEOUT_MS", value_name = "MS",
          default_value_t = Limits::DEFAULT.lookup_timeout_ms)]
    pub lookup_timeout_ms: NonZeroU64,

    /// Most items one QUERY or POST batch may hold.
    #[arg(long, env = "OG_MAX_BATCH_ITEMS", value_name = "COUNT",
          default_value_t = Limits::DEFAULT.max_batch_items)]
    pub max_batch_items: NonZeroUsize,

    /// Most rows one /jobs upload may hold.
    #[arg(long, env = "OG_MAX_JOB_ROWS", value_name = "COUNT",
          default_value_t = Limits::DEFAULT.max_job_rows)]
    pub max_job_rows: NonZeroUsize,

    /// Largest /jobs upload, in MiB. Defaults to 256 bytes a row of
    /// --max-job-rows.
    #[arg(long, env = "OG_MAX_JOB_UPLOAD_MIB", value_name = "MIB")]
    pub max_job_upload_mib: Option<NonZeroU64>,

    /// Hours a job's results are kept after it stops.
    #[arg(long, env = "OG_JOB_RETENTION_HOURS", value_name = "HOURS",
          default_value_t = Limits::DEFAULT.job_retention_hours)]
    pub job_retention_hours: NonZeroU64,

    /// Most GiB the results of every kept job may take on disk. While they
    /// take more, a new job is refused with 503 `busy`.
    #[arg(long, env = "OG_MAX_JOB_STORE_GIB", value_name = "GIB",
          default_value_t = Limits::DEFAULT.max_job_store_gib)]
    pub max_job_store_gib: NonZeroU64,
}

impl Limits {
    pub const DEFAULT: Self = Self {
        lookup_timeout_ms: NonZeroU64::new(1000).unwrap(),
        max_batch_items: NonZeroUsize::new(1000).unwrap(),
        max_job_rows: NonZeroUsize::new(1_000_000).unwrap(),
        max_job_upload_mib: None,
        job_retention_hours: NonZeroU64::new(72).unwrap(),
        max_job_store_gib: NonZeroU64::new(10).unwrap(),
    };

    pub(crate) fn lookup_timeout(&self) -> Duration {
        Duration::from_millis(self.lookup_timeout_ms.get())
    }

    pub(crate) fn max_batch_items(&self) -> usize {
        self.max_batch_items.get()
    }

    pub(crate) fn max_batch_body_bytes(&self) -> usize {
        self.max_batch_items().saturating_mul(BATCH_ITEM_BYTES)
    }

    pub(crate) fn max_job_rows(&self) -> usize {
        self.max_job_rows.get()
    }

    pub(crate) fn max_job_upload_bytes(&self) -> u64 {
        self.max_job_upload_mib.map_or_else(
            || (self.max_job_rows() as u64).saturating_mul(JOB_ROW_BYTES),
            |mib| mib.get().saturating_mul(1 << 20),
        )
    }

    pub(crate) fn job_retention(&self) -> Duration {
        Duration::from_secs(self.job_retention_hours.get().saturating_mul(3600))
    }

    pub(crate) fn max_job_store_bytes(&self) -> u64 {
        self.max_job_store_gib.get().saturating_mul(1 << 30)
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Serve {
        #[command(flatten)]
        limits: Limits,
    }

    fn parse(args: &[&str]) -> Result<Limits, clap::Error> {
        Serve::try_parse_from(std::iter::once("serve").chain(args.iter().copied()))
            .map(|serve| serve.limits)
    }

    #[test]
    fn no_flags_are_the_defaults() {
        assert_eq!(parse(&[]).expect("parse"), Limits::DEFAULT);
        let limits = Limits::DEFAULT;
        assert_eq!(limits.lookup_timeout(), Duration::from_secs(1));
        assert_eq!(limits.max_batch_body_bytes(), 1000 * 4096);
        assert_eq!(limits.max_job_upload_bytes(), 256_000_000);
        assert_eq!(limits.job_retention(), Duration::from_secs(72 * 3600));
        assert_eq!(limits.max_job_store_bytes(), 10 << 30);
    }

    #[test]
    fn the_upload_cap_follows_the_row_cap_until_it_is_set() {
        let demo = parse(&["--max-job-rows", "100000"]).expect("parse");
        assert_eq!(demo.max_job_upload_bytes(), 25_600_000);
        let set =
            parse(&["--max-job-rows", "100000", "--max-job-upload-mib", "64"]).expect("parse");
        assert_eq!(set.max_job_upload_bytes(), 64 << 20);
    }

    #[test]
    fn a_zero_limit_is_refused() {
        for flag in [
            "--lookup-timeout-ms",
            "--max-batch-items",
            "--max-job-rows",
            "--max-job-upload-mib",
            "--job-retention-hours",
            "--max-job-store-gib",
        ] {
            assert!(parse(&[flag, "0"]).is_err(), "{flag}");
        }
    }
}
