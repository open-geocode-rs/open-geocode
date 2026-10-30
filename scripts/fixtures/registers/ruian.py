from . import fetch, paths, unzip

RELEASE = "20260930"
SOURCE = {
    "register": "RÚIAN address places",
    "country": "CZ",
    "publisher": "Český úřad zeměměřický a katastrální (ČÚZK)",
    "licence": "CC BY 4.0",
    "attribution": "ČÚZK, 2026",
    "url": f"https://vdp.cuzk.gov.cz/vymenny_format/csv/{RELEASE}_OB_ADR_csv.zip",
    "notes": "current address places only (the CSV holds no historic ones); house_number is číslo popisné/číslo orientační with its letter as Czech post writes it (e.g. 2/3a), the bare číslo popisné when there is no orientační, 'č. ev. N' for evidenční numbers; city is the obec; points from S-JTSK Krovak (EPSG:5514, CSV gives positive Y and X, negated) to WGS84 with pyproj",
}


def download(cache):
    path, _ = fetch(SOURCE["url"], cache / SOURCE["url"].rsplit("/", 1)[1])
    unzip(path, cache / "csv", keep=lambda name: name.endswith(".csv"), encoding="cp1250")
    return [path], RELEASE


def query(files):
    found = paths(files[0].parent / "csv", "*.csv")
    return f"""
        select "Kód ADM" as id, -try_cast("Souřadnice Y" as double) as x, -try_cast("Souřadnice X" as double) as y,
               'EPSG:5514' as crs,
               concat(case when "Typ SO" = 'č.ev.' then 'č. ev. ' end, "Číslo domovní",
                      case when nullif("Číslo orientační", '') is not null
                           then '/' || "Číslo orientační" || coalesce("Znak čísla orientačního", '') end) as house_number,
               "Název ulice" as road, "PSČ" as postcode, "Název obce" as city
        from read_csv({found}, delim=';', quote='"', header=true, all_varchar=true)
    """
