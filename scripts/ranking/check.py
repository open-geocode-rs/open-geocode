"""Check /autocomplete ranking against fixed cases, and compare two builds.

    python scripts/ranking/check.py sections
    python scripts/ranking/check.py check --server http://127.0.0.1:8080
    python scripts/ranking/check.py compare --base http://127.0.0.1:8132 --new http://127.0.0.1:8133
    python scripts/ranking/check.py sample --base http://127.0.0.1:8132 --new http://127.0.0.1:8133

Run it against servers on a planet Pack. Run both builds side by side, so load
and the lookup time budget fall on them alike.

`compare` exits 1 when a case the base passes fails on the new build. It also
lists every case whose first answer changed, so a change nobody wrote a case
for is still seen. `sample` draws towns and postcodes from the Pack with a
fixed seed and types each one letter by letter, so the cases are not chosen
by hand.

Cases live in fixtures/ranking/<section>.tsv, one section per kind of query:

    query <TAB> focus <TAB> top <TAB> expect <TAB> note

focus is `lon,lat` or empty. expect is one or more `&`-separated groups,
and a case passes when every group holds for the first `top` answers. A group
holds when one of its `|`-separated expectations does:

    KIND@LAT,LON~KM[:NAME]   an answer of KIND within KM of the point
    KIND@near~KM[:NAME]      an answer of KIND within KM of the focus
    !KIND@...                no answer of KIND within KM of the point
    distinct                 the first `top` labels all differ
    script                   every label is written in the query's scripts

KIND is place (any area or settlement), postcode, poi, street, address or
any. NAME, when given, must appear in the answer's name or label, ignoring
case and accents.
"""

import argparse
import concurrent.futures
import json
import math
import random
import sys
import time
import unicodedata
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

CASES = Path(__file__).resolve().parents[2] / "fixtures" / "ranking"

SECTIONS = {
    "city": "A city's whole name, with and without a caller position.",
    "city_prefix": "The first letters of a well-known city.",
    "namesake": "A name several towns share; the caller's position picks one.",
    "local_town": "The first letters of the small town the caller is in.",
    "neighbourhood": "A district or neighbourhood inside a city.",
    "region": "A state, province or region.",
    "country": "A country, in English and in its own language.",
    "postcode": "A postcode alone, in each country's own format.",
    "postcode_town": "A postcode with its town, in either order.",
    "town_region": "A town followed by its state or province.",
    "town_country": "A town followed by its country.",
    "extra_words": "A place with words people add around it.",
    "landmark": "A famous landmark by its name alone.",
    "landmark_town": "A landmark followed by its town.",
    "chain_near": "A chain store's name; the branch near the caller is meant.",
    "airport": "An airport by its code or by its city.",
    "abbreviation": "A short form or nickname of a place.",
    "misspelling": "A place name with a common spelling mistake.",
    "other_script": "A place in a non-Latin script.",
    "address_town": "A street, number and the first letters of its town.",
    "labels": "Same-named answers carry labels that tell them apart, in the query's script.",
    "famous_slot": "Near matches leave a row for a famous place with the typed name.",
}

MIN_CASES = 5
PLACE_LAYERS = {
    "country", "macroregion", "region", "macrocounty", "county", "district",
    "subdistrict", "locality", "localadmin", "borough", "sublocality",
    "neighbourhood", "place", "island", "dependency", "macrohood",
}


def fold(text):
    text = unicodedata.normalize("NFKD", (text or "").casefold().replace("ß", "ss"))
    return "".join(c for c in text if not unicodedata.combining(c))


def km(a, b):
    lon1, lat1, lon2, lat2 = map(math.radians, (*a, *b))
    h = math.sin((lat2 - lat1) / 2) ** 2 + math.cos(lat1) * math.cos(lat2) * math.sin((lon2 - lon1) / 2) ** 2
    return 12742 * math.asin(min(1.0, math.sqrt(h)))


def parse_point(text):
    lon, lat = (float(part) for part in text.split(","))
    return lon, lat


def load(section=None):
    cases = []
    for path in sorted(CASES.glob("*.tsv")):
        if section and path.stem != section:
            continue
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            if not line.strip() or line.startswith("#"):
                continue
            fields = line.split("\t")
            if len(fields) < 4:
                sys.exit(f"{path.name}:{number}: want query, focus, top, expect")
            query, focus, top, expect = fields[:4]
            cases.append({
                "section": path.stem,
                "where": f"{path.name}:{number}",
                "query": query,
                "focus": parse_point(focus) if focus.strip() else None,
                "top": int(top),
                "expect": [[part.strip() for part in group.split("|")] for group in expect.split("&")],
                "note": fields[4] if len(fields) > 4 else "",
            })
    if not cases:
        wanted = f"section {section!r}" if section else "any section"
        sys.exit(f"no cases for {wanted} in {CASES}")
    return cases


def ask(server, path, params, tries=15):
    url = f"{server.rstrip('/')}{path}?{urllib.parse.urlencode(params)}"
    for attempt in range(tries):
        try:
            with urllib.request.urlopen(url, timeout=60) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            if error.code != 503 or attempt == tries - 1:
                raise
        except (urllib.error.URLError, TimeoutError, ConnectionError):
            if attempt == tries - 1:
                raise
        time.sleep(1 + attempt)


def autocomplete(server, query, focus, limit=10):
    params = {"q": query, "limit": limit}
    if focus:
        params["focus"] = f"{focus[0]},{focus[1]}"
    answers = []
    for feature in ask(server, "/autocomplete", params).get("features", []):
        props = feature.get("properties", {})
        answers.append({
            "id": props.get("id"),
            "name": props.get("name") or "",
            "label": props.get("label") or "",
            "layer": props.get("layer") or "",
            "point": tuple(feature["geometry"]["coordinates"][:2]) if feature.get("geometry") else None,
        })
    return answers


def meets(answer, expect, focus):
    kind, _, rest = expect.partition("@")
    where, _, name = rest.partition(":")
    spot, _, radius = where.partition("~")
    layer = answer["layer"]
    if kind == "place" and layer not in PLACE_LAYERS:
        return False
    if kind not in ("place", "any") and layer != kind:
        return False
    if name and fold(name) not in fold(answer["name"]) and fold(name) not in fold(answer["label"]):
        return False
    if spot == "near":
        if not focus:
            return False
        centre = focus
    else:
        lat, lon = (float(part) for part in spot.split(","))
        centre = (lon, lat)
    return answer["point"] is not None and km(answer["point"], centre) <= float(radius)


def scripts(text):
    return {unicodedata.name(c, "?").split()[0] for c in text if c.isalpha()}


def holds(expect, first, case):
    if expect == "distinct":
        labels = [answer["label"] for answer in first]
        return bool(first) and len(set(labels)) == len(labels)
    if expect == "script":
        allowed = scripts(case["query"])
        return bool(first) and all(scripts(answer["label"]) <= allowed for answer in first)
    if expect.startswith("!"):
        return not any(meets(answer, expect[1:], case["focus"]) for answer in first)
    return any(meets(answer, expect, case["focus"]) for answer in first)


def passes(case, answers):
    first = answers[: case["top"]]
    return all(any(holds(expect, first, case) for expect in group) for group in case["expect"])


def run(server, cases):
    def one(case):
        answers = autocomplete(server, case["query"], case["focus"], max(case["top"], 5))
        return case["where"], answers, passes(case, answers)

    with concurrent.futures.ThreadPoolExecutor(2) as pool:
        return {where: (answers, ok) for where, answers, ok in pool.map(one, cases)}


def show(answers, count=3):
    return " | ".join(f"{a['label']} [{a['layer']}]" for a in answers[:count]) or "(none)"


def focus_text(case):
    return f" @{case['focus'][0]},{case['focus'][1]}" if case["focus"] else ""


def table(rows, heads):
    widths = [max(len(str(row[i])) for row in [heads, *rows]) for i in range(len(heads))]
    for row in [heads, *rows]:
        print("  ".join(str(cell).ljust(width) for cell, width in zip(row, widths)))


def coverage(cases):
    counts = {name: 0 for name in SECTIONS}
    for case in cases:
        counts[case["section"]] = counts.get(case["section"], 0) + 1
    rows, missing = [], []
    for name, count in counts.items():
        about = SECTIONS.get(name, "(not a known section)")
        ok = name in SECTIONS and count >= MIN_CASES
        if not ok:
            missing.append(name)
        rows.append((name, count, "ok" if ok else "MISSING", about))
    return rows, missing


def cmd_sections(args):
    rows, missing = coverage(load())
    table(rows, ("section", "cases", "", "what it tests"))
    if missing:
        print(f"\nsections short of {MIN_CASES} cases: {', '.join(missing)}")
        return 1
    return 0


def cmd_check(args):
    cases = load(args.section)
    results = run(args.server, cases)
    rows = []
    for name in sorted({case["section"] for case in cases}):
        mine = [case for case in cases if case["section"] == name]
        rows.append((name, len(mine), sum(results[case["where"]][1] for case in mine)))
    table(rows, ("section", "cases", "pass"))
    print(f"\ntotal {sum(r[2] for r in rows)}/{sum(r[1] for r in rows)}\n")
    for case in cases:
        answers, ok = results[case["where"]]
        if not ok:
            print(f"FAIL {case['where']} {case['query']!r}{focus_text(case)} top {case['top']} {case['note']}")
            print(f"     got {show(answers)}")
    return 0 if all(results[case["where"]][1] for case in cases) else 1


def cmd_compare(args):
    cases = load(args.section)
    base = run(args.base, cases)
    new = run(args.new, cases)
    rows = []
    for name in sorted({case["section"] for case in cases}):
        mine = [case for case in cases if case["section"] == name]
        rows.append((
            name,
            len(mine),
            sum(base[case["where"]][1] for case in mine),
            sum(new[case["where"]][1] for case in mine),
        ))
    table(rows, ("section", "cases", "base", "new"))
    print(f"\ntotal base {sum(r[2] for r in rows)}  new {sum(r[3] for r in rows)}  of {sum(r[1] for r in rows)}")

    lost = [case for case in cases if base[case["where"]][1] and not new[case["where"]][1]]
    gained = [case for case in cases if new[case["where"]][1] and not base[case["where"]][1]]
    changed = [
        case for case in cases
        if case not in lost and case not in gained
        and [(a["id"], a["label"]) for a in base[case["where"]][0][:1]]
        != [(a["id"], a["label"]) for a in new[case["where"]][0][:1]]
    ]
    for title, group in (("REGRESSIONS", lost), ("FIXED", gained), ("FIRST ANSWER CHANGED", changed)):
        print(f"\n{title} ({len(group)})")
        for case in group:
            print(f"  {case['where']} {case['query']!r}{focus_text(case)} top {case['top']} {case['note']}")
            print(f"    base {show(base[case['where']][0])}")
            print(f"    new  {show(new[case['where']][0])}")
    return 1 if lost else 0


# Areas whose towns the sample draws from: lon_min, lat_min, lon_max, lat_max.
LAND = [
    (-124, 32, -70, 48), (-117, 15, -87, 30), (-80, -35, -35, 5), (-72, -45, -58, -20),
    (-10, 36, 30, 60), (20, 44, 45, 60), (-17, 4, 40, 15), (15, -35, 40, -5),
    (30, 22, 60, 40), (68, 8, 90, 32), (95, 0, 122, 25), (100, 22, 140, 45),
    (115, -38, 153, -15), (166, -46, 178, -35),
]
CITIES = [
    (-74.0, 40.71), (-87.63, 41.88), (-118.24, 34.05), (-79.38, 43.65), (-99.13, 19.43),
    (-46.63, -23.55), (-58.38, -34.6), (-74.07, 4.71), (-0.13, 51.51), (2.35, 48.86),
    (13.4, 52.52), (12.5, 41.9), (-3.7, 40.42), (21.01, 52.23), (28.98, 41.01),
    (37.62, 55.75), (31.24, 30.04), (3.38, 6.52), (36.82, -1.29), (28.05, -26.2),
    (55.27, 25.2), (72.88, 19.08), (77.21, 28.61), (100.5, 13.75), (106.85, -6.21),
    (121.47, 31.23), (139.69, 35.69), (126.98, 37.57), (151.21, -33.87), (174.76, -36.85),
]


def draw_targets(server, seed, count):
    rng = random.Random(seed)
    towns, postcodes, seen = [], [], set()
    tries = 0
    while (len(towns) < count or len(postcodes) < count // 2) and tries < count * 40:
        tries += 1
        if rng.random() < 0.5:
            box = rng.choice(LAND)
            lon, lat = rng.uniform(box[0], box[2]), rng.uniform(box[1], box[3])
        else:
            lon, lat = rng.choice(CITIES)
            lon, lat = lon + rng.uniform(-0.3, 0.3), lat + rng.uniform(-0.3, 0.3)
        features = ask(server, "/reverse", {"lat": f"{lat:.5f}", "lon": f"{lon:.5f}"}).get("features", [])
        areas = {f["properties"]["layer"]: f for f in features}
        context = next((f["properties"].get("context") or {} for f in features), {})
        town = context.get("locality") or (
            {"id": areas["locality"]["properties"]["id"], "name": areas["locality"]["properties"]["name"]}
            if "locality" in areas else None
        )
        if town and town["id"] not in seen and len(towns) < count and len(fold(town["name"])) >= 4:
            seen.add(town["id"])
            towns.append({"kind": "town", "id": town["id"], "name": town["name"], "point": (lon, lat)})
        code = areas.get("postcode")
        if code and code["properties"]["id"] not in seen and len(postcodes) < count // 2:
            seen.add(code["properties"]["id"])
            postcodes.append({
                "kind": "postcode",
                "id": code["properties"]["id"],
                "name": code["properties"]["name"],
                "point": tuple(code["geometry"]["coordinates"][:2]),
            })
    return towns + postcodes


def found(target, answers):
    for answer in answers:
        if answer["id"] == target["id"]:
            return True
        same = fold(answer["label"].split(",")[0]) == fold(target["name"])
        if same and answer["layer"] != "poi" and answer["point"] and km(answer["point"], target["point"]) <= 30:
            return True
    return False


def letters_to(server, target, focus, top):
    name = target["name"]
    start = 2 if target["kind"] == "postcode" else 3
    for length in range(start, len(name) + 1):
        answers = autocomplete(server, name[:length], focus, 5)
        if found(target, answers[:top]):
            return length
    return len(name) + 1


def cmd_sample(args):
    targets = draw_targets(args.new, args.seed, args.count)
    rng = random.Random(args.seed + 1)
    jobs = []
    for target in targets:
        lon, lat = target["point"]
        abroad = max(CITIES, key=lambda city: km(city, target["point"]))
        for where, focus in (
            ("local", (lon + rng.uniform(-0.04, 0.04), lat + rng.uniform(-0.04, 0.04))),
            ("abroad", abroad),
            ("none", None),
        ):
            for top in (1, 5):
                jobs.append((target, where, focus, top))

    def one(job):
        target, where, focus, top = job
        return job, letters_to(args.base, target, focus, top), letters_to(args.new, target, focus, top)

    with concurrent.futures.ThreadPoolExecutor(2) as pool:
        results = list(pool.map(one, jobs))

    groups = {}
    for (target, where, _, top), base, new in results:
        groups.setdefault((target["kind"], where, top), []).append((target, base, new))
    rows, worse = [], []
    for (kind, where, top), entries in sorted(groups.items()):
        base_mean = sum(e[1] for e in entries) / len(entries)
        new_mean = sum(e[2] for e in entries) / len(entries)
        base_wins = sum(e[1] < e[2] for e in entries)
        new_wins = sum(e[2] < e[1] for e in entries)
        rows.append((kind, where, f"top {top}", len(entries), f"{base_mean:.2f}", f"{new_mean:.2f}", base_wins, new_wins))
        if new_wins < base_wins:
            worse.append(f"{kind} {where} top {top}")
    print(f"{len(targets)} targets drawn with seed {args.seed}; letters typed until the target shows\n")
    table(rows, ("kind", "focus", "rank", "n", "base", "new", "base sooner", "new sooner"))
    lost = sorted(
        ((new - base, target, where, top) for (target, where, _, top), base, new in results if new > base),
        key=lambda row: -row[0],
    )[: args.worst]
    if lost:
        print(f"\nWORST FOR NEW ({len(lost)})")
        for delta, target, where, top in lost:
            print(f"  {target['name']!r} {target['id']} {where} top {top}: {delta} letters later")
    if worse:
        print(f"\ngroups where base wins more often: {', '.join(worse)}")
        return 1
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("sections")
    check = commands.add_parser("check")
    check.add_argument("--server", required=True)
    check.add_argument("--section")
    compare = commands.add_parser("compare")
    compare.add_argument("--base", required=True)
    compare.add_argument("--new", required=True)
    compare.add_argument("--section")
    sample = commands.add_parser("sample")
    sample.add_argument("--base", required=True)
    sample.add_argument("--new", required=True)
    sample.add_argument("--seed", type=int, default=1)
    sample.add_argument("--count", type=int, default=60)
    sample.add_argument("--worst", type=int, default=20)
    args = parser.parse_args()
    return {"sections": cmd_sections, "check": cmd_check, "compare": cmd_compare, "sample": cmd_sample}[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
