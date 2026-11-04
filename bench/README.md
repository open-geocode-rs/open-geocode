# bench

The speed benchmark behind the README's Speed table. It runs open-geocode,
Photon and Pelias on the Netherlands, all built from the same OpenStreetMap
extract (2026-11-01). They run one at a time on the same machine.

## Run

```sh
cargo build --release
./target/release/bench smoke --engine all                # ~7 min: checks the setup
./target/release/bench all --engine all --profile quick  # ~1.5 h: 1 repeat
./target/release/bench all --engine all                  # 3 repeats
```

It needs Docker, passwordless `sudo` and `oha` at `~/og-bench/bin/oha`.

Build the engines first:

- og: `og build --region netherlands`
- Photon: `engines/photon/import.sh`
- Pelias: `engines/pelias/import.sh`

Results go to `results/<run>/`, with every raw measurement.

## Rules

- Each engine runs alone on cores 16–31. Its cgroup caps its memory, page
  cache included. The load generator runs on cores 8–15.
- Each engine is sized as its own documentation says
  (`engines/<name>/size.sh`).
- Queries are 10,000 forward and 10,000 reverse, drawn with a fixed seed from
  the extract (`data/queries/`).
- **Latency:** one request at a time.
- **Max throughput:** the highest rate with p99 under 100 ms and at least
  99.9% of requests answered, on 16 vCPU / 64 GiB.
- **Smallest machine:** the fewest CPUs, then the least memory, that hold
  100 req/s under the same rule. Every step starts cold.
- **Accuracy:** the share of first answers within 100 m of the true address.
