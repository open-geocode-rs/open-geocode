# bench

The speed benchmark harness. It measures the geocoders in `engines/` on the
same Netherlands extract, one at a time, in a fixed slice of one machine, and
writes every raw number it saw next to the tables it draws from them.

## Running

```sh
export PATH=$HOME/.cargo/bin:$PATH
cargo build --release
./target/release/bench queries                  # once; output is committed
./target/release/bench smoke --engine all       # minutes; catches config errors
./target/release/bench all --engine all         # hours; the real run
./target/release/bench report --out results/<run>
```

Subcommands: `queries`, `measure`, `ceiling`, `floor`, `scale`, `all`,
`smoke`, `report`. Run ones take `--engine og|photon|pelias|all` (default
`all`), `--repeats N` (default 3; `floor` and `scale` run once) and
`--out DIR` (default `results/<UTC date-time>`). Long runs belong in tmux:
SIGHUP stops the run and restores the host.

The harness needs passwordless `sudo` (slices, scopes, cache eviction, `du`),
the `docker` group, and `~/og-bench/bin/oha` (or `--oha PATH`).

## Host layout

| Cores | Who | How |
|---|---|---|
| 0-7 | everything already running | `AllowedCPUs=0-7` on `system.slice`, `user.slice`, `init.scope`, `--runtime`, reset to empty on exit, error, Ctrl-C, SIGTERM or SIGHUP |
| 8-15 | oha | `systemd-run --scope --slice=load.slice -p AllowedCPUs=8-15` |
| 16-31 | engine under test | `sut.slice`: `AllowedCPUs` = first N cores from 16, `MemoryMax` = `MemoryLow` = the box, `MemorySwapMax=0`, `IOWeight=1000` |

`memory.max` counts page cache, so a box's memory is a limit the engine lived
within, not an RSS reading. `sut.slice` is stopped and started for every
session, which also resets its `memory.peak`.

A cold start is `compose down`, then `posix_fadvise(DONTNEED)` over every file
under the engine's `data` paths (never a global `drop_caches`, which would
empty production's cache too), then `size.sh`, then `compose up`, timed to the
first 200 whose coordinate pointer resolves for the first forward query.
Before `up` the engine's port must be free; after `down` every container must
be gone and every port opened since `up` closed again.

Every timed step samples `/proc/stat` and `/proc/pressure/{cpu,io,memory}`
before and after. A step whose steal exceeds 1% of the SUT cores' time is
re-run, up to twice; the reasons are kept and a step that never gets under
the limit is kept but marked invalid.

## Engine contract

`engines/<name>/` holds `engine.toml` (`name`, `port`, `forward` with `{q}`,
`reverse` with `{lat}` and `{lon}`, `coords` as a JSON pointer to `[lon, lat]`,
`data`, `images`), `size.sh CPUS MEM_MIB` which writes `.env` (exit 1: the
box is below what the engine can be configured for; exit 2: usage error,
stops the run), and `compose.yml` with `network_mode: host` and
`cgroup_parent: sut.slice` on every service.

## Queries

`bench queries` runs `osmium tags-filter nw/addr:housenumber` and
`osmium export -f geojsonseq` over the pbf, keeps every node or way with a
house number, a street, and a city or postcode (ways become their area or
line centroid), and samples 20,000 of them with ChaCha8 seeded 20261101:
10,000 forward queries `"{street} {housenumber}, {city}"` (postcode when there
is no city) and 10,000 reverse points jittered uniformly within 50 m.
`data/queries/meta.json` records the eligible-address count and the commands.

## Measurements

- **measure**, 16 CPUs / 64 GiB: cold start; correctness over every query,
  one at a time from the harness (first result within 100 m, and any result);
  one untimed oha pass over each URL file; service time with `-c 1`;
  `memory.peak` and `memory.stat` of `sut.slice`; `du` of `data` and image
  sizes from `docker image inspect` (content size in the image store).
- **pass at a rate**: a 10 s untimed lead-in, then 60 s of
  `oha -q RATE -c 256 --latency-correction -w`. Passes if p99 < 100 ms, no
  errors and no non-2xx, and 2xx responses per second ≥ 99% of the rate.
- **ceiling**, 16 CPUs / 64 GiB: start at half of CPUs / p50, ×1.5 while
  passing (÷1.5 if the start fails), then bisect to within 5%.
- **floor**: the job is 100 req/s. CPUs: 4, 2, 1 at 64 GiB, stopping at the
  first fail; 16 if 4 fails; none means "does not meet job". Memory: geometric
  bisection from 64 GiB down to 256 MiB to within 10%, every step from cold.
  The found box and the next memory step down are each run three more times.
- **scale**, og only: the ceiling at 1, 2, 4, 8 and 16 CPUs with 64 GiB.
- **load-generator headroom**, before any ceiling: oha against a trivial
  server on the SUT cores, searched like a ceiling. Ceilings above a third of
  it are reported as load-generator bound, not as numbers.
- **all**: manifest, headroom, then per repeat measure and ceiling in rotated
  engine order (og, photon, pelias; photon, pelias, og; pelias, og, photon),
  then floor, then scale, then the report.
- **smoke**: all of it once, with 10 s steps, 2 s lead-ins and 1,000 queries.

## Output

`results/<run>/`: `manifest.json` (pbf and query hashes, engine.toml and image
digests, og commit, kernel, CPU, virtualisation, Docker and oha versions, fio
4k random and 1M sequential reads cached in `results/fio-cache.json`),
`results.jsonl` (one record per measurement), `raw/` (every oha JSON),
`urls/`, `correctness/` (per-query outcomes), `bench.log`, and from those
`summary.json` and `report.md`.
