# open-geocode

Fast, lightweight, self-hosted geocoding in pure Rust.

`open-geocode` is a mininmal Rust-native geocoding engine for address search to geo coordnates, reverse
geocoding (coordinates to address). It turns
OpenStreetMap, open address data, and private location data into compact regional
datasets that can be queried without a heavy database or search cluster.

Self-hosting is designed around a single Rust runtime for the HTTP API,
parser/search engine, and local object-storage index loading, not a PostGIS,
Elasticsearch, Redis, or JVM stack.

## Why open-geocode?

`open-geocode` focuses on pure OSS self-hosting, not paid third-party geocoding
APIs or vendor-locked map platforms like Google Maps and MapBox.

Comapred to other OSS geocoding alternatives:
| Option | Tradeoff | open-geocode focus |
|---|---|---|
| Nominatim | Heavy PostgreSQL/PostGIS deployment | Static regional packs, no required database |
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
