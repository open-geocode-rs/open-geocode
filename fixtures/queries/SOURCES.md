# Benchmark data sources

## G-NAF (Geoscape Geocoded National Address File)

The `tas-gnaf-*.json` answer keys derive from the **Geoscape Geocoded National
Address File (G-NAF)**, August 2026 release, GDA2020, PSV distribution.

Incorporates or developed using G-NAF © Geoscape Australia, licensed by the
Commonwealth of Australia under the
[Open Geo-coded National Address File (G-NAF) End User Licence Agreement](https://data.gov.au/data/dataset/geocoded-national-address-file-g-naf).

The EULA terms are based on **Creative Commons Attribution 4.0 International
(CC BY 4.0)**, with one added restriction: open G-NAF data must not be used to
generate an address or a compilation of addresses **for the sending of mail**
unless each address has been verified as able to receive mail by reference to a
secondary source. These fixtures exist to score geocoding accuracy and are not
used for mail.

Source: <https://data.gov.au/data/dataset/geocoded-national-address-file-g-naf>

## OpenStreetMap

The Pack under test is built from a Geofabrik Tasmania extract.
© OpenStreetMap contributors, licensed under the
[Open Database License (ODbL) 1.0](https://www.openstreetmap.org/copyright).

## Pinned inputs

| Input | Value |
|---|---|
| G-NAF release | AUG 2026, GDA2020, PSV |
| G-NAF archive | 1,854,931,188 bytes |
| Tasmania PBF | `tasmania-latest.osm.pbf`, 53,990,757 bytes |
| Tasmania PBF sha256 | `25c2ba70433431f6dabce4bc8a56bb5dd65d0f3f7105f5815febb63efc29334f` |
| PBF retrieved | 2026-10-12T03:40:28Z |
| Sample seed | 2026 |

## Regenerating

```
python3 scripts/gnaf_fixture.py \
  --gnaf-dir data/gnaf --state TAS \
  --street-types data/gnaf/Authority_Code_STREET_TYPE_AUT_psv.psv \
  --answerable-streets /tmp/tas_pack_streets.json \
  --dev 2000 --holdout 8000 --seed 2026 --out-dir fixtures/queries
```

Scoring:

```
cargo run --release -- bench-geocode \
  --pack data/tas-pack --queries fixtures/queries/tas-gnaf-holdout.json
```
