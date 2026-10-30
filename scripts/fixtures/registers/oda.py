import csv
import io
import re
import zipfile

from . import fetch, paths, unzip

BASE = "https://www150.statcan.gc.ca/n1/en/pub/46-26-0001/2021001/"
# StatCan publishes no file for NL, YT or NU.
PROVINCES = ("AB", "BC", "MB", "NB", "NS", "NT", "ON", "PE", "QC", "SK")
# Census province codes, as in the ODA's pruid.
PRUID = {
    "10": "NL", "11": "PE", "12": "NS", "13": "NB", "24": "QC", "35": "ON", "46": "MB", "47": "SK",
    "48": "AB", "59": "BC", "60": "YT", "61": "NT", "62": "NU",
}

OGL = "Contains information licensed under the Open Government Licence - Canada."
SOURCE = {
    "register": "Open Database of Addresses",
    "country": "CA",
    "publisher": "Statistics Canada",
    "licence": "Open Government Licence - Canada (StatCan states every source's open licence is compatible with it)",
    "attribution": f"{OGL} Statistics Canada, Open Database of Addresses (ODA) v1.0.",
    "url": BASE,
    "notes": (
        "all 10 provincial and territorial files StatCan publishes (none for NL, YT, NU); id is the ODA id, "
        "StatCan's hash of point and address; points are the sources' WGS84 degrees as given; any unit, "
        "or a street_no of two numbers (a suite written before the civic number), marks a unit; road is "
        "the source's full street with type and direction; city is the source's "
        "city, else StatCan's processed city_pcs; where both are empty (all of New Brunswick) the census "
        "subdivision name csdname is the town, as OpenCage's CA template prints no municipality; state is "
        "the province code of pruid; no point type or retirement flag exists to filter on; values a source "
        "writes all in capitals title-cased, mixed-case values left as written; city and town lose "
        "administrative labels (former, City of, _municipality, underscores, -County); attribution lists every "
        "provider statement of Data_Sources.csv"
    ),
}
RELEASE = "v1.0, 2021"
# Codes that stay in capitals when a value is title-cased.
UPPER = {"NE", "NW", "SE", "SW", "RR", "PEI", "BC"}
ROMAN = re.compile(r"\b(?=[ivx]{2,}\b)[ivx]+\b", re.IGNORECASE)
# French particles, lower-cased inside a name as Canada Post writes them.
PARTICLES = {"de", "du", "des", "la", "le", "les", "d'", "l'", "et", "aux", "au"}
# Some sources write the suite before the civic number in street_no ('10 - 4271',
# '206 52521') and leave unit empty.
UNIT_FIRST = r"^[0-9]+[A-Za-z]?( +| *- *)[0-9]"
# Sources' administrative labels ('former Toronto', 'City of Hamilton',
# 'North_cowichan_municipality'), which no person or Canada Post writes.
PLACE_PREFIX = re.compile(r"^(former|city of|town of|village of|municipality of|district of|county of)\s+", re.IGNORECASE)
PLACE_SUFFIX = re.compile(r"\s+municipality$", re.IGNORECASE)


def download(cache):
    files = []
    for province in PROVINCES:
        path, _ = fetch(f"{BASE}ODA_{province}_v1.zip", cache / f"ODA_{province}_v1.zip")
        unzip(path, cache / f"csv_{province}", keep=lambda name: name.startswith("ODA_") and name.endswith(".csv"))
        files.append(path)
    # The providers' licences ask for their own statements wherever their
    # data goes, and StatCan ships those statements only inside the archives.
    SOURCE["attribution"] = " ".join([f"{OGL} Statistics Canada, Open Database of Addresses (ODA) v1.0.", *providers(files[0])])
    return files, RELEASE


def providers(path):
    with zipfile.ZipFile(path) as zipped:
        text = zipped.read("Data_Sources.csv").decode("cp1252")
    statements = []
    for row in csv.DictReader(io.StringIO(text)):
        statement = row["Attribution statement (in language of origin)"].strip()
        # Two rows carry CC's licence summary, not a statement; their credit is
        # the provider and its licence.
        if not statement or statement.startswith("Attribution —"):
            statement = f"{row['Data Provider'].strip()}, {row['Licence Title'].strip() or 'open data'}"
        statement = statement.rstrip(".") + "."
        if statement not in statements:
            statements.append(statement)
    return statements


def title(text):
    if not text or not text.isupper():
        return text
    words = re.sub(r"\b([^\W\d_])", lambda m: m[1].upper(), text.lower()).split(" ")
    out = []
    for i, word in enumerate(words):
        if word.upper() in UPPER:
            out.append(word.upper())
        elif i and word.lower() in PARTICLES:
            out.append(word.lower())
        else:
            out.append(word)
    return ROMAN.sub(lambda m: m[0].upper(), " ".join(out))


def query(files):
    found = [path for province in PROVINCES for path in paths(files[0].parent / f"csv_{province}", "*.csv")]
    provinces = " ".join(f"when '{code}' then '{name}'" for code, name in PRUID.items())
    return f"""
        select id, longitude as x, latitude as y, 'EPSG:4326' as crs,
               nullif(unit, '') is not null or regexp_matches(street_no, '{UNIT_FIRST}') as unit,
               street_no as house_number, street as road, postal_code as postcode,
               coalesce(nullif(city, ''), city_pcs) as city,
               case when coalesce(nullif(city, ''), nullif(city_pcs, '')) is null then csdname end as town,
               case pruid {provinces} end as state
        from read_csv({found}, quote='"', header=true, all_varchar=true, union_by_name=true)
    """


def place(text):
    if not text:
        return text
    # An underscored label is a lower-cased slug ('North_cowichan'), so it is
    # recased whole.
    slug = "_" in text
    text = PLACE_SUFFIX.sub("", PLACE_PREFIX.sub("", text.replace("_", " "))).replace("-County", " County")
    return text.title() if slug or not any(ch.isupper() for ch in text) else text


def finish(row):
    for key in ("road", "city", "town"):
        row[key] = title(row[key])
    for key in ("city", "town"):
        row[key] = place(row[key])
