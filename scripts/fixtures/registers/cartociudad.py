from . import derived_path, fetch, unzip

CSV = "es_addresses.csv"
RELEASE = "2026-11-11 (OpenAddresses CartoCiudad extract)"
SOURCE = {
    "register": "CartoCiudad",
    "country": "ES",
    "publisher": "Instituto Geográfico Nacional and Centro Nacional de Información Geográfica",
    "register_url": "https://www.cartociudad.es/web/portal/directorio-de-servicios/descarga",
    "licence": "CC BY 4.0",
    "licence_url": "https://www.cartociudad.es/web/portal/faq",
    "attribution": "CartoCiudad (IGN-CNIG), CC BY 4.0, www.scne.es/productos.html#CartoCiudad. Processed by OpenAddresses and open-geocode.",
    "url": "https://data.openaddresses.io/cache/uploads/jeffdefacto/2026-11-11-5r8yr/es_addresses.csv.zip",
    "notes": (
        "OpenAddresses merged 52 provincial CartoCiudad packages into a CSV. "
        "The fixture uses address points (Portal) and their original id_porpk, excluding kilometre markers (PK). "
        "Extension is part of the portal number. State is the autonomous community. "
        "The Pack source openaddresses:es/countrywide uses the same dataset."
    ),
}


def download(cache):
    archive, _ = fetch(SOURCE["url"], cache / f"{CSV}.zip")
    return [archive], RELEASE


def query(files):
    archive = files[0]
    csv = unzip(archive, derived_path(archive, "csv"), keep=lambda name: name == CSV) / CSV
    csv = csv.as_posix().replace("'", "''")
    return f"""
        select id_porpk as id, X as x, Y as y, 'EPSG:4258' as crs,
               nullif(numero, '') || coalesce(trim(extension), '') as house_number,
               concat_ws(' ', tipo_vial, nombre_via) as road, cod_postal as postcode,
               municipio as city, comunidad_autonoma as state
        from read_csv('{csv}', delim=',', quote='"', escape='"', header=true, all_varchar=true)
        where lower(tipo) = 'portal'
    """
