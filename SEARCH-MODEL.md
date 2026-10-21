# Search model: work order

Plan for [#36](https://github.com/open-geocode-rs/open-geocode/issues/36). Written to be picked up
without reading the issue thread first, though [the design
comment](https://github.com/open-geocode-rs/open-geocode/issues/36#issuecomment-6306470448) holds
the research and the citation corrections behind it.

Baseline for every number here is `a4f4431` with #32 and #33 merged **as widened** — see step 0,
both have open design requests on them and neither should land as currently written.

The standing constraint on all of it: og is a global geocoder. A fix that works for AU, NZ and CA
because those are the sets we test with most is not a fix. Every step below is judged on whether it
holds for a planet file.

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

0. Land #32 and #33 **widened to their review comments**. Rebuild Packs. Re-baseline.
1. Run acceptance before the cut.
2. Carry the parser's corrections into retrieval.
3. Give places a prior. **Blocked on a data decision, see below.**

Step 4 of the issue — the Fellegi-Sunter model — does not start until 1-3 have landed and been
measured. The reason is in the issue: its acceptance criterion is per-country calibration, and the
gold sets three PRs cite are not in this repo.

---

## 0. Land #32 and #33, widened

Both pass CI as written and both are scoped too narrowly to ship. The design requests are on the
PRs; the short version:

**#32** keys postcode accumulation by input file. That keys on how the Pack was assembled rather
than on what a postcode means: a planet file or any multi-country extract still fuses AU and NZ
"2110", and NSW + ACT as two files splits a shared code that should be one. The direction instead
is to make a street's own addresses the evidence — give each street the set of postcodes its
addresses use, let an address with no tag borrow its street's set, and keep the postcode dot as the
fallback for when that set is empty, built per country with the country in the id
(`derived:osm:postcode:nz:2110`).

`postcode_agreement` (`src/search.rs:805-833`) already returns confirmed / contradicted / unknown,
so in-list / mismatch / empty-list maps onto what exists. Mismatch should rank down via
`RankKey.postcode_agreement` (`:1445`), not hard-drop.

**#33** folds accents by NFD plus mark-stripping, which cannot touch a letter that has no mark:
`ł`, `ø`, `ß`, `æ`, `đ`, `ı` all survive, so `Lodz` will not find `Łódź` nor `Strasse` `Straße`.
The direction is CLDR: `Latin-ASCII` for Latin, script-to-Latin tables for the rest, which also
replaces the hand-rolled kana/Cyrillic/Greek tables in `src/util/transliterate.rs`. Two keys per
name — exact with accents, search in plain Latin — so exact spelling can still win ties, which one
key makes impossible. NFC before folding, so `Wa\u0304naka` stops splitting into two words at the
combining mark.

`icu_normalizer` and `icu_properties` are already in `Cargo.lock` transitively via `url` <- `ureq`,
so the CLDR route is close to free on build time.

Both bump a schema version, so doing the fuller job now costs one Pack rebuild instead of two.

Re-run the dev key after and record the numbers: everything below is measured against that, not
against today's `main`.

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

**Depends on step 0 and must be measured with it, not after it.** The fuzzy matcher runs on folded
text, so a more aggressive fold makes edit distance strictly more permissive: `Latin-ASCII` plus two
edits collides names that today's fold plus two edits keeps apart. The thresholds at
`src/parse/lattice.rs:64-77` were tuned against the current fold and should be re-measured against
the new one rather than assumed to carry over. The parse bench is the harness.

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

The source has to be global and uniform, or the prior just moves the bias around — a signal that
ranks US and European places well and leaves the rest on a default is worse than no signal, because
it is wrong in a way that looks confident.

That rules out the OSM `population` tag on its own: its coverage follows mapper density, so it is
thin exactly where ranking is hardest. It also rules out Nominatim's Wikipedia importance file as a
primary signal — inbound-link counts track encyclopaedic coverage in large Wikipedias, which is a
Western-language bias by construction. It is separately blocked on licence: the generator is GPLv2
and no licence is stated for the output data.

The one globally uniform option is **GHSL** (JRC, CC BY 4.0), because it is modelled from satellite
built-up surface rather than from who bothered to write it down:

- **GHS-UCDB** R2024A — 11,422 urban centres as a vector table with population attributes, a few MB.
  Point-in-polygon against a place node is correct by construction for everything it covers.
- **GHS-POP** R2023A — 100 m raster, 17.7 GB compressed, for the long tail UCDB does not reach.
  A single-cell sample at a place node is wrong by orders of magnitude (the node is often a town
  hall), so it needs a radius-weighted sum keyed off `place=`, or a zonal sum over the boundary
  relation where one exists.

Open question is not which source but how much of the tail to buy: UCDB alone is cheap and covers
cities, UCDB + GHS-POP covers villages and costs a build-time raster download. Decide that before
starting; ship one quantised byte per place either way, never the input.

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
- A place with no prior at all is not systematically beaten by one that has it. This is the global
  test: most of the planet's places will have no UCDB polygon, and the fallback must not read as
  "unimportant".

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
