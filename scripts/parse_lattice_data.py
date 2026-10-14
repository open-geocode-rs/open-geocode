"""Regenerate the vendored data of the lattice address parser.

Writes two files under src/parse/lattice/data/:

- postcodes.tsv: one `CC<TAB>regex` row per country, the `zip` pattern of
  Google's libaddressinput country data (Apache 2.0), from its
  testdata/countryinfo.txt.
- templates.tsv: one `CC<TAB>template` row per country, the address template
  of OpenCageData address-formatting conf/countries/worldwide.yaml (MIT), with
  YAML anchors and `use_country` resolved and line breaks written as `\\n`.

Both upstream files are read as data only. Usage:

    python -I scripts/parse_lattice_data.py \\
        --countryinfo countryinfo.txt --worldwide worldwide.yaml \\
        --out-dir src/parse/lattice/data
"""

import argparse
import json
import os

import yaml


def postcodes(path):
    rows = {}
    with open(path, encoding="utf-8") as file:
        for line in file:
            key, _, value = line.rstrip("\n").partition("=")
            parts = key.split("/")
            if len(parts) != 2 or parts[0] != "data" or len(parts[1]) != 2:
                continue
            data = json.loads(value)
            if data.get("zip"):
                rows[parts[1]] = data["zip"]
    return rows


def templates(path):
    with open(path, encoding="utf-8") as file:
        config = yaml.safe_load(file)
    rows = {}
    for code, entry in config.items():
        if len(code) != 2 or not code.isupper() or not isinstance(entry, dict):
            continue
        seen = set()
        while "use_country" in entry and code not in seen:
            seen.add(code)
            entry = config[str(entry["use_country"])]
        template = entry.get("address_template")
        if template:
            rows[code] = template
    if isinstance(config.get("default"), dict):
        rows["ZZ"] = config["default"]["address_template"]
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--countryinfo", required=True)
    parser.add_argument("--worldwide", required=True)
    parser.add_argument("--out-dir", required=True)
    args = parser.parse_args()
    with open(os.path.join(args.out_dir, "postcodes.tsv"), "w", encoding="utf-8", newline="\n") as out:
        for code, pattern in sorted(postcodes(args.countryinfo).items()):
            out.write(f"{code}\t{pattern}\n")
    with open(os.path.join(args.out_dir, "templates.tsv"), "w", encoding="utf-8", newline="\n") as out:
        for code, template in sorted(templates(args.worldwide).items()):
            lines = [" ".join(line.split()) for line in template.strip().splitlines()]
            out.write(code + "\t" + "\\n".join(line for line in lines if line) + "\n")


if __name__ == "__main__":
    main()
