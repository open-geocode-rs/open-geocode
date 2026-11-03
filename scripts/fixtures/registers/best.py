import csv
import re
import xml.etree.ElementTree as ET
import zipfile

from . import fetch

SOURCE = {
    "register": "BeST Address",
    "country": "BE",
    "publisher": "FPS BOSA, from Digitaal Vlaanderen, Paradigm and Service public de Wallonie",
    "licence": "CC BY 4.0",
    "attribution": "The data are made available by respectively DigitaalVlaanderen (Flemish Region), Paradigm (Brussels-Capital Region), Service public de Wallonie (Walloon Region)",
    "url": "https://opendata.bosa.be/download/best/best-full-latest.zip",
    "notes": "status current only (proposed, rejected, retired dropped), officially unassigned but current addresses kept; box numbers are units; positionSpecification street or municipality marked estimated; Lambert 2008 (EPSG:3812, per each point's srsName) to WGS84 by PROJ; names in the municipality's language: Dutch in Flanders, French in Wallonia except German in the nine German-speaking municipalities, French in bilingual Brussels, then Dutch, French, German in that order where that one is missing; locality is the municipality, postcode is the postal info code, state is the region; Walloon hamlet-type street names kept as the road",
}
COM = "{http://fsb.belgium.be/data/common}"
TNS = "{http://fsb.belgium.be/mappingservices/FullDownload/v1_00}"
REGIONS = ("Flanders", "Wallonia", "Brussels")
# The German-speaking Community, whose addresses are written in German.
GERMAN = ("63001", "63012", "63013", "63023", "63040", "63048", "63061", "63067", "63087")
STATE = {
    ("Flanders", "nl"): "Vlaams Gewest",
    ("Wallonia", "fr"): "Région wallonne",
    ("Wallonia", "de"): "Wallonische Region",
    ("Brussels", "fr"): "Région de Bruxelles-Capitale",
}
ESTIMATED = ("street", "municipality")
LANGUAGES = ("nl", "fr", "de")


def download(cache):
    archive, _ = fetch(SOURCE["url"], cache / "best-full-latest.zip")
    inner = cache / "full"
    if not inner.is_dir():
        inner.mkdir()
        with zipfile.ZipFile(archive) as zipped:
            zipped.extractall(inner)
    dates = {}
    for path in inner.glob("*Address*.zip"):
        region, date = re.fullmatch(r"([A-Za-z]+)Address(\d{8})\.zip", path.name).groups()
        dates[region] = f"{date[:4]}-{date[4:6]}-{date[6:]}"
    tables(cache)
    return [archive], ", ".join(f"{region} {dates[region]}" for region in REGIONS)


def ref(element):
    namespace = element.findtext(f"{COM}namespace")
    return f"{namespace.rstrip('/')}/{element.findtext(f'{COM}objectIdentifier')}"


def parse(path, tag):
    with zipfile.ZipFile(path) as zipped:
        (name,) = zipped.namelist()
        with zipped.open(name) as handle:
            events = ET.iterparse(handle, events=("start", "end"))
            _, root = next(events)
            for event, element in events:
                if event == "end" and element.tag == TNS + tag:
                    yield element
                    root.clear()


def tables(cache):
    """The XML once as three tab-separated tables duckdb joins."""
    out = cache / "tables"
    if out.is_dir():
        return out
    partial = cache / "tables.part"
    partial.mkdir(exist_ok=True)
    inner = cache / "full"
    with (
        open(partial / "addresses.tsv", "w", encoding="utf-8", newline="") as addresses,
        open(partial / "names.tsv", "w", encoding="utf-8", newline="") as names,
    ):
        address_writer = csv.writer(addresses, delimiter="\t")
        name_writer = csv.writer(names, delimiter="\t")
        address_writer.writerow(
            ("id", "region", "status", "nis", "x", "y", "srs", "box", "position", "house", "street", "municipality", "postcode")
        )
        name_writer.writerow(("ref", *LANGUAGES))
        for region in REGIONS:
            for kind, tag in (("StreetName", "streetName"), ("Municipality", "municipality")):
                (path,) = inner.glob(f"{region}{kind}*.zip")
                for element in parse(path, tag):
                    spelled = {name.findtext(f"{COM}language"): name.findtext(f"{COM}spelling") for name in element.iter(f"{COM}name")}
                    name_writer.writerow((ref(element.find(f"{COM}code")), *(spelled.get(language) or "" for language in LANGUAGES)))
            (path,) = inner.glob(f"{region}Address*.zip")
            for address in parse(path, "address"):
                municipality = address.find(f"{COM}hasMunicipality")
                pos = address.find(f"{COM}position/{COM}pointGeometry/{COM}point/{COM}pos")
                x = y = srs = ""
                if pos is not None and pos.text:
                    x, y = pos.text.split()[:2]
                    srs = pos.get("srsName").rsplit("/", 1)[1]
                postal = address.find(f"{COM}hasPostalInfo")
                address_writer.writerow((
                    ref(address.find(f"{COM}code")),
                    region,
                    address.findtext(f"{COM}addressStatus/{COM}status") or "",
                    municipality.findtext(f"{COM}objectIdentifier"),
                    x,
                    y,
                    srs,
                    address.findtext(f"{COM}boxNumber") or "",
                    address.findtext(f"{COM}position/{COM}positionSpecification") or "",
                    address.findtext(f"{COM}houseNumber") or "",
                    ref(address.find(f"{COM}hasStreetName")),
                    ref(municipality),
                    postal.findtext(f"{COM}objectIdentifier") if postal is not None else "",
                ))
    partial.replace(out)
    return out


def pick(table, language):
    order = [language, *(other for other in LANGUAGES if other != language)]
    return "coalesce(" + ", ".join(f"nullif({table}.{name}, '')" for name in order) + ")"


def query(files):
    out = files[0].parent / "tables"
    read = lambda name: f"read_csv('{(out / name).as_posix()}', delim='\t', quote='\"', header=true, all_varchar=true)"
    language = f"case when a.region = 'Flanders' then 'nl' when a.region = 'Wallonia' and a.nis in {GERMAN} then 'de' else 'fr' end"
    states = " ".join(f"when a.region = '{region}' and l.language = '{lang}' then '{name}'" for (region, lang), name in STATE.items())
    named = lambda table: " ".join(f"when '{lang}' then {pick(table, lang)}" for lang in LANGUAGES)
    return f"""
        select a.id, a.x, a.y, 'EPSG:' || a.srs as crs, nullif(a.box, '') is not null as unit,
               a.position in {ESTIMATED} as estimated, a.house as house_number,
               case l.language {named('s')} end as road,
               case l.language {named('m')} end as municipality,
               a.postcode, case {states} when a.region = 'Wallonia' then 'Région wallonne' end as state
        from {read('addresses.tsv')} a
        cross join lateral (select {language} as language) l
        left join {read('names.tsv')} s on s.ref = a.street
        left join {read('names.tsv')} m on m.ref = a.municipality
        where a.status = 'current'
    """
