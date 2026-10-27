from . import fetch

SOURCE = {
    "register": "BD-Adresses (geocoded addresses of the Registre national des localités et des rues, CACLR)",
    "country": "LU",
    "publisher": "Administration du cadastre et de la topographie (ACT)",
    "licence": "CC0 1.0",
    "attribution": "Administration du cadastre et de la topographie, BD-Adresses, data.public.lu",
    "url": "https://data.public.lu/fr/datasets/r/5cadc5b8-6a7d-4283-87bc-f9e58dd771f7",
    "notes": "every row kept, the register has no status or placement flag; id is id_caclr_bat, the CACLR address id; point is the register's own WGS84 columns (they agree with its LUREF EPSG:2169 columns through pyproj to 1e-7 degrees); localite is the city, postcode as written without the L- prefix",
}


def download(cache):
    path, release = fetch(SOURCE["url"], cache / "addresses.csv")
    return [path], release


def query(files):
    return f"""
        select id_caclr_bat as id, lon_wgs84 as x, lat_wgs84 as y, 'EPSG:4326' as crs,
               numero as house_number, rue as road, code_postal as postcode, localite as city
        from read_csv('{files[0].as_posix()}', delim=';', quote='"', header=true, all_varchar=true)
    """
