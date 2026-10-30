from . import fetch

SOURCE = {
    "register": "NZ Addresses",
    "country": "NZ",
    "publisher": "Toitū Te Whenua Land Information New Zealand (LINZ)",
    "licence": "CC BY 4.0",
    "attribution": "Contains data sourced from the LINZ Data Service licensed for reuse under CC BY 4.0",
    "url": "https://hub.arcgis.com/api/v3/datasets/3632c8130f034ff8bbfb122a50533550_0/downloads/data?format=csv&spatialRefId=2193&where=1%3D1",
    "notes": "LDS layer 123113 as LINZ's own public ArcGIS Online feature service (LINZ_NZ_Addresses) exports it, the keyless bulk route, since LDS exports need an account; NZTM (EPSG:2193) reprojected to WGS84 with pyproj; house_number is full_address_number, road full_road_name, suburb suburb_locality, city town_city; unit_value rows are units; Current lifecycle only, so Proposed addresses dropped; no postcode or region in the register",
}


def download(cache):
    path, release = fetch(SOURCE["url"], cache / "LINZ_NZ_Addresses.csv")
    return [path], release


def query(files):
    return f"""
        select address_id as id, X as x, Y as y, 'EPSG:2193' as crs, nullif(unit, '') is not null as unit,
               full_address_number as house_number, full_road_name as road,
               suburb_locality as suburb, town_city as city
        from read_csv('{files[0].as_posix()}', quote='"', header=true, all_varchar=true)
        where address_lifecycle = 'Current'
    """
