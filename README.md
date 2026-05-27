# open-geocode

Fast, lightweight, self-hosted geocoding in pure Rust.

`open-geocode` is a minimal Rust-native geocoding engine for address search to
geo coordinates and reverse geocoding from coordinates to address-first location
context. It turns OpenStreetMap, open address data, and private location data
into compact binary Packs that can be queried without a heavy database or search
cluster.

Self-hosting is designed around a single Rust runtime for the HTTP API,
parser/search engine, and Pack loading, not a PostGIS, Elasticsearch, Redis, or
JVM stack.

## Why open-geocode?

`open-geocode` focuses on pure OSS self-hosting, not paid third-party geocoding
APIs or vendor-locked map platforms like Google Maps and MapBox.

Compared to other OSS geocoding alternatives:

| Option | Tradeoff | open-geocode focus |
|---|---|---|
| Nominatim | Heavy PostgreSQL/PostGIS deployment | Static binary Packs, no required database |
| Pelias | Elasticsearch and multi-service ops | Single Rust runtime, no service graph |

## Use Cases

| Capability | Example use case |
|---|---|
| Forward geocoding | Turn customer, store, vendor, or service addresses into coordinates |
| Reverse geocoding | Convert fleet, delivery, device, or field-work GPS pings into readable locations |
| Autocomplete | Power address forms, checkout flows, internal tools, and store locators |
| Batch geocoding | Enrich CSVs, database tables, and large address lists without per-row API pricing |
| Search optimization | Handle messy addresses, abbreviations, typos, partial queries, and ranked candidates |
| Private data | Geocode internal addresses, custom places, service zones, or proprietary datasets |

## License

`open-geocode` is licensed under the [MIT License](LICENSE).

Third-party code dependencies remain under their own OSS licenses. Generated
Packs preserve source metadata needed for attribution and auditability; users are
responsible for following the license terms of the geospatial data they build
from.
