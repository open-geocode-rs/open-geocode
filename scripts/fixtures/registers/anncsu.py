import csv
import io
import zipfile

from pyproj import CRS, Transformer

from . import derived_path, fetch, unzip

ARCHIVE = "INDIR_ITA_20260703_GEO.csv.zip"
CSV = "INDIR_ITA_20260703_GEO_final.csv"
BOUNDARIES = "Limiti01012026_g.zip"
BOUNDARY_URL = (
    "https://www.istat.it/storage/cartografia/confini_amministrativi/generalizzati/2026/" + BOUNDARIES
)
MUNICIPALITIES = "Com01012026_g/Com01012026_g_WGS84"
RELEASE = "2026-07-03 (ANNCSU national address release)"
SOURCE = {
    "register": "Archivio Nazionale dei Numeri Civici delle Strade Urbane (ANNCSU)",
    "country": "IT",
    "publisher": "Istat and Agenzia delle Entrate, distributed by OpenAddresses",
    "register_url": "https://www.anncsu.gov.it/it/consultazione-dellarchivio/open-data/",
    "licence": "CC BY 4.0",
    "licence_url": "https://www.istat.it/wp-content/uploads/2022/05/Specifiche-tecniche-anccsu-2024.pdf",
    "boundary_licence_url": "https://www.istat.it/note-legali/",
    "attribution": "ANNCSU, Istat and Agenzia delle Entrate, CC BY 4.0. Processed by OpenAddresses and open-geocode.",
    "url": "https://data.openaddresses.io/cache/uploads/jeffdefacto/2026-07-30-o5ss4/" + ARCHIVE,
    "boundary_url": BOUNDARY_URL,
    "notes": (
        "The national entrance ID is PROGRESSIVO_ACCESSO. "
        "Civic numbers retain their suffix and local qualifier, with METRICO used where the municipality numbers by distance. "
        "Coordinates are RDN2008 (EPSG:6706). Only reported survey and territorial-data methods (1–4) "
        "within the municipality's bounding box from Istat's 2026 statistical boundaries are eligible. "
        "The source has no postcode. The Pack source openaddresses:it/countrywide uses the same dataset."
    ),
}


def download(cache):
    archive, _ = fetch(SOURCE["url"], cache / ARCHIVE)
    boundaries, _ = fetch(BOUNDARY_URL, cache / BOUNDARIES)
    return [archive, boundaries], RELEASE


def municipality_bounds(archive, output):
    if output.is_file():
        return

    import shapefile

    partial = output.with_suffix(".part")
    with zipfile.ZipFile(archive) as zipped, partial.open("w", newline="") as handle:
        crs = CRS.from_wkt(zipped.read(MUNICIPALITIES + ".prj").decode())
        transformer = Transformer.from_crs(crs, "EPSG:6706", always_xy=True)
        writer = csv.writer(handle)
        writer.writerow(("code", "west", "south", "east", "north"))
        with shapefile.Reader(
            shp=io.BytesIO(zipped.read(MUNICIPALITIES + ".shp")),
            dbf=io.BytesIO(zipped.read(MUNICIPALITIES + ".dbf")),
        ) as municipalities:
            for entry in municipalities.iterShapeRecords(fields=["PRO_COM_T"]):
                bounds = transformer.transform_bounds(*entry.shape.bbox, errcheck=True)
                writer.writerow((entry.record["PRO_COM_T"], *bounds))
    partial.replace(output)


def query(files):
    archive, boundaries = files
    addresses = unzip(archive, derived_path(archive, "csv"), keep=lambda name: name == CSV) / CSV
    bounds = derived_path(boundaries, "bounds.csv")
    municipality_bounds(boundaries, bounds)
    addresses = addresses.as_posix().replace("'", "''")
    bounds = bounds.as_posix().replace("'", "''")
    return f"""
        with civic as (
            select *, try_cast(COORD_X_COMUNE as double) as x, try_cast(COORD_Y_COMUNE as double) as y
            from read_csv('{addresses}', delim=',', quote='"', escape='"', header=true, all_varchar=true)
        )
        select PROGRESSIVO_ACCESSO as id, x, y, 'EPSG:6706' as crs,
               not coalesce(METODO in ('1', '2', '3', '4') and
                            x between west and east and y between south and north, false) as estimated,
               concat_ws(' ', coalesce(CIVICO, METRICO) || coalesce('/' || ESPONENTE, ''), SPECIFICITA) as house_number,
               ODONIMO as road, COMUNE as city, PROVINCIA as state
        from civic left join read_csv('{bounds}', types={{'code': 'VARCHAR'}}) on CODICE_ISTAT = code
    """
