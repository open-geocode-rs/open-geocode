# Region catalog

`regions.json` lists the regions `og` can download and build.
It is generated from pinned upstream snapshots and never edited by hand.

```sh
python -I catalog/catalog.py generate                                  # rebuild regions.json offline
python -I catalog/catalog.py check                                     # CI: fails if anything is stale or unresolved
python -I catalog/catalog.py update geofabrik|openaddresses|overture|all  # refresh a pin
```

Manual decisions (names, ISO codes, parents, groups) live in `curation.toml`, each with a `why`.
After an update, resolve what it reports there, run `generate`, and commit the snapshot, `sources.lock.json`, `curation.toml` and `regions.json` together.
