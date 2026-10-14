"""Regenerate the vendored word lists and numeral table of the lattice parser.

Writes under src/parse/lattice/data/:

- libpostal/<list>.txt: the phrases of libpostal's open dictionaries
  (resources/dictionaries/<lang>/<file>.txt, MIT), every language of each
  list in one file, a `# <lang>` line before each language's phrases, one
  phrase per line with its `|`-separated variants. Variants written in
  brackets (`(株)`, `（一）`) are abbreviations of a company or road class,
  not words a query writes on their own, and are dropped.
- LICENSE-libpostal.txt: libpostal's licence.
- numerals.tsv: `character<TAB>value` rows, the ideographs whose Unihan
  kPrimaryNumeric value is at most 10000 (Unicode License V3). Formal and
  other numeric readings (kAccountingNumeric, kOtherNumeric) are left out:
  most of those characters are common words with a numeric reading on the
  side.
- LICENSE-unicode.txt: the Unicode licence.

Every upstream file is read as data only. Usage:

    python -I scripts/parse_lattice_words.py \\
        --dictionaries libpostal/resources/dictionaries \\
        --libpostal-license libpostal/LICENSE \\
        --unihan Unihan_NumericValues.txt --unicode-license license.txt \\
        --out-dir src/parse/lattice/data
"""

import argparse
import os
import re

# Each vendored list and the libpostal files it gathers.
LISTS = {
    "unit_types": ["unit_types_numbered", "unit_types_standalone"],
    "po_boxes": ["post_office"],
    "level_types": [
        "level_types_numbered",
        "level_types_standalone",
        "level_types_basement",
        "level_types_mezzanine",
        "level_types_sub_basement",
    ],
    "building_types": ["building_types"],
    "street_types": ["street_types"],
    "directionals": ["directionals"],
    "qualifiers": ["qualifiers"],
}

BRACKETED = re.compile(r"^\s*[\(（\[].*[\)）\]]\s*$")
MAX_NUMERAL = 10000


def phrases(path):
    out = []
    with open(path, encoding="utf-8") as file:
        for line in file:
            variants = [v.strip() for v in line.strip().split("|")]
            variants = [v for v in variants if v and not BRACKETED.match(v)]
            if variants:
                out.append("|".join(dict.fromkeys(variants)))
    return out


def numerals(path):
    rows = []
    with open(path, encoding="utf-8") as file:
        for line in file:
            parts = line.rstrip("\n").split("\t")
            if len(parts) != 3 or parts[1] != "kPrimaryNumeric":
                continue
            value = int(parts[2].split()[0])
            if value <= MAX_NUMERAL:
                rows.append((chr(int(parts[0][2:], 16)), value))
    return sorted(rows, key=lambda row: (row[1], row[0]))


def copy(source, target):
    with open(source, encoding="utf-8") as file:
        text = file.read()
    with open(target, "w", encoding="utf-8", newline="\n") as out:
        out.write(text)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--dictionaries", required=True)
    parser.add_argument("--libpostal-license", required=True)
    parser.add_argument("--unihan", required=True)
    parser.add_argument("--unicode-license", required=True)
    parser.add_argument("--out-dir", required=True)
    args = parser.parse_args()
    os.makedirs(os.path.join(args.out_dir, "libpostal"), exist_ok=True)
    languages = sorted(
        name
        for name in os.listdir(args.dictionaries)
        if os.path.isdir(os.path.join(args.dictionaries, name))
    )
    for name, files in LISTS.items():
        target = os.path.join(args.out_dir, "libpostal", name + ".txt")
        with open(target, "w", encoding="utf-8", newline="\n") as out:
            out.write(f"# libpostal resources/dictionaries/*/{{{','.join(files)}}}.txt (MIT)\n")
            for language in languages:
                rows = []
                for file in files:
                    path = os.path.join(args.dictionaries, language, file + ".txt")
                    if os.path.isfile(path):
                        rows.extend(phrases(path))
                if rows:
                    out.write(f"# {language}\n")
                    for row in dict.fromkeys(rows):
                        out.write(row + "\n")
    copy(args.libpostal_license, os.path.join(args.out_dir, "LICENSE-libpostal.txt"))
    copy(args.unicode_license, os.path.join(args.out_dir, "LICENSE-unicode.txt"))
    with open(os.path.join(args.out_dir, "numerals.tsv"), "w", encoding="utf-8", newline="\n") as out:
        for character, value in numerals(args.unihan):
            out.write(f"{character}\t{value}\n")


if __name__ == "__main__":
    main()
