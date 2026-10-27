import csv
import io
import re
import shutil
import xml.etree.ElementTree as ET
import zipfile

from . import fetch, paths, to_parquet, unzip

BASE = "https://ftp.ibge.gov.br/Cadastro_Nacional_de_Enderecos_para_Fins_Estatisticos/Censo_Demografico_2022/Arquivos_CNEFE/CSV/UF/"
STATES = (
    "11_RO", "12_AC", "13_AM", "14_RR", "15_PA", "16_AP", "17_TO", "21_MA", "22_PI", "23_CE", "24_RN", "25_PB",
    "26_PE", "27_AL", "28_SE", "29_BA", "31_MG", "32_ES", "33_RJ", "35_SP", "41_PR", "42_SC", "43_RS", "50_MS",
    "51_MT", "52_GO", "53_DF",
)
# CNEFE carries only the municipality code; IBGE's territorial division of
# the census year names it.
DTB = "https://geoftp.ibge.gov.br/organizacao_do_territorio/estrutura_territorial/divisao_territorial/2022/DTB_2022.zip"

SOURCE = {
    "register": "Cadastro Nacional de Enderecos para Fins Estatisticos (CNEFE), Censo Demografico 2022",
    "country": "BR",
    "publisher": "Instituto Brasileiro de Geografia e Estatistica (IBGE)",
    "licence": "Public domain on condition the source is cited (IBGE), Brazilian federal open data (Decreto 8.777/2016)",
    "attribution": "Fonte: IBGE, Cadastro Nacional de Enderecos para Fins Estatisticos, Censo Demografico 2022",
    "url": BASE,
    "notes": (
        "all 27 UF files; SIRGAS 2000 (EPSG:4674, per Nota metodologica n. 01) to WGS84 by PROJ; "
        "NV_GEO_COORD 4 (block face midpoint), 5 (localidade) and 6 (census tract centroid) marked estimated, "
        "1 to 3 kept as Nota metodologica n. 01 classes them as geocoded at the address; any complemento "
        "element marks a unit; DSC_MODIFICADOR SN means no number (so 97% of DF, which IBGE records as SN), "
        "KM gives 'KM <n>', any other modifier follows the number as IBGE says it complements it; road is "
        "tipo, titulo and nome of the logradouro, without IBGE's pseudo-tipo EDF (Endereco do Distrito Federal); "
        "suburb is DSC_LOCALIDADE, city is the municipality name from IBGE DTB 2022 by COD_MUNICIPIO, state is "
        "the UF code of the file name; rows of one COD_UNICO_ENDERECO listed once per especie with identical "
        "address fields are one address; CNEFE's capitals title-cased, Portuguese particles lower-cased"
    ),
}
# NV_GEO_COORD levels that are not the address's own point.
ESTIMATED = ("4", "5", "6")
PARTICLES = {"a", "o", "as", "os", "da", "de", "do", "das", "dos", "e", "em", "na", "no", "nas", "nos"}
UF = {code.split("_")[1] for code in STATES} | {"BR"}
ROMAN = {"II", "III", "IV", "VI", "VII", "VIII", "IX", "XI", "XII", "XIII", "XIV", "XV", "XX", "XXI", "XXIII"}
COLUMNS = (
    "COD_UNICO_ENDERECO", "COD_MUNICIPIO", "CEP", "DSC_LOCALIDADE", "NOM_TIPO_SEGLOGR", "NOM_TITULO_SEGLOGR",
    "NOM_SEGLOGR", "NUM_ENDERECO", "DSC_MODIFICADOR", "LATITUDE", "LONGITUDE", "NV_GEO_COORD",
    *(f"{kind}_COMP_ELEM{i}" for i in range(1, 6) for kind in ("NOM", "VAL")),
)


def download(cache):
    files = []
    dates = []
    for state in STATES:
        path, date = fetch(f"{BASE}{state}.zip", cache / f"{state}.zip")
        files.append(path)
        dates.append(date)
    dtb, _ = fetch(DTB, cache / "DTB_2022.zip")
    files.append(dtb)
    parquet = cache / "cnefe.parquet"
    if not parquet.is_file():
        # 18 GB of CSV once, so only the columns the query reads stay.
        text = cache / "csv"
        for path in files[:-1]:
            unzip(path, text / path.stem)
        found = paths(text, "*/*.csv")
        columns = ", ".join(COLUMNS)
        to_parquet(
            f"select {columns}, regexp_extract(filename, '[0-9]+_([A-Z]{{2}})', 1) as UF "
            f"from read_csv({found}, delim=';', quote='\"', header=true, all_varchar=true, filename=true)",
            parquet,
        )
        shutil.rmtree(text)
    names = cache / "municipios.csv"
    if not names.is_file():
        with open(names, "w", encoding="utf-8", newline="") as handle:
            writer = csv.writer(handle)
            writer.writerow(("code", "name"))
            writer.writerows(municipalities(dtb).items())
    return files, f"Censo Demografico 2022, files of {max(dates)}"


def municipalities(path):
    table = "{urn:oasis:names:tc:opendocument:xmlns:table:1.0}"
    member = "RELATORIO_DTB_BRASIL_DISTRITO.ods"
    with zipfile.ZipFile(path) as dtb, zipfile.ZipFile(io.BytesIO(dtb.read(member))) as ods:
        root = ET.fromstring(ods.read("content.xml"))
    names = {}
    header = None
    for row in root.iter(f"{table}table-row"):
        cells = []
        for cell in row.findall(f"{table}table-cell"):
            # Trailing empty cells are stored as one cell repeated to the
            # sheet's edge.
            repeat = min(int(cell.get(f"{table}number-columns-repeated", "1")), 32)
            cells += ["".join(cell.itertext())] * repeat
        if header is None:
            if cells and cells[0] == "UF":
                header = cells
                code = header.index("Código Município Completo")
                name = header.index("Nome_Município")
            continue
        if len(cells) > name and cells[code]:
            names[cells[code]] = cells[name]
    return names


def title(text):
    if not text:
        return text
    words = re.sub(r"\b([^\W\d_])", lambda m: m[1].upper(), text.lower()).split()
    out = []
    for i, word in enumerate(words):
        upper = word.upper()
        following = words[i + 1] if i + 1 < len(words) else ""
        if upper in ROMAN or (upper in UF and following[:1].isdigit()):
            out.append(upper)
        elif i and word.lower() in PARTICLES:
            out.append(word.lower())
        else:
            out.append(word)
    return " ".join(out)


def query(files):
    cache = files[0].parent
    unit = " or ".join(f"nullif({column}, '') is not null" for column in COLUMNS if "_COMP_ELEM" in column)
    return f"""
        select c.COD_UNICO_ENDERECO as id, c.LONGITUDE as x, c.LATITUDE as y, 'EPSG:4674' as crs,
               ({unit}) as unit, c.NV_GEO_COORD in {ESTIMATED} as estimated,
               case c.DSC_MODIFICADOR when 'SN' then null
                                      when 'KM' then 'KM ' || c.NUM_ENDERECO
                                      else concat_ws(' ', c.NUM_ENDERECO, nullif(c.DSC_MODIFICADOR, '')) end
                   as house_number,
               -- EDF is IBGE's placeholder tipo for Brasilia's sector addresses, not a word anyone writes.
               concat_ws(' ', nullif(nullif(c.NOM_TIPO_SEGLOGR, ''), 'EDF'), nullif(c.NOM_TITULO_SEGLOGR, ''),
                         nullif(c.NOM_SEGLOGR, '')) as road,
               c.DSC_LOCALIDADE as suburb, m.name as city, c.CEP as postcode, c.UF as state
        from read_parquet('{(cache / "cnefe.parquet").as_posix()}') c
        left join read_csv('{(cache / "municipios.csv").as_posix()}', header=true, all_varchar=true) m
          on m.code = c.COD_MUNICIPIO
    """


def finish(row):
    row["road"] = title(row["road"])
    row["suburb"] = title(row["suburb"])
    number, _, modifier = row["house_number"].partition(" ")
    if modifier and number != "KM":
        row["house_number"] = f"{number} {title(modifier)}"
