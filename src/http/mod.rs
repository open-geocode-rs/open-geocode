//! HTTP boundary policy for the public-demo Runtime.
//!
//! These modules implement the API-boundary decisions in ADR 0017 (Problem
//! Details errors, request-id propagation, method allow-listing, health
//! endpoints, request bounds, and the PMTiles full-file fetch guard). They are
//! deliberately separate from the geocoder and Pack so the boundary contract can
//! evolve without touching query behavior.

pub(crate) mod bounds;
pub(crate) mod health;
pub(crate) mod method;
pub(crate) mod pmtiles_guard;
pub(crate) mod problem;
pub(crate) mod request_id;
