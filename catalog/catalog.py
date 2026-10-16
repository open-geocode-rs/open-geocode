#!/usr/bin/env python3
"""Build catalog/regions.json, the region catalog the og CLI embeds.

Each upstream is pinned the way a submodule pins a commit: `update` fetches
it, writes a trimmed snapshot to catalog/upstream/ and records the snapshot's
sha256 in catalog/sources.lock.json. `generate` is offline: it reads only the
snapshots and catalog/curation.toml, so two runs give byte-identical output.
`check` regenerates in memory and fails if anything drifted.

    python -I catalog/catalog.py update geofabrik|openaddresses|overture|all
    python -I catalog/catalog.py generate
    python -I catalog/catalog.py check

`generate` and `check` need only the standard library. `update overture`
needs the packages in catalog/requirements.txt.
"""
import argparse, datetime, email.utils, hashlib, json, os, sys, time, tomllib
import urllib.error, urllib.parse, urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CATALOG = os.path.join(ROOT, "catalog")
UPSTREAM = os.path.join(CATALOG, "upstream")
LOCK = os.path.join(CATALOG, "sources.lock.json")
CURATION = os.path.join(CATALOG, "curation.toml")
REGIONS = os.path.join(CATALOG, "regions.json")

GEOFABRIK_INDEX = "https://download.geofabrik.de/index-v1-nogeom.json"
GEOFABRIK_SHAPES = "https://download.geofabrik.de/index-v1.json"
GEOFABRIK_PREFIX = "https://download.geofabrik.de/"
PBF_SUFFIX = "-latest.osm.pbf"
PLANET = "https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf"
OA_DATA = "https://batch.openaddresses.io/api/data"
OA_JOB = "https://batch.openaddresses.io/api/job/{}"
OVERTURE_BUCKET = "overturemaps-us-west-2"
OVERTURE_LIST = "https://overturemaps-us-west-2.s3.amazonaws.com/?list-type=2&prefix={}&delimiter=/"
OVERTURE_THEMES = (("places", "places", "place"), ("addresses", "addresses", "address"))

SNAPSHOTS = {name: f"upstream/{name}.json" for name in ("geofabrik", "openaddresses", "overture")}
KINDS = ("world", "continent", "country", "subregion", "group")
USER_AGENT = "og-catalog (https://github.com/nexa-ca/open-geocode)"
SIZE_CHANGE = 0.20


def log(*parts):
    print(*parts, file=sys.stderr, flush=True)


# --- HTTP ---------------------------------------------------------------

def request(url, method="GET", tries=5):
    """One HTTP request with retries on transient failures."""
    url = urllib.parse.quote(url, safe=":/?&=%#~+,;@!$'()*")  # Some source paths are not ASCII.
    for attempt in range(tries):
        try:
            req = urllib.request.Request(url, method=method, headers={"User-Agent": USER_AGENT})
            with urllib.request.urlopen(req, timeout=120) as resp:
                return resp, resp.read()
        except urllib.error.HTTPError as e:
            if e.code not in (429, 500, 502, 503, 504) or attempt == tries - 1:
                raise
        except (urllib.error.URLError, TimeoutError, ConnectionError):
            if attempt == tries - 1:
                raise
        time.sleep(2 ** attempt)


def get_json(url):
    return json.loads(request(url)[1])


def head(url):
    """Size and modification time of a download, following redirects."""
    resp, _ = request(url, method="HEAD")
    modified = email.utils.parsedate_to_datetime(resp.headers["Last-Modified"])
    return dict(url=resp.url, bytes=int(resp.headers["Content-Length"]),
                updated=modified.astimezone(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"))


def now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


# --- Files --------------------------------------------------------------

def line(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def snapshot_text(meta, key, items):
    """A snapshot: metadata first, then one item per line so diffs stay readable."""
    head_lines = [f"{json.dumps(k)}:{line(v)}" for k, v in meta.items()]
    if isinstance(items, dict):
        body = [f"{json.dumps(k, ensure_ascii=False)}:{line(v)}" for k, v in items.items()]
        open_, close = "{", "}"
    else:
        body = [line(v) for v in items]
        open_, close = "[", "]"
    return ("{\n" + "".join(h + ",\n" for h in head_lines)
            + f"{json.dumps(key)}:{open_}\n" + ",\n".join(body) + f"\n{close}}}\n")


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)


def read_bytes(path):
    with open(path, "rb") as f:
        return f.read()


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def load_lock():
    if not os.path.exists(LOCK):
        return {}
    return json.loads(read_bytes(LOCK))


def save_lock(lock):
    write(LOCK, json.dumps(dict(sorted(lock.items())), indent=2) + "\n")


def pin(lock, name, text, **fields):
    """Write a snapshot and record its hash in the lock."""
    write(os.path.join(CATALOG, SNAPSHOTS[name]), text)
    lock[name] = dict(fields, snapshot=SNAPSHOTS[name], sha256=sha256(text.encode("utf-8")))


def load_snapshot(name):
    path = os.path.join(CATALOG, SNAPSHOTS[name])
    return json.loads(read_bytes(path)) if os.path.exists(path) else None


# --- Update: Geofabrik --------------------------------------------------

def update_geofabrik(lock):
    raw = request(GEOFABRIK_INDEX)[1]
    features = [f["properties"] for f in json.loads(raw)["features"]]
    log(f"geofabrik: {len(features)} regions in the index, reading sizes")
    urls = [f["urls"]["pbf"] for f in features] + [PLANET]
    sizes = {}
    # Geofabrik asks for polite use: a few requests at a time.
    with ThreadPoolExecutor(4) as pool:
        futures = {pool.submit(head, url): url for url in urls}
        for done, future in enumerate(as_completed(futures), 1):
            sizes[futures[future]] = future.result()
            if done % 100 == 0:
                log(f"  {done}/{len(urls)}")
    regions = []
    for f in sorted(features, key=lambda f: f["id"]):
        pbf = f["urls"]["pbf"]
        entry = dict(id=f["id"], parent=f.get("parent"), name=f["name"], pbf=pbf)
        if f.get("iso3166-1:alpha2"):
            entry["iso3166_1"] = f["iso3166-1:alpha2"]
        if f.get("iso3166-2"):
            entry["iso3166_2"] = f["iso3166-2"]
        entry.update(bytes=sizes[pbf]["bytes"], updated=sizes[pbf]["updated"])
        regions.append(entry)
    planet = sizes[PLANET]
    meta = dict(source=GEOFABRIK_INDEX,
                planet=dict(url=PLANET, resolved=planet["url"], bytes=planet["bytes"], updated=planet["updated"]))
    text = snapshot_text(meta, "regions", regions)
    pin(lock, "geofabrik", text, index=GEOFABRIK_INDEX, index_sha256=sha256(raw), planet=PLANET, fetched=now())


# --- Update: OpenAddresses ----------------------------------------------

def oa_license(data, layer_name):
    """The license block for one address layer of a source file, either schema."""
    for layer in (data.get("layers") or {}).get("addresses") or []:
        if layer.get("name") == layer_name and isinstance(layer.get("license"), (dict, str)):
            return layer["license"]
    return data.get("license")


def trim_license(lic):
    if not lic:
        return None
    if isinstance(lic, str):
        return dict(name=lic)
    out = {}
    for src, dst in (("text", "name"), ("url", "url"), ("attribution", "attribution"), ("share-alike", "share_alike")):
        if lic.get(src) not in (None, ""):
            out[dst] = lic[src]
    return out or None


def bbox(bounds):
    if not bounds:
        return None
    points = [p for ring in bounds["coordinates"] for p in ring]
    return [min(p[0] for p in points), min(p[1] for p in points),
            max(p[0] for p in points), max(p[1] for p in points)]


def update_openaddresses(lock):
    datasets = [d for d in get_json(OA_DATA) if d["layer"] == "addresses"]
    log(f"openaddresses: {len(datasets)} address data sets, reading jobs")
    jobs = {}
    with ThreadPoolExecutor(8) as pool:
        futures = {pool.submit(get_json, OA_JOB.format(d["job"])): d["job"] for d in datasets}
        for done, future in enumerate(as_completed(futures), 1):
            jobs[futures[future]] = future.result()
            if done % 500 == 0:
                log(f"  {done}/{len(datasets)}")
    # Older jobs carry no license; it is in the source file at the job's commit.
    need = sorted({jobs[d["job"]]["source"] for d in datasets
                   if jobs[d["job"]]["status"] == "Success" and not jobs[d["job"]].get("license")})
    log(f"  reading {len(need)} source files for licenses")
    def source_file(url):
        try:
            return get_json(url)
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return {}  # The commit or branch is gone; the license stays unknown.
            raise

    sources = {}
    with ThreadPoolExecutor(8) as pool:
        for url, data in zip(need, pool.map(source_file, need)):
            sources[url] = data
    rows, skipped = [], 0
    for d in datasets:
        job = jobs[d["job"]]
        if job["status"] != "Success":
            skipped += 1
            continue
        url = job["source"]
        lic = job.get("license") or oa_license(sources[url], d["name"])
        commit = urllib.parse.urlparse(url).path.split("/")[3]
        row = dict(source=d["source"], name=d["name"], job=d["job"], rows=job["count"],
                   bytes=job.get("size") if job.get("size") is not None else d.get("size"),
                   bbox=bbox(job.get("bounds")),
                   license=trim_license(lic), commit=commit)
        rows.append({k: v for k, v in row.items() if v is not None})
    rows.sort(key=lambda r: (r["source"], r["name"], r["job"]))
    unlicensed = sum(1 for r in rows if "license" not in r)
    log(f"  kept {len(rows)}, skipped {skipped} without a successful job, {unlicensed} with no license found")
    text = snapshot_text(dict(source=OA_DATA), "datasets", rows)
    pin(lock, "openaddresses", text, api=OA_DATA, fetched=now())


# --- Update: Overture ---------------------------------------------------

GRID = 4  # cells per degree for the point index


def overture_files(release, theme, kind):
    """Keys and sizes of one theme's parquet files, from the public bucket listing."""
    import xml.etree.ElementTree as ET
    ns = "{http://s3.amazonaws.com/doc/2006-03-01/}"
    prefix = f"release/{release}/theme={theme}/type={kind}/"
    keys, token = [], None
    while True:
        url = OVERTURE_LIST.format(prefix)
        if token:
            url += "&continuation-token=" + urllib.parse.quote(token)
        root = ET.fromstring(request(url)[1])
        keys += [c.find(ns + "Key").text for c in root.iter(ns + "Contents")
                 if c.find(ns + "Key").text.endswith(".parquet")]
        next_token = root.find(ns + "NextContinuationToken")
        if next_token is None:
            return keys
        token = next_token.text


def overture_releases():
    import xml.etree.ElementTree as ET
    ns = "{http://s3.amazonaws.com/doc/2006-03-01/}"
    root = ET.fromstring(request(OVERTURE_LIST.format("release/"))[1])
    return sorted(p.find(ns + "Prefix").text.split("/")[1] for p in root.iter(ns + "CommonPrefixes"))


class Shapes:
    """Region polygons with a grid of cells each one fully covers or only touches.

    A point in a covered cell is inside without a test; only points in
    touched cells are tested against the polygon, so counts stay exact.
    """

    def __init__(self, features):
        import numpy as np, shapely
        self.np, self.shapely = np, shapely
        self.nx, self.ny = 360 * GRID, 180 * GRID
        self.ids, self.geoms, self.inside, self.edge = [], [], [], []
        with ThreadPoolExecutor(os.cpu_count()) as pool:
            for rid, geom, inside, edge in pool.map(self._prepare, features):
                self.ids.append(rid)
                self.geoms.append(geom)
                self.inside.append(inside)
                self.edge.append(edge)

    def _prepare(self, feature):
        np, shapely = self.np, self.shapely
        geom = shapely.geometry.shape(feature["geometry"])
        shapely.prepare(geom)
        minx, miny, maxx, maxy = geom.bounds
        xs = np.arange(self._ix(minx), self._ix(maxx) + 1)
        ys = np.arange(self._iy(miny), self._iy(maxy) + 1)
        gx, gy = (a.ravel() for a in np.meshgrid(xs, ys))
        boxes = shapely.box(gx / GRID - 180, gy / GRID - 90, (gx + 1) / GRID - 180, (gy + 1) / GRID - 90)
        inside = shapely.contains_properly(geom, boxes)
        edge = shapely.intersects(geom, boxes) & ~inside
        cells = gx * self.ny + gy
        return feature["properties"]["id"], geom, np.sort(cells[inside]), np.sort(cells[edge])

    def _ix(self, x):
        return min(max(int((x + 180) * GRID), 0), self.nx - 1)

    def _iy(self, y):
        return min(max(int((y + 90) * GRID), 0), self.ny - 1)

    def count(self, x, y):
        """Points inside each region, in the order of self.ids."""
        np, shapely = self.np, self.shapely
        if len(x) and (x.min() < -180 or x.max() > 180 or y.min() < -90 or y.max() > 90):
            raise SystemExit("overture: a point lies outside -180..180, -90..90")
        ix = np.clip(((x + 180) * GRID).astype(np.int64), 0, self.nx - 1)
        iy = np.clip(((y + 90) * GRID).astype(np.int64), 0, self.ny - 1)
        cell = ix * self.ny + iy
        order = np.argsort(cell, kind="stable")
        per_cell = np.bincount(cell, minlength=self.nx * self.ny)
        starts = np.concatenate(([0], np.cumsum(per_cell)[:-1]))
        out = []
        for geom, inside, edge in zip(self.geoms, self.inside, self.edge):
            n = int(per_cell[inside].sum())
            lens = per_cell[edge]
            total = int(lens.sum())
            if total:
                # Indices of every point in the touched cells, without a Python loop.
                offsets = np.repeat(starts[edge] - np.concatenate(([0], np.cumsum(lens)[:-1])), lens)
                idx = order[offsets + np.arange(total)]
                n += int(shapely.contains_xy(geom, x[idx], y[idx]).sum())
            out.append(n)
        return out

    def brute(self, i, x, y):
        return int(self.shapely.contains_xy(self.geoms[i], x, y).sum())


def update_overture(lock):
    import duckdb
    releases = overture_releases()
    release = releases[-1]
    log(f"overture: releases {', '.join(releases)}; pinning {release}")
    raw = request(GEOFABRIK_SHAPES)[1]
    shapes = Shapes(json.loads(raw)["features"])
    log(f"  {len(shapes.ids)} region polygons indexed")
    counts = {theme: [0] * len(shapes.ids) for theme, _, _ in OVERTURE_THEMES}
    totals = {theme: 0 for theme, _, _ in OVERTURE_THEMES}
    work = [(theme, key) for theme, t, k in OVERTURE_THEMES for key in overture_files(release, t, k)]

    def fetch(item):
        con = duckdb.connect()
        con.execute("LOAD httpfs; LOAD spatial; SET s3_region='us-west-2'; SET threads=4;")
        url = f"s3://{OVERTURE_BUCKET}/{item[1]}"
        cols = con.execute(f"SELECT ST_X(geometry) AS x, ST_Y(geometry) AS y FROM read_parquet('{url}')").fetchnumpy()
        con.close()
        return item, cols["x"], cols["y"]

    con = duckdb.connect()
    con.execute("INSTALL httpfs; INSTALL spatial;")
    con.close()
    started, checked = time.time(), False
    with ThreadPoolExecutor(3) as pool:
        futures = [pool.submit(fetch, item) for item in work]
        for done, future in enumerate(as_completed(futures), 1):
            (theme, key), x, y = future.result()
            got = shapes.count(x, y)
            if not checked:
                # The grid must agree with testing every point, on a few regions.
                for i in range(0, len(shapes.ids), max(1, len(shapes.ids) // 5)):
                    if got[i] != shapes.brute(i, x, y):
                        raise SystemExit(f"overture: grid count disagrees for {shapes.ids[i]}")
                checked = True
            counts[theme] = [a + b for a, b in zip(counts[theme], got)]
            totals[theme] += len(x)
            log(f"  {done}/{len(work)} {theme} {len(x)} points, {time.time() - started:.0f}s")
    by_id = {rid: [counts["places"][i], counts["addresses"][i]] for i, rid in enumerate(shapes.ids)}
    meta = dict(release=release, boundaries=GEOFABRIK_SHAPES, boundaries_sha256=sha256(raw),
                columns=["places", "addresses"], planet=[totals["places"], totals["addresses"]])
    text = snapshot_text(meta, "counts", dict(sorted(by_id.items())))
    pin(lock, "overture", text, release=release, bucket=f"s3://{OVERTURE_BUCKET}/release/{release}/",
        boundaries=GEOFABRIK_SHAPES, boundaries_sha256=sha256(raw), fetched=now())


# --- Generate -----------------------------------------------------------

class Curation:
    """curation.toml: tables of decisions, each with a `why`."""

    SECTIONS = ("names", "accept_names", "iso", "covers", "kind", "parent")

    def __init__(self, path):
        with open(path, "rb") as f:
            data = tomllib.load(f)
        self.problems = []
        self.continents = self._ids(data, "continents")
        planet = data.get("planet", {})
        self._need_why("planet", planet)
        self.planet_name, self.planet_aliases = planet.get("name", "World"), planet.get("aliases", [])
        self.tables = {s: {} for s in self.SECTIONS}
        for section in self.SECTIONS:
            for group, table in data.get(section, {}).items():
                self._need_why(f"{section}.{group}", table)
                entries = table.get("ids", []) if section == "accept_names" else table.get("set", {})
                for rid in entries:
                    if rid in self.tables[section]:
                        self.problems.append(f"curation: {rid} is decided twice in [{section}]")
                    self.tables[section][rid] = True if section == "accept_names" else entries[rid]

    def _ids(self, data, key):
        table = data.get(key, {})
        self._need_why(key, table)
        return list(table.get("ids", []))

    def _need_why(self, where, table):
        if table and not str(table.get("why", "")).strip():
            self.problems.append(f"curation: [{where}] has no why")

    def ids(self):
        out = set(self.continents)
        for table in self.tables.values():
            out.update(table)
        return out


def ancestry_code(code):
    """The country part of an ISO code: FR for FR-973, FR for FR."""
    return code.split("-")[0]


def oa_code(source):
    """An OpenAddresses source path as an ISO code: ca/on/x is CA-ON, fr/countrywide is FR."""
    parts = source.split("/")
    return parts[0].upper() if len(parts) == 2 else f"{parts[0].upper()}-{parts[1].upper()}"


def within(code, region_codes):
    return any(code == c or ("-" not in c and code.startswith(c + "-")) for c in region_codes)


def above(code, region_codes):
    return any("-" in c and code == ancestry_code(c) for c in region_codes)


def build(geofabrik, openaddresses, overture, curation, lock):
    """The catalog and the list of unresolved findings."""
    unresolved = list(curation.problems)
    upstream = {r["id"]: r for r in geofabrik["regions"]}
    tops = {rid for rid, r in upstream.items() if not r["parent"]}
    continents = [c for c in curation.continents if c in tops]

    def our_id(r):
        path = r["pbf"][len(GEOFABRIK_PREFIX):]
        if not r["pbf"].startswith(GEOFABRIK_PREFIX) or not path.endswith(PBF_SUFFIX):
            raise SystemExit(f"geofabrik: {r['id']} has an unexpected download url {r['pbf']}")
        path = path[:-len(PBF_SUFFIX)]
        first, _, rest = path.partition("/")
        return rest if rest and first in continents else path

    ids = {rid: our_id(r) for rid, r in upstream.items()}
    if len(set(ids.values())) != len(ids):
        raise SystemExit("geofabrik: two download urls map to the same region id")
    by_ours = {ids[rid]: r for rid, r in upstream.items()}
    known = set(by_ours) | {"planet"}
    for rid in sorted(curation.ids() - known):
        unresolved.append(f"{rid}: in curation.toml but not upstream any more")

    names, accept = curation.tables["names"], curation.tables["accept_names"]
    iso_set, covers = curation.tables["iso"], curation.tables["covers"]
    kinds, parents = curation.tables["kind"], curation.tables["parent"]

    paths = {r["pbf"][len(GEOFABRIK_PREFIX):-len(PBF_SUFFIX)]: ids[rid] for rid, r in upstream.items()}
    regions = {}
    for rid, r in upstream.items():
        ours = ids[rid]
        parent = ids[r["parent"]] if r["parent"] else None
        # The download path is the parent of record: the index gives US states
        # north-america as parent although they live under us/.
        prefix = r["pbf"][len(GEOFABRIK_PREFIX):-len(PBF_SUFFIX)].rpartition("/")[0]
        if prefix in paths and paths[prefix] != parent:
            parent = paths[prefix]
        if ours in parents:
            parent = parents[ours]
            if parent not in known:
                unresolved.append(f"{ours}: curated parent {parent} does not exist")
        elif parent is None:
            if rid in continents:
                parent = "planet"
            else:
                unresolved.append(f"{ours}: no parent upstream and no decision")
        regions[ours] = dict(up=r, parent=parent)

    def depth_kind(ours):
        parent = regions[ours]["parent"]
        if parent == "planet":
            return "continent"
        if parent in regions and regions[parent]["parent"] == "planet":
            return "country"
        return "subregion"

    for ours, reg in regions.items():
        r = reg["up"]
        reg["kind"] = kinds.get(ours) or depth_kind(ours)
        if reg["kind"] not in KINDS:
            unresolved.append(f"{ours}: unknown kind {reg['kind']}")
        a2, i2 = r.get("iso3166_1", []), r.get("iso3166_2", [])
        if ours in iso_set:
            value = iso_set[ours]
            codes = [value] if isinstance(value, str) else list(value)
        elif ours in covers:
            codes = []
        elif len(a2) + len(i2) == 1:
            codes = a2 + i2
        elif not a2 and not i2:
            codes = []
        else:
            codes = []
            unresolved.append(f"{ours}: several ISO codes upstream ({', '.join(a2 + i2)}) and no decision")
        reg["iso"] = codes[0] if codes else None
        reg["codes"] = codes + list(covers.get(ours, []))
        # An extract at country level with no single country code may be a
        # multi-country group; somebody has to say what it is.
        if reg["kind"] == "country" and len(a2) != 1 and ours not in kinds and ours not in iso_set and ours not in covers:
            unresolved.append(f"{ours}: country level with {len(a2)} country codes and no kind or iso decision")
        name = r["name"]
        if ours in names:
            name, *aliases = names[ours]
        else:
            aliases = []
            odd = (not name.isascii()) or any(ch in name for ch in "<>/")
            if odd and ours not in accept:
                unresolved.append(f"{ours}: name {name!r} needs an English name or an accept decision")
        reg["name"], reg["aliases"] = name, aliases

    holders = {}
    for ours, reg in regions.items():
        for code in reg["codes"]:
            holders.setdefault(code, []).append(ours)
    for code, owners in sorted(holders.items()):
        if len(owners) > 1:
            unresolved.append(f"ISO {code} is on {', '.join(sorted(owners))}")

    # OpenAddresses: a data set is within a region when its code equals or is
    # below one of the region's codes, partial when it is above one.
    datasets = openaddresses["datasets"] if openaddresses else []
    codes_of = [oa_code(d["source"]) for d in datasets]
    children = {}
    for ours, reg in regions.items():
        if reg["kind"] != "group":
            children.setdefault(reg["parent"], []).append(ours)
    memo = {}

    def oa_sets(ours):
        if ours in memo:
            return memo[ours]
        if ours == "planet":
            result = (set(range(len(datasets))), set())
        else:
            reg = regions[ours]
            codes = reg["codes"]
            if codes:
                inn = {i for i, c in enumerate(codes_of) if within(c, codes)}
                part = {i for i, c in enumerate(codes_of) if above(c, codes)} - inn
            elif reg["kind"] == "continent":
                inn, part = set(), set()
                for child in children.get(ours, []):
                    ci, cp = oa_sets(child)
                    inn |= ci
                    part |= cp
                part -= inn
            else:
                # No code, so containment cannot be decided: everything the
                # nearest coded ancestor holds is partial.
                inn, part = set(), set()
                up = reg["parent"]
                while up != "planet" and not regions[up]["codes"] and regions[up]["kind"] != "continent":
                    up = regions[up]["parent"]
                if up != "planet":
                    ui, up_part = oa_sets(up)
                    part = ui | up_part
            result = (inn, part)
        memo[ours] = result
        return result

    def oa_block(ours):
        inn, part = oa_sets(ours)
        if not inn and not part:
            return None
        sized = [datasets[i]["bytes"] for i in inn if "bytes" in datasets[i]]
        block = dict(sources=len(inn), rows=sum(datasets[i]["rows"] for i in inn),
                     bytes=sum(sized), partial_sources=len(part))
        if len(sized) < len(inn):
            block["unsized_sources"] = len(inn) - len(sized)
        return block

    ov_counts = {}
    if overture:
        for rid, pair in overture["counts"].items():
            if rid in ids:
                ov_counts[ids[rid]] = pair
        ov_counts["planet"] = overture["planet"]

    def entry(ours, name, kind, parent, iso, aliases, osm):
        out = dict(id=ours, name=name, kind=kind, parent=parent)
        if iso:
            out["iso"] = iso
        out["aliases"] = aliases
        out["osm"] = osm
        block = oa_block(ours)
        if block:
            out["openaddresses"] = block
        if ours in ov_counts:
            places, addresses = ov_counts[ours]
            out["overture"] = dict(places=places, addresses=addresses)
        return out

    planet = geofabrik["planet"]
    out = [entry("planet", curation.planet_name, "world", None, None, curation.planet_aliases,
                 dict(url=planet["url"], bytes=planet["bytes"], updated=planet["updated"]))]
    for ours in sorted(regions):
        reg, r = regions[ours], regions[ours]["up"]
        out.append(entry(ours, reg["name"], reg["kind"], reg["parent"], reg["iso"], reg["aliases"],
                         dict(url=r["pbf"], bytes=r["bytes"], updated=r["updated"])))

    pins = dict(geofabrik=lock["geofabrik"]["fetched"][:10])
    if openaddresses:
        pins["openaddresses"] = lock["openaddresses"]["fetched"][:10]
    if overture:
        pins["overture"] = overture["release"]
    text = ('{"schema":1,\n"pins":' + line(pins) + ',\n"regions":[\n'
            + ",\n".join(line(r) for r in out) + "\n]}\n")
    return text, unresolved


def generate_text():
    lock = load_lock()
    geofabrik = load_snapshot("geofabrik")
    if geofabrik is None:
        raise SystemExit("no Geofabrik snapshot; run `update geofabrik` first")
    return build(geofabrik, load_snapshot("openaddresses"), load_snapshot("overture"), Curation(CURATION), lock)


def print_unresolved(unresolved):
    if unresolved:
        log(f"UNRESOLVED ({len(unresolved)}): decide each in catalog/curation.toml")
        for item in unresolved:
            log(f"  {item}")


def cmd_generate(_):
    text, unresolved = generate_text()
    if unresolved:
        print_unresolved(unresolved)
        return 1
    write(REGIONS, text)
    log(f"wrote {os.path.relpath(REGIONS, ROOT)}: {len(text.encode('utf-8'))} bytes")
    return 0


def cmd_check(_):
    failed = False
    lock = load_lock()
    for name, rel in SNAPSHOTS.items():
        path = os.path.join(CATALOG, rel)
        pinned = lock.get(name, {}).get("sha256")
        actual = sha256(read_bytes(path)) if os.path.exists(path) else None
        if pinned != actual:
            log(f"{rel}: sha256 {actual} does not match the lock ({pinned})")
            failed = True
    text, unresolved = generate_text()
    if unresolved:
        print_unresolved(unresolved)
        failed = True
    current = read_bytes(REGIONS).decode("utf-8") if os.path.exists(REGIONS) else None
    if current != text:
        log("catalog/regions.json is not what `generate` produces; run it and commit the result")
        failed = True
    if not failed:
        log("catalog is up to date")
    return 1 if failed else 0


# --- Update command and review report -----------------------------------

def review(before, after):
    """What changed between two catalogs, for a person to read before committing."""
    old = {r["id"]: r for r in json.loads(before)["regions"]} if before else {}
    new = {r["id"]: r for r in json.loads(after)["regions"]}
    added, removed = sorted(set(new) - set(old)), sorted(set(old) - set(new))
    lines = []
    if added:
        lines.append(f"added ({len(added)}): " + ", ".join(added))
    if removed:
        lines.append(f"removed ({len(removed)}): " + ", ".join(removed))
    for rid in sorted(set(old) & set(new)):
        a, b = old[rid], new[rid]
        if a["name"] != b["name"]:
            lines.append(f"renamed {rid}: {a['name']!r} -> {b['name']!r}")
        if a["osm"]["bytes"] and abs(b["osm"]["bytes"] - a["osm"]["bytes"]) / a["osm"]["bytes"] > SIZE_CHANGE:
            lines.append(f"osm size {rid}: {a['osm']['bytes']} -> {b['osm']['bytes']} bytes")
    return lines or ["no region changes"]


def cmd_update(args):
    lock = load_lock()
    started = time.time()
    steps = dict(geofabrik=update_geofabrik, openaddresses=update_openaddresses, overture=update_overture)
    for name in (steps if args.source == "all" else [args.source]):
        step = time.time()
        steps[name](lock)
        save_lock(lock)
        log(f"{name}: pinned in {time.time() - step:.0f}s")
    before = read_bytes(REGIONS).decode("utf-8") if os.path.exists(REGIONS) else None
    text, unresolved = generate_text()
    print("\n".join(review(before, text)))
    if unresolved:
        print_unresolved(unresolved)
        log("catalog/regions.json left as it was")
        return 1
    write(REGIONS, text)
    log(f"wrote catalog/regions.json: {len(text.encode('utf-8'))} bytes in {time.time() - started:.0f}s")
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = p.add_subparsers(dest="command", required=True)
    up = sub.add_parser("update", help="fetch an upstream, pin it and regenerate")
    up.add_argument("source", choices=["geofabrik", "openaddresses", "overture", "all"])
    up.set_defaults(run=cmd_update)
    sub.add_parser("generate", help="build regions.json from the pinned snapshots").set_defaults(run=cmd_generate)
    sub.add_parser("check", help="fail if regions.json or a snapshot drifted").set_defaults(run=cmd_check)
    a = p.parse_args()
    sys.exit(a.run(a))


if __name__ == "__main__":
    main()
