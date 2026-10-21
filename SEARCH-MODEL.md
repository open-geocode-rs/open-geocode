# Search model: work order

Plan for [#36](https://github.com/open-geocode-rs/open-geocode/issues/36). Written to be picked up
without reading the issue thread first, though [the design
comment](https://github.com/open-geocode-rs/open-geocode/issues/36#issuecomment-6306470448) holds
the research and the citation corrections behind it.

Baseline for every number here is `a4f4431` with #32 and #33 merged.

## Verification

Every step passes this before it is done:

```
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

And scores no worse than the step before it on the dev key:

```
cargo run --release -- pack build --input data/tasmania.osm.pbf --pack data/tas-pack
cargo run --release -- geocode bench --pack data/tas-pack --queries fixtures/queries/tas-gnaf-dev.json
```

`fixtures/queries/*-holdout.json` stays holdout. It is read once, when the whole sequence is done,
and never during development.

Steps 1 and 2 need no data beyond the committed fixtures.

## Order

0. Merge #32, then #33. Rebuild Packs. Re-baseline.
1. Run acceptance before the cut.
2. Carry the parser's corrections into retrieval.
3. Give places a prior. **Blocked on a data decision, see below.**

Step 4 of the issue — the Fellegi-Sunter model — does not start until 1-3 have landed and been
measured. The reason is in the issue: its acceptance criterion is per-country calibration, and the
gold sets three PRs cite are not in this repo.

---

## 0. Merge #32 and #33

Both verified together on `a4f4431`: fmt clean, clippy clean, 347 tests pass, no conflicts in
either order. Merge #32 first (builder only), then #33 (bumps `TEXT_INDEX_SCHEMA_VERSION` 10 -> 11).

Packs must be rebuilt. Re-run the dev key and record the numbers: everything below is measured
against that, not against today's `main`.

---

## 1. Run acceptance before the cut

### Problem

`ranked_hits` truncates to `limit` (`src/search.rs:1028`), then `search_plan` applies `accept`
(`:710`). The rank window is `limit * RANK_WINDOW_FACTOR` capped at `MAX_RANK_WINDOW` (`:962-965`)
and exists to give ranking headroom, but it is discarded before rejection sees it. So acceptance
can only shrink a pass, never backfill from the window, and a pass can come back empty while an
acceptable hit sat just past the cut.

`geocode_filters_hits_ranked_beyond_the_requested_limit` (`:2193`) currently passes, because
postcode nearness ranks the right hit into the top `limit`. It passes despite the ordering, not
because of it.

### Change

`ranked_hits` takes the predicate and applies it to each hydrated hit before the `truncate(limit)`
at `:1028`. `search_plan` stops filtering at `:709-713` and keeps its own `truncate` for the
neighbour estimate pushed at `:701-706`.

The predicate is `&mut impl FnMut(&QueryPlan, &QueryPostcode, &TextSearchHit) -> Result<bool>`,
already threaded through `search_variants` -> `search_plan`. `ranked_hits` has `ranking` and so has
`ranking.postcode`; it needs `plan` passed in, or the closure partially applied at the `search_plan`
call site so `ranked_hits` only sees `&TextSearchHit`. Prefer the second — `ranked_hits` should not
learn what a `QueryPlan` is.

Hydration is the expensive part (`:999-1019`: `document_records`, `record_summary`,
`estimate_house`, `place_on_street_line`, `TieBreak::new`), so filtering after hydration and before
the sort keeps the cost where it already is.

Keep the cutoff prune at `:991-996` as is. It is guarded on `!ranking.numbered` and is about not
hydrating what cannot place, which is a different concern.

### Done when

- `:2193` still passes.
- A new test: more than `limit` hits rank above an acceptable one, and the acceptable one still
  comes back. Build it like `:2193` does — write records directly with `Writer`, then
  `geocode_address` with `limit` 1 and 10.
- Dev key unchanged or better. This is a refactor; a large move means something else changed.

---

## 2. Carry the parser's corrections into retrieval

### Problem

The parser already does fuzzy matching and the result never reaches the index.

`Dictionaries::lookup` (`src/parse/lattice.rs:918-947`) tries exact spellings, then runs a
Levenshtein automaton, and returns `Match { entry, distance }`. Thresholds at `:64-77`
(`MIN_FUZZY_CHARS`, `TWO_EDIT_CHARS`, `MAX_TWO_EDIT_CHARS`, `MAX_FUZZY_CHARS`,
`MIN_FUZZY_POI_CHARS`) are already tuned.

But `Tagged` is built from the written word (`:493-496`), `QueryPlan::from_tagged` copies
`tagged.token` verbatim (`src/search.rs:1889-1894`), and retrieval issues exact `TermQuery`s. So
"Torotno" is labelled `Locality` correctly, then searched as the term `torotno`, which matches
nothing. The label is used; the correction is dropped.

`levenshtein_automata` is already a direct dependency (`Cargo.toml:48`). This step adds none.

### Change

Carry the matched entry's canonical name alongside the written token, and issue it as an
alternative in retrieval.

Do **not** widen `Tagged`. It is the parse contract — `src/parse/types.rs:48-52`, consumed by
`Tokens`, by `parse/bench/` and by the `Tagger` trait — and widening it drags the bench fixtures
along. Carry the correction on `QueryPlan` instead: a parallel `Vec` or a small struct per part
holding `{ written, canonical: Option<String>, distance: u8 }`, populated in `from_tagged` from
what the tagger already resolved.

That means `ParsedQuery` has to surface it. `LatticeTagger::parse` (`:614-616`) returns
`ParsedQuery { tokens, pieces, country, score }` (`:524-536`); the dictionary `Match` is available
where pieces are built and is currently discarded. Add it to the per-piece data, not to `Tagged`.

In retrieval, where a part has a canonical form distinct from what was written, issue a
`DisjunctionMaxQuery` over both rather than replacing — `area_term_query` (`:487-492`) and
`street_word_query` (`:465-470`) are the two sites. Boost the exact written form above the
corrected one so exact spelling wins ties, which is an explicit acceptance criterion in #36. The
existing `AREA_STANDS_IN_BOOST` (`:166`) is the precedent for how such a constant is documented.

`distance` is available as a ranking signal. `Field::country_vote`
(`src/parse/lattice/dictionary.rs:238-246`) already discounts by `1 / (1 + distance)` and is the
local precedent, but it is hand-set — do not add a second hand-set weight table here without a
number to justify it. Simplest defensible thing: use `distance` only to order alternatives, not to
scale scores.

### Done when

- `Taupo` finds Taupō the town, `Taupō` still does, and the Dunedin street does not displace it.
- `Lamton Quay Wellington` finds Lambton Quay. (From #27, which this closes in substance.)
- An exact-spelling query ranks the exact record above a one-edit neighbour.
- Parse bench does not regress: `cargo run --release -- parse bench --gold fixtures/parse/gold
  --predictions fixtures/parse/predictions`.
- Dev key improves. This is the step with the clearest expected gain.

---

## 3. Give places a prior

### Problem

`PlaceRecord` (`src/record.rs:72-83`) carries `name`, `alt_names`, `place_type`, `geometry`,
`source`. There is no population, importance, rank or prominence field anywhere in the record model,
and `src/builder/osm/place.rs` never reads the OSM `population` tag.

So `Richmond` returning a quarry (#28) is not a ranking bug. The only thing separating a town from a
quarry of the same name today is BM25 IDF, `NAME_BOOST`, and `TieBreak::source`
(`src/search.rs:1623-1629`), which prefers lower OSM object ids — an arbitrary proxy.

### Blocked

Pick a source before starting:

- **GHS-UCDB** (CC BY 4.0, 11,422 urban centres, a vector table) with a radius-weighted GHS-POP
  sample for the long tail. Better coverage where the prior earns its keep. Adds a build-time data
  dependency. Do not ship the raster — it is 17.7 GB compressed; derive a scalar at build time.
- **OSM `population` tag** plus `place_type` rank. Free, ships today, no new input. Weaker, and its
  coverage is biased towards wealthy and urban regions.

Nominatim's Wikipedia importance file is a third option and is deliberately **not** listed: its
generator is GPLv2 and no licence is stated for the output data. That needs resolving with upstream
before it goes near a Pack.

### Shape, once unblocked

Add a `prior` scalar to `PlaceRecord`; bump `RECORDS_VERSION` (`src/records.rs:98`). Ship one
quantised byte per place, not the inputs. Combine population-like signals in log space —
`log(1 + pop) / log(1 + pop_max)` — since that is the scale both GHSL and Nominatim importance
already use, and Pelias applies `log1p` to both its popularity and population factors.

Bound it against lexical evidence. Photon sums an importance factor of 30 onto clause boosts of
0.01-1.5 and lets the prior dominate; that is the documented failure mode to avoid. Nominatim bounds
it implicitly — importance is in `[0, 1]` against penalties of 0-3.

### Done when

- `Richmond` returns the town first; `Hastings` returns the city first.
- A stated region still overrides the prior: `Richmond Tasman` and `Hastings Hawke's Bay` are
  unaffected by it.
- Dev key improves, and POI accuracy does not regress — `pack bench` against
  `fixtures/queries/ontario-poi.json` is the guard, since the prior must not demote POIs.

---

## Fix in passing

`src/batch.rs:606` hardcodes `countries: Vec::new()`, so the country filter is unreachable from the
batch path. One line, unrelated to the above, and it compounds the cross-country postcode problem
#32 fixes.

## Before step 4

The NZ invoices and NZ register gold sets that #32 and #33 measure against are not in this repo.
`fixtures/queries/SOURCES.md` pins G-NAF with a seed, a sha256, a retrieval date and a regeneration
script. The NZ sets need the same treatment, or at least a pinned generator, before anything can be
calibrated per country.
