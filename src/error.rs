//! The crate's error type.
//!
//! Variants are grouped by what a caller can do about them: fix the request,
//! rebuild or redownload the Pack, report a build bug, or give up. The groups
//! are what `http::problem` maps to status codes, so a new variant has to be
//! classified there before it compiles.

use std::{borrow::Cow, path::PathBuf};

/// Anything the library can fail with.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    // --- the caller asked for something impossible ----------------------
    /// A query with nothing in it to search for.
    #[error("search query is empty")]
    EmptyQuery,

    /// A query the index cannot be asked about, after parsing.
    #[error("cannot parse search query {query:?}: it has no words to search")]
    UnparsableQuery { query: String },

    /// A coordinate that is NaN or infinite.
    #[error("coordinate must be finite")]
    CoordinateNotFinite,

    /// Longitude outside -180..180, or latitude outside -90..90.
    #[error("coordinate {value} is out of range")]
    CoordinateOutOfRange { value: f64 },

    /// Options that contradict each other, named as the caller wrote them.
    #[error("{0}")]
    BadOptions(Cow<'static, str>),

    /// A CSV column the options name but the input does not have.
    #[error("{input} has no column {column}")]
    MissingColumn { input: PathBuf, column: String },

    // --- the Pack cannot be used ----------------------------------------
    /// The file does not start with the Pack magic.
    #[error("not an open-geocode Pack file")]
    NotAPack,

    /// A Pack written against a different container format.
    #[error("Pack container version {found} is unsupported; rebuild the Pack")]
    UnsupportedVersion { found: u32 },

    /// A section's bytes do not hash to what the table of contents recorded.
    #[error("Pack section {section} is corrupt: its checksum does not match")]
    ChecksumMismatch { section: String },

    /// The file ends before its table of contents says it should.
    #[error("Pack file is truncated or still being written")]
    Truncated,

    /// A section a reader needs is not in the file.
    #[error("Pack has no {section}")]
    MissingSection { section: &'static str },

    /// Two sections share one name.
    #[error("duplicate Pack section {section}")]
    DuplicateSection { section: String },

    /// A section's recorded extent falls outside the file.
    #[error("Pack section {section} is out of bounds")]
    SectionOutOfBounds { section: String },

    /// The table of contents does not account for every byte.
    #[error("Pack table of contents has trailing bytes")]
    TrailingBytes,

    /// The manifest disagrees with the sections beside it.
    #[error("Pack manifest does not match the build")]
    ManifestMismatch,

    // --- the Pack's structures disagree with each other ------------------
    /// An index, table or block that does not match what it indexes. The
    /// detail says which; no caller can do more than rebuild either way.
    #[error("Pack is inconsistent: {0}")]
    Inconsistent(String),

    /// An id past the end of the table it indexes.
    #[error("{what} id {id} is out of range")]
    IdOutOfRange { what: &'static str, id: u64 },

    /// Encoded bytes that do not decode: truncated, over-long, or a tag
    /// this build does not know.
    #[error("Pack data is malformed: {0}")]
    Decode(&'static str),

    /// Stored geometry that does not decode.
    #[error("stored geometry is invalid: {0}")]
    BadGeometry(String),

    // --- the build could not finish --------------------------------------
    /// A line with more segments than the spatial index can address.
    #[error("line record {id} has too many segments to index")]
    TooManySegments { id: String },

    /// A spill file came back malformed, which means the disk or the
    /// encoding lied rather than that the input was bad.
    #[error("scratch file {path} is corrupt: {detail}")]
    BadScratch { path: PathBuf, detail: &'static str },

    /// The machine ran out of a resource the build needs. The message says
    /// which and what to do, because only the operator can act on it.
    #[error("{0}")]
    OutOfResources(String),

    /// The input extract is not in the layout the build reads.
    #[error("{0}")]
    BadInput(String),

    /// A training or fixture row that does not parse.
    #[error("{0}")]
    BadFixtureRow(String),

    // --- the caller stopped the run --------------------------------------
    /// The cancel flag was set, so the run wrote nothing.
    #[error("the run was cancelled")]
    Cancelled,

    // --- from below -------------------------------------------------------
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// An io failure where the path is worth keeping: the message otherwise
    /// says only "No such file" with nothing to act on.
    #[error("{action} {path}: {source}")]
    IoPath {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("search index error: {0}")]
    Index(#[from] tantivy::TantivyError),

    #[error("reading the OSM extract failed: {0}")]
    Osm(#[from] osmpbf::Error),

    #[error("csv error: {0}")]
    Csv(#[from] csv::Error),

    #[error("dictionary error: {0}")]
    Fst(#[from] fst::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<crate::builder::progress::BuildCancelled> for Error {
    fn from(_: crate::builder::progress::BuildCancelled) -> Self {
        Self::Cancelled
    }
}

impl From<crate::batch::Cancelled> for Error {
    fn from(_: crate::batch::Cancelled) -> Self {
        Self::Cancelled
    }
}

impl From<std::num::TryFromIntError> for Error {
    fn from(_: std::num::TryFromIntError) -> Self {
        Self::Decode("a stored number does not fit this build's type")
    }
}

/// The crate's result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// Whether the caller sent something the library could never answer, as
    /// opposed to the Pack or the machine being at fault. `http::problem`
    /// turns this into 400 against 500.
    #[must_use]
    pub fn is_caller_error(&self) -> bool {
        matches!(
            self,
            Self::EmptyQuery
                | Self::UnparsableQuery { .. }
                | Self::CoordinateNotFinite
                | Self::CoordinateOutOfRange { .. }
                | Self::BadOptions(_)
                | Self::MissingColumn { .. }
        )
    }
}
