import csv
import os
import re
import xml.etree.ElementTree as ET
import zipfile
from concurrent.futures import ProcessPoolExecutor

from . import fetch

SOURCE = {
    "register": "Basisregistratie Adressen en Gebouwen (BAG)",
    "country": "NL",
    "publisher": "Kadaster",
    "licence": "CC0 1.0",
    "attribution": "Kadaster, Basisregistratie Adressen en Gebouwen (BAG)",
    "url": "https://service.pdok.nl/kadaster/bag/atom/downloads/lvbag-extract-nl.zip",
    "notes": "id is the nummeraanduiding; only each object's voorkomen valid on the extract date; nummeraanduiding status Naamgeving uitgegeven only, its verblijfsobject in use (in gebruik, in gebruik niet ingemeten, verbouwing) or its ligplaats or standplaats aangewezen, so planned, unrealised, withdrawn and out-of-use addresses are dropped; hoofd- and nevenadressen both kept with their object's point; verblijfsobject punt, ligplaats and standplaats as the centroid of their polygon (they have no point); RD New EPSG:28992 to WGS84 by PROJ; huisletter and huisnummertoevoeging are part of the Dutch house number, written as PDOK does (12A-2), never units; road is the openbare ruimte naam, city the woonplaats (the nummeraanduiding's own if it has one, else its openbare ruimte's), postcode as written (1234AB)",
}
OBJECTEN = "{www.kadaster.nl/schemas/lvbag/imbag/objecten/v20200601}"
REF = "{www.kadaster.nl/schemas/lvbag/imbag/objecten-ref/v20200601}"
HISTORIE = "{www.kadaster.nl/schemas/lvbag/imbag/historie/v20200601}"
GML = "{http://www.opengis.net/gml/3.2}"
KINDS = {
    "VBO": "Verblijfsobject",
    "LIG": "Ligplaats",
    "STA": "Standplaats",
    "NUM": "Nummeraanduiding",
    "OPR": "OpenbareRuimte",
    "WPL": "Woonplaats",
}
# An address whose object is in use, including one being rebuilt or not yet
# surveyed: its point is still the register's.
LIVE = {
    "Verblijfsobject in gebruik",
    "Verblijfsobject in gebruik (niet ingemeten)",
    "Verbouwing verblijfsobject",
    "Plaats aangewezen",
}


def download(cache):
    archive, _ = fetch(SOURCE["url"], cache / "lvbag-extract-nl.zip")
    inner = cache / "extract"
    with zipfile.ZipFile(archive) as zipped:
        stand = re.search(
            r"<[^>]*StandTechnischeDatum>([0-9-]+)<", zipped.read("Leveringsdocument-BAG-Extract.xml").decode()
        ).group(1)
        if not inner.is_dir():
            inner.mkdir()
            for name in zipped.namelist():
                if name[4:7] in KINDS and name.endswith(".zip"):
                    zipped.extract(name, inner)
    return [archive], stand


def current(element, stand):
    voorkomen = element.find(f"{OBJECTEN}voorkomen/{HISTORIE}Voorkomen")
    if voorkomen.find(f"{HISTORIE}eindRegistratie") is not None:
        return False
    if voorkomen.find(f"{HISTORIE}tijdstipInactief") is not None:
        return False
    if voorkomen.findtext(f"{HISTORIE}beginGeldigheid") > stand:
        return False
    end = voorkomen.findtext(f"{HISTORIE}eindGeldigheid")
    return end is None or end > stand


def centroid(polygon):
    pos = polygon.find(f"{GML}exterior/{GML}LinearRing/{GML}posList")
    dimension = int(pos.get("srsDimension") or polygon.get("srsDimension") or 2)
    values = list(map(float, pos.text.split()))
    ring = list(zip(values[0::dimension], values[1::dimension]))
    x0, y0 = ring[0]
    area = cx = cy = 0.0
    for (x1, y1), (x2, y2) in zip(ring, ring[1:]):
        x1, y1, x2, y2 = x1 - x0, y1 - y0, x2 - x0, y2 - y0
        cross = x1 * y2 - x2 * y1
        area += cross
        cx += (x1 + x2) * cross
        cy += (y1 + y2) * cross
    if not area:
        return "", ""
    return x0 + cx / (3 * area), y0 + cy / (3 * area)


def point(element):
    geometry = element.find(f"{OBJECTEN}geometrie")
    if geometry is None:
        return "", ""
    pos = geometry.find(f"{OBJECTEN}punt/{GML}Point/{GML}pos")
    if pos is not None:
        x, y = pos.text.split()[:2]
        return x, y
    polygon = geometry.find(f".//{GML}Polygon")
    return centroid(polygon) if polygon is not None else ("", "")


def parse(task):
    path, member, kind, out, stand = task
    tag = OBJECTEN + KINDS[kind]
    with zipfile.ZipFile(path) as zipped, zipped.open(member) as handle, open(out, "w", encoding="utf-8", newline="") as sink:
        writer = csv.writer(sink, delimiter="\t")
        events = ET.iterparse(handle, events=("start", "end"))
        _, root = next(events)
        for event, element in events:
            if event != "end" or element.tag != tag:
                continue
            if current(element, stand):
                ident = element.findtext(f"{OBJECTEN}identificatie")
                status = element.findtext(f"{OBJECTEN}status")
                if kind in ("VBO", "LIG", "STA"):
                    if status in LIVE:
                        x, y = point(element)
                        for ref in element.iter(f"{REF}NummeraanduidingRef"):
                            writer.writerow((ref.text, x, y))
                elif kind == "NUM":
                    if status == "Naamgeving uitgegeven":
                        writer.writerow((
                            ident,
                            element.findtext(f"{OBJECTEN}huisnummer") or "",
                            element.findtext(f"{OBJECTEN}huisletter") or "",
                            element.findtext(f"{OBJECTEN}huisnummertoevoeging") or "",
                            element.findtext(f"{OBJECTEN}postcode") or "",
                            element.findtext(f".//{REF}OpenbareRuimteRef") or "",
                            element.findtext(f".//{REF}WoonplaatsRef") or "",
                        ))
                elif kind == "OPR":
                    writer.writerow((ident, element.findtext(f"{OBJECTEN}naam") or "", element.findtext(f".//{REF}WoonplaatsRef") or ""))
                else:
                    writer.writerow((ident, element.findtext(f"{OBJECTEN}naam") or ""))
            # Clearing the object alone leaves it attached to its parent, so a
            # member's millions of objects would stay in memory until EOF.
            root.clear()
    return out


def tables(archive, stand):
    inner = archive.parent / "extract"
    parsed = archive.parent / f"parsed-{stand}"
    done = parsed / "complete"
    if done.is_file():
        return parsed
    parsed.mkdir(exist_ok=True)
    tasks = []
    for path in sorted(inner.glob("*.zip")):
        kind = path.name[4:7]
        with zipfile.ZipFile(path) as zipped:
            for member in zipped.namelist():
                if member.endswith(".xml"):
                    tasks.append((path, member, kind, parsed / f"{member[:-4]}.tsv", stand))
    with ProcessPoolExecutor(max_workers=max(1, (os.cpu_count() or 2) - 2)) as pool:
        list(pool.map(parse, tasks, chunksize=1))
    done.write_text(stand)
    return parsed


def query(files):
    with zipfile.ZipFile(files[0]) as zipped:
        stand = re.search(
            r"<[^>]*StandTechnischeDatum>([0-9-]+)<", zipped.read("Leveringsdocument-BAG-Extract.xml").decode()
        ).group(1)
    parsed = tables(files[0], stand)

    def table(kind, columns):
        found = sorted(path.as_posix() for path in parsed.glob(f"9999{kind}*.tsv"))
        names = {column: "VARCHAR" for column in columns}
        return f"read_csv({found}, delim='\t', header=false, quote='\"', columns={names})"

    return f"""
        select n.id, p.x, p.y, 'EPSG:28992' as crs,
               concat(n.nr, n.letter, case when nullif(n.toev, '') is not null then '-' || n.toev end) as house_number,
               o.naam as road, n.postcode, coalesce(nullif(wn.naam, ''), wo.naam) as city
        from {table('NUM', ('id', 'nr', 'letter', 'toev', 'postcode', 'opr', 'wpl'))} n
        join (
            select * from {table('VBO', ('num', 'x', 'y'))}
            union all select * from {table('LIG', ('num', 'x', 'y'))}
            union all select * from {table('STA', ('num', 'x', 'y'))}
        ) p on p.num = n.id
        left join {table('OPR', ('id', 'naam', 'wpl'))} o on o.id = n.opr
        left join {table('WPL', ('id', 'naam'))} wn on wn.id = nullif(n.wpl, '')
        left join {table('WPL', ('id', 'naam'))} wo on wo.id = o.wpl
    """
