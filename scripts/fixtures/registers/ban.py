from . import fetch

SOURCE = {
    "register": "Base Adresse Nationale",
    "country": "FR",
    "publisher": "IGN, DINUM and ANCT",
    "licence": "Licence Ouverte 2.0",
    "attribution": "Base Adresse Nationale, adresse.data.gouv.fr",
    "url": "https://adresse.data.gouv.fr/data/ban/adresses/latest/csv/adresses-france.csv.gz",
}


def download(cache):
    path, release = fetch(SOURCE["url"], cache / "adresses-france.csv.gz")
    return [path], release


def query(files):
    return f"""
        select id, lon as x, lat as y, 'EPSG:4326' as crs,
               concat_ws(' ', numero, nullif(rep, '')) as house_number,
               nom_voie as road, code_postal as postcode, nom_commune as city
        from read_csv('{files[0].as_posix()}', delim=';', quote='"', escape='"', header=true, all_varchar=true)
    """
