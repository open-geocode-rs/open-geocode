import os

from . import fetch

JOBS = range(946703, 946719)
JOB_URL = "https://batch.openaddresses.io/api/job/{}/output/source.geojson.gz"
RELEASE = "2026-10-30 (OpenAddresses PRG jobs for all 16 voivodeships)"
SOURCE = {
    "register": "Państwowy Rejestr Granic (PRG), address points",
    "country": "PL",
    "publisher": "Główny Urząd Geodezji i Kartografii (GUGiK), distributed by OpenAddresses",
    "register_url": "https://www.geoportal.gov.pl/pl/dane/panstwowy-rejestr-granic-prg/",
    "licence": "Polish Law on Geodesy and Cartography of 17 May 1989 (free for any use per GUGiK)",
    "licence_url": "https://www.geoportal.gov.pl/pl/dane/panstwowy-rejestr-granic-prg/#pobieranie-danych-prg",
    "attribution": "GUGiK, Państwowy Rejestr Granic (PRG). Processed by OpenAddresses and open-geocode.",
    "urls": [JOB_URL.format(job) for job in JOBS],
    "notes": (
        "The 16 OpenAddresses region jobs retain the PRG address ID and reproject points to WGS84. "
        "The fixture requires a street, so locality-only numbering is outside this sample. "
        "The same regional sources are available to the Pack builder. An uncached rebuild requires OA_TOKEN."
    ),
}


def download(cache):
    paths = [cache / f"{job}.geojson.gz" for job in JOBS]
    token = os.environ.get("OA_TOKEN", "").strip()
    if any(not path.is_file() for path in paths) and not token:
        raise SystemExit("downloading PRG jobs from OpenAddresses requires OA_TOKEN")
    files = [fetch(url, path, bearer=token)[0] for url, path in zip(SOURCE["urls"], paths, strict=True)]
    return files, RELEASE


def query(files):
    paths = [path.as_posix() for path in files]
    return f"""
        select properties.id as id, geometry.coordinates[1] as x, geometry.coordinates[2] as y,
               'EPSG:4326' as crs, properties.number as house_number, properties.street as road,
               properties.postcode as postcode, properties.city as city, properties.region as state
        from read_ndjson({paths}, columns={{
            properties: 'STRUCT(id VARCHAR, number VARCHAR, street VARCHAR, postcode VARCHAR, city VARCHAR, region VARCHAR)',
            geometry: 'STRUCT(coordinates DOUBLE[])'
        }})
    """
