# Region catalog

`catalog/regions.json` is the list of regions the `og` CLI embeds: the planet, continents, countries, subregions and groups, each with its OpenStreetMap download and what OpenAddresses and Overture hold inside it.
It is generated, never edited by hand.

## Layout

| Path | What it is |
| --- | --- |
| `catalog/sources.lock.json` | One pin per upstream: what was fetched, when, and the sha256 of the snapshot. |
| `catalog/upstream/geofabrik.json` | Geofabrik's index trimmed to what the catalog needs, with each download's size and date. |
| `catalog/upstream/openaddresses.json` | Every OpenAddresses address data set with a successful job: rows, bytes, bounds, license, commit. |
| `catalog/upstream/overture.json` | Overture places and addresses counted inside each Geofabrik polygon. |
| `catalog/curation.toml` | Hand decisions, grouped by reason, each group with a `why`. |
| `catalog/regions.json` | The output. |
| `catalog/catalog.py` | The generator. |

## Commands

Run Python isolated (`-I`) from the repository root.

```sh
python -I catalog/catalog.py generate
python -I catalog/catalog.py check
python -I catalog/catalog.py update geofabrik|openaddresses|overture|all
```

`generate` reads only the snapshots and `curation.toml`, so it works offline and two runs give byte-identical output.
`check` regenerates in memory and fails if `regions.json` differs, if a snapshot's sha256 differs from the lock, or if anything is unresolved.
CI runs `check`.

`generate` and `check` need only the standard library (Python 3.11 or later).
`update overture` needs the packages in `requirements.txt`; install them in a virtual environment outside the repository.

## Updating a pin

1. Run `update <source>` (or `update all`).
   It fetches the upstream, writes the snapshot, records it in the lock and regenerates.
2. Read the review report it prints: added ids, removed ids, renamed regions, OpenStreetMap downloads whose size moved more than 20%, and UNRESOLVED entries.
3. If anything is unresolved the command exits non-zero and leaves `regions.json` as it was.
   Resolve each entry (below) and run `generate`.
4. Commit the snapshot, the lock, `curation.toml` and `regions.json` together.

What each update costs, measured on 2026-10-16:

| Source | Requests | Time |
| --- | --- | --- |
| Geofabrik | the index, then one HEAD per download (555), four at a time | about 2 minutes |
| OpenAddresses | the data list, one job per data set (about 3,100), and the source file of jobs that carry no license | about 1.5 minutes |
| Overture | the point geometry column of every places and addresses file (about 6.9 GB) | about 27 minutes on a 5 MB/s link |

Overture pins the newest release in the public bucket.
Its counts are made against the Geofabrik polygons in `https://download.geofabrik.de/index-v1.json`; that file is not committed, its sha256 is in the lock.
Rerun `update overture` after a Geofabrik update that adds regions, or the new regions have no Overture counts.

## Resolving an unresolved entry

Every finding names a region id and what is wrong.
Decide it in `catalog/curation.toml`, in an existing group with the same reason or in a new group with its own `why`.

| Finding | Decision |
| --- | --- |
| name needs an English name or an accept decision | Add the id to a `[names.*.set]` table: English name first, then aliases for search. If the name is already English, add the id to an `[accept_names.*]` table's `ids`. |
| country level with N country codes and no kind or iso decision | A new multi-country or territory extract. If other extracts already cover all of it, make it a `group` in `[kind.*]`. If it is the only extract for some country, make it a `country` and list its countries in `[covers.*]`. If it is one territory, give it codes in `[iso.*]`. |
| several ISO codes upstream and no decision | Put the codes in `[iso.*]`, the one to show first. |
| ISO code is on several regions | Upstream tagged two regions with one code. Give each its right code in `[iso.*]`. |
| no parent upstream and no decision | A new top-level extract. Add it to `[continents]` or give it a parent in `[parent.*]`. |
| in curation.toml but not upstream any more | The region was removed or renamed upstream. Delete or move the decision. |

The generator flags names that are not ASCII or that contain markup or a slash.
Names in a local language written in plain ASCII (such as `Bayern`) cannot be detected; review added ids in the report for those.

## How the output is built

- **Ids** are the Geofabrik download path without the continent and without `-latest.osm.pbf`: `canada/ontario`, `germany`, `russia/kaliningrad`.
  Continents keep their own name; the planet is `planet`.
- **Parents** follow the download path when a region exists at the path's prefix, since the index gives US states `north-america` as parent.
  Otherwise the index parent applies, then `[parent.*]` decisions.
- **Kinds** follow depth (continent, country under a continent, subregion below) unless `[kind.*]` decides.
- **`iso`** is the first code in `[iso.*]`, or the single upstream code; it is left out for regions with none or with several countries (`[covers.*]`).
- **`openaddresses`** maps each data set to an ISO code from its source path: `ca/on/toronto` is CA-ON, `fr/countrywide` is FR.
  A data set whose code equals or is below one of the region's codes is within it, and its rows and bytes are summed.
  One whose code is above (a country-wide set against a subregion) is counted in `partial_sources` with no rows added.
  A region with no code cannot decide containment, so everything its nearest coded ancestor holds is partial.
  Continents sum their countries; the planet holds everything.
  `sources` counts data sets (one address layer of one source file), and data sets that overlap are summed as they are.
  Some data sets have no size upstream; `bytes` sums the known sizes and `unsized_sources` says how many are missing.
- **`overture`** is the exact number of Overture places and addresses whose point lies inside the region's Geofabrik polygon.
  The planet's count is every point in the release.

## Schema

```json
{
  "schema": 1,
  "pins": { "geofabrik": "2026-10-16", "openaddresses": "2026-10-16", "overture": "2026-09-23.1" },
  "regions": [
    {
      "id": "canada/ontario",
      "name": "Ontario",
      "kind": "subregion",
      "parent": "canada",
      "iso": "CA-ON",
      "aliases": [],
      "osm": { "url": "https://download.geofabrik.de/north-america/canada/ontario-latest.osm.pbf", "bytes": 1032650854, "updated": "2026-10-16T04:50:38Z" },
      "openaddresses": { "sources": 86, "rows": 7382066, "bytes": 203774702, "partial_sources": 1, "unsized_sources": 18 },
      "overture": { "places": 600510, "addresses": 5436016 }
    }
  ]
}
```

The planet is the first region; the rest are sorted by id.
`parent` is null only for the planet.
`iso`, `openaddresses`, `openaddresses.unsized_sources` and `overture` are left out when there is nothing to say.
