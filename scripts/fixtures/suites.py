"""Write a `geocode bench` answer key from another geocoder's own test suite.

    pip install -r scripts/fixtures/requirements.txt
    npm ci --prefix scripts/fixtures            # Node 22 or later
    python scripts/fixtures/suites.py pelias --suite <acceptance-tests checkout> --cache <dir>
    python scripts/fixtures/suites.py photon --suite <geocoder-tester checkout> --cache <dir>

Every search case that names one house keeps its query word for word. The
suite says which address is right by its fields; that country's register,
read through the same adapter its own answer key uses, says where it is. A
case is kept only when the register holds exactly one place for it. Cases that
need a caller position (location bias) or a search filter are dropped, so
every case is a plain query as in the register keys.
"""

import argparse
import csv
import importlib
import importlib.util
import io
import json
import math
import re
import subprocess
import sys
import unicodedata
from collections import Counter, defaultdict
from pathlib import Path

import duckdb
import yaml

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
TEMPLATES = HERE / "node_modules" / "@fragaria" / "address-formatter" / "src" / "templates"
# Register points of one address further apart than this are two places.
SAME_PLACE_M = 50.0
ISO3 = {
    "AUS": "AU", "AUT": "AT", "BEL": "BE", "BRA": "BR", "CAN": "CA", "CHE": "CH", "CZE": "CZ",
    "DEU": "DE", "DNK": "DK", "ESP": "ES", "EST": "EE", "FIN": "FI", "FRA": "FR", "GBR": "GB",
    "ISL": "IS", "ITA": "IT", "JPN": "JP", "KOR": "KR", "LTU": "LT", "LUX": "LU", "LVA": "LV",
    "NLD": "NL", "NOR": "NO", "NZL": "NZ", "POL": "PL", "SVN": "SI", "USA": "US",
}
PHOTON_COUNTRIES = {"france": "FR", "germany": "DE", "italy": "IT", "netherlands": "NL", "poland": "PL", "usa": "US"}
NUMBER = re.compile(r"^(\d+)\s*([a-z]*)")
LEADING_NUMBER = re.compile(r"^\s*(\d+)\s*(bis|ter|quater|[a-z])?\b", re.IGNORECASE)
NAMED_HOUSE = re.compile(r"^(\d+[a-zA-Z]?)\s+(.+)$")
UPSTREAM = {
    "pelias": {
        "suite": "pelias/acceptance-tests",
        "url": "https://github.com/pelias/acceptance-tests",
        "licence": "MIT (package.json); Copyright (c) Pelias contributors",
    },
    "photon": {
        "suite": "geocoders/geocoder-tester, the suite photon's README names",
        "url": "https://github.com/geocoders/geocoder-tester",
        "licence": "MIT; Copyright (c) geocoder-tester contributors",
    },
}


def driver():
    # registers.py shares its name with the adapters' package, so it is
    # loaded by path for the draw rules and writer both keys use.
    spec = importlib.util.spec_from_file_location("driver", HERE / "registers.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def fold(text):
    text = unicodedata.normalize("NFKD", str(text or ""))
    text = "".join(ch for ch in text if not unicodedata.combining(ch)).lower()
    return " ".join(re.sub(r"[^\w]+", " ", text).split())


def abbreviations():
    """Every road abbreviation OpenCage lists, folded: suites and registers
    disagree on `Street` and `St`, and neither is wrong."""
    words = {}
    for lists in json.loads((TEMPLATES / "abbreviations.json").read_text(encoding="utf-8")).values():
        for entry in lists:
            if entry["component"] == "road":
                for pair in entry["replacements"]:
                    words[fold(pair["src"])] = fold(pair["dest"])
    return words


ABBREVIATED = abbreviations()


def street_key(text):
    # Registers and suites disagree on ordinals: NAD writes "West 72 Street"
    # where Pelias writes "West 72nd Street".
    words = re.sub(r"\b(\d+)(?:st|nd|rd|th|e|er|eme)\b", r"\1", fold(text)).split()
    return " ".join(ABBREVIATED.get(word, word) for word in words)


def same_place(expected, register):
    """One name opening the other, word for word: the suite's "Paris" is
    BAN's "Paris 10e Arrondissement", but "York" is not "New York"."""
    for a in expected:
        for b in register:
            words_a, words_b = a.split(), b.split()
            shorter = min(len(words_a), len(words_b))
            if words_a[:shorter] == words_b[:shorter]:
                return True
    return False


def number_key(text):
    match = NUMBER.match(fold(text).replace(" ", ""))
    if not match:
        return None
    suffix = {"b": "bis", "t": "ter", "q": "quater"}.get(match.group(2), match.group(2))
    return match.group(1).lstrip("0") or "0", suffix


def metres(a, b):
    lat = math.radians((a[1] + b[1]) / 2)
    return math.hypot((a[0] - b[0]) * 111_320 * math.cos(lat), (a[1] - b[1]) * 110_540)


def case(ref, query, country, number, street, localities=(), postcode="", regions=(), needs="", cohort=""):
    """`needs` names what the query cannot be run without, if anything."""
    return {
        "ref": ref,
        "q": query,
        "country": country,
        "number": str(number),
        "street": street,
        "localities": {fold(name) for name in localities if name},
        "postcode": fold(postcode).replace(" ", ""),
        "regions": {fold(name) for name in regions if name},
        "needs": needs,
        "cohort": cohort,
    }


def pelias(checkout):
    """Search cases whose expected first result is one house. The house is
    its housenumber and street fields, or an address name that opens with a
    number."""
    cases, dropped = [], Counter()
    for path in sorted((checkout / "test_cases").glob("*.json")):
        suite = json.loads(path.read_text(encoding="utf-8"))
        for test in suite.get("tests", []):
            if test.get("endpoint", suite.get("endpoint", "search")) != "search":
                dropped["not a search"] += 1
                continue
            given = test.get("in") or {}
            properties = (test.get("expected") or {}).get("properties") or []
            if isinstance(properties, dict):
                properties = [properties]
            first = properties[0] if properties and isinstance(properties[0], dict) else {}
            number, street = first.get("housenumber"), first.get("street")
            if not (number and street):
                named = NAMED_HOUSE.match(first.get("name") or "")
                if named and first.get("layer") == "address":
                    number, street = named.groups()
            if not (given.get("text") and number and street):
                dropped["no house expected"] += 1
                continue
            cases.append(
                case(
                    f"{path.name}#{test.get('id')}",
                    given["text"],
                    ISO3.get(first.get("country_a") or ""),
                    number,
                    street,
                    [first.get(key) for key in ("locality", "localadmin", "borough", "neighbourhood")],
                    str(first.get("postalcode") or ""),
                    [first.get("region"), first.get("region_a")],
                    "a caller position" if "focus.point.lat" in given else "",
                    path.stem,
                )
            )
    return cases, dropped


def photon(checkout):
    """CSV and YAML cases expecting a house: the expected housenumber, or,
    when the expectation stops at the street, the number the query opens
    with. A YAML case may name the house as one `name` ('1 Chemin des
    Falaises')."""
    world = checkout / "geocoder_tester" / "world"
    cases, dropped = [], Counter()
    for path in sorted(world.rglob("*.yml")):
        relative = path.relative_to(world).as_posix()
        for name, spec in yaml.safe_load(path.read_text(encoding="utf-8")).items():
            expected = spec.get("expected") or {}
            query = str(spec.get("query", name)).strip()
            number, street = str(expected.get("housenumber") or "").strip(), str(expected.get("street") or "").strip()
            if not (number and street):
                named = NAMED_HOUSE.match(str(expected.get("name") or ""))
                if named:
                    number, street = named.groups()
            if not (query and street and number):
                dropped["no house expected"] += 1
                continue
            needs = ""
            if spec.get("lat") is not None and spec.get("lon") is not None:
                needs = "a caller position"
            elif "citycode" in spec:
                needs = "a search filter"
            cases.append(
                case(
                    f"{relative}:{name}",
                    query,
                    PHOTON_COUNTRIES.get(relative.split("/", 1)[0]),
                    number,
                    street,
                    [str(expected.get("city") or "")],
                    str(expected.get("postcode") or ""),
                    [str(expected.get("state") or "")],
                    needs,
                    Path(relative).stem.removeprefix("test_"),
                )
            )
    for path in sorted(world.rglob("*.csv")):
        relative = path.relative_to(world).as_posix()
        text = path.read_text(encoding="utf-8")
        header = text.split("\n", 1)[0]
        delimiter = ";" if header.count(";") > header.count(",") else ","
        for line, row in enumerate(csv.DictReader(io.StringIO(text), delimiter=delimiter), 2):
            query = (row.get("query") or row.get("tried_query") or "").strip()
            street = (row.get("expected_street") or "").strip()
            number = (row.get("expected_housenumber") or "").strip()
            if not number and street:
                opening = LEADING_NUMBER.match(query)
                if opening:
                    number = "".join(part for part in opening.groups() if part)
            if not (query and street and number):
                dropped["no house expected"] += 1
                continue
            cases.append(
                case(
                    f"{relative}:{line}",
                    query,
                    PHOTON_COUNTRIES.get(relative.split("/", 1)[0]),
                    number,
                    street,
                    [row.get("expected_city")],
                    row.get("expected_postcode") or "",
                    [row.get("expected_state")],
                    "a caller position" if (row.get("lat") or "").strip() and (row.get("lon") or "").strip() else "",
                    Path(relative).stem.removeprefix("test_"),
                )
            )
    return cases, dropped


def adapters():
    found = {}
    for path in sorted((HERE / "registers").glob("*.py")):
        if path.stem != "__init__":
            module = importlib.import_module(f"registers.{path.stem}")
            found[module.SOURCE["country"]] = (path.stem, module)
    return found


def resolve(cases, cache, rules):
    """Each case's register points. A register is read once, narrowed in SQL
    to rows with a wanted leading house number, then matched by street,
    number and postcode or locality."""
    from registers import LOCALITY, standard

    found = adapters()
    used = {}
    points = defaultdict(list)
    wanted = defaultdict(lambda: defaultdict(list))
    for index, item in enumerate(cases):
        key = number_key(item["number"])
        if item["country"] in found and key:
            wanted[item["country"]][(street_key(item["street"]), key)].append(index)
    for country, keys in wanted.items():
        name, module = found[country]
        directory = cache / name
        directory.mkdir(parents=True, exist_ok=True)
        files, release = module.download(directory)
        used[name] = {"register": module.SOURCE["register"], "release": release}
        numbers = sorted({key[1][0] for key in keys})
        rows = duckdb.sql(f"""
            select * from {standard(module, files)}
            where {rules} is null and regexp_extract(house_number, '^[0-9]+') in {tuple(numbers) + ('',)}
        """)
        names = rows.columns
        for values in rows.fetchall():
            row = dict(zip(names, values))
            indexes = keys.get((street_key(row["road"]), number_key(row["house_number"])))
            if not indexes:
                continue
            places = {fold(row[key]) for key in LOCALITY} - {""}
            postcode = fold(row["postcode"]).replace(" ", "")
            for index in indexes:
                item = cases[index]
                if item["postcode"] and item["postcode"] != postcode:
                    continue
                if not item["postcode"] and item["localities"] and not same_place(item["localities"], places):
                    continue
                region = {fold(row["state"]), fold(row["state_code"])} - {""}
                if item["regions"] and region and not region & item["regions"]:
                    continue
                points[index].append(row)
    return points, found, used


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("suite", choices=tuple(UPSTREAM))
    parser.add_argument("--suite", dest="checkout", required=True, type=Path)
    parser.add_argument("--cache", required=True, type=Path)
    args = parser.parse_args()

    rules = driver()
    cases, dropped = (pelias if args.suite == "pelias" else photon)(args.checkout)
    points, found, used = resolve(cases, args.cache, rules.REASON)
    rules.to_wgs84([row for rows in points.values() for row in rows])
    out = []
    for index, item in enumerate(cases):
        rows = points.get(index, [])
        places = [(row["lon"], row["lat"]) for row in rows]
        if item["needs"]:
            dropped[f"needs {item['needs']}"] += 1
        elif item["country"] is None:
            dropped["no country stated"] += 1
        elif item["country"] not in found:
            dropped[f"no register for {item['country']}"] += 1
        elif number_key(item["number"]) is None:
            dropped["unreadable house number"] += 1
        elif not places:
            dropped["not in register"] += 1
        elif any(metres(places[0], other) > SAME_PLACE_M for other in places[1:]):
            dropped["several places in register"] += 1
        else:
            out.append(
                {
                    "name": item["ref"],
                    "q": item["q"],
                    "form": "verbatim",
                    "cohort": item["cohort"],
                    "expect_lat": round(places[0][1], 7),
                    "expect_lon": round(places[0][0], 7),
                }
            )
    # A query the suite asks twice is one case, and none when its tests
    # expect different places: the suite then disagrees with itself.
    by_query = defaultdict(list)
    for entry in out:
        by_query[entry["q"]].append(entry)
    out = []
    for entries in by_query.values():
        places = [(entry["expect_lon"], entry["expect_lat"]) for entry in entries]
        if any(metres(places[0], other) > SAME_PLACE_M for other in places[1:]):
            dropped["suite expects several places"] += len(entries)
        else:
            out.append(entries[0])
            if len(entries) > 1:
                dropped["repeated query"] += len(entries) - 1
    commit = subprocess.run(
        ["git", "-C", str(args.checkout), "rev-parse", "HEAD"], capture_output=True, text=True, check=True
    ).stdout.strip()
    meta = {
        **UPSTREAM[args.suite],
        "commit": commit,
        "queries": "verbatim from the suite",
        "truth": "the point of the suite's expected address in that country's register, read as its register key reads it",
        "registers": used,
        "same_place_m": SAME_PLACE_M,
        "cases_read": len(cases) + dropped["not a search"] + dropped["no house expected"],
        "dropped": dict(sorted(dropped.items())),
        "rebuild": f"python scripts/fixtures/suites.py {args.suite} --suite <checkout> --cache <dir>",
    }
    path = REPO / "fixtures" / "queries" / f"{args.suite}.json"
    rules.write(path, meta, out)
    print(f"{path.relative_to(REPO)}: {len(out)} cases, dropped {dict(dropped)}", file=sys.stderr)


if __name__ == "__main__":
    sys.path.insert(0, str(HERE))
    main()
