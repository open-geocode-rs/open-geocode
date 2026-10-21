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

## The problem, in one line

**One unrecognised token zeros the query.**

`subject_clauses` makes every identifying word `Occur::Must` (`src/search.rs:402`),
`locality_part_query` ANDs every word of a locality (`:406`, `:419`), and `QueryPlan::from_tagged`
routes `Label::Other` — the catch-all for tokens the parser could not place — into `plan.required`
alongside `Label::Street` (`:1920-1923`). So a suite designator, a country exonym, a route number or
a stray `\n` is treated as mandatory evidence, and a query that names the right street in the right
city returns nothing.

## Why this is the first thing to fix

*MESSY STREETS* (Gaere & von Wangenheim, [arXiv:2609.01612](https://arxiv.org/abs/2609.01612), June
2026, CC BY 4.0) ran 1,000 verbatim web addresses through 12 geocoders. Candidate Return Rate is
whether anything came back at all:

| Geocoder | CRR % | GH4 (±20 km) % |
|---|---|---|
| Google Maps v3 | 100.0 | 98.6 |
| ArcGIS | 99.8 | 97.9 |
| **Pelias v1** | **96.6** | 85.9 |
| Photon | 88.1 | 84.9 |
| Nominatim | 68.2 | 66.6 |

Read the rows across. For every system GH4 tracks CRR closely: **once a candidate comes back it is
usually in the right place.** Nominatim loses 31.8 points at the door and a further 1.6 to
mislocation. The paper names the cause as conjunctive matching — "a single unrecognised token zeros
an otherwise valid query" — which is what og does today.

Caveats, stated fairly: the set is 94-98% Latin script and North America 49% / Europe 43%, so it is
not the global set either, and open geocoders were hit via public demo endpoints. Neither explains a
32-point gap.

**Pelias does not use MUST-ALL.** Its production config is
`'minimum_should_match': '1<-1 3<-25%'` ([search_defaults.js](https://github.com/pelias/api/blob/master/query/search_defaults.js)),
adopted in [pelias/api#1333](https://github.com/pelias/api/pull/1333) as a *latency* control. Drop at
most 1 token up to 5; at 6+, 75% must match. That is most of the distance between 96.6 and 68.2.

## Parity, then ahead

Steps A-D bring og level with Pelias on retrieval. Steps E-G are where it goes past, and they rest
on one asset Pelias does not have: **a CRF parser that labels every token**. Pelias drops tokens by
count and cannot know whether it just dropped a suite number or the street name. og can.

| | Step | Beats Pelias? |
|---|---|---|
| A | Stop one token zeroing the query | parity |
| B | Tiered fuzzy with prefix anchoring | parity |
| C | `place_type` into ranking | parity-ish; Pelias has `population`, we would have the OSM size class |
| D | Acceptance before the cut | parity |
| E | Drop by label, not by count | **ahead** |
| F | Score the N-best readings jointly | **ahead** |
| G | Learned per-country weights and a real confidence | **ahead** |

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

Steps A-E need no data beyond the committed fixtures.

Two measurements worth taking on a planet file before committing to B: Tantivy fuzzy k=1 vs k=2
cost, and edge-ngram index blowup if that route is taken. Both are cheap and both are load-bearing.

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
key makes impossible. NFC before folding, so `Wānaka` stops splitting into two words at the
combining mark.

`icu_normalizer` and `icu_properties` are already in `Cargo.lock` transitively via `url` <- `ureq`,
so the CLDR route is close to free on build time.

Both bump a schema version, so doing the fuller job now costs one Pack rebuild instead of two.

Re-run the dev key after and record the numbers: everything below is measured against that, not
against today's `main`.

---

## A. Stop one token zeroing the query

### Problem

Three sites make retrieval conjunctive:

- `src/search.rs:402` — every word of `plan.required` is `Occur::Must`.
- `:406` / `:419` — `locality_part_query` ANDs **every word** of a locality part.
- `:1920-1923` — `Label::Other`, the parser's catch-all for tokens it could not place, is routed
  into `plan.required` alongside `Label::Street`.

The other nine `Occur::Must` sites are legitimate filters (layer at `:367` and `:1220`, country at
`:370`, the postcode lookup at `:871-939`) and stay as they are.

### Change

`BooleanQuery::with_minimum_required_clauses(clauses, n)` exists in Tantivy 0.25
(`boolean_query.rs:118`), and its `minimum_number_should_match` counts **`Should` clauses only**, so
`Must` filters coexist with it. That is exactly the shape needed: filters stay `Must`, subject words
become `Should` under a minimum.

Build it in `build_query` (`:350-372`), which is where the clause vector is assembled.

Start with Pelias's curve as the baseline so the first measurement is against a known quantity —
`n <= 1 -> 1`, `2..=5 -> n-1`, `>= 6 -> n - floor(0.25 * n)` — then move to the label-aware rule in
step E, which is the part that beats it.

Two things to watch:

- **Scoring changes shape.** With `Must`, every returned doc had every word, so BM25 only ever
  compared *how well*. With `Should`, a doc missing a word competes with one that has it, and the
  sum of matched-term scores is what separates them. Expect the dev key to move in both directions
  on individual queries; the aggregate is what matters.
- **It should get faster, not slower.** [tantivy#2390](https://github.com/quickwit-oss/tantivy/issues/2390)
  notes `BooleanWeight` leaves the Block-WAND path when any `Must` clause is present. Moving subject
  words to `Should` puts the common case back on it. Worth timing with `pack bench` either way.

### Done when

- `Ponsonby Road Christchurch` returns the Christchurch street, or nothing — never Auckland's
  silently. (This one needs step E or #30's reporting to be fully right; A alone stops the zeroing.)
- A query with one unrecognised token still finds the record: a suite designator, a country exonym
  on a foreign address, a duplicated postcode.
- `pack bench` latency does not regress, and ideally improves.
- Dev key improves. This is the largest expected gain in the whole sequence.

---

## B. Tiered fuzzy with prefix anchoring

### Problem

The parser already runs a Levenshtein automaton — `Dictionaries::lookup`
(`src/parse/lattice.rs:918-947`) returns `Match { entry, distance }` — and the correction is then
discarded. `Tagged` keeps the written word (`:493-496`) and `QueryPlan::from_tagged` copies
`tagged.token` verbatim (`src/search.rs:1889-1894`). "Torotno" is labelled `Locality` correctly and
issued as the exact term `torotno`, which matches nothing.

### The constraint that shapes the design

**Tantivy's `FuzzyTermQuery` is constant-scored.** Verified in 0.25:
`fuzzy_query.rs:178` discards `_enable_scoring`, and `automaton_weight.rs:109` returns a
`ConstScorer`. An exact match and a 2-edit match score identically
([tantivy#563](https://github.com/tantivy-search/tantivy/issues/563), open since 2019).

So a single blended query **cannot** satisfy #36's "exact spelling wins ties" criterion. The
structure has to be tiered: exact first, and only fall to distance 1, then 2, when a tier returns
too little. That is also the maintainer's own recommendation in #563.

Do not use `FuzzyTermQuery::new_prefix` at distance 2 —
[tantivy#3200](https://github.com/quickwit-oss/tantivy/issues/3200) (Oct 2026, open) reports it
returns incomplete matches, ~30% query-level defect rate on real dictionaries.

### Change

Carry the matched entry's canonical form alongside the written token and issue it as an alternative.

Do **not** widen `Tagged`. It is the parse contract (`src/parse/types.rs:48-52`), consumed by
`Tokens`, by `parse/bench/` and by the `Tagger` trait, and widening it drags the bench fixtures
along. Surface the `Match` on `ParsedQuery` where pieces are built, and carry it on `QueryPlan`.

Gate it the way every production system converges on — Photon's
`SearchQueryBuilder.java:49` is the geocoder-specific precedent: exact only below 4 characters,
`prefixLength` 1 when the term is <= 6 characters and 2 above. `prefixLength` is the dominant cost
lever; it is what makes distance 2 affordable at all. The thresholds already in
`src/parse/lattice.rs:64-77` were chosen for the parser's own dictionary lookups and are a
reasonable starting point, but they are not automatically right for index retrieval.

**Never fuzzy a house number or a postcode.** In a dense numeric space every 1-edit neighbour is a
real, different address on the same street: "12 Main St" and "13 Main St" are one edit apart and
edit distance carries no signal there.

### Depends on step 0 and must be measured with it

The fuzzy matcher runs on folded text, so a more aggressive fold makes edit distance strictly more
permissive: CLDR `Latin-ASCII` plus two edits collides names that today's fold plus two edits keeps
apart. Re-measure the thresholds against the new fold rather than assuming they carry over. The
parse bench is the harness.

### Done when

- `Taupo` finds Taupō the town, `Taupō` still does, and the Dunedin street does not displace it.
- `Lamton Quay Wellington` finds Lambton Quay. (From #27, which this closes in substance.)
- An exact-spelling query ranks the exact record above a one-edit neighbour. This is the criterion
  the const-scoring constraint exists to protect.
- `12 Main St` does not match `13 Main St`.
- Parse bench does not regress.

---

## C. `place_type` into ranking

### Problem

`place_layer` (`src/builder/osm/place.rs:72`) collapses `city | town | village | hamlet | locality`
into one `PlaceLayer::Locality`. The distinction OSM mappers already made — a globally uniform,
human-assigned size class — is discarded at build time.

The string itself *does* survive onto the record (`src/record.rs:82`), but `place_type` appears in
`src/search.rs` only inside test fixtures. Ranking never reads it.

So `Richmond` returning a quarry (#28) is partly a missing-signal problem with the signal already in
the Pack.

### Change

Read `place_type` in ranking and prefer the larger class when names tie. This needs no new data, no
download, no licence question and no schema bump.

Keep it a tie-break, not a multiplier. The documented failure mode across every geocoder surveyed is
a global prior overwhelming lexical evidence — Photon sums an importance factor of 30 onto clause
boosts of 0.01-1.5 and lets it dominate. A stated region must still override the size class.

### Done when

- `Richmond` returns the town first; `Hastings` returns the city first.
- `Richmond Tasman` and `Hastings Hawke's Bay` are unchanged — a stated area still wins.
- POI accuracy does not regress: `pack bench` against `fixtures/queries/ontario-poi.json`.

---

## D. Acceptance before the cut

### Problem

`ranked_hits` truncates to `limit` (`src/search.rs:1028`), then `search_plan` applies `accept`
(`:710`). The rank window is `limit * RANK_WINDOW_FACTOR` capped at `MAX_RANK_WINDOW` (`:962-965`)
and exists to give ranking headroom, but it is discarded before rejection sees it. Acceptance can
only shrink a pass, never backfill, so a pass can come back empty while an acceptable hit sat just
past the cut.

`geocode_filters_hits_ranked_beyond_the_requested_limit` (`:2193`) currently passes, because
postcode nearness ranks the right hit into the top `limit`. It passes despite the ordering, not
because of it.

### Change

`ranked_hits` applies the predicate to each hydrated hit before the `truncate(limit)` at `:1028`.
`search_plan` stops filtering at `:709-713` and keeps its own `truncate` for the neighbour estimate
pushed at `:701-706`.

The predicate is `&mut impl FnMut(&QueryPlan, &QueryPostcode, &TextSearchHit) -> Result<bool>`,
already threaded through `search_variants` -> `search_plan`. Partially apply it at the `search_plan`
call site so `ranked_hits` only ever sees `&TextSearchHit` — it should not learn what a `QueryPlan`
is.

Filter after hydration and before the sort: hydration (`:999-1019`) is the expensive part and
already happens there.

Keep the cutoff prune at `:991-996`. It is guarded on `!ranking.numbered` and is about not
hydrating what cannot place — a different concern.

### Done when

- `:2193` still passes.
- A new test: more than `limit` hits rank above an acceptable one, and the acceptable one still
  comes back. Build it like `:2193` does — write records directly with `Writer`, then
  `geocode_address` with `limit` 1 and 10.
- Dev key unchanged or better. This is a refactor; a large move means something else changed.

---

## E. Drop by label, not by count — the first real lead

### The asset Pelias does not have

Pelias's `1<-1 3<-25%` is blind. It knows how many tokens matched, never which. It will drop the
street name and keep the suite designator, because to it they are both just tokens.

og labels every token before retrieval. `QueryPlan::from_tagged` (`src/search.rs:1896-1927`) already
routes by label, and it already makes exactly this distinction in one place: `Label::Unit` goes to
`plan.optional` (soft), while `Label::Street` goes to `plan.required` (hard).

The bug is that **`Label::Other` is routed to `required` alongside `Label::Street`** (`:1920-1923`).
`Other` is the catch-all for tokens the parser could not place — which is precisely the set that
should be droppable.

### Change

Replace the count-based minimum from step A with a label-aware policy:

- **Never droppable**: `Street`, `HouseNumber`. These identify the record; without them there is no
  query.
- **Freely droppable, scoring only**: `Unit`, `Other`, and a `Country` part that contradicts nothing
  (a German exonym on a UK address should rank, not reject).
- **Droppable with a penalty**: `Locality`, `Region`, `Postcode`. A stated area that *disagrees*
  must still remove a candidate — that is #36's "never answer in a locality the query did not name"
  criterion, and it stays a hard rule. But a stated area that is merely *unknown to the Pack* should
  not zero the query.

The distinction between "disagrees" and "unknown" is the whole point, and the machinery exists:
`postcode_agreement` (`:805-833`) already returns a tri-state `Option<bool>` meaning confirmed /
contradicted / unknown. Generalise that shape to locality and region rather than inventing a second
mechanism.

This subsumes step A's numeric minimum. Keep the numeric rule as the floor for the case where
everything is `Other`.

### Done when

- `9501 Roosevelt Blvd Ste 410 Philadelphia PA 19114` resolves — the suite designator is dropped,
  not required.
- A country exonym on a foreign address does not zero the query.
- `Ponsonby Road Christchurch` does **not** return Auckland. A locality that is known to the Pack and
  disagrees still rejects.
- A locality the Pack has never heard of does not zero the query; it ranks instead.

---

## F. Score the N-best readings jointly

### Problem

`LatticeTagger::parse` returns up to `N_BEST = 3` readings, each with a `score: f32`
(`src/parse/lattice.rs:524-536`). `search_variants` (`src/search.rs:613-666`) converts each to a
`QueryPlan`, **discards the score**, and runs a first-wins cascade: for each plan, for each pass,
return the first non-empty result.

So a `Pass::Lenient` hit from reading #1 beats a `Pass::Strict` hit from reading #2 that never ran.
The doc comment at `:605-611` states the intended invariant and the loop honours it — plans are
outermost — but "the first reading that returns anything wins" is not the same as "the best
interpretation wins".

Nominatim enumerates ~16 interpretations against a penalty budget and takes the best;
`ranking = accuracy - importance`, minimised. Pelias runs a fallback ladder and resolves post-hoc.
og currently does neither.

### Change

Score readings against each other instead of taking the first that returns. The parser's `score` is
an unnormalised Viterbi path score, so it needs putting on a common scale with retrieval evidence
before the two can be added — that is the design work here, and it is the piece that most wants a
measurement before a decision.

Cheapest defensible version: keep the cascade, but when two readings both return, prefer the one
whose combined (parse score, retrieval evidence) is better rather than whichever ran first. Full
version: one objective over all readings × passes, which is step G's model.

### Done when

- A query where reading #2 is correct and reading #1 returns something plausible-but-wrong picks
  reading #2. Needs a fixture built for it.
- No latency regression: this must not turn into running every reading to completion every time.
  An anytime budget like Nominatim's (`min_ranking = penalty + 1.5`, `i > 15`) is the pattern.

---

## G. Learned per-country weights, and a confidence worth printing

### Why this beats Pelias structurally

Pelias has one global `address:postcode:boost: 20` for the entire planet. A UK postcode identifies
roughly 15 addresses; a US ZIP identifies thousands. One constant cannot be right for both.

Under Fellegi-Sunter a part's weight **is** `log2(m_i / u_i)`, and `u` is estimated from the Pack's
own term frequencies. The weights come out different per country automatically, with no hand-tuning
and no per-country config. Term-frequency adjustment extends the same idea within a field, so
"Main St" and a rare street name are weighted differently too.

This is also the honest answer to the hand-set constants scattered through `search.rs` today —
`NAME_BOOST = 5.5`, `NUMBER_BOOST = 2.0`, `AREA_STANDS_IN_BOOST = 0.5` — each added to fix a
specific regression, each untested outside the countries it was tuned against.

There is a local precedent for the shape: `Field::country_vote`
(`src/parse/lattice/dictionary.rs:238-246`) is already a weighted vote discounted by edit distance.
It is hand-set; this step is that function learned.

### Shape

Fellegi-Sunter skeleton, with the channel model assigning **comparison levels** rather than a
continuous probability — the two probabilities are not on a common scale and no source composes them
directly. Per part: agree / disagree / unknown, weight `log2(m/u)`, `u` from the Pack's term
frequencies, `m` by EM over blocked pairs, nulls excluded rather than scored. Sum in log-odds, add
the prior and any proximity decay, convert once.

Then calibrate: one temperature per country, fit on the dev splits, shrunk toward a pooled estimate
for data-poor countries (`log T_c ~ N(log T_global, tau^2)`). Temperature scaling provably does not
reorder candidates, so it can ship without re-running ranking evaluations. Cost to the Pack is one
float per country.

The result is a probability that means the same thing in every response — which Pelias's
`confidence` explicitly does not: it is an unweighted mean of five heuristics, two of which are
z-scores relative to the returned set, so the same hit scores differently depending on what else came
back ([confidenceScore.js](https://github.com/pelias/api/blob/master/middleware/confidenceScore.js)).

### Blocked on evaluation data

This cannot start before the gold sets exist. #36's acceptance criterion is "probabilities are
calibrated per country", and the NZ invoices / NZ register sets that #32 and #33 measure against are
not in this repo. `fixtures/queries/SOURCES.md` pins G-NAF with a seed, a sha256, a retrieval date
and a regeneration script; the NZ sets need the same treatment, or at least a pinned generator.

### Done when

- A UK postcode and a US ZIP carry different weights, derived rather than configured.
- Reliability diagrams and ECE per country, measured on holdout, read once.
- The hand-set constants in `search.rs` are gone or justified by a number.

---

## The prior, deferred

A population prior was the original step 3 and is now out of the critical path, because C extracts
most of its value from data already in the Pack.

When it returns, the source has to be globally uniform or it just moves the bias around. That rules
out the OSM `population` tag alone (coverage follows mapper density) and Nominatim's Wikipedia
importance file as a primary signal (inbound-link counts track encyclopaedic coverage in large
Wikipedias). The importance file is separately blocked on licence: the generator is GPLv2 and no
licence is stated for the output data.

**GHSL** (JRC, CC BY 4.0) is the uniform option because it is modelled from satellite built-up
surface rather than from who bothered to write it down. GHS-UCDB R2024A is 11,422 urban centres as a
vector table, a few MB, point-in-polygon against a place node. GHS-POP R2023A is a 100 m raster,
17.7 GB compressed, for the tail UCDB does not reach — and a single-cell sample at a place node is
wrong by orders of magnitude, since the node is often a town hall, so it needs a radius-weighted sum.

Open question when it comes back: UCDB alone, or UCDB plus the raster for villages. Ship one
quantised byte per place either way, never the input.

Population also does not rank what has no residents — airports, stations, landmarks. One unused
globally-uniform signal is already in the builder for that: `alternate_names`
(`src/builder/osm/tags.rs:61`) parses every `name:<lang>` tag and flattens them, discarding the
**count**. Heathrow carries dozens of language names, a suburban park carries one, and unlike
Wikipedia link counts it measures how many language communities named a thing rather than which
Wikipedia linked it. Unmeasured; worth a look before buying a dataset.

---

## Fix in passing

`src/batch.rs:606` hardcodes `countries: Vec::new()`, so the country filter is unreachable from the
batch path. One line, unrelated to the above, and it compounds the cross-country postcode problem
#32 fixes.
