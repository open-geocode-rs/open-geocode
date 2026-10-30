"""What every register adapter shares.

An adapter has `SOURCE`, `download(cache) -> (files, release)` and
`query(files) -> SQL`. The SQL yields one row per register address with any
of the columns in COLUMNS; `unit` and `estimated` are booleans, `x`/`y` are in
the `crs` the row names, and `country` overrides SOURCE's country for a row
the world files under another code. `state_code` is only matched against, by
suites.py, where a suite names the region by its code; it is never formatted. An adapter may add `finish(row)`, which recases the
values of a drawn row: it runs on the draw only and must not empty a field,
since eligibility was decided in SQL.
"""

import email.utils
import io
import shutil
import urllib.request
import zipfile
from pathlib import Path

import duckdb

COLUMNS = (
    "id", "x", "y", "crs", "country", "unit", "estimated",
    "house_number", "road", "postcode", "city", "town", "village", "suburb", "hamlet", "municipality", "state",
    "state_code",
)
LOCALITY = ("city", "town", "village", "suburb", "hamlet", "municipality")


def fetch(url, path: Path):
    """Download `url` to `path` once, and the date the server says it was
    published beside it, as the release of a register that names none."""
    stamp = path.with_name(path.name + ".release")
    if not path.is_file():
        partial = path.with_name(path.name + ".part")
        request = urllib.request.Request(url, headers={"User-Agent": "open-geocode fixtures"})
        with urllib.request.urlopen(request) as response, open(partial, "wb") as handle:
            shutil.copyfileobj(response, handle, 1 << 20)
            modified = response.headers.get("Last-Modified")
        partial.replace(path)
        date = email.utils.parsedate_to_datetime(modified).date().isoformat() if modified else ""
        stamp.write_text(date)
    return path, stamp.read_text().strip() if stamp.is_file() else ""


def unzip(archive: Path, out: Path, keep=lambda name: True, encoding=None):
    """Extract the members `keep` accepts into `out` once, as UTF-8 when the
    archive holds another encoding, since duckdb reads UTF-8 only."""
    if out.is_dir():
        return out
    partial = out.with_name(out.name + ".part")
    shutil.rmtree(partial, ignore_errors=True)
    partial.mkdir(parents=True)
    with zipfile.ZipFile(archive) as zipped:
        for name in zipped.namelist():
            if name.endswith("/") or not keep(name):
                continue
            target = partial / Path(name).name
            with zipped.open(name) as source, open(target, "wb") as sink:
                if encoding:
                    text = io.TextIOWrapper(source, encoding=encoding, newline="")
                    for chunk in iter(lambda: text.read(1 << 20), ""):
                        sink.write(chunk.encode())
                else:
                    shutil.copyfileobj(source, sink, 1 << 20)
    partial.replace(out)
    return out


def to_parquet(select, out: Path):
    """Keep only what a query needs, once, as Parquet: a register shipped as
    tens of GB of text is then read in seconds on every rebuild."""
    if not out.is_file():
        partial = out.with_name(out.name + ".part")
        duckdb.sql(f"copy ({select}) to '{partial.as_posix()}' (format parquet, compression zstd)")
        partial.replace(out)
    return out


def paths(directory: Path, pattern):
    return [path.as_posix() for path in sorted(directory.glob(pattern))]


def standard(module, files):
    """The adapter's rows with every column in COLUMNS, missing ones null."""
    sql = module.query(files)
    present = {row[0] for row in duckdb.sql(f"describe {sql}").fetchall()}
    select = []
    for column in COLUMNS:
        if column not in present:
            value = "false" if column in ("unit", "estimated") else "null"
            select.append(f"{value} as {column}")
        elif column in ("x", "y"):
            select.append(f"try_cast({column} as double) as {column}")
        elif column in ("unit", "estimated"):
            select.append(f"coalesce({column}, false) as {column}")
        else:
            select.append(f"nullif(trim(cast({column} as varchar)), '') as {column}")
    return f"(select {', '.join(select)} from ({sql}))"
