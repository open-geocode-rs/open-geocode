## Problem

<!-- The query, the endpoint, the answer og gives and the answer it should give.
"/search for `12 Rue de la Paix 75002 Paris` returns Rue de la Paix in Lyon." -->

<!-- Where it goes wrong: parser, retrieval, pass, ranking, confidence or Pack data.
"The parser's best reading labels 75002 as a house number, so no stated postcode reaches retrieval." -->

<!-- Why that produces the wrong answer.
"With no stated area, the lenient pass ranks every Rue de la Paix by text alone, and Lyon's has more address records." -->

## Change

<!-- The exact functions, readings, passes or Pack sections changed. -->

<!-- What was deliberately left alone, and why.
"Five-digit numbers before a street keep the house-number label: `10000 Main St` exists in US registers." -->

## Example

<!-- One query, run before and after on the same Pack. -->
Before:
After:

## Verification

<!-- The test that fails on main and passes here. -->
<!-- What was not checked. "Checked on the planet Pack only; not on a region Pack." -->

## Impact

<!-- Every row is filled. A row that does not change says so, and why.
"Space: unchanged; no Pack section or record field is added." -->

Pack: <!-- planet or region, Pack fingerprint, generation -->
Machine: <!-- CPU, cores, disk -->
Rebuild needed: <!-- no / yes -->
API contract changed: <!-- no / yes: which field, parameter or error -->

| | main | this PR |
|---|---|---|
| Accuracy | <!-- per fixture register: right answers out of 2,000; answers changed --> | |
| Time | <!-- /search p50 and p99; candidates scored per query; build time if rebuilt --> | |
| Space | <!-- Pack size; peak query memory; binary size; build peak RAM and scratch if rebuilt --> | |

## Downsides

<!-- Each row that got worse, and why it is accepted.
"p99 rises 40 ms: the street pass now tries the second reading, which fixes 31 Danish addresses." -->
