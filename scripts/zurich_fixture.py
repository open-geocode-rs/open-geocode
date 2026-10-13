#!/usr/bin/env python3
"""Build a bench-geocode answer key from the City of Zurich address register.

Same cohort rules as scripts/gnaf_fixture.py, against a second, denser and
independently licensed source (CC0), so a result is not an artefact of one
country's data. Coordinates here are building entrances rather than parcel
centroids, which is stated in the fixture metadata.
"""
import argparse, json, math, os, random, sys, collections

EARTH_M_PER_DEG = 111320.0


def metres(a, b):
    (la1, lo1), (la2, lo2) = a, b
    mlon = EARTH_M_PER_DEG * math.cos(math.radians((la1 + la2) / 2))
    return math.hypot((lo2 - lo1) * mlon, (la2 - la1) * EARTH_M_PER_DEG)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--input", required=True)
    p.add_argument("--dev", type=int, default=2000)
    p.add_argument("--holdout", type=int, default=8000)
    p.add_argument("--seed", type=int, default=2026)
    p.add_argument("--out-dir", required=True)
    p.add_argument("--answerable-streets")
    a = p.parse_args()

    features = json.load(open(a.input, encoding="utf-8"))["features"]
    print(f"features: {len(features)}", file=sys.stderr)

    rows, skipped = [], collections.Counter()
    for feature in features:
        q = feature["properties"]
        street = (q.get("lokalisationsname") or "").strip()
        number = str(q.get("hausnummer") or "").strip()
        lat, lon = q.get("hausnummer_koord_lat"), q.get("hausnummer_koord_long")
        if not street or not number or lat is None or lon is None:
            skipped["incomplete"] += 1
            continue
        postcode = q.get("plz")
        postcode = str(int(postcode)) if isinstance(postcode, float) else str(postcode or "")
        rows.append(dict(
            pid=str(q.get("gebaeudeeingangnummer") or q.get("objectid")),
            number=number, street=street,
            locality=(q.get("plz_ortschaft") or "Zürich").strip(),
            postcode=postcode, lat=float(lat), lon=float(lon)))
    print(f"  usable: {len(rows)}  skipped: {dict(skipped)}", file=sys.stderr)

    def full_form(r):
        return f"{r['number']} {r['street']}, {r['locality']} {r['postcode']}".strip()

    def messy_form(r):
        return f"{r['number']} {r['street']}".strip()

    answerable = None
    if a.answerable_streets and os.path.exists(a.answerable_streets):
        answerable = {s.lower() for s in json.load(open(a.answerable_streets))}
        print(f"  pack street names: {len(answerable)}", file=sys.stderr)

    # Uniqueness is checked per query form, after the transform: dropping the
    # locality can make a row unanswerable, and scoring that as a miss would
    # measure the transform rather than the engine.
    buckets = {"full": {}, "messy": {}}
    for r in rows:
        buckets["full"].setdefault(full_form(r).lower(), []).append(r)
        buckets["messy"].setdefault(messy_form(r).lower(), []).append(r)

    cases, stats = [], collections.Counter()
    for r in rows:
        for form, build in (("full", full_form), ("messy", messy_form)):
            q = build(r)
            peers = buckets[form].get(q.lower(), [])
            ambiguous = len(peers) > 1 and any(
                metres((r["lat"], r["lon"]), (p["lat"], p["lon"])) > 50
                for p in peers if p is not r)
            cohort = "ambiguous" if ambiguous else "primary"
            covered = True
            if answerable is not None:
                covered = r["street"].strip().lower() in answerable
            cases.append(dict(
                name=f"{r['pid']}:{form}", q=q, form=form, cohort=cohort,
                covered=covered, expect_lat=r["lat"], expect_lon=r["lon"],
                gnaf_pid=r["pid"], locality=r["locality"], postcode=r["postcode"]))
            stats[f"{form}/{cohort}"] += 1
    for k in sorted(stats):
        print(f"  {k:22s} {stats[k]}", file=sys.stderr)

    rng = random.Random(a.seed)
    eligible = [c for c in cases if c["cohort"] == "primary"]
    rng.shuffle(eligible)
    dev, hold = eligible[:a.dev], eligible[a.dev:a.dev + a.holdout]

    os.makedirs(a.out_dir, exist_ok=True)
    meta = dict(
        state="Zurich", source="Stadt Zurich Adressen (CC0)", datum="WGS84",
        seed=a.seed, coordinate="building entrance",
        buckets_m=dict(exact=25, correct=100, near=500, off=5000),
        hit5_rule="best distance among top 5")
    for label, data in (("dev", dev), ("holdout", hold)):
        path = os.path.join(a.out_dir, f"zurich-dev.json" if label == "dev" else "zurich-holdout.json")
        json.dump(dict(meta=meta, cases=data), open(path, "w"), indent=1)
        print(f"wrote {path}: {len(data)}", file=sys.stderr)


if __name__ == "__main__":
    main()
