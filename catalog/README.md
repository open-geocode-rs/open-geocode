# Region catalog

`regions.json` lists the regions `og` can download and build.
`places.json` lists the map presets whose named objects are places, and `categories.json` the words that name each of them.
All three are generated from pinned upstream snapshots and never edited by hand.

```sh
python -I catalog/catalog.py generate                                  # rebuild the generated files offline
python -I catalog/catalog.py check                                     # CI: fails if anything is stale or unresolved
python -I catalog/catalog.py update geofabrik|openaddresses|overture|id-tagging-schema|id-tagging-schema-words|organicmaps|all  # refresh a pin
```

Manual decisions (names, ISO codes, parents, groups) live in `curation.toml`, each with a `why`.
After an update, resolve what it reports there, run `generate`, and commit the snapshot, `sources.lock.json`, `curation.toml` and the generated files together.

## Category sources

- iD tagging schema (ISC), pinned to the version of `upstream/id-tagging-schema.json`.
  Preset names, aliases and terms in every translated language, and the tags, fields and translated names that put presets in groups.
- Organic Maps `data/categories.txt` (Apache-2.0), pinned by commit.
  Only its `@group` blocks and the types that name them, with each synonym's minimum prefix length.
