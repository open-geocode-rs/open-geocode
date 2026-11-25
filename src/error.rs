use std::{borrow::Cow, path::PathBuf};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("search query is empty")]
    EmptyQuery,

    #[error("cannot parse search query {query:?}: it has no words to search")]
    UnparsableQuery { query: String },

    #[error("coordinate must be finite")]
    CoordinateNotFinite,

    /// Longitude outside -180..180, or latitude outside -90..90.
    #[error("coordinate {value} is out of range")]
    CoordinateOutOfRange { value: f64 },

    #[error("{0}")]
    BadOptions(Cow<'static, str>),

    #[error("{input} has no column {column}")]
    MissingColumn { input: PathBuf, column: String },

    #[error("not an open-geocode Pack file")]
    NotAPack,

    #[error("Pack container version {found} is unsupported; rebuild the Pack")]
    UnsupportedVersion { found: u32 },

    #[error("the Pack's {part} is format {found} and this og reads {expected}; rebuild the Pack")]
    UnsupportedFormat {
        part: String,
        found: u32,
        expected: u32,
    },

    #[error("Pack section {section} is corrupt: its checksum does not match")]
    ChecksumMismatch { section: String },

    #[error("Pack file is truncated or still being written")]
    Truncated,

    #[error("Pack has no {section}")]
    MissingSection { section: &'static str },

    #[error("duplicate Pack section {section}")]
    DuplicateSection { section: String },

    #[error("Pack section {section} is out of bounds")]
    SectionOutOfBounds { section: String },

    #[error("Pack table of contents has trailing bytes")]
    TrailingBytes,

    #[error("Pack manifest does not match the build")]
    ManifestMismatch,

    /// An index, table or block that does not match what it indexes. The
    /// detail says which; no caller can do more than rebuild either way.
    #[error("Pack is inconsistent: {0}")]
    Inconsistent(String),

    #[error("{what} id {id} is out of range")]
    IdOutOfRange { what: &'static str, id: u64 },

    #[error("Pack data is malformed: {0}")]
    Decode(&'static str),

    #[error("stored geometry is invalid: {0}")]
    BadGeometry(String),

    #[error("line record {id} has too many segments to index")]
    TooManySegments { id: String },

    #[error("scratch file {path} is corrupt: {detail}")]
    BadScratch { path: PathBuf, detail: &'static str },

    /// The machine ran out of a resource the build needs. The message says
    /// which and what to do, because only the operator can act on it.
    #[error("{0}")]
    OutOfResources(String),

    #[error("fetching {url} failed: {message}")]
    Fetch { url: String, message: String },

    #[error("{0}")]
    BadInput(String),

    #[error("{0}")]
    BadFixtureRow(String),

    /// The cancel flag was set, so the run wrote nothing.
    #[error("the run was cancelled")]
    Cancelled,

    #[error("the lookup took longer than {} ms", after.as_millis())]
    TimedOut { after: std::time::Duration },

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
