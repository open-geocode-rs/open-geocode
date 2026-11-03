## Reason

<!-- What the current shape makes hard, with an example.
"Strict, street and lenient passes each copy the agreement checks, so a postcode rule changed in one is missed in the others." -->

## Change

<!-- What moved, merged or was deleted. -->

## Behavior

<!-- Why answers are identical: what was compared, and how.
Every fixture register returns byte-identical answers to main on the same Pack. -->

## Verification

<!-- Tests run, and what was not checked. -->

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
