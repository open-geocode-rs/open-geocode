# open-geocode

Fast, lightweight, self-hosted geocoding in pure Rust.

`open-geocode` is a minimal Rust-native geocoding engine: address search,
Tantivy-backed autocomplete, and reverse geocoding from coordinates to
address-first location context. It turns OpenStreetMap PBF extracts into compact
binary Packs — a memory-mapped record store, a Tantivy text index, and an
H3-backed mmap spatial index — with no database or search cluster to run.

## Why open-geocode?

`open-geocode` focuses on pure OSS self-hosting, not paid third-party geocoding
APIs or vendor-locked map platforms like Google Maps and MapBox. Its core shape: `osmpbf` ingestion, GeoRust geometry
normalization, Tantivy search and typeahead, H3 spatial partitioning, address
interpolation, and Pack-local audit metadata.

Compared to other OSS geocoding alternatives:

| Option | Tradeoff | open-geocode focus |
|---|---|---|
| Nominatim | Heavy PostgreSQL/PostGIS deployment | Static binary Packs with memory-mapped records and spatial lookup |
| Pelias | Elasticsearch and multi-service ops | Single Rust runtime with Tantivy text search and no service graph |

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
