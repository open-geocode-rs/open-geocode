#!/usr/bin/env python3
"""Build a bench-geocode answer key from G-NAF per-state PSV files.

Cohorts (nothing is dropped for being hard):
  primary    principal, current, positional geocode, unique per query form
  unit       has FLAT_NUMBER or LEVEL_NUMBER
  ambiguous  query form resolves to >1 distinct address >50m apart
  retired    DATE_RETIRED set
  alias      ALIAS_PRINCIPAL != 'P'
Genuine exclusions: no positional geocode (nothing to compare against),
referential-integrity failures.
"""
import argparse, csv, json, math, os, random, sys, collections

POSITIONAL = {"PC", "PCM", "BC", "BCM"}
EARTH_M_PER_DEG = 111320.0


def metres(a, b):
    (la1, lo1), (la2, lo2) = a, b
    mlon = EARTH_M_PER_DEG * math.cos(math.radians((la1 + la2) / 2))
    return math.hypot((lo2 - lo1) * mlon, (la2 - la1) * EARTH_M_PER_DEG)


def load(path, delim="|"):
    with open(path, newline="", encoding="utf-8", errors="replace") as f:
        yield from csv.DictReader(f, delimiter=delim)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--gnaf-dir", required=True)
    p.add_argument("--state", required=True)
    p.add_argument("--street-types", required=True, help="Authority_Code_STREET_TYPE_AUT_psv.psv")
    p.add_argument("--dev", type=int, default=2000)
    p.add_argument("--holdout", type=int, default=8000)
    p.add_argument("--seed", type=int, default=2026)
    p.add_argument("--out-dir", required=True)
    p.add_argument("--answerable-streets", help="JSON list of street names present in the pack")
    a = p.parse_args()

    g, st = a.gnaf_dir, a.state
    ledger = []

    def step(name, n):
        ledger.append((name, n))
        print(f"  {name:<44s} {n:>9d}", file=sys.stderr)

    print(f"=== {st}: building answer key ===", file=sys.stderr)

    # STREET_TYPE_AUT maps the full word to its abbreviation
    # (CODE=CRESCENT, NAME=CR), so STREET_TYPE_CODE already holds the full
    # word and the AUT gives the short form used by the messy variant.
    abbrev = {r["CODE"]: r["NAME"] for r in load(a.street_types)}

    geo = {}
    gtypes = collections.Counter()
    for r in load(f"{g}/{st}_ADDRESS_DEFAULT_GEOCODE_psv.psv"):
        if r["DATE_RETIRED"].strip():
            continue
        gtypes[r["GEOCODE_TYPE_CODE"]] += 1
        if r["GEOCODE_TYPE_CODE"] in POSITIONAL and r["LATITUDE"] and r["LONGITUDE"]:
            geo[r["ADDRESS_DETAIL_PID"]] = (
                float(r["LATITUDE"]), float(r["LONGITUDE"]), r["GEOCODE_TYPE_CODE"])
    step("default geocodes (live)", sum(gtypes.values()))
    step("  of which positional", len(geo))

    sl = {r["STREET_LOCALITY_PID"]: (r["STREET_NAME"], r["STREET_TYPE_CODE"], r["LOCALITY_PID"])
          for r in load(f"{g}/{st}_STREET_LOCALITY_psv.psv")}
    loc = {r["LOCALITY_PID"]: r["LOCALITY_NAME"]
           for r in load(f"{g}/{st}_LOCALITY_PID_placeholder" if False else f"{g}/{st}_LOCALITY_psv.psv")}

    cohorts = collections.Counter()
    rows = []
    total = 0
    for r in load(f"{g}/{st}_ADDRESS_DETAIL_psv.psv"):
        total += 1
        pid = r["ADDRESS_DETAIL_PID"]
        cohort = None
        if r["DATE_RETIRED"].strip():
            cohort = "retired"
        elif r["ALIAS_PRINCIPAL"] != "P":
            cohort = "alias"
        gi = geo.get(pid)
        if gi is None:
            cohorts["excluded_no_positional_geocode"] += 1
            continue
        s = sl.get(r["STREET_LOCALITY_PID"])
        if not s or not r["NUMBER_FIRST"]:
            cohorts["excluded_integrity"] += 1
            continue
        if cohort is None:
            cohort = "unit" if (r["FLAT_NUMBER"] or r["LEVEL_NUMBER"]) else "primary"
        name, tcode, lpid = s
        rows.append(dict(
            pid=pid, cohort=cohort,
            number=r["NUMBER_FIRST"], street=name,
            street_type=tcode or "",
            street_type_short=abbrev.get(tcode, tcode) or "",
            flat=r["FLAT_NUMBER"], level=r["LEVEL_NUMBER"],
            locality=loc.get(lpid, ""), postcode=r["POSTCODE"],
            lat=gi[0], lon=gi[1], geocode_type=gi[2]))
        cohorts[cohort] += 1
    step("ADDRESS_DETAIL rows", total)
    for k in ("primary", "unit", "retired", "alias",
              "excluded_no_positional_geocode", "excluded_integrity"):
        step(f"  {k}", cohorts[k])

    # ---- query forms ----
    def full_form(r):
        parts = [r["number"], r["street"], r["street_type"]]
        q = " ".join(x for x in parts if x).strip()
        return f"{q}, {r['locality']} {st} {r['postcode']}".strip()

    def messy_form(r):
        """Abbreviated type, no locality, no postcode: the shape real input takes."""
        return " ".join(
            x for x in [r["number"], r["street"], r["street_type_short"]] if x
        ).strip()

    # ---- post-transform uniqueness, PER FORM (review finding 1) ----
    forms = {}
    for r in rows:
        forms.setdefault(full_form(r).lower(), []).append(r)
    messy = {}
    for r in rows:
        messy.setdefault(messy_form(r).lower(), []).append(r)

    def ambiguous(bucket, r, key):
        peers = bucket.get(key, [])
        if len(peers) < 2:
            return False
        base = (r["lat"], r["lon"])
        return any(metres(base, (p["lat"], p["lon"])) > 50 for p in peers if p is not r)

    answerable = None
    if a.answerable_streets and os.path.exists(a.answerable_streets):
        answerable = {s.lower() for s in json.load(open(a.answerable_streets))}
        print(f"  answerable street names loaded: {len(answerable)}", file=sys.stderr)

    cases = []
    form_stats = collections.Counter()
    for r in rows:
        for form_name, builder, bucket in (("full", full_form, forms),
                                           ("messy", messy_form, messy)):
            q = builder(r)
            cohort = r["cohort"]
            if cohort in ("primary", "unit") and ambiguous(bucket, r, q.lower()):
                cohort = "ambiguous"
            covered = True
            if answerable is not None:
                # The Pack stores a street as "<name> <type>" ("bracken lane"),
                # G-NAF keeps the two apart, so compare the joined form and
                # fall back to the bare name for streets that carry no type.
                joined = f"{r['street']} {r['street_type']}".strip().lower()
                covered = joined in answerable or r["street"].strip().lower() in answerable
            cases.append(dict(
                name=f"{r['pid']}:{form_name}", q=q, form=form_name,
                cohort=cohort, covered=covered,
                expect_lat=r["lat"], expect_lon=r["lon"],
                gnaf_pid=r["pid"], geocode_type=r["geocode_type"],
                locality=r["locality"], postcode=r["postcode"]))
            form_stats[f"{form_name}/{cohort}"] += 1
    for k in sorted(form_stats):
        step(f"  form {k}", form_stats[k])

    # ---- sample ----
    rng = random.Random(a.seed)
    eligible = [c for c in cases if c["cohort"] in ("primary", "unit")]
    rng.shuffle(eligible)
    need = a.dev + a.holdout
    picked = eligible[:need]
    dev, hold = picked[:a.dev], picked[a.dev:a.dev + a.holdout]
    extras = [c for c in cases if c["cohort"] in ("ambiguous", "retired")]
    rng.shuffle(extras)
    extras = extras[:1000]

    os.makedirs(a.out_dir, exist_ok=True)
    meta = dict(
        state=st, gnaf_release="AUG 2026", datum="GDA2020", seed=a.seed,
        positional_types=sorted(POSITIONAL),
        buckets_m=dict(exact=25, correct=100, near=500, off=5000),
        geohash_backstop=["gh6", "gh4"],
        hit5_rule="best distance among top 5",
        note="buckets frozen before first scored run; offset vector is diagnostic only")
    for label, data in (("dev", dev), ("holdout", hold), ("cohorts", extras)):
        path = os.path.join(a.out_dir, f"tas-gnaf-{label}.json")
        json.dump(dict(meta=meta, cases=data), open(path, "w"), indent=1)
        print(f"wrote {path}: {len(data)} cases", file=sys.stderr)
    json.dump(dict(meta=meta, ledger=ledger, geocode_types=dict(gtypes)),
              open(os.path.join(a.out_dir, "tas-gnaf-ledger.json"), "w"), indent=1)


if __name__ == "__main__":
    main()
