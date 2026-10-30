## Problem

<!-- What is slow or large, measured, naming the Pack, the query set and the machine.
"On the planet Pack, p99 of /search over the Photon suite is 2.3 s." -->

<!-- Where the time or memory goes.
"Queries with no stated area try every typo tier on every reading, and each tier rescans the subject's postings." -->

## Change

<!-- What changed on the query path or in the build. -->

## Verification

<!-- How the numbers were measured: the command, the runs, the spread between runs. -->
<!-- Work counts (candidates scored, Pack pages touched, allocations) beside the times. -->
<!-- Accuracy must not drop: every answer is kept, or each changed one is listed. -->
<!-- What was not checked. -->

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
