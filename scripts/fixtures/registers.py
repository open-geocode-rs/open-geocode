"""Write a `geocode bench` answer key from a national address register.

    pip install -r scripts/fixtures/requirements.txt
    npm ci --prefix scripts/fixtures            # Node 22 or later
    python scripts/fixtures/registers.py <register> --cache <dir>

Every eligible row is ranked by sha256('<seed>:<register id>') and the lowest
SAMPLE ids are kept: a hash-based draw, so it is uniform, does not depend on
file order or threads, and never looks at og. duckdb ranks the whole register
in one parallel pass. Each drawn row is written twice, as OpenCage's postal
template formats it (`full`) and with postcode, region and country dropped and
words abbreviated by OpenCage's lists (`messy`). The truth is the register's
own point, converted to WGS84 by PROJ when the register uses another CRS.
PROJ fetches the grids it needs from its CDN and must use the best operation,
so every machine converts through the same grids.
"""

import argparse
import hashlib
import importlib
import json
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

import duckdb
import pyproj
from pyproj import Transformer
from pyproj.transformer import TransformerGroup
from pyproj.network import set_network_enabled

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
SEED = 2026
SAMPLE = 2000
FORMATTER = "@fragaria/address-formatter 7.1.2"
COMPONENTS = ("house_number", "road", "postcode", "city", "town", "village", "suburb", "hamlet", "municipality", "state")

# BeST writes `0 0` for an address it has not placed.
REASON = """
    case
        when x is null or y is null or isnan(x) or isnan(y) or (x = 0 and y = 0) then 'no point'
        when estimated then 'estimated point'
        when unit then 'unit'
        when house_number is null or not regexp_matches(house_number, '[0-9]') then 'no house number'
        when road is null then 'no street'
        when coalesce(city, town, village, suburb, hamlet, municipality) is null then 'no locality'
    end
"""


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def draw(rows):
    """The rows read, why each dropped one was dropped, and the SAMPLE
    lowest-ranked eligible ids. An id the register lists with differing
    fields has no one right answer and is dropped; exact repeats of a row are
    one address. Copies of an id share its rank, so they arrive together."""
    con = duckdb.connect()
    counts = dict(con.sql(f"select coalesce({REASON}, 'kept'), count(*) from {rows} group by all").fetchall())
    seen = sum(counts.values())
    columns = ", ".join(("id", "x", "y", "crs", "country", *COMPONENTS))
    # Counted over the whole register, in rows, as every other reason is.
    (several,) = con.sql(f"""
        select count(*) from {rows}
        where {REASON} is null and id in (
            select id from (select distinct {columns} from {rows} where {REASON} is null)
            group by id having count(*) > 1
        )
    """).fetchone()
    counts["kept"] = counts.get("kept", 0) - several
    if several:
        counts["several points"] = several
    dropped = {reason: count for reason, count in sorted(counts.items()) if reason != "kept"}
    limit = 2 * SAMPLE
    while True:
        result = con.sql(f"""
            select {columns} from {rows}
            where {REASON} is null
            order by sha256('{SEED}:' || id), id
            limit {limit}
        """)
        names = result.columns
        found = [dict(zip(names, values)) for values in result.fetchall()]
        by_id = defaultdict(set)
        order = []
        for row in found:
            if row["id"] not in by_id:
                order.append(row["id"])
            by_id[row["id"]].add(tuple(row.items()))
        # The last id may have copies past the limit, so it is never trusted.
        complete = order[:-1] if len(found) == limit else order
        single = [rid for rid in complete if len(by_id[rid]) == 1]
        if len(single) >= SAMPLE or len(found) < limit:
            break
        limit *= 2
    kept = single[:SAMPLE]
    return [dict(next(iter(by_id[rid]))) for rid in kept], seen, dropped


def to_wgs84(rows):
    """Each row's lon and lat, and the PROJ operation used per CRS. Without
    `only_best`, PROJ quietly falls back to a coarser Helmert shift when a
    grid is not installed, so two machines would write different truths."""
    set_network_enabled(True)
    by_crs = defaultdict(list)
    for row in rows:
        by_crs[row["crs"] or "EPSG:4326"].append(row)
    operations = {}
    for crs, group in sorted(by_crs.items()):
        transformer = Transformer.from_crs(crs, "EPSG:4326", always_xy=True, only_best=True)
        lons, lats = transformer.transform([row["x"] for row in group], [row["y"] for row in group], errcheck=True)
        # A transformer holding several candidate operations describes none,
        # so the best candidate, the one only_best uses, is named instead.
        operations[crs] = TransformerGroup(crs, "EPSG:4326", always_xy=True).transformers[0].description
        for row, lon, lat in zip(group, lons, lats):
            row["lon"], row["lat"] = lon, lat
    return operations


def formatted(rows, country):
    lines = "".join(
        json.dumps(
            {
                "id": row["id"],
                "countryCode": row.get("country") or country,
                **{key: row[key] for key in COMPONENTS if row[key]},
            },
            ensure_ascii=False,
        )
        + "\n"
        for row in rows
    )
    result = subprocess.run(["node", str(HERE / "format.mjs")], input=lines.encode(), capture_output=True, cwd=HERE)
    if result.returncode:
        sys.exit(f"format.mjs failed:\n{result.stderr.decode()}")
    return {item["id"]: item for item in map(json.loads, result.stdout.decode().splitlines())}


def write(path, meta, cases):
    with open(path, "w", encoding="utf-8", newline="\n") as handle:
        handle.write('{\n "meta": ')
        handle.write(json.dumps(meta, ensure_ascii=False, indent=1).replace("\n", "\n "))
        handle.write(',\n "cases": [\n')
        handle.write(",\n".join("  " + json.dumps(case, ensure_ascii=False) for case in cases))
        handle.write("\n ]\n}\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("register", help="module name under scripts/fixtures/registers")
    parser.add_argument("--cache", required=True, type=Path, help="where register downloads are kept")
    args = parser.parse_args()

    from registers import standard

    if not (HERE / "node_modules" / "@fragaria" / "address-formatter").is_dir():
        sys.exit("the formatter is not installed: npm ci --prefix scripts/fixtures")
    module = importlib.import_module(f"registers.{args.register}")
    cache = args.cache / args.register
    cache.mkdir(parents=True, exist_ok=True)
    files, release = module.download(cache)
    rows, seen, dropped = draw(standard(module, files))
    if len(rows) < SAMPLE:
        sys.exit(f"only {len(rows)} eligible ids, {SAMPLE} wanted")
    finish = getattr(module, "finish", None)
    for row in rows:
        if finish:
            finish(row)
    operations = to_wgs84(rows)
    text = formatted(rows, module.SOURCE["country"])

    cases = []
    for row in rows:
        for form in ("full", "messy"):
            cases.append(
                {
                    "name": f"{row['id']}:{form}",
                    "q": text[row["id"]][form],
                    "form": form,
                    "cohort": "address",
                    "expect_lat": round(row["lat"], 7),
                    "expect_lon": round(row["lon"], 7),
                }
            )
    meta = {
        **module.SOURCE,
        "release": release,
        "files": {path.name: sha256(path) for path in files},
        "rows": seen,
        "dropped": dropped,
        "seed": SEED,
        "sample": SAMPLE,
        "draw": f"lowest sha256('{SEED}:' || register id) among eligible rows",
        "forms": {
            "full": f"OpenCage postal template with country, {FORMATTER}",
            "messy": "same, without postcode, region and country, abbreviated by OpenCage's lists",
        },
        "truth": "the register's point, converted to WGS84 by PROJ where the register uses another CRS",
        "proj": {"version": pyproj.proj_version_str, "operations": operations},
        "rebuild": f"python scripts/fixtures/registers.py {args.register} --cache <dir>",
    }
    out = REPO / "fixtures" / "queries" / f"{args.register}.json"
    write(out, meta, cases)
    print(f"{out.relative_to(REPO)}: {len(cases)} cases from {seen} rows, dropped {dropped}", file=sys.stderr)


if __name__ == "__main__":
    sys.path.insert(0, str(HERE))
    main()
