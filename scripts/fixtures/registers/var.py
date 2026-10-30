from . import fetch

BASE = "https://data.gov.lv/dati/dataset/6b06a7e8-dedf-4705-a47b-2a7c51177473/resource/"
TABLES = {
    "aw_eka": "a510737a-18ce-400f-ad4b-04fce5228272",
    "aw_iela": "3c4ab802-76cf-433c-9c1c-89215e28d833",
    "aw_pilseta": "ee02baa4-2bc3-4f77-a6cb-5427a3e9befe",
    "aw_ciems": "0d3810f4-1ac0-4fba-8b10-0188084a361b",
}
SOURCE = {
    "register": "Valsts adrešu reģistrs (State Address Register)",
    "country": "LV",
    "publisher": "Valsts zemes dienests (State Land Service)",
    "licence": "CC BY 4.0",
    "attribution": "Valsts zemes dienests, Valsts adrešu reģistra atvērtie dati",
    "url": f"{BASE}{TABLES['aw_eka']}/download/aw_eka.csv",
    "notes": "building and land-parcel addresses with status EKS only (deleted DEL and erroneous ERR dropped); house_number is the address object's name, road its street, city the pilsēta or village the ciems the street belongs to; addresses directly under a pagasts or ciems with no street have no road; points are the register's own DD_N/DD_E degrees (its LKS-2020 centroid already in geographic coordinates), unchanged",
}


def download(cache):
    files = []
    release = ""
    for name, resource in TABLES.items():
        path, stamp = fetch(f"{BASE}{resource}/download/{name}.csv", cache / f"{name}.csv")
        files.append(path)
        release = max(release, stamp)
    return files, release


def query(files):
    table = {path.stem: f"read_csv('{path.as_posix()}', quote='\"', header=true, all_varchar=true)" for path in files}
    return f"""
        select e.KODS as id, e.DD_E as x, e.DD_N as y, 'EPSG:4326' as crs,
               e.NOSAUKUMS as house_number, i.NOSAUKUMS as road, e.ATRIB as postcode,
               case when i.VKUR_TIPS = '104' then p.NOSAUKUMS end as city,
               case when i.VKUR_TIPS = '106' then c.NOSAUKUMS end as village
        from {table['aw_eka']} e
        left join {table['aw_iela']} i on e.VKUR_TIPS = '107' and i.KODS = e.VKUR_CD
        left join {table['aw_pilseta']} p on p.KODS = i.VKUR_CD
        left join {table['aw_ciems']} c on c.KODS = i.VKUR_CD
        where e.STATUSS = 'EKS'
    """
