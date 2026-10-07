//! Small crate-internal helpers shared across modules.
//!
//! These are deliberately generic (encoding, geometry, text) so that domain
//! modules depend on one source of truth instead of copy-pasting the same few
//! lines.

pub(crate) mod codec;
pub(crate) mod geo;
pub(crate) mod hilbert;
pub(crate) mod text;
