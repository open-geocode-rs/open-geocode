import re
import shutil
import subprocess
from pathlib import Path

from . import fetch, to_parquet

SOURCE = {
    "register": "National Address Database",
    "country": "US",
    "publisher": "US Department of Transportation",
    "licence": "Public domain (US Government work, data.gov rights: public)",
    "attribution": "National Address Database (NAD), US Department of Transportation",
    "url": "https://data.transportation.gov/download/fc2s-wawr/application/x-zip-compressed",
    "notes": (
        "TXT release, Longitude and Latitude as published (WGS84 degrees); retired records dropped: "
        "Lifecycle Retired, Inactive or Not Active in any case, or an Expire date from 1971 to the "
        "compilation date (earlier ones are 1899, 1900, 1969 and 1970 zero-date sentinels);Placement Linear Geocode marked estimated, every other Placement (Unknown, "
        "Structure, Site, Property Access, Parcel, Other) kept; any of Building, Floor, Unit, Room, Seat, "
        "Addtl_Loc or SubAddress marks a unit; house number AddNo_Full; road StNam_Full; postcode Zip_Code; "
        "city is Post_City, else Inc_Muni when Post_City is 'Not stated', never the placeholders 'Not "
        "stated' or 'Unincorporated'; state the two-letter State; words of road and city written in capitals "
        "title-cased (sources mix capital names with mixed-case types), route and direction codes kept;the archive is Deflate64 and is read through 7-Zip"
    ),
}
COMPILED = "2026-09-30"
RELEASE = f"Release 24, compiled {COMPILED}"
# Only Linear Geocode is placed along the street rather than at the address.
ESTIMATED = {"Linear Geocode"}
RETIRED = {"retired", "inactive", "not active"}
SUBADDRESS = ("Building", "Floor", "Unit", "Room", "Seat", "Addtl_Loc", "SubAddress")
# Route and direction codes that stay in capitals when a value is title-cased.
UPPER = {"NE", "NW", "SE", "SW", "US", "SR", "CR", "FM", "RM", "RR"}
ROMAN = re.compile(r"\b(?=[ivx]{2,}\b)[ivx]+\b", re.IGNORECASE)
MEMBER = "TXT/NAD_r24.txt"


COLUMNS = ("UUID", "Lifecycle", "Expire", "Placement", "Latitude", "Longitude", "AddNo_Full", "StNam_Full",
           "Zip_Code", "Post_City", "Inc_Muni", "State", *SUBADDRESS)


def download(cache):
    archive, _ = fetch(SOURCE["url"], cache / "NAD_r24_TXT.zip")
    parquet = cache / "nad.parquet"
    if not parquet.is_file():
        # Python's zipfile and duckdb cannot read Deflate64, so 7-Zip unpacks
        # the 42 GB text once; only the Parquet of the needed columns stays.
        text = cache / "txt"
        subprocess.run([sevenzip(), "e", str(archive), MEMBER, f"-o{text}", "-y"], check=True, stdout=subprocess.DEVNULL)
        member = text / Path(MEMBER).name
        columns = ", ".join(f'"{column}"' for column in COLUMNS)
        to_parquet(f"select {columns} from read_csv('{member.as_posix()}', quote='\"', header=true, all_varchar=true)", parquet)
        member.unlink()
        text.rmdir()
    return [archive], RELEASE


def title(text):
    # Many sources write the name in capitals and the type expanded in mixed
    # case ('LINDEN Road'), so capitals are recased word by word.
    words = []
    for word in text.split(" "):
        if word.isupper() and word not in UPPER:
            word = re.sub(r"\b([^\W\d_])", lambda m: m[1].upper(), word.lower())
        words.append(word)
    return ROMAN.sub(lambda m: m[0].upper(), " ".join(words))


def sevenzip():
    found = shutil.which("7z") or shutil.which("7zz")
    default = Path("C:/Program Files/7-Zip/7z.exe")
    if found:
        return found
    if default.is_file():
        return str(default)
    raise SystemExit("nad needs 7-Zip: the archive is Deflate64, which neither zipfile nor duckdb reads")


def query(files):
    parquet = (files[0].parent / "nad.parquet").as_posix()
    placeholder = "('', 'not stated', 'unincorporated')"
    subaddress = " or ".join(f"nullif(trim({column}), '') is not null" for column in SUBADDRESS)
    # ISO dates order as strings. 1899, 1900, 1969 and 1970 are the zero
    # dates of spreadsheets and Unix, a source's empty field, not a retirement.
    return f"""
        select UUID as id, Longitude as x, Latitude as y, 'EPSG:4326' as crs,
               ({subaddress}) as unit, Placement = '{next(iter(ESTIMATED))}' as estimated,
               AddNo_Full as house_number, StNam_Full as road, Zip_Code as postcode,
               coalesce(case when lower(trim(Post_City)) not in {placeholder} then trim(Post_City) end,
                        case when lower(trim(Inc_Muni)) not in {placeholder} then trim(Inc_Muni) end) as city,
               State as state
        from read_parquet('{parquet}')
        where lower(trim(coalesce(Lifecycle, ''))) not in {tuple(sorted(RETIRED))}
          and not (regexp_matches(coalesce(Expire, ''), '^[0-9]{{4}}-[0-9]{{2}}-[0-9]{{2}}')
                   and left(Expire, 10) between '1971' and '{COMPILED}')
    """


def finish(row):
    for key in ("road", "city"):
        row[key] = title(row[key])
