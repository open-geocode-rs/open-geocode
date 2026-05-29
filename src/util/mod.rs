//! Small crate-internal helpers shared across modules.
//!
//! These are deliberately generic (string/fs/geometry) so that domain modules
//! depend on one source of truth instead of copy-pasting the same few lines.

pub(crate) mod fs;
pub(crate) mod geo;
pub(crate) mod text;
