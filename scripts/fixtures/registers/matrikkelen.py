from . import fetch, paths, unzip

SOURCE = {
    "register": "Matrikkelen - Adresse",
    "country": "NO",
    "publisher": "Kartverket",
    "licence": "CC BY 4.0",
    "attribution": "© Kartverket",
    "url": "https://nedlasting.geonorge.no/geonorge/Basisdata/MatrikkelenAdresse/CSV/Basisdata_0000_Norge_4258_MatrikkelenAdresse_CSV.zip",
    "notes": "national file of current official addresses; id is adresseId; point is the address point (Nord/Øst), EPSG:4258 transformed to EPSG:4326 with pyproj, access points not used; house_number is nummer and bokstav joined as the register writes them; poststed, written in capitals as Norwegian postal practice does, kept as written as city; matrikkeladresser (farm/parcel numbers) carry no street or house number and fall out on that",
}


def download(cache):
    path, release = fetch(SOURCE["url"], cache / SOURCE["url"].rsplit("/", 1)[1])
    unzip(path, cache / "csv", keep=lambda name: name.endswith(".csv"))
    return [path], release


def query(files):
    found = paths(files[0].parent / "csv", "*.csv")
    return f"""
        select adresseId as id, "Øst" as x, "Nord" as y, 'EPSG:' || "EPSG-kode" as crs,
               concat(nummer, bokstav) as house_number, adressenavn as road,
               postnummer as postcode, poststed as city
        from read_csv({found}, delim=';', quote='"', header=true, all_varchar=true)
    """
