from . import fetch

SOURCE = {
    "register": "Base Adresse Nationale",
    "country": "FR",
    "publisher": "IGN, DINUM and ANCT",
    "licence": "Licence Ouverte 2.0",
    "attribution": "Base Adresse Nationale, adresse.data.gouv.fr",
    "url": "https://adresse.data.gouv.fr/data/ban/adresses/latest/csv/adresses-france.csv.gz",
    "notes": "type_position segment (projected on the street), service technique and délivrance postale marked "
    "estimated, every other type_position kept; house_number is numero and rep; road nom_voie; postcode "
    "code_postal; city nom_commune; country is the ISO code of the overseas department or collectivity for "
    "code_insee 971 to 988, FR otherwise",
}
# Points not placed at the address: on the street line, or at a utility or
# postal delivery point.
ESTIMATED = ("segment", "service technique", "délivrance postale")
# INSEE prefixes OpenCage, OSM and most geocoders file under their own country
# codes. Saint-Barthélemy and Saint-Martin have Guadeloupe's 971 postcodes,
# so the INSEE code decides, not the postcode.
OVERSEAS = {"971": "GP", "972": "MQ", "973": "GF", "974": "RE", "975": "PM", "976": "YT", "977": "BL", "978": "MF", "986": "WF", "987": "PF", "988": "NC"}


def download(cache):
    path, release = fetch(SOURCE["url"], cache / "adresses-france.csv.gz")
    return [path], release


def query(files):
    overseas = " ".join(f"when '{prefix}' then '{code}'" for prefix, code in OVERSEAS.items())
    return f"""
        select id, lon as x, lat as y, 'EPSG:4326' as crs,
               case left(code_insee, 3) {overseas} end as country,
               type_position in {ESTIMATED} as estimated,
               concat_ws(' ', numero, nullif(rep, '')) as house_number,
               nom_voie as road, code_postal as postcode, nom_commune as city
        from read_csv('{files[0].as_posix()}', delim=';', quote='"', escape='"', header=true, all_varchar=true)
    """
