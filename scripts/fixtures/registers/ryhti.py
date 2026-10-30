import csv
from itertools import count

import duckdb

from . import fetch

SOURCE = {
    "register": "Ryhti building addresses (successor of DVV Rakennusten osoitetiedot)",
    "country": "FI",
    "publisher": "Finnish Environment Institute (Syke)",
    "licence": "CC BY 4.0",
    "attribution": "Lähde: Syke Ryhti",
    "url": "https://paikkatiedot.ymparisto.fi/geoserver/ryhti_building/wfs?service=WFS&version=2.0.0&request=GetFeature&typeNames=ryhti_building:open_address&outputFormat=csv&sortBy=address_key",
    "notes": "DVV stopped publishing its building address file on 14.3.2025 and handed the building register to Syke's Ryhti, so this is Ryhti's open_address layer (retired addresses are a separate layer, not read); id is address_key; point is the building's point, EPSG:3067 transformed to EPSG:4326 with pyproj; house_number is number, letter and the second number or letter of a range as the register joins them; road and postal office in Finnish, Swedish where the register has no Finnish; postal office written as the register writes it (capitals) as city; no unit or estimated flag exists in the register",
}
# GeoServer caps a GetFeature at 100000 features, so the layer comes in pages
# ordered by its key.
PAGE = 100_000


def download(cache):
    files = []
    for page in count():
        url = f"{SOURCE['url']}&count={PAGE}&startIndex={page * PAGE}"
        path, _ = fetch(url, cache / f"open_address_{page:03d}.csv")
        files.append(path)
        with open(path, encoding="utf-8", newline="") as handle:
            if sum(1 for _ in csv.reader(handle)) - 1 < PAGE:
                break
    # The service sends no Last-Modified; the newest edit in the layer dates it.
    paths = [path.as_posix() for path in files]
    newest = duckdb.sql(f"select max(modified_timestamp_utc) from read_csv({paths}, all_varchar=true)").fetchone()[0]
    return files, newest[:10]


def query(files):
    found = [path.as_posix() for path in files]
    point = r"'^POINT \(([-0-9.eE]+) ([-0-9.eE]+)\)$'"
    return f"""
        select address_key as id,
               regexp_extract(location_geometry_data, {point}, 1) as x,
               regexp_extract(location_geometry_data, {point}, 2) as y, 'EPSG:3067' as crs,
               concat(number_part_of_address_number, subdivision_letter_of_address_number,
                      case when coalesce(number_part_of_address_number2, subdivision_letter_of_address_number2) is not null
                           then '-' || concat(number_part_of_address_number2, subdivision_letter_of_address_number2) end)
                   as house_number,
               coalesce(nullif(address_name_fin, ''), address_name_swe) as road, postal_code as postcode,
               coalesce(nullif(postal_office_fin, ''), postal_office_swe) as city
        from read_csv({found}, quote='"', header=true, all_varchar=true)
    """
