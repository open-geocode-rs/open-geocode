from . import fetch, paths, unzip

SOURCE = {
    "register": "Amtliches Verzeichnis der Gebäudeadressen",
    "country": "CH",
    "publisher": "Federal Office of Topography swisstopo",
    "licence": "swisstopo terms of use for free geodata and geoservices (open use, commercial use allowed, source must be credited)",
    "attribution": "Bundesamt für Landestopografie swisstopo",
    "url": "https://data.geo.admin.ch/ch.swisstopo.amtliches-gebaeudeadressverzeichnis/amtliches-gebaeudeadressverzeichnis_ch/amtliches-gebaeudeadressverzeichnis_ch_2056.csv.zip",
    "notes": "Swiss file only, so no Liechtenstein addresses; planned (projektiert) addresses dropped, real ones kept whether or not ADR_OFFICIAL; id ADR_EGAID; house_number ADR_NUMBER as written (GWR decimals like 31.1 included); road STN_LABEL; ZIP_LABEL split into postcode and city (the postal locality); state is the canton code COM_CANTON; LV95 EPSG:2056 transformed to EPSG:4326 with pyproj",
}


def download(cache):
    path, release = fetch(SOURCE["url"], cache / SOURCE["url"].rsplit("/", 1)[1])
    unzip(path, cache / "csv", keep=lambda name: name.endswith(".csv"))
    return [path], release


def query(files):
    found = paths(files[0].parent / "csv", "*.csv")
    return f"""
        select ADR_EGAID as id, ADR_EASTING as x, ADR_NORTHING as y, 'EPSG:2056' as crs,
               ADR_NUMBER as house_number, STN_LABEL as road,
               split_part(ZIP_LABEL, ' ', 1) as postcode, regexp_extract(ZIP_LABEL, '^[^ ]+ (.*)$', 1) as city,
               COM_CANTON as state
        from read_csv({found}, delim=';', quote='"', header=true, all_varchar=true)
        where ADR_STATUS = 'real'
    """
