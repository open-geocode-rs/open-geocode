# Search timing profile

Where `/search` spends its time on the planet Pack, measured 2026-10-27 on branch `feat/search-rework`.
Read this before changing search for speed: it says what is slow and what is not.

## Setup

- Pack: `E:\og\planet-oa-pack` (planet OSM plus OpenAddresses, 26 GB, built before this branch; no rebuild needed).
- Machine: 24 cores, 32 GB RAM, Windows.
- Inputs: `E:\og\speed\corpus\search.json`, the MESSY STREETS gold addresses shuffled by `E:\og\speed\make_corpus.py`.
- One client, one `/search?limit=1` at a time, over loopback HTTP.

## Whole-query times, first 2000 inputs

| | before (`4d86365`) | after (this branch) |
|---|---|---|
| p50 | 23 ms | 39 ms |
| p90 | 367 ms | 190 ms |
| p99 | 2658 ms | 496 ms |
| max | 25069 ms | 1427 ms |
| queries over 1 s | 69 | 2 |
| total | 379 s | 153 s |

The before run went after a warm-up pass, so its cache was warmer than the after run's.
The nested retries are gone: the 25 s query (`Lakes Dr #225 West Covina CA 1050 US`) and the other 10 s+ ones now take under 1.5 s.
p50 got worse, and 66 of the 2000 top answers moved by more than 100 m; those have not been judged against the MESSY STREETS targets yet.

## Per-step times, the 183 inputs that took 200 ms or more

Measured with temporary timers around each step (not committed), second pass with a warm cache.

| step | time | share | calls | per call |
|---|---|---|---|---|
| Tantivy collect of the rank window | 30.3 s | 44% | 1275 | 24 ms |
| Tantivy re-collect of 1024 docs on a tie | 14.2 s | 21% | 404 | 35 ms |
| rank keys of the collected docs | 6.7 s | 10% | 230k docs | 0.03 ms |
| typo lookups (`spellings`, dictionary walks) | 6.1 s | 9% | 6706 walks | 1.2 ms |
| neighbour estimate of a missing number | 3.3 s | 5% | 315 | 10 ms |
| area fallback | 2.8 s | 4% | 75 | 37 ms |
| parse, postcode weights, accept checks, record reads, joint decoding | under 1 s each | | | |

The same 183 inputs took 114 s on the colder first pass and 68 s on the second: about 46 s went to reading the Pack from disk while ranking and hydrating.

## What it means

- One Tantivy search costs 24 ms on average and up to 1.5 s, and a slow query still runs about 6 of them.
- A search visits every document that matches, so its cost is the size of its match set.
  Each required word is a union of several fields, up to 32 typo neighbours and an area stand-in; the postcode only ranks; a house number like `1` matches millions.
  Tantivy's block-max skipping applies only to a flat list of optional terms, so these nested queries get none of it.
- A third of searches end their window in a tie and re-collect 1024 documents.
- The 300 ms budget is wall-clock: the same query ran 3.1 searches on the cold pass and 5.7 on the warm one, so its answer can depend on machine load.
- Typo lookups run for every word of every query before any search, outside the budget; they are 9%, not the main cost.

## Next fix, no Pack rebuild needed

1. Narrow by the selective key first: a postcode the Pack holds filters (`postcode_exact`, Must) instead of only ranking; a known town stays required.
2. Keep required words to one term in the field their role names; no typo neighbours or stand-ins inside the required part.
3. Look up typos only for words the index does not hold, or when the exact search finds nothing.
4. Score postcode nearness, stand-ins, focus and ranges on the collected window, not as clauses every match is checked against.
5. Drop the 1024-document tie re-collect.
6. Budget by number of searches, not milliseconds.

Before changing the query shape, count the documents each search matches on the slow inputs to confirm the match sets are the cost.

## How to measure again

1. `cargo build --release`, then `target/release/open-geocode.exe serve --pack E:/og/planet-oa-pack --bind 127.0.0.1:8103`.
2. Send the inputs one at a time to `/search?limit=1` and time each request.
3. For per-step times, wrap the steps of `TextSearcher::search_variants`, `search_plan` and `ranked_hits` in timers that print one JSON line per query to stderr, then remove them.
