#!/usr/bin/env python3
"""Draw a seeded per-country sample from libpostal's parser training dump.

Reads the decompressed dump on stdin and stops as soon as every country quota
is full, so the 8 GB archive is never stored:

  curl -sL https://archive.org/download/libpostal-parser-training-data-20170304/formatted_addresses_tagged.random.tsv.gz \
    | gzip -dc | python -I scripts/parse-eval/sample_libpostal.py --out-dir tmp/raw/libpostal

The dump is already shuffled, so each row of a wanted country is kept with
probability --keep from a seeded generator: the same seed over the same file
keeps the same rows. Rows are written unchanged, with libpostal's own labels;
`open-geocode parse-fixture normalize` maps and normalizes them.
"""
import argparse, os, random, sys

COUNTRIES = "ca ch de hu jp in id gr ru gb fr us au it".split()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--out-dir", required=True)
    p.add_argument("--per-country", type=int, default=5000)
    p.add_argument("--seed", type=int, default=2026)
    p.add_argument("--keep", type=float, default=0.5)
    p.add_argument("--countries", default=",".join(COUNTRIES))
    a = p.parse_args()

    rng = random.Random(a.seed)
    wanted = a.countries.split(",")
    os.makedirs(a.out_dir, exist_ok=True)
    outs = {cc: open(os.path.join(a.out_dir, f"{cc}.tsv"), "w", encoding="utf-8", newline="\n") for cc in wanted}
    counts = dict.fromkeys(wanted, 0)
    open_quotas = len(wanted)
    seen = 0
    stdin = open(sys.stdin.fileno(), encoding="utf-8", errors="replace", newline="\n")
    for line in stdin:
        seen += 1
        fields = line.rstrip("\n").split("\t")
        if len(fields) != 3:
            continue
        cc = fields[1]
        if counts.get(cc, a.per_country) >= a.per_country:
            continue
        # One draw per eligible row keeps the stream position of every draw
        # fixed, so the sample depends only on the seed and the file.
        if rng.random() >= a.keep:
            continue
        outs[cc].write(line if line.endswith("\n") else line + "\n")
        counts[cc] += 1
        if counts[cc] == a.per_country:
            open_quotas -= 1
            print(f"{cc} full after {seen} lines", file=sys.stderr, flush=True)
            if open_quotas == 0:
                break
        if seen % 10_000_000 == 0:
            print(f"{seen} lines {counts}", file=sys.stderr, flush=True)
    for out in outs.values():
        out.close()
    print(f"read {seen} lines; kept {counts}", file=sys.stderr)


if __name__ == "__main__":
    main()
