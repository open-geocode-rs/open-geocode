from . import fetch, unzip

ARCHIVE = "jp_abr_residential.csv.zip"
CSV = "jp_abr_residential.csv"
RELEASE = "2026-10-01 (OpenAddresses residential extract)"
SOURCE = {
    "register": "アドレス・ベース・レジストリ (Address Base Registry)",
    "country": "JP",
    "publisher": "Digital Agency, Government of Japan (デジタル庁), distributed by OpenAddresses",
    "register_url": "https://www.digital.go.jp/policies/base_registry_address",
    "licence": "PDL 1.0 (CC BY 4.0 compatible)",
    "licence_url": "https://www.digital.go.jp/policies/base_registry_address_tos",
    "attribution": "デジタル庁「アドレス・ベース・レジストリ」をOpenAddressesおよびopen-geocodeが加工",
    "url": "https://data.openaddresses.io/cache/uploads/jeffdefacto/2026-10-01-tkes5/" + ARCHIVE,
    "notes": (
        "The OpenAddresses extract covers residential numbering (住居表示). "
        "Truth is the registry's representative point for the residence number, in JGD2011 (EPSG:6668). "
        "The town and chōme (street in the extract) and optional koaza stand in for road. "
        "Number holds the block and residence numbers. Postcodes are from the town master. "
        "The pack source openaddresses:jp/countrywide imports the same extract."
    ),
}


def download(cache):
    archive, _ = fetch(SOURCE["url"], cache / ARCHIVE)
    unzip(archive, cache / "csv", keep=lambda name: name == CSV)
    return [archive], RELEASE


def query(files):
    csv = files[0].parent / "csv" / CSV
    return f"""
        select id, lon as x, lat as y, 'EPSG:6668' as crs,
               number as house_number, concat(street, koaza) as road,
               regexp_replace(postcode, '^([0-9]{{3}})([0-9]{{4}})$', '\\1-\\2') as postcode,
               city, pref as state
        from read_csv('{csv.as_posix()}', header=true, all_varchar=true)
    """
