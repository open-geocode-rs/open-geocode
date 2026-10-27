import re
import zipfile

from . import fetch

SOURCE = {
    "register": "Adressregister, Adresse Relationale Tabellen - Stichtagsdaten",
    "country": "AT",
    "publisher": "BEV - Bundesamt für Eich- und Vermessungswesen",
    "licence": "CC BY 4.0",
    "attribution": "© Österreichisches Adressregister, Stichtagsdaten vom {stichtag}",
    "url": "https://data.bev.gv.at/download/Adressregister/Adresse_Relationale_Tabellen_Stichtagsdaten.zip",
    "notes": "address level (ADRESSE.csv) only, so no Stiege or building sub-addresses; Grundstücksnummer pseudo-addresses (GNRADRESSE=1, 'GNR ...') dropped; id ADRCD; house_number HNR_ADR_ZUSAMMEN as written; road STRASSENNAME plus STRASSENNAMENZUSATZ where SZUSADRBEST says the suffix is part of the name; city is the postal Zustellort of the street, postcode PLZ; no state, as the register has no region name; no point marked estimated, as every BESTIMMUNGSART places the point on the address's own parcel; MGI / Austria GK West, Central and East (EPSG:31254, 31255, 31256, per row) transformed to EPSG:4326 with pyproj",
}
TABLES = ("ADRESSE.csv", "STRASSE.csv", "Aktualitaetsstand.txt")


def download(cache):
    archive, _ = fetch(SOURCE["url"], cache / SOURCE["url"].rsplit("/", 1)[1])
    tables = cache / "tables"
    if not tables.is_dir():
        with zipfile.ZipFile(archive) as zipped:
            tables.mkdir()
            for name in TABLES:
                (tables / name).write_bytes(zipped.read(name))
    # The archive at this URL is replaced each half year; its own stamp names the Stichtag.
    day, month, year = re.search(r"(\d\d)\.(\d\d)\.(\d{4})", (tables / "Aktualitaetsstand.txt").read_text("latin-1")).groups()
    SOURCE["attribution"] = SOURCE["attribution"].format(stichtag=f"{day}.{month}.{year}")
    return [archive], f"Stichtag {year}-{month}-{day}"


def query(files):
    tables = files[0].parent / "tables"

    def table(name):
        return f"read_csv('{(tables / name).as_posix()}', delim=';', quote='\"', escape='\"', header=true, all_varchar=true)"

    return f"""
        select a.ADRCD as id, a.RW as x, a.HW as y, 'EPSG:' || a.EPSG as crs,
               a.HNR_ADR_ZUSAMMEN as house_number,
               concat_ws(' ', s.STRASSENNAME, case when s.SZUSADRBEST = '1' then s.STRASSENNAMENZUSATZ end) as road,
               a.PLZ as postcode, s.ZUSTELLORT as city
        from {table('ADRESSE.csv')} a
        join {table('STRASSE.csv')} s using (SKZ)
        where a.GNRADRESSE = '0'
    """
