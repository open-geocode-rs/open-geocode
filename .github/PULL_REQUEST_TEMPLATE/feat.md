## Need

<!-- Who needs it and what they cannot do today.
"A batch user with 2 million rows cannot tell og which country all of them are in." -->

## Change

<!-- What is added: an endpoint parameter, a CLI flag, a layer, a Pack section or a source. -->

<!-- What it deliberately does not do.
"`country=` restricts answers; it does not add the country to the query as a stated area." -->

## Example

<!-- A request and its response, as a user sends it. -->

## Verification

<!-- Tests added, and what was not checked. -->

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
