from . import fetch, paths, unzip

BASE = "https://aks.geoportaal.ee/aks-open-api/public/valjav6te/"
RELEASE = "20261021"
# Extract 9 holds every address with its components and point; extract 17
# is the only one that carries postcodes.
EXTRACTS = (f"vv9_{RELEASE}_010609.zip", f"vv17_{RELEASE}_004636.zip")
SOURCE = {
    "register": "Aadressiandmete süsteem (ADS)",
    "country": "EE",
    "publisher": "Maa- ja Ruumiamet (Estonian Land and Spatial Development Board)",
    "licence": "Estonian open data, no licence agreement (CC0 1.0 per INSPIRE metadata)",
    "attribution": "Estonian address data: Maa- ja Ruumiamet (Estonian Land and Spatial Development Board)",
    "url": BASE + EXTRACTS[0],
    "notes": "extract 9, valid addresses only (olek K; cancelled T and obsolete V dropped), postcode joined from extract 17 by adr_id; unit is a level-8 building part; house_number is level 7, road level 5 short name; locality from level 3 as written (linn to city, alev/alevik to town, any other settlement to village), a Tallinn linnaosa becomes suburb with the level-2 city, and with no level 3 the level-2 linn is city or the vald municipality; state is the maakond; points from L-EST97 (EPSG:3301, viitepunkt_x is northing) to WGS84 with pyproj",
}


def download(cache):
    return [fetch(BASE + name, cache / name)[0] for name in EXTRACTS], RELEASE


def query(files):
    main = paths(unzip(files[0], files[0].parent / "vv9", encoding="utf-8-sig"), "*")
    codes = paths(unzip(files[1], files[1].parent / "vv17", encoding="utf-8-sig"), "*")
    settlement, municipality = "a.tase3_nimetus", "a.tase2_nimetus"
    return f"""
        select a.adr_id as id, a.viitepunkt_y as x, a.viitepunkt_x as y, 'EPSG:3301' as crs,
               nullif(a.tase8_kood, '') is not null as unit,
               a.tase7_nimetus as house_number, a.tase5_nimetus as road, p.sihtnumber as postcode,
               case when {settlement} like '%linnaosa' then {settlement} end as suburb,
               case when {settlement} like '%linnaosa' then {municipality}
                    when {settlement} like '% linn' then {settlement}
                    when coalesce({settlement}, '') = '' and {municipality} not like '% vald' then {municipality} end as city,
               case when {settlement} like '% alev' or {settlement} like '% alevik' then {settlement} end as town,
               case when coalesce({settlement}, '') <> '' and {settlement} not like '%linnaosa'
                     and {settlement} not like '% linn' and {settlement} not like '% alev'
                     and {settlement} not like '% alevik' then {settlement} end as village,
               case when coalesce({settlement}, '') = '' and {municipality} like '% vald' then {municipality} end as municipality,
               a.tase1_nimetus as state
        from read_csv({main}, delim=';', quote='"', header=true, all_varchar=true) a
        left join read_csv({codes}, delim=';', quote='"', header=true, all_varchar=true) p using (adr_id)
        where a.olek = 'K'
    """
