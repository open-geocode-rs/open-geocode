# open-geocode

Fast, lightweight, self-hosted geocoding in pure Rust.

`open-geocode` is a minimal Rust-native geocoding engine: address search,
Tantivy-backed autocomplete, and reverse geocoding from coordinates to
address-first location context. It turns OpenStreetMap PBF extracts into a
single compact Pack file holding a memory-mapped record store, a Tantivy text
index, and an H3-backed spatial index, with no database or search cluster to run.

## Why open-geocode?

Address data does not change minute to minute. You build it from an OSM extract
and rebuild it when a newer one comes out, so it behaves like a lookup table
rather than a live document you keep editing. The standard open-source options,
Nominatim and Pelias, serve it from stateful systems that stay running, a
PostgreSQL/PostGIS database or an Elasticsearch cluster.

open-geocode skips that. It compiles the extract once into one Pack file and
serves it from a single small binary. No database, no Java, no cluster to
babysit. To update, you build a new Pack and swap it in. And because a Pack is
just a file, you can copy it, cache it, or ship it to the edge like any other
static asset.

open-geocode is stateless and read-only: it memory-maps the Pack for
zero-overhead lookups, so it can run on scale-to-zero platforms like Cloud Run or
Fly Machines and idle at near-zero cost between bursts. Always-on PostgreSQL or
Elasticsearch can't scale to zero, so for spiky or low-traffic workloads, og can
be far cheaper to run.

## Quickstart

Building the Ontario pack (~940 MB PBF) takes under a minute on a 24-core machine and produces a ~300 MB Pack.

1. Download an OSM extract, for example Ontario from [Geofabrik](https://download.geofabrik.de/north-america/canada/ontario.html), and save it as `data/ontario.pbf`.
2. Build the pack:
   ```
   cargo run --release -- build --input data/ontario.pbf --pack data/pack
   ```
3. Serve the API and demo UI on `http://127.0.0.1:8080`:
   ```
   cargo run --release -- serve --pack data/pack
   ```

## What's next

I'm actively working on turning open-geocode into a free batch geocoder for the whole planet: drop in a spreadsheet of addresses, get it back with coordinates.

Think Adobe's file converter, but for addresses, and for the whole planet.

## Rebuilding packs safely

Rebuilding the same `--pack` path writes a separate version under `generations/` and updates `CURRENT` only after the new version finishes and passes validation.
A failed build removes its partial output and leaves the previous version available.
Existing servers keep using the version they opened; restart the server to use the replacement.
Published versions are retained, so repeated builds use additional disk space.

A Pack is one file, `generations/<id>/pack.ogp`.
Every command that takes `--pack` accepts either the Pack directory or that file, so you can copy, cache, or upload the file on its own.
The build audit (`audit/rejections.jsonl` and `audit/build-report.json`) sits next to the file but is not part of the Pack you serve.

## Hosted demo

The public live demo and its Cloudflare Worker + Tunnel + VM deployment live in a
separate repo, `open-geocode-live`. None of it is needed to run open-geocode
locally.

## Benchmarks

Three geocoders on the same inputs and resources: **open-geocode**, **Nominatim**,
and **Pelias**, each serving the same Ontario OSM extract (~2.03M addresses) in
Docker, one at a time, capped at an identical 4 CPU / 10 GB, over loopback HTTP.
Host: 24-core / 32 GB, Windows 11 + Docker Desktop:

- **Service time**: closed-loop, single client: time for one request with **no
  queue**, so it can't be inflated by overload. This is the honest latency.
- **Throughput**: open-loop: max sustained arrival rate holding **p99 < 100 ms**.

| | open-geocode | Pelias | Nominatim |
| --- | ---: | ---: | ---: |
| Deployable footprint ³ | **1.28 GiB** | 5.88 GiB | 5.92 GiB |
| RAM while serving | **~1.2 GiB** ¹ | 4.60 GiB | 2.75 GiB |
| Forward service-time p50 / p99 | **1.0 / 2.0 ms** | 11.5 / 67.6 ms | 40.0 / 1384.7 ms |
| Reverse service-time p50 / p99 | **1.0 / 1.5 ms** | 5.0 / 9.5 ms | 7.5 / 14.2 ms |
| Forward throughput (req/s @ p99<100ms) | **1000** ² | 100 | 25 |
| Reverse throughput (req/s @ p99<100ms) | **1000** ² | 500 | 100 |
| Addresses indexed | 2,032,851 | 2,036,529 | 2,037,682 |

Single binary, no database or cluster: **~4.6× smaller on disk, lower memory use,
~10–40× higher forward throughput, sub-millisecond latency.**

### Notes

- ¹ open-geocode memory-maps its pack, so process RSS is only ~0.03 GiB but ~1.2 GiB figure is the working set: resident in the OS page cache
- ² open-geocode's true throughput ceiling is higher; the load grid stopped at 1000.
- ³ Measured with the previous multi-file Pack format, where the Ontario Pack was about 1.2 GB.
  The single-file Pack for the same extract is about 0.3 GB; the serving benchmarks have not been re-run with it yet.
- Engines do unequal work per query (Pelias libpostal parsing, Nominatim hierarchy); reported, not corrected.
- Nominatim's forward p99 is a genuine heavy tail under the cap (p50 is 40 ms), not overload.
- Single host: CPU capped, cache/disk shared; ratios travel better than absolute numbers.
- Only the ~2.03M addresses are directly comparable; each engine indexes different extra layers.

## Use Cases

| Capability | Example use case |
|---|---|
| Forward geocoding | Turn customer, store, vendor, or service addresses into coordinates |
| Reverse geocoding | Convert fleet, delivery, device, or field-work GPS pings into readable locations using H3 candidate lookup and address-first gates |
| Autocomplete | Power address forms, checkout flows, internal tools, and store locators with Tantivy-native prefix queries |
| Batch geocoding | Enrich CSVs, database tables, and large address lists without per-row API pricing |
| Search optimization | Handle messy addresses, abbreviations, partial queries, field-aware matches, interpolation ranges, and ranked candidates |
| Private data | Geocode internal addresses, custom places, service zones, or proprietary datasets |

## License

`open-geocode` is licensed under the [MIT License](LICENSE).

Third-party code dependencies remain under their own OSS licenses. Generated
Packs preserve source metadata needed for attribution and auditability; users are
responsible for following the license terms of the geospatial data they build
from.
