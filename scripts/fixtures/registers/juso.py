import zipfile
from urllib.parse import urlencode

from . import fetch

DOWNLOAD = "https://business.juso.go.kr/api/jst/download?"
# The September 2026 full monthly releases, as business.juso.go.kr's own
# download page requests them; no account or key is needed.
ADDRESSES = DOWNLOAD + urlencode({
    "rtlDtaDtlSn": "8", "regYmd": "2026", "reqType": "ALLMTCHG", "ctprvnCd": "00", "stdde": "202609",
    "fileName": "202609_주소DB_전체분.zip", "realFileName": "202609ALLMTCHG00.zip", "intFileNo": "0", "intNum": "0",
})
ENTRANCES = DOWNLOAD + urlencode({
    "rtlDtaDtlSn": "27", "regYmd": "2026", "reqType": "GEODATA", "ctprvnCd": "00", "stdde": "202609",
    "fileName": "202609_위치정보요약DB_전체분.zip", "realFileName": "ENTRC_DB_2609.zip", "intFileNo": "149987", "intNum": "125",
})
SOURCE = {
    "register": "도로명주소 주소DB and 위치정보요약DB",
    "country": "KR",
    "publisher": "Ministry of the Interior and Safety (행정안전부), business.juso.go.kr",
    "licence": "KOGL Type 1 (공공누리 제1유형: 출처표시)",
    "attribution": "출처: 행정안전부 도로명주소 주소DB, 위치정보요약DB (business.juso.go.kr)",
    "url": ADDRESSES,
    "urls": [ADDRESSES, ENTRANCES],
    "notes": "every road-name address in 주소DB with its 도로명주소관리번호, joined on the 위치정보요약DB key (도로명코드, 지하여부, 건물본번, 건물부번, 법정동코드) to its main-entrance point; GRS80 UTM-K (EPSG:5179) reprojected to WGS84 with pyproj; addresses on a road the register marks unused (사용여부 1) get no road; no point where the register withholds it (비공개 buildings); house_number is 건물본번[-건물부번] with 지하 before it for underground addresses; road 도로명, city 시군구명, suburb only for 읍 and 면 since the official road-name address names them and not 동, state 시도명, postcode 기초구역번호; Sejong has no 시군구, so 세종특별자치시 is its city and no state is given",
}
RELEASE = "2026-09-30 (September 2026 full release)"


def download(cache):
    addresses, _ = fetch(ADDRESSES, cache / "202609ALLMTCHG00.zip")
    entrances, _ = fetch(ENTRANCES, cache / "ENTRC_DB_2609.zip")
    text = cache / "utf8"
    if not text.is_dir():
        staging = cache / "utf8.part"
        staging.mkdir(exist_ok=True)
        for archive in (addresses, entrances):
            with zipfile.ZipFile(archive) as zipped:
                for info in zipped.infolist():
                    try:
                        # The address archive stores its Korean names in CP949 with
                        # neither the UTF-8 flag nor a Unicode path field.
                        name = info.filename.encode("cp437").decode("cp949")
                    except UnicodeError:
                        name = info.filename
                    if name.startswith("주소_"):
                        out = f"address_{info.CRC:08x}.txt"
                    elif name.startswith("개선_도로명코드"):
                        out = "road.txt"
                    elif name.startswith("entrc_"):
                        out = name
                    else:
                        continue
                    (staging / out).write_bytes(zipped.read(info).decode("cp949").encode())
        staging.replace(text)
    return [addresses, entrances], RELEASE


def query(files):
    text = files[0].parent / "utf8"

    def table(glob, columns):
        names = ", ".join(f"'{name}': 'VARCHAR'" for name in columns)
        return f"read_csv('{(text / glob).as_posix()}', delim='|', header=false, quote='', escape='', columns={{{names}}})"

    address = table("address_*.txt", ["id", "road_code", "emd_seq", "underground", "main", "sub", "postcode",
                                       "reason", "notice", "previous", "detail"])
    road = table("road.txt", ["road_code", "road", "road_roman", "emd_seq", "sido", "sido_roman", "sigungu",
                              "sigungu_roman", "emd", "emd_roman", "emd_kind", "emd_code", "unused", "change",
                              "history", "noticed", "retired"])
    entrance = table("entrc_*.txt", ["sigungu_code", "entrance", "bjd_code", "sido", "sigungu", "emd", "road_code",
                                     "road", "underground", "main", "sub", "building", "postcode", "use", "group",
                                     "admin_dong", "x", "y"])
    return f"""
        select a.id, e.x, e.y, 'EPSG:5179' as crs,
               concat(case when a.underground = '1' then '지하 ' end, cast(a.main as int),
                      case when cast(coalesce(nullif(a.sub, ''), '0') as int) <> 0 then '-' || cast(a.sub as int) end)
                   as house_number,
               r.road, a.postcode,
               coalesce(nullif(r.sigungu, ''), r.sido) as city,
               case when nullif(r.sigungu, '') is not null then r.sido end as state,
               case when r.emd_kind = '0' then r.emd end as suburb
        from {address} a
        left join {road} r on r.road_code = a.road_code and r.emd_seq = a.emd_seq and r.unused = '0'
        left join {entrance} e
          on e.road_code = a.road_code and e.underground = a.underground
         and cast(e.main as int) = cast(a.main as int) and cast(e.sub as int) = cast(a.sub as int)
         and e.bjd_code = left(a.road_code, 5) || r.emd_code || '00'
    """
