import zipfile

from . import fetch

SOURCE = {
    "register": "G-NAF",
    "country": "AU",
    "publisher": "Geoscape Australia",
    "licence": "Open G-NAF End User Licence Agreement (CC BY 4.0 with a no-mailing restriction)",
    "attribution": "Incorporates or developed using G-NAF (c) Geoscape Australia licensed by the Commonwealth of Australia under the Open Geo-coded National Address File (G-NAF) End User Licence Agreement",
    "url": "https://data.gov.au/data/dataset/19432f89-dc3a-4ef3-b943-5326ef1dbecc/resource/b023544a-5836-4d43-b6d8-da2f73e8d2bf/download/g-naf_aug26_allstates_gda2020_psv_110.zip",
    "notes": "principal, unretired addresses with their default geocode, estimated geocodes (GG, STL, LOC) dropped; street suffix written as its STREET_SUFFIX_AUT word (EAST, not E); names title-cased from G-NAF's capitals; GDA2020 (EPSG:7844) to WGS84 by PROJ",
}
RELEASE = "AUG 2026, GDA2020"
# Gap-geocoded, street-locality and locality points: G-NAF's estimates.
ESTIMATED = {"GG", "STL", "LOC"}
TABLES = ("ADDRESS_DETAIL", "ADDRESS_DEFAULT_GEOCODE", "STREET_LOCALITY", "LOCALITY", "STATE")
AUTHORITY = ("STREET_SUFFIX_AUT",)


def download(cache):
    archive, _ = fetch(SOURCE["url"], cache / SOURCE["url"].rsplit("/", 1)[1])
    standard = cache / "standard"
    authority = cache / "authority"
    if not (standard.is_dir() and authority.is_dir()):
        with zipfile.ZipFile(archive) as zipped:
            for name in zipped.namelist():
                if "/Standard/" in name and any(name.endswith(f"_{table}_psv.psv") for table in TABLES):
                    out = standard
                elif "/Authority Code/" in name and any(name.endswith(f"_{table}_psv.psv") for table in AUTHORITY):
                    out = authority
                else:
                    continue
                out.mkdir(exist_ok=True)
                (out / name.rsplit("/", 1)[1]).write_bytes(zipped.read(name))
    return [archive], RELEASE


def title(text):
    return " ".join(word.capitalize() for word in (text or "").split())


def query(files):
    standard = files[0].parent / "standard"

    def table(name):
        # A state's files are <STATE>_<TABLE>_psv.psv, and LOCALITY is also
        # the tail of STREET_LOCALITY, so the prefix must be exactly a state.
        found = [path.as_posix() for path in sorted(standard.iterdir()) if path.name.split("_", 1)[1] == f"{name}_psv.psv"]
        return f"read_csv({found}, delim='|', header=true, all_varchar=true)"

    # STREET_SUFFIX_CODE is the abbreviation (E, CN), and the word (EAST,
    # CENTRAL) is in the authority table. G-NAF's authority tables disagree on
    # which column holds the word (STREET_TYPE_AUT keeps it in CODE), so the
    # longer one is taken.
    (suffixes,) = [path.as_posix() for path in (files[0].parent / "authority").glob("*_STREET_SUFFIX_AUT_psv.psv")]

    return f"""
        select d.ADDRESS_DETAIL_PID as id, g.LONGITUDE as x, g.LATITUDE as y, 'EPSG:7844' as crs,
               d.FLAT_NUMBER is not null as unit,
               g.GEOCODE_TYPE_CODE in {tuple(sorted(ESTIMATED))} as estimated,
               concat(d.NUMBER_FIRST, d.NUMBER_FIRST_SUFFIX,
                      case when d.NUMBER_FIRST is not null and d.NUMBER_LAST is not null
                           then '-' || d.NUMBER_LAST || coalesce(d.NUMBER_LAST_SUFFIX, '') end) as house_number,
               concat_ws(' ', s.STREET_NAME, s.STREET_TYPE_CODE, coalesce(sx.word, s.STREET_SUFFIX_CODE)) as road,
               l.LOCALITY_NAME as suburb, d.POSTCODE as postcode, st.STATE_NAME as state,
               st.STATE_ABBREVIATION as state_code
        from {table('ADDRESS_DETAIL')} d
        join {table('STREET_LOCALITY')} s using (STREET_LOCALITY_PID)
        join {table('LOCALITY')} l on l.LOCALITY_PID = d.LOCALITY_PID
        join {table('STATE')} st on st.STATE_PID = l.STATE_PID
        left join {table('ADDRESS_DEFAULT_GEOCODE')} g on g.ADDRESS_DETAIL_PID = d.ADDRESS_DETAIL_PID
        left join (select CODE, case when length(NAME) > length(CODE) then NAME else CODE end as word
                   from read_csv('{suffixes}', delim='|', header=true, all_varchar=true)) sx
          on sx.CODE = s.STREET_SUFFIX_CODE
        where d.ALIAS_PRINCIPAL = 'P' and d.DATE_RETIRED is null
    """


def finish(row):
    for key in ("road", "suburb", "state"):
        row[key] = title(row[key])
