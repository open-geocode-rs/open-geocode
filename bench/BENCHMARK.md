# Speed benchmark: og, Photon and Pelias on the Netherlands

What this benchmark measures, how it keeps the comparison fair, what it has
found so far, and what it does not show. [README.md](README.md) covers running
the harness.

## The question

Three self-hosted geocoders serve the same country from the same OpenStreetMap
snapshot. The benchmark answers three questions:

1. **On the same hardware, how fast is each one?** Service time per request,
   and the ceiling: the most requests per second it serves before p99 latency
   passes 100 ms.
2. **What is the smallest machine each one needs?** The floor: the fewest CPUs
   and the least memory that still serve a fixed job of 100 requests per
   second.
3. **What does each one cost to run?** Disk, build time and start-up time.

The Netherlands has close to every address in OpenStreetMap, so the answers
cover every address in a whole country.

## Data

| | |
|---|---|
| Extract | `netherlands-261101.osm.pbf`, the 2026-11-01 snapshot, 1,409,516,668 bytes, sha256 `323cbbff3f97ffb2962e3c00ce5172752af5a1c9a56c6461ddfe7ec9a2f101d7` |
| Eligible addresses | 10,014,021 nodes and ways with a house number, a street, and a city or postcode |
| Forward queries | 10,000 addresses written as `"{street} {housenumber}, {city}"`, drawn with seed 20261101; the address's own location is the truth |
| Reverse queries | 10,000 addresses, each point moved up to 50 m at random (seeded); the address is the truth |

Every engine indexes the same snapshot. Photon reads the published JSON dump
made from the same day's data rather than the extract itself. The query files
are committed in `data/queries/`, with their hashes in `meta.json`.

## Engines

Each engine runs as a user would deploy it, set up the way its own
documentation says for the budget it is given. Each setting is in
`engines/<name>/` with a link to the documentation it comes from.

| | og | Photon | Pelias |
|---|---|---|---|
| Version | `main` (`cad9f81`) plus the fix below (`ab33aca`) | 1.3.0 | API v7.11.0, Elasticsearch 7.17.27, placeholder v5.11.0, pip v5.0.0, libpostal-service |
| Processes | 1 | 1 JVM, search node built in | 5 containers |
| Built from | the extract | the same-day JSON dump, names in nl, en, de, fr, it | the extract, split into its 12 provinces and imported in parallel |
| Indexed | 10,832,528 records, 9,774,004 addresses | 11,933,775 documents | 10,705,946 documents, 9,949,770 addresses |
| Sizing | none: og reads its CPUs and memory from its cgroup | heap half the memory, at most 31 GiB | Elasticsearch heap half of what libpostal (2 GiB), pip, placeholder and the API leave, at most 31 GiB; API and placeholder workers = CPUs |
| Left out | | | interpolation; postcodes from Who's on First |

Pelias's street layer is built from the same extract, so every engine can
answer with the street when the house number is missing. Every image is
pinned by digest in `engine.toml`.

## Fairness

- **One engine at a time.** The previous engine's containers are gone and its
  ports closed before the next one starts.
- **Pinned cores, not a CPU quota.** On the 32-vCPU host:
  - cores 16–31 are for the engine under test;
  - cores 8–15 are for the load generator;
  - cores 0–7 hold everything else, production services included.

  A quota throttles a busy server in bursts and adds latency that is not the
  engine's.
- **One budget per engine, not per container.** All of Pelias's containers
  share `sut.slice`, the same budget og's single process gets.
- **Memory is enforced, page cache included.** `memory.max` counts the page
  cache, so a box's memory is a limit the engine lived within, not an RSS
  reading.
- **Cold starts are cold.** The engine's own files are evicted from the page
  cache before every start. No global cache drop is used, since that would
  empty production's cache too.
- **Same requests.** One result per query (`limit=1` or `size=1`), keep-alive,
  no compression, the same 10,000 URLs per direction.
- **Speed is never reported without accuracy.** Every engine's answers are
  scored against the truth: the share within 100 m, and the share with any
  answer.
- **Noise is measured.** Every timed step records the VM's steal time and CPU,
  IO and memory pressure. A step with steal above 1% of the engine's cores is
  re-run.

## Protocol

- **Load generator:** `oha` 1.16.0 on its own 8 cores. Before any ceiling, it
  is run against a trivial server on the engine's cores to find its own limit
  (181,540–186,880 req/s here). A ceiling above a third of that limit would be
  reported as load-generator bound.
- **Service time:** one connection, requests back to back, all 10,000 queries
  after one untimed warm-up pass. No queue, so overload cannot inflate it.
- **Passing a rate:** a 5 s untimed lead-in, then a 30 s open-loop run at that
  rate with latency correction. A step passes if all three hold:
  - p99 is under 100 ms;
  - there are no errors and no non-2xx responses;
  - at least 99% of the rate is answered.
- **Ceiling** (16 vCPU / 64 GiB): start from the service time, raise the rate
  ×1.5 while passing, then bisect to within 10%.
- **Floor** (the job is 100 req/s):
  1. Try 4, 2 and 1 CPUs at 64 GiB.
  2. On the fewest CPUs that pass, step memory down 16, 8, 4, 2 GiB, 1 GiB,
     512 and 256 MiB until a step fails.
  3. Start every step from cold.
- **Profile:** the results below are the `quick` profile: one repeat, 30 s
  steps. The `full` profile adds three repeats in rotated engine order, 60 s
  steps, finer bisection and confirmation runs.

Host:
- AMD EPYC-Milan, 32 vCPU, KVM guest, 122 GiB, no swap.
- Ubuntu 26.04, kernel 7.0.0-38, cgroup v2.
- Docker 29.1.3.

## Results

Quick profile, one repeat, 2026-11-04. Each run's manifest, raw `oha` output
and per-query answers are in `results/`.

### Footprint and start-up

| | og | Photon | Pelias |
|---|---:|---:|---:|
| Data on disk | 0.34 GB | 3.83 GB | 4.99 GB |
| Images | 0.02 GB | 0.12 GB | 2.01 GB |
| **Disk total** | **0.36 GB** | 3.94 GB | 7.00 GB |
| Build or import ¹ | 1 min 55 s | 6 min 50 s | 10 min 18 s |
| **Start to first correct answer** | **0.36 s** | 4.8 s | 15.3 s |

¹ The imports ran side by side on cores 8–31, so each is somewhat slower than
it would be alone.

### Same machine: 16 vCPU / 64 GiB

| | og | Photon | Pelias |
|---|---:|---:|---:|
| Forward p50 / p99 / p99.9 | **0.72 / 4.2 / 11.7 ms** | 2.0 / 19.4 / 30.5 ms | 4.7 / 9.6 / 14.3 ms |
| Reverse p50 / p99 / p99.9 | **0.20 / 0.37 / 0.92 ms** | 1.8 / 3.0 / 3.9 ms | 5.3 / 10.5 / 13.6 ms |
| **Forward ceiling** | **6,191 req/s** ² | 2,194 req/s | 2,323 req/s |
| **Reverse ceiling** | **32,153 req/s** ³ | 3,098 req/s | 2,406 req/s |
| Forward: first answer within 100 m | 98.9% | 99.7% | 99.5% |
| Reverse: first answer within 100 m | 98.8% | 99.9% | 100.0% |
| Any answer, both directions | 100% | ≥ 99.9% | 100% |

Against the better of the other two, og:

- answers forward queries 2.8× faster at p50;
- answers reverse queries 9× faster at p50;
- serves 2.7× the forward traffic and 10× the reverse traffic;
- starts 13× faster;
- takes 11× less disk.

² og refuses a lookup with `503 busy` once four per CPU are running, and the
pass rule allows no refusals. At 11,144 req/s og answered 334,223 requests and
refused 91 (0.03%), with p99 7 ms. The 6,191 is the highest rate with none.

³ The same rule sets the reverse ceiling. At 33,845 req/s og refused 6
requests out of about a million, with p99 2.0 ms, so that step failed. An
earlier run on the build without the fix passed 42,215 req/s with p99 19 ms;
reverse lookups do not use the cache the fix changes. Whether a burst briefly
fills every slot within 30 seconds is chance, so og's ceilings vary from run
to run and understate what it serves.

The memory each engine used on this machine is not in the table. The JVM
engines are sized to the box (Photon reached 23.5 GiB, Pelias 39.4 GiB, og
1.1 GiB), so those figures show their configuration, not their need. The
floor is the fair memory comparison.

### Smallest machine for 100 req/s

| | og | Photon | Pelias |
|---|---|---|---|
| Forward | **1 vCPU / 1 GiB** (p99 4.4 ms; 512 MiB fails) | 2 vCPU / 4 GiB (p99 75.1 ms; 1 vCPU and 2 GiB fail) | running |
| Reverse | **1 vCPU / 256 MiB** (p99 9.5 ms; the ladder's lowest step, so og's floor is lower) | running | running |

## What the benchmark found in og

The first quick run measured og's forward ceiling at 301 req/s. On 16 CPUs,
after one step past its limit, og went on refusing almost every request, even
at a tenth of the load it had just served.

- **Cause.** Ranking keeps a per-thread cache of two-edit Levenshtein
  automata for typo matching, up to 4,096 of them.
  - A state is a table of 256 `u32`, 1 KiB, and a ten-letter word's automaton
    has about 250 states, so a full cache is about a gigabyte per thread.
  - Lookups run on the blocking threads of the async runtime, which come and
    go, so under load each new thread filled one more.
  - Memory reached the 64 GiB limit and the kernel pushed the Pack out of the
    page cache. Every lookup slowed, every slot filled, and new requests got
    `503 busy`.
- **Fix** (`fix/typo-automata-memory`, `e4d8834`): each thread now keeps at
  most 16,384 states, 16 MiB. Measured on the Netherlands Pack, 2 CPUs:

  | | Before | After |
  |---|---:|---:|
  | Memory after 10,000 lookups, one at a time | 893 MiB | 94 MiB |
  | Memory after 20,000 | 1,407 MiB | 93 MiB |
  | Memory after 30,000 more, 8 at a time | 5,128 MiB | 676 MiB |
  | p50 / p99, one at a time | 0.52 / 3.26 ms | 0.55 / 3.52 ms |

- **After the fix:** og served 11,144 req/s with 0.03% refused, then served
  every request at 6,191 req/s straight after. Overload no longer leaves it
  broken.

## Not shown here

- **Accuracy in general.** The 100 m check is a guard against fast wrong
  answers on clean, well-formed addresses. Accuracy on real messy input is in
  the README's accuracy table.
- **Absolute numbers on other hardware.** This is one VM, over loopback. The
  ratios between engines carry over to other hardware better than the
  absolute numbers do.
- **Planet scale.** This is one country.
- **Equal work per query.** Pelias parses with libpostal and looks up admin
  areas; Photon does no parsing; og parses, ranks and calibrates a confidence.
  This is reported, not corrected for.
- **Reverse accuracy gap.** og is about one point below the others on reverse
  within 100 m. The misses have not been examined yet.

## Reproducing

```sh
# og: the extract placed where og looks for it, so nothing is downloaded
mkdir -p ~/og-bench/data/og/pbf
ln ~/og-bench/data/pbf/netherlands-261101.osm.pbf ~/og-bench/data/og/pbf/netherlands.osm.pbf
OG_HOME=~/og-bench/data/og og build --region netherlands

bash engines/photon/import.sh  # Photon from the same-day dump
bash engines/pelias/import.sh  # Pelias, 12 provinces in parallel

cargo build --release
./target/release/bench smoke --engine all                  # ~7 minutes
./target/release/bench all --engine all --profile quick    # ~1.5 hours
./target/release/bench all --engine all                    # the full profile
```

Changes to any engine's setup are welcome as pull requests against
`engines/<name>/`. A change that follows that engine's documentation will be
measured again and published.
