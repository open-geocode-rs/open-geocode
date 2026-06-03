//! HTTP boundary policy for the public-demo Runtime.
//!
//! These modules implement the API-boundary decisions in ADR 0017 (Problem
//! Details errors, request-id propagation, method allow-listing, health
//! endpoints, and request bounds). They are deliberately separate from the
//! geocoder and Pack so the boundary contract can evolve without touching query
//! behavior.
//!
//! Note: PMTiles egress/abuse protection is intentionally NOT here. In the
//! public demo the basemap is served from Cloudflare R2, whose free egress
//! makes it a non-issue (ADR 0017 Decisions 6/35a, amended 2026-06-03); the
//! bundled Runtime serves the basemap file raw as a local-dev convenience.

pub(crate) mod bounds;
pub(crate) mod health;
pub(crate) mod method;
pub(crate) mod problem;
pub(crate) mod request_id;
