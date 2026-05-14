# open-geocode

Fast, self-hosted geocoding for address search, autocomplete, reverse geocoding,
and batch workflows.

`open-geocode` builds compact region packs from open and private location data,
then serves geocoding APIs from a single Rust binary. It is designed for teams
that need predictable infrastructure, private data handling, and clear result
confidence for operational location workflows.

## Features

- Forward geocoding for addresses, streets, postcodes, and admin areas
- Reverse geocoding for GPS points, fleet events, delivery scans, and field work
- Autocomplete for internal tools, store locators, and address forms
- Batch geocoding for CSV files, tables, and API streams
- Source, precision, and confidence metadata on every result
- Static region packs built offline and served by a lightweight runtime
- Embedded search powered by Tantivy

## Quick Start

Build a region pack:

```sh
open-geocode build \
  --input ./data \
  --region us-ca \
  --output ./packs/us-ca
```

Start the API server:

```sh
open-geocode serve ./packs/us-ca --listen 127.0.0.1:8080
```

Search for an address:

```sh
curl 'http://127.0.0.1:8080/search?q=1600+Amphitheatre+Parkway,+Mountain+View,+CA'
```

Reverse geocode a point:

```sh
curl 'http://127.0.0.1:8080/reverse?lat=37.422&lon=-122.084'
```

Run a batch job:

```sh
open-geocode batch \
  --pack ./packs/us-ca \
  --input ./addresses.csv \
  --output ./geocoded.csv
```

## Architecture

```text
source data
  -> normalize
  -> index
  -> write region pack
  -> serve search, reverse, autocomplete, and batch APIs
```

The offline builder handles imports, normalization, deduplication, scoring, and
index creation. The runtime loads finished region packs and serves requests
without PostgreSQL/PostGIS, Elasticsearch/OpenSearch, Redis, or JVM services.

## Repository Layout

```text
Cargo.toml             # Cargo workspace
rust-toolchain.toml    # Rust toolchain configuration
crates/                # internal Rust crates
benches/               # benchmark harnesses and scenarios
docs/                  # product and architecture notes
fixtures/              # test datasets and sample inputs
docker/                # container and deployment assets
.github/workflows/     # CI workflows
```

## Benchmarks

Benchmarks track import time, disk size, memory usage, P50/P95 latency, QPS,
batch throughput, match rate, and confidence calibration against established
open-source geocoders.

## Documentation

- [Product thesis](docs/why.md)
- [Architecture direction](docs/spec.md)
