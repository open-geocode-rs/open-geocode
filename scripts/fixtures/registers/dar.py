import io
import json
import re
import urllib.request
import zipfile

from . import fetch

SOURCE = {
    "register": "Danmarks Adresseregister (DAR)",
    "country": "DK",
    "publisher": "Klimadatastyrelsen",
    "licence": "CC BY 4.0",
    "attribution": "Indeholder data fra Klimadatastyrelsen, Danmarks Adresseregister (DAR)",
    "url": "ftp://ftp3.datafordeler.dk/DAR_Aktuelt_Totaludtraek_JSON_HF_<timestamp>.zip",
    "notes": "Datafordeleren's anonymous daily current extract; one row per husnummer (access address), so floor and door addresses are not read; husnumre with status nedlagt (4) or henlagt (5) excluded, foreløbig (2) kept; id is the husnummer id_lokalId; point is its adgangspunkt, EPSG:25832 transformed to EPSG:4326 with pyproj; points with nøjagtighedsklasse U (not placed at the address) marked estimated; house_number is husnummertekst; road is the full vejnavn; city is the postnummer navn, supplerende bynavn not used",
}
FOLDER = "ftp://ftp3.datafordeler.dk/"
PATTERN = re.compile(r"DAR_Aktuelt_Totaludtraek_JSON_HF_(\d{14})\.zip")
# The rest of the extract (Adresse units, road geometry, divisions) is not needed.
KEEP = {
    "HusnummerList": ("id_lokalId", "status", "adgangspunkt", "husnummertekst", "navngivenVej", "postnummer"),
    "AdressepunktList": ("id_lokalId", "oprindelse_nøjagtighedsklasse", "position"),
    "NavngivenVejList": ("id_lokalId", "vejnavn"),
    "PostnummerList": ("id_lokalId", "postnr", "navn"),
}
RETIRED = ("4", "5")


def latest():
    # The folder keeps a week of daily files under timestamped names.
    with urllib.request.urlopen(FOLDER) as response:
        names = PATTERN.findall(response.read().decode())
    return max(names)


def extract(archive, out):
    """Split the 10 GB single-object JSON into one NDJSON per needed list. The
    extract prints one field per line and objects open on a line of their own,
    so lines are enough to stream it."""
    out.mkdir()
    handles = {name: open(out / f"{name}.ndjson", "w", encoding="utf-8") for name in KEEP}
    with zipfile.ZipFile(archive) as zipped:
        name = next(name for name in zipped.namelist() if not name.endswith("_Metadata.json"))
        with zipped.open(name) as raw:
            current, lines = None, None
            for line in io.TextIOWrapper(raw, encoding="utf-8"):
                if match := re.match(r'^,?"(\w+List)":\s*\[', line):
                    current = match.group(1)
                elif current not in KEEP:
                    continue
                elif line.strip() in ("{", ",{"):
                    lines = ["{"]
                elif line.strip() == "}":
                    item = json.loads("".join(lines) + "}")
                    handles[current].write(json.dumps({key: item[key] for key in KEEP[current]}, ensure_ascii=False) + "\n")
                    lines = None
                elif lines is not None:
                    lines.append(line)
    for handle in handles.values():
        handle.close()


def download(cache):
    stamp = latest()
    archive, _ = fetch(f"{FOLDER}DAR_Aktuelt_Totaludtraek_JSON_HF_{stamp}.zip", cache / f"DAR_Aktuelt_Totaludtraek_JSON_HF_{stamp}.zip")
    out = cache / f"lists_{stamp}"
    if not out.is_dir():
        partial = cache / f"lists_{stamp}.part"
        extract(archive, partial)
        partial.replace(out)
    return [archive], f"{stamp[:4]}-{stamp[4:6]}-{stamp[6:8]}"


def query(files):
    out = files[0].parent / f"lists_{PATTERN.match(files[0].name).group(1)}"

    def table(name):
        columns = ", ".join(f"'{key}': 'VARCHAR'" for key in KEEP[name])
        path = (out / f"{name}.ndjson").as_posix()
        return f"read_json('{path}', format='newline_delimited', columns={{{columns}}})"

    point = r"'^POINT\(([-0-9.eE]+) ([-0-9.eE]+)\)$'"
    return f"""
        select h.id_lokalId as id, regexp_extract(a.position, {point}, 1) as x,
               regexp_extract(a.position, {point}, 2) as y, 'EPSG:25832' as crs,
               a."oprindelse_nøjagtighedsklasse" = 'U' as estimated,
               h.husnummertekst as house_number, v.vejnavn as road, p.postnr as postcode, p.navn as city
        from {table('HusnummerList')} h
        left join {table('AdressepunktList')} a on a.id_lokalId = h.adgangspunkt
        left join {table('NavngivenVejList')} v on v.id_lokalId = h.navngivenVej
        left join {table('PostnummerList')} p on p.id_lokalId = h.postnummer
        where h.status not in {RETIRED}
    """
