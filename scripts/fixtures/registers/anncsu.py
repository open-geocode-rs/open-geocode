from . import fetch, unzip

ARCHIVE = "INDIR_ITA_20260703_GEO.csv.zip"
CSV = "INDIR_ITA_20260703_GEO_final.csv"
RELEASE = "2026-07-03 (ANNCSU national address release)"
SOURCE = {
    "register": "Archivio Nazionale dei Numeri Civici delle Strade Urbane (ANNCSU)",
    "country": "IT",
    "publisher": "Istat and Agenzia delle Entrate, distributed by OpenAddresses",
    "register_url": "https://www.anncsu.gov.it/it/consultazione-dellarchivio/open-data/",
    "licence": "CC BY 4.0",
    "licence_url": "https://www.istat.it/wp-content/uploads/2022/05/Specifiche-tecniche-anccsu-2024.pdf",
    "attribution": "ANNCSU, Istat and Agenzia delle Entrate, CC BY 4.0. Processed by OpenAddresses and open-geocode.",
    "url": "https://data.openaddresses.io/cache/uploads/jeffdefacto/2026-07-30-o5ss4/" + ARCHIVE,
    "notes": (
        "The national entrance ID is PROGRESSIVO_ACCESSO. "
        "Civic numbers retain their suffix and local qualifier, with METRICO used where the municipality numbers by distance. "
        "Coordinates are RDN2008 (EPSG:6706). Only the reported survey and territorial-data methods (1–4) are eligible. "
        "The source has no postcode. The Pack source openaddresses:it/countrywide uses the same dataset."
    ),
}


def download(cache):
    archive, _ = fetch(SOURCE["url"], cache / ARCHIVE)
    unzip(archive, cache / "csv", keep=lambda name: name == CSV)
    return [archive], RELEASE


def query(files):
    csv = files[0].parent / "csv" / CSV
    return f"""
        select PROGRESSIVO_ACCESSO as id, COORD_X_COMUNE as x, COORD_Y_COMUNE as y,
               'EPSG:6706' as crs, METODO not in ('1', '2', '3', '4') or METODO is null as estimated,
               concat_ws(' ', coalesce(CIVICO, METRICO) || coalesce('/' || ESPONENTE, ''), SPECIFICITA) as house_number,
               ODONIMO as road, COMUNE as city, PROVINCIA as state
        from read_csv('{csv.as_posix()}', delim=',', quote='"', escape='"', header=true, all_varchar=true)
    """
