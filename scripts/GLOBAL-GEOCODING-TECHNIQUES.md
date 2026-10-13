# Techniques for a global geocoder

What mature geocoders and the literature do about the input shapes
open-geocode handles badly. Ranked by measured payoff over effort.

Every "gap" figure is from our own evals (`fixtures/queries/`); every "share of
input" is from profiling the 10,000 gold-tier rows of MESSY STREETS. Nothing
here is estimated from intuition. Effort is a reading of our code, not a
measurement, and is marked where it is a guess.

## Where we stand

| Eval | Result |
| --- | --- |
| Zurich held-out, `full` form | 92.7% ≤25 m, 97.8% ≤100 m, 5.2 m median |
| Zurich held-out, `messy` form | 85.1% ≤25 m, 89.3% ≤100 m |
| Tasmania held-out, `full` form | 36.7% ≤25 m, 57.1% ≤100 m |
| Tasmania, address present in OSM | **99.5% found** |

Tasmania scores low because OSM holds 103k of G-NAF's 343k addresses there, not
because ranking fails: when the address is in the pack we return it 99.5% of the
time. The engine's real weakness is **unstructured input**, which the 8.5-point
gap between the Zurich `full` and `messy` cohorts isolates on dense data.

## What real input looks like

Profiled over the 10,000 gold rows:

| Feature | Share |
| --- | ---: |
| No comma anywhere | **81.2%** |
| Runs of whitespace | 34.2% |
| Non-ASCII characters | 21.7% |
| French street word | 11.9% |
| Number range or slash (`12-14`, `3/5`) | 8.1% |
| Unit or floor word | 7.6% |
| Number with letter suffix (`198a`) | 6.8% |
| German street word | 6.4% |
| Spanish street word | 3.2% |
| Dotted number (`32.1`) | 0.1% |

---

## 1. Comma-free input

Gap: Zurich messy 89.3% vs full 97.8% (≤100 m); 366 far misses against 7
Share of real input: **81.2%**

Who: libpostal; Pelias (as a second stage)
Technique: CRF sequence labelling. Every token is tagged
  (`house_number` / `road` / `unit` / `city` / `postcode`) in one structured
  prediction, so each decision conditions the others. Punctuation is a weak
  feature among many, not the segmentation rule.
Cost: build = trained model shipped as data (~2 GB, or ~750 MB for the
  parser alone); query = one Viterbi pass, O(n·k²) in tokens and label count,
  no index access
Beats: splitting on commas collapses `80 rue jules Raimu  LE MANS  72100` into
  one blob with no locality boundary. libpostal reports 99.45% full-parse
  accuracy trained on ~1.2B addresses across 230 countries.

Fit for OG: libpostal itself does not fit — C library, multi-GB model, and the
  Rust bindings wrap the C build rather than replace it. The portable subset of
  the same idea: anchor on a **trailing postcode regex per country**, match the
  longest suffix against **locality names already in the pack**, and treat
  recognised **street-type keywords** as segment boundaries. That is a
  gazetteer lookup plus two regexes, not a model.
Effort: M (unverified) Gain: bounded by the 8.5 pt full/messy delta

## 2. Number forms

Gap: 60 of 1,000 geometalab Zurich addresses unanswered, purely on `32.1`
Share of real input: 8.1% ranges or slashes, 6.8% letter suffixes, 0.1% dotted

Who: Nominatim; libpostal
Technique: house numbers are normalised into **searchable variants** at index
  time rather than parsed at query time. `12-14` indexes as `12-14`, `12` and
  `14`; `198a` as `198a` and `198`. Nominatim's ICU tokenizer does this through
  its variant generator; `HouseNumber::base` already does the letter-suffix half
  for us.
Cost: build = a few extra terms per address, linear; query = none
Beats: query-time parsing has to guess which form the data used; indexing both
  means either spelling finds it.

Fit for OG: extends `HouseNumber` and the `house_number` indexing in
  `text_index.rs`. `with_base_house_number` is the existing hook. Low risk: it
  only adds terms.
Effort: **S** Gain: ~6% of input, 60/1000 measured on one corpus

## 3. Whitespace and punctuation normalisation — mostly done

Gap: none found for whitespace
Share of real input: 34.2% have runs of whitespace

Who: everyone. Nominatim's ICU tokenizer runs a normalisation stage before
  tokenising
Technique: collapse whitespace runs, strip zero-width and control characters,
  normalise dashes and apostrophes, before tokenising
Cost: O(n) once per query and per indexed name

Fit for OG: `util::text::collapse_whitespace` already handles the whitespace
  third of this, with a test for internal and edge runs, and
  `normalize_for_compare` builds on it. What is **not** covered: zero-width and
  control characters, and dash and apostrophe variants. Checked rather than
  assumed — the 34.2% figure above is not an open gap.
Effort: **S** Gain: small; the large share of input this touches is already
  handled

## 4. Street-type synonyms across languages

Gap: not directly measured; implicated in the 9.2% Zurich messy far misses
Share of real input: ~21% carry a French, German or Spanish street word

Who: Nominatim (ICU tokenizer variants), Photon (multilingual index)
Technique: a per-language table mapping street types to their abbreviations and
  back (`road → rd`, `strasse → str`, `rue`, `calle`), applied at index time to
  generate searchable variants. Nominatim ships this as editable YAML, with
  entries like `bridge -> bdge,br,brdg,bri,brg`.
Cost: build = a handful of variants per street name; query = none
Beats: query-time expansion needs the language known up front; index-time
  variants do not.

Fit for OG: `expand_final_direction` and `search_query_variants` already do the
  English half. This is the same mechanism with a bigger, per-language table —
  data, not new code. **Not** language modelling.
Effort: **S/M** Gain: bounded by ~21% of input, unquantified

## 5. Locality disambiguation without stated context

Gap: 366 far misses (9.2%) on Zurich messy against 7 on full
Share of real input: follows from the 81.2% with no comma

Who: Pelias, via the **Placeholder** service
Technique: a separate gazetteer keyed on administrative hierarchy, consulted
  before the main index. It resolves "which Springfield" relationally —
  something Pelias found Elasticsearch handles badly — and returns every
  plausible context rather than one, so the search can try each.
Cost: build = a second index of admin names and their parents; query = one
  lookup before the main search
Beats: ranking localities inside the text index only works when the query says
  which locality; Placeholder handles the case where it does not.

Fit for OG: we already hold the hierarchy (`AdminContextTuple`, the boundary
  layers). The piece we lack is using it to **generate candidate contexts** for
  an unanchored query. Needs no second datastore, unlike Pelias, which depends
  on Who's On First.
Effort: **M/L** (unverified) Gain: bounded by the 9.2% far-miss rate

## 6. Typo tolerance

Gap: `91 BROOKSTON DRIVE` missed `91 Brookstone Drive` — one measured case
Share of real input: not measurable from the corpus; it is a property of users

Who: Photon, whose stated purpose is search-as-you-type with typo tolerance
Technique: fuzzy term matching over the index, bounded edit distance, usually
  with a Levenshtein automaton so the fuzzy term enumerates against the term
  dictionary rather than scanning
Cost: query = a constant factor over exact matching, worse at edit distance 2
Beats: exact matching misses a single transposed letter entirely

Fit for OG: `levenshtein_automata` is **already in our dependency tree** via
  Tantivy, which exposes `FuzzyTermQuery`. The work is deciding where to allow
  it — street names yes, house numbers never — and keeping it out of the strict
  pass so it cannot outrank an exact match.
Effort: **S/M** Gain: unquantified, likely small on clean corpora

## 7. Street-level interpolation

Gap: 18.2% of Tasmanian primary cases fall back to a street centroid; of those,
  a midpoint lands in the right GH6 cell 49% of the time against a 73% ceiling
Share of real input: n/a, this is a data-sparsity response

Who: Pelias; Nominatim (`addr:interpolation` ways)
Technique: estimate the position of an unknown number from the addressed points
  that exist on the street, interpolating along the line
Cost: query = a bounded range scan for neighbours either side

Fit for OG: `place_between_neighbours` **already implements this** and is
  correct. It does not fire in Tasmania because the neighbours do not exist —
  OSM has no addressed points on those streets. The ceiling is 24 points above
  the midpoint we now return, but reaching it needs the data the fallback
  exists to compensate for. Not worth more engine work; it is the data problem
  in disguise.
Effort: n/a Gain: **~0 without more address data**

## 8. More address data

Gap: Tasmania OSM 103k addresses against G-NAF's 343k; 56% of misses have no
  OSM object within 50 m of the truth
Share of real input: n/a

Who: Pelias imports OpenAddresses alongside OSM
Technique: build from several address sources, deduplicating on
  street+number+proximity
Cost: build = proportional to the extra rows; query = none

Fit for OG: PR #15 already adds an OpenAddresses CSV reader. ⚠️ **This breaks
  the eval if done carelessly**: building from G-NAF and scoring against G-NAF
  makes the benchmark self-referential. A held-out slice must be excluded from
  the build, or scoring must move to an independent source.
Effort: M Gain: the single largest available, and the only route past ~44% on
  Tasmania

---

## Order to build

| # | Item | Effort | Payoff | Why here |
| --- | --- | --- | --- | --- |
| 1 | Number forms (§2) | S | 6% of input, 60/1000 measured | Cheapest proven win |
| 2 | Street-type tables (§4) | S/M | ~21% of input | Data, not code |
| 3 | Comma-free parsing (§1) | M | 81% of input, ≤8.5 pt | Largest structural gap |
| 4 | Locality candidates (§5) | M/L | ≤9.2% far misses | Needs §1 first |
| 5 | Typo tolerance (§6) | S/M | unquantified | Dependency already present |
| 6 | More address data (§8) | M | largest overall | Changes how we measure |
| — | Character normalisation (§3) | S | small | Whitespace already handled |
| — | Interpolation (§7) | — | ~0 | Already built; blocked on data |

§1 is the biggest gap but not first: §2–§4 are cheap, independent, and shrink
the problem §1 has to solve.

## What not to copy

- **libpostal wholesale.** 99.45% parse accuracy, but a C library with a
  multi-GB model, against a single Rust binary whose Ontario pack is 330 MB.
  Take the idea, not the dependency.
- **Who's On First.** Pelias's hierarchy is excellent and arrives as a second
  dataset to build and ship. We derive the same relationships from boundaries
  we already index.
- **Optimising for GH6.** The ~1.2 km cell used by cross-provider benchmarks
  rewards landing anywhere in the right neighbourhood. Our 25 m and 100 m
  buckets are the ones that describe a usable geocoder; a GH6 gain can come
  with a 25 m loss.

## Sources

Peer-reviewed:
- Yassine, Beauchemin, Laviolette, Lamontagne, *Multinational Address Parsing:
  A Zero-Shot Evaluation*, International Journal of Information Science and
  Technology (iJIST), 2021. arXiv:2112.04008. Subword embeddings plus an RNN;
  99% parse accuracy on 20 source countries, ≥80% on most of 41 unseen ones.
  Evidence that a *small* multilingual model generalises without per-country
  rules — the route if §1's gazetteer subset proves insufficient.
- Yassine, Beauchemin et al., *Leveraging Subword Embeddings for Multinational
  Address Parsing*, arXiv:2006.16152.

Documentation and source:
- libpostal — <https://github.com/openvenues/libpostal>, and Senzing's data-model
  notes for the 1.2B-record training set and 99.45% figure.
- Nominatim tokenizers and ICU variant generation —
  <https://nominatim.org/release-docs/latest/customize/Tokenizers/>
- Nominatim place ranking and per-country address levels —
  <https://nominatim.org/release-docs/latest/customize/Ranking/>
- Pelias Placeholder —
  <https://github.com/pelias/placeholder>, <https://pelias.io>
- Photon — <https://github.com/komoot/photon>

Not peer-reviewed, cited only where labelled:
- Gaere & von Wangenheim, *MESSY STREETS*, arXiv:2609.01612 (preprint, June
  2026). Used here **only** as a corpus of real input strings for the profiling
  table above. Its GH6 leaderboard covers 959 sampled rows, not the full 10,000,
  and its metric is too coarse for address-level precision.
