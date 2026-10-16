# Zurich benchmark data sources

## City of Zurich address register — CC0

`zurich-dev.json` and `zurich-holdout.json` derive from the official address
register of the City of Zurich (Adressen Stadt Zürich), 59,372 building
entrances with coordinates.

Licence: **CC0** (public domain, no attribution required, no restrictions).

Source: <https://data.stadt-zuerich.ch/dataset/geo_adressen_stadt_zuerich>
Endpoint: the WFS `adrstzh_adressen_stzh_p` layer, `EPSG:4326`.

Coordinates are **building entrances**, not parcel centroids: this differs
from the G-NAF fixtures and is recorded in each fixture's metadata.

## geometalab reference set

`geometalab-zurich.json` is the 1,000-address list published with the
geometalab Geocoder-Benchmark, covering five municipalities (Zürich,
Oberengstringen, Schlieren, Uitikon Waldegg, Unterengstringen).

Source: <https://github.com/geometalab/Geocoder-Benchmark> (`Vorlage in WGS84.csv`)

⚠️ Only the addresses and their reference coordinates are used. That
repository's own comparison figures were produced in **March 2018** against
the OSM of the time and are not comparable with a current extract; they are
not reproduced here.

## OpenStreetMap

The pack under test is built from a BBBike Zurich extract.
© OpenStreetMap contributors, ODbL 1.0.

## Pinned inputs

| Input | Value |
|---|---|
| Zurich PBF | `Zuerich.osm.pbf`, 91,035,139 bytes |
| PBF sha256 | `970f8606a8250b024bd3c744a8c44306c34828f164f08a87ea67eeac5e3a33b6` |
| Retrieved | 2026-10-12 |
| Register features | 59,372 |
| Sample seed | 2026 |

## Regenerating

```
python3 scripts/zurich_fixture.py \
  --input data/zurich-addresses.geojson \
  --answerable-streets /tmp/zurich_streets.json \
  --dev 2000 --holdout 8000 --seed 2026 --out-dir fixtures/queries
```
