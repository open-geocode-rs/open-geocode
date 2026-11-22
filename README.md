<p align="center">
  <a href="https://opengeocode.com">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.png">
      <img src="assets/logo-light.png" alt="open-geocode" width="220">
    </picture>
  </a>
</p>

<h1 align="center">open-geocode</h1>

<p align="center">Fast, lightweight, self-hosted geocoding in pure Rust.</p>

<p align="center">
  <a href="https://github.com/open-geocode-rs/open-geocode/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/open-geocode-rs/open-geocode/ci.yml?branch=main&style=flat-square&label=ci" alt="CI"></a>
  <a href="https://crates.io/crates/open-geocode"><img src="https://img.shields.io/crates/v/open-geocode.svg?style=flat-square" alt="crates.io"></a>
  <a href="https://docs.rs/open-geocode"><img src="https://img.shields.io/docsrs/open-geocode?style=flat-square" alt="docs.rs"></a>
  <a href="LICENSE"><img src="https://img.shields.io/crates/l/open-geocode.svg?style=flat-square" alt="license"></a>
</p>

<p align="center">
  <a href="https://opengeocode.com/docs/"><b>Docs</b></a> •
  <a href="https://opengeocode.com/docs/quickstart/"><b>Quickstart</b></a> •
  <a href="https://opengeocode.com/docs/api/"><b>API</b></a> •
  <a href="https://opengeocode.com/docs/benchmarks/"><b>Benchmarks</b></a>
</p>

`open-geocode` is a self-hosted address geocoding engine built entirely on open data (OpenStreetMap and OpenAddresses).
Forward, autocomplete, reverse and batch geocoding, served from one file.

Country-scale indexes build in minutes and serve 6× the forward and over 20× the reverse queries per second of Photon and Pelias, on a quarter of the memory.
og is an alternative to commercial API quotas and to the days-long builds of Pelias and Nominatim, whether for a single region or for global geocoding.

**Features**

- **Parser:** semi-Markov CRF, trained on every build from the addresses it serves.
- **Search:** Tantivy index and FST dictionaries, with Levenshtein-automata typo tolerance and Unicode transliteration across scripts. Free-form or structured input.
- **Reverse:** Hilbert-packed R-trees and point-in-polygon on real admin boundaries, returning the nearest address with its full locality, region and country.
- **Results:** calibrated confidence, plus a rooftop, interpolated or street-level accuracy tier.

## Why open-geocode?

Geocoding your address data through a commercial API means per-lookup fees after the free tier, rate limits, and every address sent to a third party.

Nominatim and Pelias, the open-source options, have no batch endpoint and are hard to self-host.
Nominatim needs PostgreSQL with PostGIS, and a planet import takes days on a machine with 64 GB of RAM.

open-geocode runs on your own hardware: drop in a CSV, get it back with coordinates, and no address leaves your network.

There is no PostgreSQL or Elasticsearch to keep running, and an update is a new file swapped in.
Because the file is memory-mapped, og runs on scale-to-zero platforms like Cloud Run or Fly Machines and costs next to nothing between batches.

## Benchmarks

### Accuracy

[MESSY STREETS](https://arxiv.org/abs/2609.01612) (Gaere and von Wangenheim, SIGSPATIAL 2026) is a benchmark of verbatim addresses taken from the web, each verified to exist.
The paper scored 12 geocoders on its gold tier: 10 runs of 100 rows at each resolution, 959 distinct inputs.
og was given the same rows, as one free-text query each with default settings, as the paper queried the others.
Ranked by CRR, as the paper ranks them:

| Geocoder | CRR (%) | GH1 (%) ±2500 km | GH4 (%) ±20 km | GH6 (%) ±0.6 km |
| --- | ---: | ---: | ---: | ---: |
| Google Maps v3 | 100.0±0.0 | 100.0±0.0 | 98.6±0.7 | 88.3±2.0 |
| MapQuest v1 | 100.0±0.0 | 98.5±0.5 | 97.1±0.5 | 84.0±1.5 |
| **open-geocode** † * | **100.0** | **99.1** | **93.9** | **78.0** |
| ArcGIS | 99.8±0.3 | 99.8±0.3 | 97.9±1.3 | 85.3±2.8 |
| Geoapify v1 | 99.7±0.4 | 99.2±0.5 | 96.3±1.2 | 79.1±3.7 |
| Here v7 | 99.1±0.6 | 99.0±0.5 | 97.6±0.5 | 85.1±2.0 |
| Mapbox v5 | 99.1±0.5 | 98.6±0.7 | 95.1±1.5 | 81.0±2.9 |
| OpenCage v1 | 98.4±1.0 | 97.4±1.0 | 90.2±1.8 | 68.2±2.9 |
| Azure Maps | 97.2±0.8 | 97.1±0.9 | 94.8±1.5 | 81.3±3.7 |
| Pelias v1 † | 96.6±1.8 | 94.1±2.5 | 85.9±3.3 | 67.8±4.4 |
| TomTom v2 | 94.0±1.3 | 93.9±1.4 | 91.8±1.7 | 79.4±3.6 |
| Photon † | 88.1±1.0 | 88.0±1.1 | 84.9±1.1 | 62.8±2.5 |
| Nominatim † | 68.2±4.3 | 68.2±4.3 | 66.6±4.7 | 55.6±3.6 |

CRR: share of queries with any answer.
GH1/GH4/GH6: share whose first answer is in the target's geohash cell.
† Open-source geocoder.
Other rows are the paper's (public endpoints, June 2026).

- \* Planet Pack from OpenStreetMap and OpenAddresses (October 2026): the whole planet in one 26.3 GB file, memory-mapped when served, 630.9M records.
  Built in 5 h 20 min on a 24-core / 32 GB desktop with a 9 GB memory budget.
  52 inputs are answered with a `fallback` area (postcode, town or country); full answers only, CRR is 94.4%.
  From OpenStreetMap alone (17.9 GB, 233.7M records, built in 2 h 51 min), GH6 is 67.7%.

The rows og was scored on: [bench/data/messy-streets/](bench/data/messy-streets/).

### Speed

The Netherlands: every address in OpenStreetMap (2026-11-01), each engine alone on the same machine.

| | **open-geocode** | Photon | Pelias |
| --- | ---: | ---: | ---: |
| Disk (data + image) | **0.36 GB** | 3.94 GB | 7.00 GB |
| Build time | **1 min 55 s** | 6 min 50 s | 10 min 18 s |
| Ready after start | **0.36 s** | 4.8 s | 15.3 s |
| RAM while serving ¹ | **1.1 GiB** | 23.5 GiB | 39.4 GiB |
| Forward latency p50 / p99 | **0.72 / 4.2 ms** | 2.0 / 19.4 ms | 4.7 / 9.6 ms |
| Reverse latency p50 / p99 | **0.20 / 0.37 ms** | 1.8 / 3.0 ms | 5.3 / 10.5 ms |
| Max forward throughput ² | **14,087 req/s** | 2,194 req/s | 2,323 req/s |
| Max reverse throughput ² | **≥ 65,606 req/s** ³ | 3,098 req/s | 2,406 req/s |
| Smallest machine for 100 req/s, forward | **1 vCPU / 1 GiB** | 2 vCPU / 4 GiB | 1 vCPU / 8 GiB |
| Smallest machine for 100 req/s, reverse | **1 vCPU / 256 MiB** ⁴ | 1 vCPU / 4 GiB | 1 vCPU / 8 GiB |
| First answer within 100 m, forward / reverse | 98.9% / 98.8% | 99.7% / 99.9% | 99.5% / 100% |

¹ Peak on 16 vCPU / 64 GiB, page cache included. Photon and Pelias size their heap to the machine, as their documentation says.
² Highest rate with p99 under 100 ms and at least 99.9% of requests answered, on 16 vCPU / 64 GiB.
³ The most the load generator could send.
⁴ The smallest size tried.

How it was measured, and how to run it again: [bench/](bench/).

## Quickstart

```
cargo install --path . --features tui --bin og
og build --region canada/ontario
og serve canada-ontario
```

Ontario's 2 million addresses build in about a minute on a 24-core machine, then the API serves on `http://127.0.0.1:8080`.
`GET /version` returns the server version and the Pack it serves: content id, schema version, and which og version built it and when.
Add `--openaddresses` to `og build` to merge in OpenAddresses points (needs an API token in `OA_TOKEN`).
A build leaves out the OpenAddresses data sets whose licence forbids commercial use or serving them at all, judged by the licence each job states, and warns of each one.
`open-geocode pack build --openaddresses-non-commercial` takes the non-commercial ones too, for a Pack used only for non-commercial purposes.

Run `og` with no arguments for the interactive interface: pick a region, build it, search it, and geocode CSV files.

### Docker

The image serves a Pack; it does not build one.
Mount a Pack directory or `.ogp` file read-only at `/packs/pack`:

```
docker run -p 8080:8080 -v ~/.local/share/open-geocode/packs/canada-ontario:/packs/pack:ro ghcr.io/open-geocode-rs/open-geocode
```

Images are published for `linux/amd64` and `linux/arm64`, tagged `latest` and `sha-<commit>`, after CI passes on `main`.
The container health check runs `open-geocode health`, which asks `/readyz`; Kubernetes can probe `/readyz` directly.
`/jobs` results go under `/tmp`: mount a volume there to keep them across restarts, or a tmpfs with `--read-only`.

## Use Cases

| Capability | Example use case |
|---|---|
| Forward geocoding | Turn customer, store, vendor, or service addresses into coordinates |
| Reverse geocoding | Convert fleet, delivery, device, or field-work GPS pings into readable locations using H3 candidate lookup and address-first gates |
| Autocomplete | Power address forms, checkout flows, internal tools, and store locators with Tantivy-native prefix queries |
| POI lookup | Find named businesses, schools, parks, and stations by name and town |
| Batch geocoding | Enrich CSVs, database tables, and large address lists without per-row API pricing |
| Search optimization | Handle messy addresses, abbreviations, partial queries, field-aware matches, interpolation ranges, and ranked candidates |
| Private data | Geocode internal addresses, custom places, service zones, or proprietary datasets |

## License

`open-geocode` is licensed under the [Apache License 2.0](LICENSE).
Versions up to and including 0.1.0 were released under the MIT License, and stay under it.

Third-party code dependencies remain under their own OSS licenses. Generated
Packs preserve source metadata needed for attribution and auditability; users are
responsible for following the license terms of the geospatial data they build
from.
