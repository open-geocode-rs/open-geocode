from . import fetch, unzip

CSV = "pt_addresses.csv"
RELEASE = "2026-02-23 (OpenAddresses INE extract)"
SOURCE = {
    "register": "Base Nacional de Moradas (INSPIRE Addresses)",
    "country": "PT",
    "publisher": "Instituto Nacional de Estatística, I.P. (Statistics Portugal), distributed by OpenAddresses",
    "register_url": "https://inspire.ine.pt/AD/atom/downloadservice.html",
    "licence": "CC BY 4.0",
    "licence_url": "https://inspire-geoportal.ec.europa.eu/srv/eng/catalog.search#/datasetdetails?country=pt&view=themeOverview&theme=none&resourceId=1b772914-e3e9-4663-8557-a6aef5aa596a",
    "attribution": "Instituto Nacional de Estatística, I.P., Base Nacional de Moradas, CC BY 4.0. Processed by OpenAddresses and open-geocode.",
    "url": "https://data.openaddresses.io/cache/uploads/jeffdefacto/2026-02-23-921i3/pt_addresses.csv.zip",
    "notes": (
        "OpenAddresses CSV extract retains the INSPIRE address ID and WGS84 point. "
        "Unit or floor rows are excluded. The extract has no state field. "
        "The Pack source openaddresses:pt/countrywide uses the same dataset."
    ),
}


def download(cache):
    archive, _ = fetch(SOURCE["url"], cache / f"{CSV}.zip")
    unzip(archive, cache / "csv", keep=lambda name: name == CSV)
    return [archive], RELEASE


def query(files):
    csv = files[0].parent / "csv" / CSV
    return f"""
        select id, lon as x, lat as y, 'EPSG:4326' as crs,
               nullif(trim(unit), '') is not null or nullif(trim(floor), '') is not null as unit,
               house as house_number, street as road, postcode, city
        from read_csv('{csv.as_posix()}', delim=',', quote='"', escape='"', header=true, all_varchar=true)
    """
