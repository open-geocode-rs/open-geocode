import json
import re
import urllib.request
import zipfile

from . import fetch, paths, unzip

SOURCE = {
    "register": "Register naslovov",
    "country": "SI",
    "publisher": "Geodetska uprava Republike Slovenije (Surveying and Mapping Authority of the Republic of Slovenia)",
    "licence": "CC BY 4.0",
    "attribution": "Geodetska uprava Republike Slovenije, Register naslovov, {date}",
    "url": "https://ipi.eprostor.gov.si/jgp-service-api/display-views/groups/120/composite-products/281/file?filterParam=DRZAVA&filterValue=1",
    "notes": "whole-country CSV from the Javni geodetski podatki service; rows with ST_STANOVANJA are flats (unit); id EID_NASLOV; house_number HS_STEVILKA plus HS_DODATEK; road ULICA_NAZIV, empty where the address has none (settlement-named addresses), Slovene names not the bilingual _DJ ones; city is the post office name POSTNI_OKOLIS_NAZIV with POSTNI_OKOLIS_SIFRA as postcode; no state; D96/TM EPSG:3794 E,N transformed to EPSG:4326 with pyproj",
}


def download(cache):
    path = cache / "RN_SLO_NASLOVI.zip"
    if not path.is_file():
        # The service answers with a short-lived signed link to the archive.
        request = urllib.request.Request(SOURCE["url"], headers={"User-Agent": "open-geocode fixtures"})
        with urllib.request.urlopen(request) as response:
            link = json.load(response)["url"]
        fetch(link, path)
    with zipfile.ZipFile(path) as zipped:
        stamp = re.search(r"_(\d{4})(\d\d)(\d\d)\.csv$", zipped.namelist()[0]).groups()
    SOURCE["attribution"] = SOURCE["attribution"].format(date=".".join(reversed(stamp)))
    return [path], "-".join(stamp)


def query(files):
    found = paths(unzip(files[0], files[0].parent / "csv"), "*.csv")
    return f"""
        select EID_NASLOV as id, E as x, N as y, 'EPSG:3794' as crs,
               nullif(ST_STANOVANJA, '') is not null as unit,
               concat(HS_STEVILKA, HS_DODATEK) as house_number, ULICA_NAZIV as road,
               POSTNI_OKOLIS_SIFRA as postcode, POSTNI_OKOLIS_NAZIV as city
        from read_csv({found}, delim=',', quote='"', header=true, all_varchar=true)
    """
