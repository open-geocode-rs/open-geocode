//! HTTP boundary policy for the public-demo Runtime.
//!
//! These modules implement the API-boundary decisions in ADR 0017 (Problem
//! Details errors, request-id propagation, method allow-listing, health
//! endpoints, and request bounds). They are deliberately separate from the
//! geocoder and Pack so the boundary contract can evolve without touching query
//! behavior.
//!
//! Note: PMTiles egress/abuse protection is intentionally NOT here. It is a
//! deployment-layer concern handled by nginx in the public demo (ADR 0017
//! Decision 35a); the bundled Runtime serves the basemap file raw.

pub(crate) mod bounds;
pub(crate) mod health;
pub(crate) mod method;
pub(crate) mod problem;
pub(crate) mod request_id;
